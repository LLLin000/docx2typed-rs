# rust_migrate_gate.ps1 - issue #56 differential gate for the Rust
# inspect + schema-1 migration tracer (rust-side reproduction of
# qualification/plan.json check "migrate" against the frozen protocol).
#
# Drives the Rust binary and the Python Reference through their public
# seams over ONE real Python schema-1 workdir (produced by
# `python -m scripts extract corpus/release/plain.docx`) and proves:
#   - classification parity: Rust inspect data payload deep-equals the
#     Python Reference inspect payload (ready / non-clean / opaque /
#     unknown-feature / source-drift / symlink classifications)
#   - migration preservation: byte-identical target template, manifest
#     lineage identities (inventory_sha256 + semantic manifest hashes)
#     cross-language identical, manifest equivalent minus producer
#     provenance, all five staged checks pass, evidence sidecar present
#   - source immutability: bytes, mtimes, and file set unchanged
#   - clean target no-op build (output == template bytes) and verify rc=0
#   - non-clean target preserves the source's build block (edit-dirty)
#   - unknown required feature fails closed with no normal target
#   - symlink/junction rejection fails closed with symlink-detected
#
# Usage: powershell -NoProfile -ExecutionPolicy Bypass -File qualification/rust_migrate_gate.ps1
#        [-Bin ..\target\release\docx2typed.exe] [-Fixture ..\corpus\release\plain.docx]
#        [-Evidence ..\qualification\evidence\rust_migrate_evidence.json]

param(
    [string]$Bin = "target\release\docx2typed.exe",
    [string]$Fixture = "corpus\release\plain.docx",
    [string]$Evidence = "qualification\evidence\rust_migrate_evidence.json"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$bin = Join-Path $root $Bin
$fixture = Join-Path $root $Fixture
if (-not (Test-Path $bin)) { throw "binary not found: $bin" }
if (-not (Test-Path $fixture)) { throw "fixture not found: $fixture" }

$scratch = Join-Path $env:TEMP ("rust-migrate-" + [guid]::NewGuid().ToString("N"))
$workdir = Join-Path $scratch "wd"
$rustTarget = Join-Path $scratch "rust-target"
$pythonTarget = Join-Path $scratch "python-target"
$buildOut = Join-Path $scratch "out.docx"
$operationId = "0" * 32
New-Item -ItemType Directory -Path $scratch -Force | Out-Null

function Write-Utf8NoBom([string]$path, [string]$content) {
    $utf8 = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($path, $content, $utf8)
}

function Get-Sha256([string]$path) {
    return (Get-FileHash -Algorithm SHA256 -Path $path).Hash.ToLower()
}

function Get-Snapshot([string]$dir) {
    $map = @{}
    Get-ChildItem -Path $dir -Recurse -File | ForEach-Object {
        $rel = $_.FullName.Substring($dir.Length).TrimStart('\').Replace('\', '/')
        $map[$rel] = @($_.Length, $_.LastWriteTimeUtc.Ticks)
    }
    return $map
}

function Invoke-Rust([string[]]$arguments) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $bin
    $psi.Arguments = ($arguments | ForEach-Object {
        if ($_ -match '\s') { '"' + $_ + '"' } else { $_ }
    }) -join ' '
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $proc = New-Object System.Diagnostics.Process
    $proc.StartInfo = $psi
    [void]$proc.Start()
    $stdout = $proc.StandardOutput.ReadToEnd()
    $stderr = $proc.StandardError.ReadToEnd()
    $proc.WaitForExit()
    return @{ rc = $proc.ExitCode; stdout = $stdout; stderr = $stderr }
}

function Invoke-Python([string[]]$arguments) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = "python"
    $psi.Arguments = ($arguments | ForEach-Object {
        if ($_ -match '\s') { '"' + $_ + '"' } else { $_ }
    }) -join ' '
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.WorkingDirectory = $root
    # The child must WRITE utf-8. These oracles print document text, and a CI
    # console is cp1252: without this they die with UnicodeEncodeError on the
    # first non-ASCII character (found on tracer59 printing 审稿人).
    $psi.EnvironmentVariables["PYTHONIOENCODING"] = "utf-8"
    $proc = New-Object System.Diagnostics.Process
    $proc.StartInfo = $psi
    [void]$proc.Start()
    $stdout = $proc.StandardOutput.ReadToEnd()
    $stderr = $proc.StandardError.ReadToEnd()
    $proc.WaitForExit()
    return @{ rc = $proc.ExitCode; stdout = $stdout; stderr = $stderr }
}

$checks = @()

# --- 1. Python Reference extract (the real schema-1 workdir) ----------------
$extract = Invoke-Python @("-m", "scripts", "extract", $fixture, "-o", $workdir)
if ($extract.rc -ne 0) { throw "python extract failed: $($extract.stderr)" }

$sourceBefore = Get-Snapshot $workdir
$sourceTemplateSha = Get-Sha256 (Join-Path $workdir "_template.docx")

# --- 2. classification parity -------------------------------------------------
$rustInspect = Invoke-Rust @("inspect", "--json", $workdir)
$rustData = $rustInspect.stdout | ConvertFrom-Json
$pythonInspect = Invoke-Python @("-m", "scripts", "--json", "inspect", $workdir)
$pythonData = $pythonInspect.stdout | ConvertFrom-Json
$classificationEqual = ($rustInspect.rc -eq 0) -and ($pythonInspect.rc -eq 0) -and
    ((ConvertTo-Json $rustData.data -Depth 20 -Compress) -eq (ConvertTo-Json $pythonData.data -Depth 20 -Compress))
$checks += @{ name = "classification parity (rust inspect deep-equals python)"; pass = $classificationEqual }
$checks += @{ name = "readiness ready"; pass = ($rustData.data.readiness -eq "ready") }
$checks += @{ name = "reason codes [ok]"; pass = (($rustData.data.reason_codes -join ',') -eq "ok") }
$checks += @{ name = "edit state clean"; pass = ($rustData.data.semantic_state.edit.state -eq "clean") }

# --- 3. rust migrate -> rust target -------------------------------------------
$migrate = Invoke-Rust @("migrate", "--json", $workdir, "--out", $rustTarget, "--operation-id", $operationId)
$migrateEnvelope = $migrate.stdout | ConvertFrom-Json
$rustManifest = Get-Content (Join-Path $rustTarget "workdir.manifest.json") -Raw | ConvertFrom-Json
$checks += @{ name = "migrate rc=0"; pass = ($migrate.rc -eq 0 -and $migrateEnvelope.outcome -eq "success") }
$checks += @{ name = "target template bytes == source template"; pass = ((Get-Sha256 (Join-Path $rustTarget "_template.docx")) -eq $sourceTemplateSha) }
$checks += @{ name = "manifest schema docx2typed-workdir-manifest-1"; pass = ($rustManifest.schema -eq "docx2typed-workdir-manifest-1") }
$checks += @{ name = "staged checks all pass"; pass = (($rustManifest.checks | Where-Object { $_.status -ne "pass" }).Count -eq 0) }
$checks += @{ name = "evidence sidecar published"; pass = (Test-Path ($rustTarget + ".migrate.evidence.json")) }
$rustIdentity = $rustManifest.source.identity
$rustSemanticManifest = $rustManifest.source.semantic_manifest_sha256

# --- 4. python migrate -> python target (same operation_id) -------------------
$pyMigrateScript = Join-Path $scratch "py_migrate.py"
Write-Utf8NoBom $pyMigrateScript @"
import sys
sys.path.insert(0, r'$root')
from scripts.inspect_migrate import migrate_workdir
migrate_workdir(r'$workdir', r'$pythonTarget', operation_id=r'$operationId',
                evidence_path=r'$pythonTarget.migrate.evidence.json')
"@
$pyMigrate = Invoke-Python @($pyMigrateScript)
if ($pyMigrate.rc -ne 0) { throw "python migrate failed: $($pyMigrate.stderr)" }
$pythonManifest = Get-Content (Join-Path $pythonTarget "workdir.manifest.json") -Raw | ConvertFrom-Json
$checks += @{ name = "lineage identity matches python (inventory_sha256)"; pass = ($rustIdentity -eq $pythonManifest.source.identity) }
$checks += @{ name = "semantic manifest hash matches python"; pass = ($rustSemanticManifest -eq $pythonManifest.source.semantic_manifest_sha256) }
$checks += @{ name = "target template == python target template"; pass = ((Get-Sha256 (Join-Path $rustTarget "_template.docx")) -eq (Get-Sha256 (Join-Path $pythonTarget "_template.docx"))) }

# manifest equivalence minus producer provenance (order-insensitive compare)
$manifestCompare = Join-Path $scratch "compare_manifests.py"
Write-Utf8NoBom $manifestCompare @"
import json
a = json.load(open(r'$rustTarget/workdir.manifest.json', encoding='utf-8'))
b = json.load(open(r'$pythonTarget/workdir.manifest.json', encoding='utf-8'))
a.pop('producer', None)
b.pop('producer', None)
print('equal' if a == b else 'diff')
"@
$manifestCompareRun = Invoke-Python @($manifestCompare)
$checks += @{ name = "manifest equivalent to python minus producer"; pass = ($manifestCompareRun.stdout.Trim() -eq "equal") }

# --- 5. clean target no-op build + verify --------------------------------------
$build = Invoke-Rust @("build", "--json", $rustTarget, "-o", $buildOut)
$buildEnvelope = $build.stdout | ConvertFrom-Json
$buildOutSha = Get-Sha256 $buildOut
$checks += @{ name = "clean target build rc=0"; pass = ($build.rc -eq 0 -and $buildEnvelope.outcome -eq "success") }
$checks += @{ name = "no-op build output == template bytes"; pass = ($buildOutSha -eq $sourceTemplateSha) }
$verify = Invoke-Rust @("verify", "--json", $rustTarget, $buildOut)
$verifyEnvelope = $verify.stdout | ConvertFrom-Json
$checks += @{ name = "clean target verify rc=0"; pass = ($verify.rc -eq 0 -and $verifyEnvelope.outcome -eq "success") }

# --- 6. source immutability -----------------------------------------------------
$sourceAfter = Get-Snapshot $workdir
$sourceEqual = ((ConvertTo-Json $sourceBefore -Compress) -eq (ConvertTo-Json $sourceAfter -Compress))
$checks += @{ name = "source bytes + mtimes + file set unchanged"; pass = $sourceEqual }

# --- 7. non-clean preservation + build block ------------------------------------
$dirtyWorkdir = Join-Path $scratch "wd-dirty"
Copy-Item -Path $workdir -Destination $dirtyWorkdir -Recurse
$editMd = Join-Path $dirtyWorkdir "edit.md"
$text = Get-Content $editMd -Raw -Encoding UTF8
$markerStart = $text.IndexOf("<!--@p id=")
$close = $text.IndexOf("-->", $markerStart)
$tail = $text.IndexOf("`n", $close) + 1
$end = $text.IndexOf("`n", $tail)
if ($end -lt 0) { $end = $text.Length }
$text = $text.Substring(0, $tail) + $text.Substring($tail, $end - $tail) + "x" + $text.Substring($end)
Write-Utf8NoBom $editMd $text
$dirtyTarget = Join-Path $scratch "dirty-target"
$dirtyMigrate = Invoke-Rust @("migrate", "--json", $dirtyWorkdir, "--out", $dirtyTarget, "--operation-id", ("1" * 32))
$dirtyManifest = Get-Content (Join-Path $dirtyTarget "workdir.manifest.json") -Raw | ConvertFrom-Json
$dirtyBuild = Invoke-Rust @("build", "--json", $dirtyTarget, "-o", (Join-Path $scratch "dirty-out.docx"))
$dirtyBuildEnvelope = $dirtyBuild.stdout | ConvertFrom-Json
$checks += @{ name = "non-clean migrate rc=0, dirty preserved"; pass = ($dirtyMigrate.rc -eq 0 -and $dirtyManifest.state.edit.state -eq "dirty") }
$checks += @{ name = "non-clean target build refused (edit-dirty)"; pass = ($dirtyBuild.rc -eq 1 -and $dirtyBuildEnvelope.diagnostics[0].code -eq "edit-dirty") }

# --- 8. unknown required feature fails closed ------------------------------------
$featureWorkdir = Join-Path $scratch "wd-feature"
Copy-Item -Path $workdir -Destination $featureWorkdir -Recurse
$formatPath = Join-Path $featureWorkdir "format.json"
$format = Get-Content $formatPath -Raw | ConvertFrom-Json
$format | Add-Member -NotePropertyName required_features -NotePropertyValue @("hybrid-fidelity", "made-up-feature") -Force
Write-Utf8NoBom $formatPath (ConvertTo-Json $format -Depth 20)
$featureTarget = Join-Path $scratch "feature-target"
$featureMigrate = Invoke-Rust @("migrate", "--json", $featureWorkdir, "--out", $featureTarget, "--operation-id", ("2" * 32))
$featureEnvelope = $featureMigrate.stdout | ConvertFrom-Json
$checks += @{ name = "unknown feature fails closed (required-feature-unsupported)"; pass = ($featureMigrate.rc -eq 1 -and $featureEnvelope.diagnostics[0].code -eq "required-feature-unsupported") }
$checks += @{ name = "failed publication leaves no target"; pass = (-not (Test-Path $featureTarget)) }

# --- 9. symlink/junction rejection (best-effort on this host) ---------------------
$junctionLink = Join-Path $workdir "linkdir"
$junctionCreated = $false
try {
    New-Item -ItemType Junction -Path $junctionLink -Target $workdir -ErrorAction Stop | Out-Null
    $junctionCreated = $true
} catch { }
if ($junctionCreated) {
    $linkInspect = Invoke-Rust @("inspect", "--json", $workdir)
    $linkData = $linkInspect.stdout | ConvertFrom-Json
    $linkTarget = Join-Path $scratch "link-target"
    $linkMigrate = Invoke-Rust @("migrate", "--json", $workdir, "--out", $linkTarget, "--operation-id", ("3" * 32))
    $linkEnvelope = $linkMigrate.stdout | ConvertFrom-Json
    $checks += @{ name = "junction blocked (symlink-detected)"; pass = ($linkData.data.readiness -eq "blocked" -and ($linkData.data.reason_codes -contains "symlink-detected")) }
    $checks += @{ name = "junction migrate fails closed, no target"; pass = ($linkMigrate.rc -eq 1 -and $linkEnvelope.diagnostics[0].code -eq "symlink-detected" -and -not (Test-Path $linkTarget)) }
    # Directory.Delete removes the junction link itself without following it.
    [System.IO.Directory]::Delete($junctionLink) | Out-Null
} else {
    $checks += @{ name = "junction blocked (symlink-detected)"; pass = $true; skipped = "host cannot create junctions" }
    $checks += @{ name = "junction migrate fails closed, no target"; pass = $true; skipped = "host cannot create junctions" }
}

# --- verdict -----------------------------------------------------------------------
$failures = @($checks | Where-Object { -not $_.pass })
$verdict = if ($failures.Count -eq 0) { "pass" } else { "fail" }
$report = [ordered]@{
    check = "migrate"
    engine = "rust"
    verdict = $verdict
    classification_parity = $classificationEqual
    readiness = $rustData.data.readiness
    reason_codes = $rustData.data.reason_codes
    edit_state = $rustData.data.semantic_state.edit.state
    source_inventory_sha256 = $rustIdentity
    source_semantic_manifest_sha256 = $rustSemanticManifest
    template_sha256 = $sourceTemplateSha
    python_target_template_sha256 = (Get-Sha256 (Join-Path $pythonTarget "_template.docx"))
    rust_target_template_sha256 = (Get-Sha256 (Join-Path $rustTarget "_template.docx"))
    noop_build_output_sha256 = $buildOutSha
    frozen_fixture_sha256 = "4323e37b7ac7e9dbce7b4923d14529bda821f0d66f0dce7005cf9299bf8d9c39"
    checks = $checks
}
$reportJson = $report | ConvertTo-Json -Depth 8
Write-Utf8NoBom (Join-Path $root $Evidence) $reportJson
$reportJson
if ($failures.Count -gt 0) {
    Write-Host "FAILED gates: $($failures.name -join ', ')" -ForegroundColor Red
    Remove-Item -Recurse -Force $scratch
    exit 1
}
Write-Host "ALL GATES PASS" -ForegroundColor Green
Remove-Item -Recurse -Force $scratch
exit 0
