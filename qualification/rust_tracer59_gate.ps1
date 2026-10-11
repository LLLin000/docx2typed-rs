# rust_tracer59_gate.ps1 - issue #59 differential gate: governed workflows
# (revisions inventory/views, decision guards + settlement, comment
# deletion, table structure ops, Unicode normalization audit) against the
# frozen Python Reference semantics (PRD decision 17 hybrid fidelity).
#
# Evidence JSON: qualification/evidence/rust_tracer59_evidence.json.
#
# Usage: powershell -NoProfile -ExecutionPolicy Bypass -File qualification/rust_tracer59_gate.ps1
#        [-Bin ..\target\release\docx2typed.exe]

param(
    [string]$Bin = "target\release\docx2typed.exe"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$bin = Join-Path $root $Bin
$oracle = Join-Path $PSScriptRoot "rust_tracer59_oracle.py"

if (-not (Test-Path $bin)) {
    $bin = Join-Path $root "target\debug\docx2typed.exe"
}
if (-not (Test-Path $bin)) {
    throw "binary not found: $bin"
}

$scratch = Join-Path $env:TEMP ("rust-tracer59-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $scratch -Force | Out-Null

function Invoke-Rust([string[]]$arguments) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $bin
    $psi.Arguments = (($arguments | ForEach-Object { '"' + $_.Replace('"', '""') + '"' }) -join " ")
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
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

function Get-Envelope([hashtable]$result) {
    return ($result.stdout | ConvertFrom-Json)
}

function New-OpId {
    return ([guid]::NewGuid().ToString("N"))
}

function Invoke-Oracle([string]$kind, [string]$path, [string]$extra = "") {
    $argsList = @($oracle, $kind, $path)
    if ($extra) { $argsList += $extra }
    $result = Invoke-Python $argsList
    if ($result.rc -ne 0) {
        # Print the traceback line by line BEFORE throwing. A CI log wraps a
        # single long message into an unreadable block, which is how a failing
        # oracle becomes a mystery instead of a diagnosis.
        Write-Host "oracle failed: $kind $path (exit $($result.rc))"
        foreach ($line in ($result.stderr -split "`r?`n")) {
            if ($line.Trim()) { Write-Host "  $line" }
        }
        throw "oracle failed: $kind $path"
    }
    return ($result.stdout.Trim())
}

function Write-Utf8NoBom([string]$path, [string]$content) {
    $utf8 = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($path, $content, $utf8)
}

function Get-Envelope([hashtable]$result) {
    return ($result.stdout | ConvertFrom-Json)
}

function New-OpId {
    return ([guid]::NewGuid().ToString("N"))
}


function Get-CommentElement([string]$path, [string]$id) {
    $stream = [System.IO.File]::OpenRead($path)
    $add = New-Object System.IO.Compression.ZipArchive($stream)
    try {
        $entry = $add.GetEntry("word/comments.xml")
        $reader = New-Object System.IO.StreamReader($entry.Open(), [System.Text.Encoding]::UTF8)
        $text = $reader.ReadToEnd()
        $reader.Close()
    } finally {
        $add.Dispose()
        $stream.Dispose()
    }
    $marker = '<w:comment w:id="' + $id + '"'
    $start = $text.IndexOf($marker)
    $end = $text.IndexOf("</w:comment>", $start) + 13
    return $text.Substring($start, $end - $start)
}

$checks = @()
$started = Get-Date

# ---------------------------------------------------------------------------
# Fixture 1: revisions.docx - inventory + views parity
# ---------------------------------------------------------------------------
$revWd = Join-Path $scratch "rev-wd"
$rust = Invoke-Rust @("extract", "--json", (Join-Path $root "corpus\release\revisions.docx"), "-o", $revWd)
$checks += @{ name = "revisions: rust extract"; pass = ($rust.rc -eq 0) }

$rust = Invoke-Rust @("revisions", "list", "--json", $revWd)
$rustInventory = (Get-Envelope $rust).data.revisions
$oracleInventory = (Invoke-Oracle "revision-inventory" (Join-Path $root "corpus\release\revisions.docx") | ConvertFrom-Json)
$rustSig = @($rustInventory | ForEach-Object { "$($_.w_id)|$($_.kind)|$($_.author)|$($_.date)|$($_.text)|$($_.fingerprint)|$($_.revision_key)" }) -join "`n"
$oracleSig = @($oracleInventory | ForEach-Object { "$($_.w_id)|$($_.kind)|$($_.author)|$($_.date)|$($_.text)|$($_.fingerprint)|$($_.revision_key)" }) -join "`n"
$checks += @{ name = "revisions: inventory parity (8 revisions, keys+fingerprints)"; pass = ($rust.rc -eq 0 -and $rustSig -eq $oracleSig) }

foreach ($action in @("accept", "reject")) {
    $rust = Invoke-Rust @("revisions", "view", "--json", $revWd, $action)
    $rustView = ((Get-Envelope $rust).data.paragraphs | ForEach-Object { $_.text }) -join "`n"
    $oracleView = ((Invoke-Oracle "view-text" (Join-Path $root "corpus\release\revisions.docx") $action | ConvertFrom-Json) -join "`n")
    $checks += @{ name = "revisions: view-$action parity"; pass = ($rust.rc -eq 0 -and $rustView -eq $oracleView) }
}

# ---------------------------------------------------------------------------
# Fixture 1b: decision guards fail closed with frozen codes + no side effect
# ---------------------------------------------------------------------------
$beforeTemplate = [System.IO.File]::ReadAllBytes((Join-Path $revWd "_template.docx"))
$stale = Invoke-Rust @("decide", "accept", "word/document.xml|insert|1|888c104169b5", "--json", "--workdir", $revWd, "--fingerprint", "deadbeef0000", "--operation-id", (New-OpId))
$checks += @{ name = "decide: stale fingerprint -> revision-fingerprint-mismatch"; pass = ($stale.rc -eq 1 -and $stale.stdout.Contains('"code":"revision-fingerprint-mismatch"')) }
$unknown = Invoke-Rust @("decide", "accept", "word/document.xml|insert|99|000000000000", "--json", "--workdir", $revWd, "--fingerprint", "000000000000", "--operation-id", (New-OpId))
$checks += @{ name = "decide: unknown key -> revision-not-found"; pass = ($unknown.rc -eq 1 -and $unknown.stdout.Contains('"code":"revision-not-found"')) }
$mark = Invoke-Rust @("decide", "accept", "word/document.xml|insert|3|e3b0c44298fc", "--json", "--workdir", $revWd, "--fingerprint", "e3b0c44298fc", "--operation-id", (New-OpId))
$checks += @{ name = "decide: paragraph-mark -> revision-not-found (Python parity)"; pass = ($mark.rc -eq 1 -and $mark.stdout.Contains('"code":"revision-not-found"')) }
$afterTemplate = [System.IO.File]::ReadAllBytes((Join-Path $revWd "_template.docx"))
$checks += @{ name = "decide: guards leave workdir bytes unchanged"; pass = (($beforeTemplate -join ",") -eq ($afterTemplate -join ",")) }

# ---------------------------------------------------------------------------
# Fixture 1c: Rust decide accept vs Python decide accept outcome parity
# ---------------------------------------------------------------------------
$rust = Invoke-Rust @("decide", "accept", "word/document.xml|insert|1|888c104169b5", "--json", "--workdir", $revWd, "--fingerprint", "888c104169b5", "--operation-id", (New-OpId))
$checks += @{ name = "decide: rust accept succeeds"; pass = ($rust.rc -eq 0) }
$rustOut = Join-Path $scratch "rust-accept.docx"
$rust = Invoke-Rust @("build", "--json", $revWd, "-o", $rustOut, "--operation-id", (New-OpId))
$checks += @{ name = "decide: rust build after accept"; pass = ($rust.rc -eq 0) }
$rustVerify = Invoke-Rust @("verify", "--json", $revWd, $rustOut)
$checks += @{ name = "decide: rust independent verify passes"; pass = ($rustVerify.rc -eq 0 -and $rustVerify.stdout.Contains('"outcome":"success"')) }

$pyWd = Join-Path $scratch "py-rev-wd"
$py = Invoke-Python @("-m", "scripts", "extract", "--json", (Join-Path $root "corpus\release\revisions.docx"), "-o", $pyWd)
$py = Invoke-Python @("-m", "scripts", "decide", "accept", "word/document.xml|insert|1|888c104169b5", "--json", "--workdir", $pyWd, "--fingerprint", "888c104169b5")
$pyOut = Join-Path $scratch "py-accept.docx"
$py = Invoke-Python @("-m", "scripts", "build", "--json", $pyWd, "-o", $pyOut)

$rustText = (Invoke-Oracle "view-text" $rustOut "accept" | ConvertFrom-Json) -join "`n"
$pyText = (Invoke-Oracle "view-text" $pyOut "accept" | ConvertFrom-Json) -join "`n"
$checks += @{ name = "decide: accept outcome parity (visible text, Python model on both)"; pass = ($rustText -eq $pyText) }

# ---------------------------------------------------------------------------
# Fixture 2: comments.docx - inventory + deletion parity + byte evidence
# ---------------------------------------------------------------------------
$comWd = Join-Path $scratch "com-wd"
$rust = Invoke-Rust @("extract", "--json", (Join-Path $root "corpus\release\comments.docx"), "-o", $comWd)
$rust = Invoke-Rust @("comment", "list", "--json", $comWd)
$rustComments = ((Get-Envelope $rust).data.comments | ForEach-Object { "$($_.id)|$($_.author)|$($_.text)" }) -join "`n"
$oracleComments = ((Invoke-Oracle "comment-inventory" (Join-Path $root "corpus\release\comments.docx") | ConvertFrom-Json) | ForEach-Object { "$($_.id)|$($_.author)|$($_.text)" }) -join "`n"
$checks += @{ name = "comments: inventory parity (3 comments)"; pass = ($rust.rc -eq 0 -and $rustComments -eq $oracleComments) }

$rust = Invoke-Rust @("comment", "delete", "--json", $comWd, "1", "--operation-id", (New-OpId))
$checks += @{ name = "comments: rust delete comment 1"; pass = ($rust.rc -eq 0) }
$rustComOut = Join-Path $scratch "rust-com.docx"
$rust = Invoke-Rust @("build", "--json", $comWd, "-o", $rustComOut, "--operation-id", (New-OpId))
$rust = Invoke-Rust @("verify", "--json", $comWd, $rustComOut)
$checks += @{ name = "comments: delete build+verify"; pass = ($rust.rc -eq 0) }

$rustCommentsAfter = ((Invoke-Oracle "comment-inventory" $rustComOut | ConvertFrom-Json) | ForEach-Object { "$($_.id)|$($_.text)" }) -join "`n"
$checks += @{ name = "comments: delete leaves 0 and 2, drops 1"; pass = ($rustCommentsAfter.Contains("0|批注一内容") -and $rustCommentsAfter.Contains("2|批注三内容") -and -not $rustCommentsAfter.Contains("1|")) }

$srcCommentPath = Join-Path $root "corpus\release\comments.docx"
$srcComment0 = (Invoke-Oracle "comment-element" $srcCommentPath "0" | ConvertFrom-Json).element
$rustComment0 = (Invoke-Oracle "comment-element" $rustComOut "0" | ConvertFrom-Json).element
$checks += @{ name = "comments: comment 0 element byte-identical after delete"; pass = ($srcComment0 -eq $rustComment0) }

# ---------------------------------------------------------------------------
# Fixture 3: table.docx - structure op parity + content-preservation guard
# ---------------------------------------------------------------------------
$tabWd = Join-Path $scratch "tab-wd"
$rust = Invoke-Rust @("extract", "--json", (Join-Path $root "corpus\release\table.docx"), "-o", $tabWd)
$rustTabOut = Join-Path $scratch "rust-tab.docx"
$rustTabWd = Join-Path $scratch "rust-tab-wd"
$rust = Invoke-Rust @("decide", "table-insert-row", "T1", "--json", "--workdir", $tabWd, "--args", "1", "--output", $rustTabOut, "--workdir-out", $rustTabWd, "--operation-id", (New-OpId))
$checks += @{ name = "tables: rust insert-row T1 new baseline"; pass = ($rust.rc -eq 0 -and (Test-Path $rustTabOut) -and (Test-Path (Join-Path $rustTabWd "_template.docx"))) }
$rustTabVerify = Invoke-Rust @("verify", "--json", $rustTabWd, $rustTabOut)
$checks += @{ name = "tables: new baseline verifies"; pass = ($rustTabVerify.rc -eq 0) }

$pyTabWd = Join-Path $scratch "py-tab-wd"
$py = Invoke-Python @("-m", "scripts", "extract", "--json", (Join-Path $root "corpus\release\table.docx"), "-o", $pyTabWd)
$pyTabOut = Join-Path $scratch "py-tab.docx"
$pyTabWd2 = Join-Path $scratch "py-tab-wd2"
$py = Invoke-Python @("-m", "scripts", "decide", "table-insert-row", "T1", "--json", "--workdir", $pyTabWd, "--args", "1", "--output", $pyTabOut, "--workdir-out", $pyTabWd2)

$rustCells = (Invoke-Oracle "table-cells" $rustTabOut "1" | ConvertFrom-Json) -join "`n"
$pyCells = (Invoke-Oracle "table-cells" $pyTabOut "1" | ConvertFrom-Json) -join "`n"
$checks += @{ name = "tables: insert-row outcome parity (row/cell text matrix)"; pass = ($rustCells -eq $pyCells) }

$mergeOut = Join-Path $scratch "merge.docx"
$mergeWd = Join-Path $scratch "merge-wd"
$merge = Invoke-Rust @("decide", "table-merge-cells", "T0", "--json", "--workdir", $tabWd, "--args", "0 0 2", "--output", $mergeOut, "--workdir-out", $mergeWd, "--operation-id", (New-OpId))
$checks += @{ name = "tables: merge content-loss guard -> merge-would-discard-content, no output"; pass = ($merge.rc -eq 1 -and $merge.stdout.Contains('"code":"merge-would-discard-content"') -and -not (Test-Path $mergeOut)) }

# ---------------------------------------------------------------------------
# Fixture 4: norm.docx - governed Unicode normalization audit parity
# ---------------------------------------------------------------------------
$rustAudit = Invoke-Rust @("audit", "--json", (Join-Path $root "corpus\release\norm.docx"))
$rustCandidates = ((Get-Envelope $rustAudit).data.candidates | ForEach-Object { $_.codepoint }) -join "`n"
$oracleCount = [int](Invoke-Oracle "unicode-candidates" (Join-Path $root "corpus\release\norm.docx"))
$checks += @{ name = "audit: rust candidate count matches oracle ($oracleCount)"; pass = ($rustAudit.rc -eq 0 -and $rustCandidates.Length -gt 0 -and ($rustCandidates -split "`n").Count -eq $oracleCount) }
$checks += @{ name = "audit: read-only (no standalone normalize surface)"; pass = (-not $rustAudit.stdout.Contains('"operation":"normalize"')) }

# ---------------------------------------------------------------------------
# S-profile resource gate
# ---------------------------------------------------------------------------
$elapsed = ((Get-Date) - $started).TotalSeconds
$checks += @{ name = "resource: governed chains within S profile (<= 120 s)"; pass = ($elapsed -le 120); detail = "$([math]::Round($elapsed, 1))s" }

# ---------------------------------------------------------------------------
# Evidence
# ---------------------------------------------------------------------------
$passed = @($checks | Where-Object { $_.pass -eq $true }).Count
$failed = @($checks | Where-Object { $_.pass -ne $true }).Count
$evidencePayload = @{
    schema = "docx2typed-qualification-evidence-1"
    gate = "rust_tracer59_gate.ps1"
    issue = 59
    generated = (Get-Date).ToString("o")
    binary = $bin
    elapsed_seconds = [math]::Round($elapsed, 1)
    passed = $passed
    failed = $failed
    checks = $checks
}
$evidencePath = Join-Path $root "qualification\evidence\rust_tracer59_evidence.json"
New-Item -ItemType Directory -Path (Split-Path -Parent $evidencePath) -Force | Out-Null
$evidenceJson = $evidencePayload | ConvertTo-Json -Depth 12
Write-Utf8NoBom $evidencePath $evidenceJson
Write-Host "rust_tracer59_gate: $passed passed, $failed failed -> $evidencePath"
if ($failed -gt 0) {
    foreach ($check in $checks) {
        if ($check.pass -ne $true) {
            Write-Host ("  FAILED: " + $check.name + " " + $check.detail)
        }
    }
    exit 1
}
Remove-Item -Recurse -Force $scratch -ErrorAction SilentlyContinue
exit 0
