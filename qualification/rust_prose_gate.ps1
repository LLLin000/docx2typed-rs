# rust_prose_gate.ps1 - issue #58 differential gate: recursive prose
# enumeration, island-local text edits, opaque locking, and the
# edit-build-verify chain against the frozen Python Reference semantics
# (scripts/typed_docx.py + scripts/edit_sync.py, PRD decision 17 hybrid
# fidelity).
#
# For each nested fixture (table/boxes/parts/complex) the gate runs the SAME
# semantic text edit through (a) the Rust tracer chain (extract -> `edit
# text` generation commit -> build -> independent verify) and (b) the Python
# Reference chain (extract -> edit_sync -> build -> verify), then proves:
#   - semantic signature parity (visible text per leaf, style spans per
#     leaf, opaque count, anchor inventory — computed by the oracle with the
#     identical Python model for both outputs),
#   - per-part byte identity of untouched parts (each output vs the source),
#   - opaque-lock rejection (locked leaf -> opaque-paragraph-mutated),
#   - cross-island/ambiguous rejection (-> invalid-edit / prose-edit-ambiguous),
#   - global invariant rejection (tampered sidecar -> build fails, no output),
#   - S-profile resource gate (complete edit-build-verify chain <= 35 s).
# Evidence JSON: qualification/evidence/rust_prose_evidence.json.
#
# Usage: powershell -NoProfile -ExecutionPolicy Bypass -File qualification/rust_prose_gate.ps1
#        [-Bin ..\target\release\docx2typed.exe]
#        [-Evidence ..\qualification\evidence\rust_prose_evidence.json]

param(
    [string]$Bin = "target\release\docx2typed.exe",
    [string]$Evidence = "qualification\evidence\rust_prose_evidence.json"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$bin = Join-Path $root $Bin
$oracle = Join-Path $PSScriptRoot "rust_prose_oracle.py"

if (-not (Test-Path $bin)) {
    Write-Host "binary not found: $bin (building release first)"
    Push-Location $root
    cargo build --release | Out-Null
    Pop-Location
    if (-not (Test-Path $bin)) { throw "release build did not produce $bin" }
} else {
    # Always rebuild: the gate must run the CURRENT source (a stale release
    # binary silently degrades the evidence).
    Write-Host "rebuilding release binary (current source)"
    Push-Location $root
    cargo build --release | Out-Null
    Pop-Location
}

$scratch = Join-Path $env:TEMP ("rust-prose-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $scratch -Force | Out-Null

function Invoke-Rust([string[]]$arguments, [hashtable]$environment = @{}) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $bin
    $psi.Arguments = (($arguments | ForEach-Object { '"' + $_.Replace('"', '""') + '"' }) -join " ")
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    foreach ($key in $environment.Keys) {
        $psi.EnvironmentVariables[$key] = $environment[$key]
    }
    $psi.StandardOutputEncoding = [System.Text.Encoding]::UTF8
    $psi.StandardErrorEncoding = [System.Text.Encoding]::UTF8
    $proc = [System.Diagnostics.Process]::Start($psi)
    $stdout = $proc.StandardOutput.ReadToEnd()
    $stderr = $proc.StandardError.ReadToEnd()
    $proc.WaitForExit()
    return @{ rc = $proc.ExitCode; stdout = $stdout; stderr = $stderr }
}

function Invoke-Python([string[]]$arguments) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = "python"
    $psi.Arguments = (($arguments | ForEach-Object { '"' + $_.Replace('"', '""') + '"' }) -join " ")
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.StandardOutputEncoding = [System.Text.Encoding]::UTF8
    $psi.StandardErrorEncoding = [System.Text.Encoding]::UTF8
    # The child must WRITE utf-8 too. These oracles print document text, and a
    # CI console is cp1252: without this they die with UnicodeEncodeError on the
    # first non-ASCII character (found on tracer59 printing 审稿人). Setting
    # StandardOutputEncoding above only fixes how WE decode their bytes.
    $psi.EnvironmentVariables["PYTHONIOENCODING"] = "utf-8"
    $proc = [System.Diagnostics.Process]::Start($psi)
    $stdout = $proc.StandardOutput.ReadToEnd()
    $stderr = $proc.StandardError.ReadToEnd()
    $proc.WaitForExit()
    return @{ rc = $proc.ExitCode; stdout = $stdout; stderr = $stderr }
}

function Write-Utf8NoBom([string]$path, [string]$content) {
    $utf8 = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($path, $content, $utf8)
}

function Get-PartHashes([string]$path) {
    $python = @"
import zipfile, hashlib, json, sys
z = zipfile.ZipFile(r'$path')
print(json.dumps({n: hashlib.sha256(z.read(n)).hexdigest() for n in z.namelist()}))
"@
    $script = Join-Path $scratch "parts-$([guid]::NewGuid().ToString('N')).py"
    Write-Utf8NoBom $script $python
    $out = Invoke-Python @($script)
    if ($out.rc -ne 0) { throw "part hash helper failed: $($out.stderr)" }
    return ($out.stdout | ConvertFrom-Json)
}

function Get-UntouchedDiffs([string]$source, [string]$output, [string]$editedPart) {
    $sourceHashes = Get-PartHashes $source
    $outputHashes = Get-PartHashes $output
    $diffs = @()
    foreach ($name in $sourceHashes.PSObject.Properties.Name) {
        $sourceHash = $sourceHashes.$name
        $outputHash = $outputHashes.$name
        if ($null -eq $outputHash) { $diffs += "$name(removed)" }
        elseif ($sourceHash -ne $outputHash -and $name -ne $editedPart) { $diffs += $name }
    }
    foreach ($name in $outputHashes.PSObject.Properties.Name) {
        if ($null -eq $sourceHashes.$name) { $diffs += "$name(added)" }
    }
    return $diffs
}

function Read-JsonFile([string]$path) {
    $text = [System.IO.File]::ReadAllText($path)
    return ($text | ConvertFrom-Json)
}

$checks = @()
$fixtures = @(
    @{ name = "table"; fixture = "corpus\release\table.docx"; leaf = "T0.R1.C1.P0.0"; old = "PVA"; new = "PLBA"; editedPart = "word/document.xml"; noTrack = $false },
    @{ name = "boxes"; fixture = "corpus\release\boxes.docx"; leaf = "B0.P0.0"; old = "框内文字"; new = "框内改字"; editedPart = "word/document.xml"; noTrack = $false },
    @{ name = "parts"; fixture = "corpus\release\parts.docx"; leaf = "header1.P0.0"; old = "Draft v1"; new = "Confidential v1"; editedPart = "word/header1.xml"; noTrack = $false },
    @{ name = "complex"; fixture = "corpus\release\complex.docx"; leaf = "T0.R0.C0.P0.0"; old = "Element"; new = "Elements"; editedPart = "word/document.xml"; noTrack = $true }
)

$fixtureEvidence = @()
foreach ($spec in $fixtures) {
    $name = $spec.name
    $fixturePath = Join-Path $root $spec.fixture
    if (-not (Test-Path $fixturePath)) { throw "fixture not found: $fixturePath" }
    $fixtureDir = Join-Path $scratch $name
    New-Item -ItemType Directory -Path $fixtureDir -Force | Out-Null
    $rustWd = Join-Path $fixtureDir "rust-wd"
    $rustOut = Join-Path $fixtureDir "rust-out.docx"
    $pyWd = Join-Path $fixtureDir "py-wd"
    $pyOut = Join-Path $fixtureDir "py-out.docx"
    $editsJson = Join-Path $fixtureDir "edits.json"
    Write-Utf8NoBom $editsJson (ConvertTo-Json @(@{ paragraph = ($spec.leaf -replace '\.\d+$', ''); old = $spec.old; new = $spec.new }))

    # ---- Rust chain: extract -> edit text (generation commit) -> build -> verify
    $chainStart = [System.Diagnostics.Stopwatch]::StartNew()
    $extract = Invoke-Rust @("extract", "--json", $fixturePath, "-o", $rustWd)
    $edit = Invoke-Rust @("edit", "text", "--json", $rustWd, $spec.leaf, $spec.old, $spec.new)
    $build = Invoke-Rust @("build", "--json", $rustWd, "-o", $rustOut)
    $verify = Invoke-Rust @("verify", "--json", $rustWd, $rustOut)
    $chainStart.Stop()
    $rustChainS = [Math]::Round($chainStart.Elapsed.TotalSeconds, 3)

    $checks += @{ name = "$name rust extract"; pass = ($extract.rc -eq 0 -and $extract.stdout.Contains('"outcome":"success"')) }
    $checks += @{ name = "$name rust island edit (generation commit)"; pass = ($edit.rc -eq 0 -and $edit.stdout.Contains('"outcome":"success"') -and (Test-Path (Join-Path $rustWd "islands.json"))) }
    $checks += @{ name = "$name rust build"; pass = ($build.rc -eq 0 -and $build.stdout.Contains('"outcome":"success"')) }
    $checks += @{ name = "$name rust independent verify"; pass = ($verify.rc -eq 0 -and $verify.stdout.Contains('"outcome":"success"')) }
    $checks += @{ name = "$name S-profile complete chain <= 35s"; pass = ($rustChainS -le 35.0); detail = "$rustChainS s" }

    # ---- Python Reference chain
    $pyResultFile = Join-Path $fixtureDir "oracle-result.json"
    $pyArgs = @($oracle, "python_touch", $spec.fixture, $pyWd, $editsJson, "--output", $pyOut, "--result-file", $pyResultFile)
    if ($spec.noTrack) { $pyArgs += "--no-track" }
    $py = Invoke-Python @($pyArgs)
    $pyResult = $null
    if ($py.rc -eq 0 -and (Test-Path $pyResultFile)) { $pyResult = Read-JsonFile $pyResultFile }
    $checks += @{ name = "$name python touched chain"; pass = ($py.rc -eq 0 -and $pyResult.ok -eq $true); detail = $(if ($pyResult.detail) { $pyResult.detail } else { "" }) }

    if ($build.rc -eq 0 -and $py.rc -eq 0 -and $pyResult.ok -eq $true) {
        # ---- Semantic signature parity (both outputs, identical Python model)
        $rustSigFile = Join-Path $fixtureDir "rust-signature.json"
        $pySigFile = Join-Path $fixtureDir "py-signature.json"
        $rustSig = Invoke-Python @($oracle, "signature", $rustOut, "--file", $rustSigFile)
        $pySig = Invoke-Python @($oracle, "signature", $pyResult.output, "--file", $pySigFile)
        $sigParity = $true
        $sigDetail = ""
        if ($rustSig.rc -ne 0 -or $pySig.rc -ne 0) { $sigParity = $false; $sigDetail = "signature computation failed" }
        elseif (-not (Test-Path $rustSigFile) -or -not (Test-Path $pySigFile)) { $sigParity = $false; $sigDetail = "signature files missing" }
        else {
            $compare = @"
import json, sys
rust = json.load(open(sys.argv[1], encoding='utf-8'))
py = json.load(open(sys.argv[2], encoding='utf-8'))
def norm(sig):
    out = []
    for p in sig['paragraphs']:
        out.append([p['id'], p['visible_text'], p['units'], p['opaque_count'], p['anchor_count']])
    return out
r, p = norm(rust), norm(py)
if r != p:
    for a, b in zip(r, p):
        if a != b:
            print(json.dumps({'id': a[0], 'rust': a, 'python': b}, ensure_ascii=False))
            break
    sys.exit(1)
if rust['opaques'] != py['opaques'] or rust['anchors'] != py['anchors']:
    print(json.dumps({'opaques': (rust['opaques'], py['opaques']), 'anchors': (rust['anchors'], py['anchors'])}))
    sys.exit(1)
print('parity')
"@
            $script = Join-Path $fixtureDir "compare.py"
            Write-Utf8NoBom $script $compare
            $cmp = Invoke-Python @($script, $rustSigFile, $pySigFile)
            if ($cmp.rc -eq 0) { $sigDetail = "signatures identical" }
            else { $sigParity = $false; $sigDetail = $cmp.stdout }
        }
        $checks += @{ name = "$name semantic signature parity"; pass = $sigParity; detail = $sigDetail }

        # ---- Untouched parts byte-identical to the source (both outputs)
        $rustDiffs = Get-UntouchedDiffs $fixturePath $rustOut $spec.editedPart
        $pyDiffs = Get-UntouchedDiffs $fixturePath $pyOut $spec.editedPart
        $checks += @{ name = "$name rust untouched parts byte-identical"; pass = ($rustDiffs.Count -eq 0); detail = ($rustDiffs -join ",") }
        $checks += @{ name = "$name python untouched parts byte-identical"; pass = ($pyDiffs.Count -eq 0); detail = ($pyDiffs -join ",") }

        # ---- Edited part must differ from the source in both outputs
        $srcHashes = Get-PartHashes $fixturePath
        $rustHashes = Get-PartHashes $rustOut
        $pyHashes = Get-PartHashes $pyOut
        $checks += @{ name = "$name edited part changed in rust output"; pass = ($rustHashes.$($spec.editedPart) -ne $srcHashes.$($spec.editedPart)) }
        $checks += @{ name = "$name edited part changed in python output"; pass = ($pyHashes.$($spec.editedPart) -ne $srcHashes.$($spec.editedPart)) }
    }

    $fixtureEvidence += @{
        name = $name
        leaf = $spec.leaf
        edited_part = $spec.editedPart
        rust_chain_s = $rustChainS
        python_chain_s = if ($pyResult) { $pyResult.elapsed_s } else { $null }
        python_touched_s = if ($pyResult) { $pyResult.elapsed_s } else { $null }
    }
}

# ---------------------------------------------------------------------------
# Opaque lock + fail-closed rejection proofs (complex.docx, the opaque-rich
# fixture)
# ---------------------------------------------------------------------------
$lockedWd = Join-Path $scratch "locked-wd"
Invoke-Rust @("extract", "--json", (Join-Path $root "corpus\release\complex.docx"), "-o", $lockedWd) | Out-Null
$locked = Invoke-Rust @("edit", "text", "--json", $lockedWd, "P12.0", "FIELD", "XXX")
$checks += @{ name = "opaque field interior edit rejected (opaque-paragraph-mutated)"; pass = ($locked.rc -eq 1 -and $locked.stdout.Contains('"code":"opaque-paragraph-mutated"')) }

$ambiguousWd = Join-Path $scratch "ambiguous-wd"
Invoke-Rust @("extract", "--json", (Join-Path $root "corpus\release\plain.docx"), "-o", $ambiguousWd) | Out-Null
$ambiguous = Invoke-Rust @("edit", "text", "--json", $ambiguousWd, "P5.0", "重复句子内容", "x")
$checks += @{ name = "ambiguous old text rejected (prose-edit-ambiguous)"; pass = ($ambiguous.rc -eq 1 -and $ambiguous.stdout.Contains('"code":"prose-edit-ambiguous"')) }

$missingWd = Join-Path $scratch "missing-wd"
Invoke-Rust @("extract", "--json", (Join-Path $root "corpus\release\plain.docx"), "-o", $missingWd) | Out-Null
$missing = Invoke-Rust @("edit", "text", "--json", $missingWd, "P0.0", "no such text here", "x")
$checks += @{ name = "cross-island old text rejected (invalid-edit)"; pass = ($missing.rc -eq 1 -and $missing.stdout.Contains('"code":"invalid-edit"')) }

# Global invariant gate: a stale/tampered sidecar referencing a locked leaf
# rejects the whole build with no output.
$invWd = Join-Path $scratch "invariant-wd"
Invoke-Rust @("extract", "--json", (Join-Path $root "corpus\release\complex.docx"), "-o", $invWd) | Out-Null
$tampered = '{"schema":"docx2typed-islands-1","edits":[{"part":"document","paragraph_id":"P12","leaf_index":0,"old":"FIELD","new":"XXX"}]}'
Write-Utf8NoBom (Join-Path $invWd "islands.json") $tampered
$invOut = Join-Path $scratch "invariant-out.docx"
$invariant = Invoke-Rust @("build", "--json", $invWd, "-o", $invOut)
$checks += @{ name = "global invariant failure rejects build (opaque-paragraph-mutated)"; pass = ($invariant.rc -eq 1 -and $invariant.stdout.Contains('"code":"opaque-paragraph-mutated"') -and -not (Test-Path $invOut)) }

# ---------------------------------------------------------------------------
# Enumeration parity: Rust enumerate vs live Python typed.md ids
# ---------------------------------------------------------------------------
$enumCount = 0
$enumPass = 0
foreach ($spec in $fixtures) {
    $enum = Invoke-Rust @("enumerate", "--json", (Join-Path $root $spec.fixture))
    if ($enum.rc -ne 0) { continue }
    $enumData = $enum.stdout | ConvertFrom-Json
    $rustIds = @($enumData.data.paragraphs | ForEach-Object { $_.id })
    $pyExtract = Join-Path $scratch ("enum-" + $spec.name)
    $py = Invoke-Python @("-m", "scripts", "extract", (Join-Path $root $spec.fixture), "-o", $pyExtract)
    $pyIds = @()
    if ($py.rc -eq 0) {
        $typed = Get-Content (Join-Path $pyExtract "typed.md") -Raw
        $matches = [regex]::Matches($typed, '<!--@p id="([^"]+)"')
        $pyIds = @($matches | ForEach-Object { $_.Groups[1].Value })
    }
    $enumCount += 1
    $ok = ($rustIds.Count -eq $pyIds.Count) -and (($rustIds -join "`n") -eq ($pyIds -join "`n"))
    if ($ok) { $enumPass += 1 }
    $checks += @{ name = "$($spec.name) enumeration parity with Python typed.md"; pass = $ok; detail = "$($rustIds.Count) paragraphs" }
}
$checks += @{ name = "enumeration parity across all fixtures"; pass = ($enumCount -gt 0 -and $enumPass -eq $enumCount); detail = "$enumPass/$enumCount" }

# ---------------------------------------------------------------------------
# Evidence
# ---------------------------------------------------------------------------
$passed = @($checks | Where-Object { $_.pass -eq $true }).Count
$failed = @($checks | Where-Object { $_.pass -ne $true }).Count
$evidencePayload = @{
    schema = "docx2typed-qual-evidence-1"
    issue = "58"
    generated = (Get-Date -Format "yyyy-MM-ddTHH:mm:ssZ")
    checks = @($checks | ForEach-Object {
        @{ name = $_.name; pass = ($_.pass -eq $true); detail = if ($_.detail) { $_.detail } else { "" } }
    })
    summary = @{ passed = $passed; failed = $failed }
    fixtures = $fixtureEvidence
    resource_profiles = @{ profile = "S"; complete_chain_budget_s = 35; rust_complete_chain_s = ($fixtureEvidence | ForEach-Object { $_.rust_chain_s }) }
    deferrals = @(
        "MCP replace_text tool deferred to #59+ (CLI edit text is the required surface)",
        "Content-control sdtPr structural edits deferred (sdt structure stays locked)",
        "Python canonical style-id (content-addressed s_...) parity deferred: the tracer reports its own raw-rPr sha256 per leaf"
    )
}
$evidencePath = Join-Path $root $Evidence
New-Item -ItemType Directory -Path (Split-Path -Parent $evidencePath) -Force | Out-Null
$evidenceJson = $evidencePayload | ConvertTo-Json -Depth 12
Write-Utf8NoBom $evidencePath $evidenceJson
Write-Host "rust_prose_gate: $passed passed, $failed failed -> $evidencePath"
if ($failed -gt 0) {
    $checks | Where-Object { $_.pass -ne $true } | ForEach-Object { Write-Host ("FAIL: " + $_.name + " :: " + ($_.detail | Out-String)) }
    exit 1
}
exit 0
