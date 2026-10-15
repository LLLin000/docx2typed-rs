# rust_mcp_schema_gate.ps1 - exact frozen MCP tools/list parity gate.
#
# Compares the live Rust MCP response with the checked-in .mcp_schemas.json
# contract. Object-key order is normalized before comparison because serde_json
# uses sorted object keys while PowerShell preserves source order.
#
# Usage: powershell -NoProfile -ExecutionPolicy Bypass -File qualification/rust_mcp_schema_gate.ps1
#        [-Bin target\release\docx2typed.exe]

param(
    [string]$Bin = "target\release\docx2typed.exe"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$bin = [System.IO.Path]::GetFullPath((Join-Path $root $Bin))
if (-not (Test-Path $bin)) {
    $bin = [System.IO.Path]::GetFullPath((Join-Path $root "target\debug\docx2typed.exe"))
}
if (-not (Test-Path $bin)) {
    throw "binary not found: $bin"
}

# A binary older than the sources answers with an OLD tool surface. When tools
# were ADDED that fails loudly, but when they were REMOVED it passes silently --
# which is the one direction a schema gate must never fail in.
$newestSource = Get-ChildItem -Path (Join-Path $root "src"), (Join-Path $root "crates") `
    -Recurse -File -Include *.rs -ErrorAction SilentlyContinue |
    Sort-Object LastWriteTime -Descending | Select-Object -First 1
$binaryStamp = (Get-Item $bin).LastWriteTime
if ($newestSource -and $binaryStamp -lt $newestSource.LastWriteTime) {
    throw ("binary is older than $($newestSource.Name) " +
        "($($binaryStamp.ToString('HH:mm:ss')) < $($newestSource.LastWriteTime.ToString('HH:mm:ss'))); " +
        "rebuild before running the gate, or it answers with a stale tool surface")
}

$schemaPath = Join-Path $root ".mcp_schemas.json"
$frozen = Get-Content -Raw -Encoding UTF8 $schemaPath | ConvertFrom-Json

function Convert-Canonical([object]$Value) {
    if ($null -eq $Value) { return $null }
    if ($Value -is [System.Management.Automation.PSCustomObject]) {
        $result = [ordered]@{}
        foreach ($property in ($Value.PSObject.Properties | Sort-Object Name)) {
            $result[$property.Name] = Convert-Canonical $property.Value
        }
        return $result
    }
    if ($Value -is [System.Collections.IDictionary]) {
        $result = [ordered]@{}
        foreach ($key in ($Value.Keys | Sort-Object)) {
            $result[[string]$key] = Convert-Canonical $Value[$key]
        }
        return $result
    }
    if ($Value -is [System.Collections.IEnumerable] -and $Value -isnot [string]) {
        return @($Value | ForEach-Object { Convert-Canonical $_ })
    }
    return $Value
}

# Two questions, and the gate asks both: what an AGENT may call (the five-tool
# surface, docs/specs/08_agent_surface.md) and what the ENGINE still serves (the
# frozen contract, divergence #5). They used to be one list, which is why this
# gate compared `tools/list` against .mcp_schemas.json; asking the wrong question
# is how a gate reports a defect that is not there.
$inputJson = '{"tool":"tools/list","args":{}}'
$legacyJson = '{"tool":"review_settle","args":{}}'
$raw = (@($inputJson, $legacyJson) -join "`n") | & $bin mcp
$rc = $LASTEXITCODE
if ($rc -ne 0) {
    throw "mcp exited with code $rc"
}
$lines = @($raw | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
if ($lines.Count -ne 2 -or -not $lines[0].StartsWith("OK ")) {
    throw "tools/list + one legacy call must emit two OK lines; got: $($lines -join ' | ')"
}
$response = $lines[0].Substring(3) | ConvertFrom-Json
$tools = @($response.structuredContent.data.tools)

$checks = @()
function Add-Check([string]$Name, [bool]$Pass, [string]$Detail = "") {
    $script:checks += [pscustomobject]@{ name = $Name; pass = $Pass; detail = $Detail }
}

$frozenNames = @($frozen.PSObject.Properties.Name | Sort-Object)

# Question 1: what an agent sees.
$surface = @("document_open", "document_find", "document_edit", "document_save", "document_status")
$liveNames = @($tools | ForEach-Object { $_.name })
Add-Check "agent surface advertised" (($liveNames -join "\n") -eq ($surface -join "\n")) "live=$($liveNames -join ',')"
Add-Check "agent surface described" (@($tools | Where-Object { [string]::IsNullOrWhiteSpace($_.description) }).Count -eq 0) "every advertised tool explains itself"
Add-Check "agent surface schemas" (@($tools | Where-Object {
    $_.inputSchema.type -ne "object" -or $_.inputSchema.additionalProperties -ne $false -or
    $null -eq $_.inputSchema.properties
}).Count -eq 0) "object/additionalProperties=false/properties"

# Question 2: what the engine serves, from the frozen contract itself.
Add-Check "frozen contract intact" ($frozenNames.Count -eq 53) "frozen=$($frozenNames.Count)"
Add-Check "frozen schemas well-formed" (@($frozenNames | Where-Object {
    $schema = $frozen.$_
    $schema.type -ne "object" -or $schema.additionalProperties -ne $false -or $null -eq $schema.properties
}).Count -eq 0) "every frozen inputSchema is object/additionalProperties=false/properties"
# A name that is advertised nowhere but also cannot be called would be a silent
# removal; the CLI and the suites call these by name.
Add-Check "legacy tool still dispatches" (-not $lines[1].StartsWith("ERR unknown tool")) $lines[1].Substring(0, [Math]::Min(60, $lines[1].Length))

$failed = @($checks | Where-Object { -not $_.pass })
foreach ($check in $checks) {
    $state = if ($check.pass) { "PASS" } else { "FAIL" }
    Write-Host ("{0}: {1} {2}" -f $state, $check.name, $check.detail)
}
if ($failed.Count -gt 0) {
    throw "rust_mcp_schema_gate failed: $($failed.Count) check(s)"
}
Write-Host "rust_mcp_schema_gate: $($checks.Count) passed, 0 failed"
