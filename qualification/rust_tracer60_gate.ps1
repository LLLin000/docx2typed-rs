# rust_tracer60_gate.ps1 - issue #60 differential/adversarial gate: the
# secured review collaboration lane (MCP 37-tool surface + stdio purity +
# one-workdir + draft lifecycle with store replay, HTTP security contract,
# capability lifecycle, throttle, CAS one-winner, restart revocation) run
# against the real installed-style `docx2typed` binary.
#
# Evidence JSON: qualification/evidence/rust_tracer60_evidence.json
#
# Usage: powershell -NoProfile -ExecutionPolicy Bypass -File qualification/rust_tracer60_gate.ps1
#        [-Bin ..\target\release\docx2typed.exe]

param(
    [string]$Bin = "target\release\docx2typed.exe"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$bin = Join-Path $root $Bin
$oracle = Join-Path $PSScriptRoot "rust_tracer60_oracle.py"

if (-not (Test-Path $bin)) {
    $bin = Join-Path $root "target\debug\docx2typed.exe"
}
if (-not (Test-Path $bin)) {
    throw "binary not found: $bin"
}

$scratch = Join-Path $env:TEMP ("rust-tracer60-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $scratch -Force | Out-Null

$checks = @()
$started = Get-Date

function Write-Utf8NoBom([string]$path, [string]$content) {
    $utf8 = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($path, $content, $utf8)
}

Write-Host "== rust_tracer60_gate: $bin =="

# ---- S-profile sanity: the gate chain must complete well inside the S
# ---- complete-chain wall budget (35 s for the edit-build-verify chain;
# ---- the review lane adds no extraction/building of its own beyond the
# ---- one extract).
$chainStart = Get-Date

# ---- 1. MCP stdio surface + one-workdir (cheap, no server) ----
$mcpInput = @(
    '{"tool":"tools/list","args":{}}',
    '{"tool":"workdir_open","args":{"workdir":"PLACEHOLDER"}}'
) -join "`n"

# ---- 2. Adversarial oracle (raw-socket HTTP + MCP stdio + parity) ----
$oracleResult = & python $oracle $bin $scratch 2>&1
$oracleExit = $LASTEXITCODE
# The oracle prints one JSON object and MAY append a status line ("FAILED:
# [...]"), so taking the last line parsed the status line as JSON whenever the
# gate was failing -- which replaced the real diagnostics with a parse error on
# exactly the runs that needed them. Take the line that IS the object.
$oracleJson = ($oracleResult | ForEach-Object { [string]$_ } | Where-Object { $_.TrimStart().StartsWith("{") } | Select-Object -Last 1)
if (-not $oracleJson) {
    throw "tracer60 oracle produced no JSON object; raw output: $($oracleResult -join ' | ')"
}
$oracle = $oracleJson | ConvertFrom-Json
if ($oracleExit -ne 0) {
    Write-Host "oracle failed (exit $oracleExit):"
    $oracleResult | ForEach-Object { Write-Host $_ }
    exit 1
}
foreach ($property in $oracle.checks.PSObject.Properties) {
    $checks += @{
        name = $property.Name
        pass = [bool]$property.Value.pass
        detail = [string]$property.Value.detail
    }
}

$chainSeconds = ((Get-Date) - $chainStart).TotalSeconds
$checks += @{
    name = "resource: review60 gate chain within S profile complete-chain budget (35 s)"
    pass = ($chainSeconds -le 35.0)
    detail = "$([math]::Round($chainSeconds, 1))s"
}

# ---- Evidence ----
$evidence = @{
    schema = "rust-tracer-60-evidence-1"
    issue = "60"
    branch = "rust-tracer-60"
    gate = "rust_tracer60_gate.ps1"
    binary = (Resolve-Path $bin).Path
    generated_at = (Get-Date).ToUniversalTime().ToString("o")
    checks = $checks
    oracle_evidence = $oracle.evidence
    profiles = @{
        s_profile = $oracle.evidence.s_profile
        complete_chain_budget_s = 35
        gate_chain_s = [math]::Round($chainSeconds, 1)
    }
}

$evidencePath = Join-Path $PSScriptRoot "evidence\rust_tracer60_evidence.json"
Write-Utf8NoBom $evidencePath ($evidence | ConvertTo-Json -Depth 12)

$passed = ($checks | Where-Object { $_.pass }).Count
$failed = ($checks | Where-Object { -not $_.pass }).Count
Write-Host "== rust_tracer60_gate: $passed passed, $failed failed ($($checks.Count) checks) =="
Write-Host "== evidence: $evidencePath =="
if ($failed -gt 0) {
    $checks | Where-Object { -not $_.pass } | ForEach-Object {
        Write-Host "  FAILED: $($_.name): $($_.detail)"
    }
    exit 1
}
exit 0
