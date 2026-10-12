# mcp_merge_tests.ps1 - the host MCP configuration contract.
#
# A binary MCP server has no `npx`/`uvx` one-liner and omp offers no `mcp add`
# command, so the installer merges the HOST's own config file. That makes it the
# installer's job to be a good citizen about a file it does not own. These are
# the properties that separate a safe merge from a destructive one:
#
#   1. another server in the config survives, with its fields intact;
#   2. re-running is idempotent - replace, never duplicate;
#   3. every shape of "no entry yet" works: an empty object, a missing
#      `mcpServers` key, an empty file;
#   4. a config that does not parse is REFUSED and left byte-identical - the
#      file belongs to the host, and a config that already fails to parse is
#      the user's to fix, not ours to overwrite;
#   5. uninstall removes exactly our entry and nothing else;
#   6. -NoMcp installs the binary and touches no configuration at all.
#
# Runs with no signing key and no release bundle: it exercises the installer
# against temp prefixes and temp host configs only, and never reads or writes
# the real host configuration.
#
# Usage: powershell -NoProfile -ExecutionPolicy Bypass -File scripts/mcp_merge_tests.ps1

param(
    [string]$Bin = "target\release\docx2typed.exe"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$bin = Join-Path $root $Bin
if (-not (Test-Path $bin)) { throw "release binary not found: $bin (build it first)" }
$installer = Join-Path $root "scripts\install_binary.ps1"

function New-Temp([string]$tag) {
    $dir = Join-Path ([System.IO.Path]::GetTempPath()) ("docx2typed-mcpmerge-$tag-" + [guid]::NewGuid().ToString("N"))
    New-Item -ItemType Directory -Path $dir -Force | Out-Null
    return $dir
}
function Write-Utf8NoBom([string]$path, [string]$content) {
    $utf8 = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($path, $content, $utf8)
}
$checks = @()
function Add-Check([string]$name, [bool]$pass, [string]$detail = "") {
    $script:checks += @{ name = $name; pass = $pass; detail = $detail }
}

$hostDir = New-Temp "host"
$hostConfig = Join-Path $hostDir "host-mcp.json"
$prefix = Join-Path $hostDir "prefix"
$args = @("-NoProfile", "-ExecutionPolicy", "Bypass", "-File", $installer,
          "-Prefix", $prefix, "-HostMcpConfig", $hostConfig)

$seed = @'
{
  "mcpServers": {
    "other-server": { "command": "C:\\elsewhere\\other.exe", "args": ["serve"], "timeout": 12345 },
    "second-server": { "command": "npx", "args": ["-y", "something"] }
  }
}
'@

# 1. merge preserves other servers
Write-Utf8NoBom $hostConfig $seed
& powershell @args -Action install -Bin $bin | Out-Null
$m = Get-Content $hostConfig -Raw | ConvertFrom-Json
$names = @($m.mcpServers.PSObject.Properties.Name)
Add-Check "our entry added" ($names -contains "docx2typed")
Add-Check "other servers preserved" (($names -contains "other-server") -and ($names -contains "second-server"))
Add-Check "other server fields intact" ($m.mcpServers.'other-server'.timeout -eq 12345)
Add-Check "entry points at the installed binary" ($m.mcpServers.docx2typed.command -eq (Join-Path $prefix "bin\docx2typed.exe"))

# 2. idempotent
& powershell @args -Action configure-mcp | Out-Null
& powershell @args -Action configure-mcp | Out-Null
$a = Get-Content $hostConfig -Raw | ConvertFrom-Json
$an = @($a.mcpServers.PSObject.Properties.Name)
Add-Check "idempotent, no duplicate" ((@($an | Where-Object { $_ -eq "docx2typed" })).Count -eq 1)
Add-Check "still exactly three servers" ($an.Count -eq 3)

# 3. every "no entry yet" shape
Write-Utf8NoBom $hostConfig '{ "mcpServers": {} }'
& powershell @args -Action configure-mcp | Out-Null
$e = Get-Content $hostConfig -Raw | ConvertFrom-Json
Add-Check "empty mcpServers gains the entry" (@($e.mcpServers.PSObject.Properties.Name) -contains "docx2typed")

Write-Utf8NoBom $hostConfig '{ "someOtherSetting": true }'
& powershell @args -Action configure-mcp | Out-Null
$k = Get-Content $hostConfig -Raw | ConvertFrom-Json
Add-Check "missing mcpServers key added" (@($k.mcpServers.PSObject.Properties.Name) -contains "docx2typed")
Add-Check "unrelated setting preserved" ($k.someOtherSetting -eq $true)

Write-Utf8NoBom $hostConfig ""
& powershell @args -Action configure-mcp | Out-Null
$z = Get-Content $hostConfig -Raw | ConvertFrom-Json
Add-Check "empty file gains the entry" (@($z.mcpServers.PSObject.Properties.Name) -contains "docx2typed")

# 4. corrupt config is refused, not rewritten
$corrupt = '{ "mcpServers": { broken'
Write-Utf8NoBom $hostConfig $corrupt
try { & powershell @args -Action configure-mcp | Out-Null; $refused = $false } catch { $refused = $true }
Add-Check "corrupt config refused" ((Get-Content $hostConfig -Raw).Trim() -eq $corrupt)

# 5. uninstall removes only ours
Write-Utf8NoBom $hostConfig $seed
& powershell @args -Action install -Bin $bin | Out-Null
& powershell @args -Action uninstall | Out-Null
$f = Get-Content $hostConfig -Raw | ConvertFrom-Json
$fn = @($f.mcpServers.PSObject.Properties.Name)
Add-Check "uninstall removes our entry" (-not ($fn -contains "docx2typed"))
Add-Check "uninstall leaves others untouched" (($fn -contains "other-server") -and ($fn -contains "second-server"))
Add-Check "uninstall leaves other fields intact" ($f.mcpServers.'other-server'.timeout -eq 12345)

# 6. -NoMcp touches nothing
$noMcpDir = New-Temp "nomcp"
$noMcpConfig = Join-Path $noMcpDir "host-mcp.json"
Write-Utf8NoBom $noMcpConfig $seed
& powershell -NoProfile -ExecutionPolicy Bypass -File $installer -Action install -Bin $bin `
    -Prefix (Join-Path $noMcpDir "prefix") -HostMcpConfig $noMcpConfig -NoMcp | Out-Null
Add-Check "no-mcp: binary installed" (Test-Path (Join-Path $noMcpDir "prefix\bin\docx2typed.exe"))
Add-Check "no-mcp: host config untouched" ((Get-Content $noMcpConfig -Raw) -eq $seed)

$failures = @($checks | Where-Object { -not $_.pass })
[ordered]@{
    schema = "docx2typed-mcp-merge-tests-1"
    checks = $checks
    verdict = if ($failures.Count -eq 0) { "pass" } else { "fail" }
} | ConvertTo-Json -Depth 8
if ($failures.Count -gt 0) {
    Write-Host "FAILED: $($failures.name -join '; ')" -ForegroundColor Red
    exit 1
}
Write-Host "ALL MCP MERGE TESTS PASS" -ForegroundColor Green
