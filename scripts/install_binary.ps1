# install_binary.ps1 - issue #61 Windows install lifecycle for the Rust
# binary: install / update / rollback / uninstall, each atomic and
# receipt-safe (the receipt records every path this tool owns; uninstall
# touches only receipt-listed files, never unrelated state).
#
# Layout under $Prefix (default %LOCALAPPDATA%\docx2typed):
#   bin\docx2typed.exe        the installed binary
#   bin\docx2typed.exe.bak    previous binary kept by the last update
#   receipt.json              install receipt (version, hash, paths, dates)
#   mcp.config.json           absolute-path MCP config snippet
#
# Atomicity: new bytes are always written to a temp file in the SAME
# directory, then published with [IO.File]::Replace (atomic replace with
# backup) or [IO.File]::Move (atomic rename). A crash at any point leaves
# either the old or the new complete state - never a torn file.
#
# Usage:
#   install  : powershell -File scripts/install_binary.ps1 -Action install -Bin target\release\docx2typed.exe
#   update   : powershell -File scripts/install_binary.ps1 -Action update -Bin target\release\docx2typed.exe
#   rollback : powershell -File scripts/install_binary.ps1 -Action rollback
#   uninstall: powershell -File scripts/install_binary.ps1 -Action uninstall
#   Tests    : add -Prefix <temp prefix> to isolate (never touches %LOCALAPPDATA%)

param(
    [ValidateSet("install", "update", "rollback", "uninstall")]
    [string]$Action = "install",
    [string]$Bin = "",
    [string]$Prefix = ""
)

$ErrorActionPreference = "Stop"

if (-not $Prefix) {
    $Prefix = Join-Path ([Environment]::GetFolderPath("LocalApplicationData")) "docx2typed"
}
$binDir = Join-Path $Prefix "bin"
$binaryPath = Join-Path $binDir "docx2typed.exe"
$backupPath = Join-Path $binDir "docx2typed.exe.bak"
$receiptPath = Join-Path $Prefix "receipt.json"
$mcpConfigPath = Join-Path $Prefix "mcp.config.json"

function Get-Sha256([string]$path) {
    $sha = [System.Security.Cryptography.SHA256]::Create()
    $stream = [System.IO.File]::OpenRead($path)
    try {
        return ([System.BitConverter]::ToString($sha.ComputeHash($stream))).Replace("-", "").ToLower()
    } finally {
        $stream.Dispose()
        $sha.Dispose()
    }
}

function Write-Utf8NoBom([string]$path, [string]$content) {
    $utf8 = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($path, $content, $utf8)
}

function Read-Json([string]$path) {
    return Get-Content -Path $path -Raw | ConvertFrom-Json
}

function Get-BinaryVersion([string]$path) {
    $json = & $path --version --json | Out-String
    $version = $json | ConvertFrom-Json
    return $version.version
}

function Publish-Atomically([string]$tempPath, [string]$destPath, [string]$backupPath) {
    # Atomic replace with backup when dest exists; atomic rename otherwise.
    if (-not (Test-Path $destPath)) {
        [System.IO.File]::Move($tempPath, $destPath)
        return
    }
    try {
        [System.IO.File]::Replace($tempPath, $destPath, $backupPath)
        return
    } catch [System.IO.IOException] {
        # The destination is locked, which on Windows almost always means a
        # running docx2typed process — an MCP server the agent host started and
        # will respawn. `File.Replace` cannot unlink a mapped image, so the only
        # fix is to stop them. Say that, and say which ones, instead of surfacing
        # a bare IOException that reads like a filesystem fault.
        $holders = @(Get-Process -Name 'docx2typed' -ErrorAction SilentlyContinue)
        if ($holders.Count -gt 0) {
            $ids = ($holders | ForEach-Object { $_.Id }) -join ', '
            throw ("the installed binary is in use by $($holders.Count) running docx2typed " +
                   "process(es) (pid $ids), so it cannot be replaced. Stop them and re-run, or " +
                   "install to a different -Prefix. The build at '$tempPath' is intact.")
        }
        throw ("could not replace '$destPath' (locked, and no docx2typed process was found to " +
               "explain it). The build at '$tempPath' is intact.")
    }
}

function Write-McpConfig([string]$binaryPath) {
    $config = [ordered]@{
        mcpServers = [ordered]@{
            docx2typed = [ordered]@{
                command = $binaryPath
                args = @("mcp")
            }
        }
    }
    Write-Utf8NoBom $mcpConfigPath ($config | ConvertTo-Json -Depth 5)
}

function Invoke-Install {
    if (-not $Bin) { throw "install requires -Bin <path to release binary>" }
    if (-not (Test-Path $Bin)) { throw "binary not found: $Bin" }
    if (Test-Path $receiptPath) {
        throw "receipt already exists at $receiptPath - run update instead of install"
    }
    New-Item -ItemType Directory -Path $binDir -Force | Out-Null
    $tempPath = Join-Path $binDir (".docx2typed.exe.tmp-" + [guid]::NewGuid().ToString("N"))
    Copy-Item -Path $Bin -Destination $tempPath -Force
    Publish-Atomically $tempPath $binaryPath $backupPath
    $version = Get-BinaryVersion $binaryPath
    $hash = Get-Sha256 $binaryPath
    Write-McpConfig $binaryPath
    $receipt = [ordered]@{
        schema = "docx2typed-install-receipt-1"
        version = $version
        installed_at = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
        binary_path = $binaryPath
        binary_sha256 = $hash
        mcp_config_path = $mcpConfigPath
        previous_version = $null
        previous_binary_backup = $null
    }
    Write-Utf8NoBom $receiptPath ($receipt | ConvertTo-Json -Depth 5)
    Write-Host "installed docx2typed $version -> $binaryPath (sha256 $hash)"
    Write-Host "MCP config snippet: $mcpConfigPath"
}

function Invoke-Update {
    if (-not $Bin) { throw "update requires -Bin <path to new release binary>" }
    if (-not (Test-Path $Bin)) { throw "new binary not found: $Bin" }
    if (-not (Test-Path $receiptPath)) { throw "no receipt at $receiptPath - run install first" }
    if (-not (Test-Path $binaryPath)) { throw "installed binary missing: $binaryPath" }
    $receipt = Read-Json $receiptPath
    $oldVersion = $receipt.version
    $oldHash = Get-Sha256 $binaryPath
    $tempPath = Join-Path $binDir (".docx2typed.exe.tmp-" + [guid]::NewGuid().ToString("N"))
    Copy-Item -Path $Bin -Destination $tempPath -Force
    Publish-Atomically $tempPath $binaryPath $backupPath
    $newVersion = Get-BinaryVersion $binaryPath
    $newHash = Get-Sha256 $binaryPath
    Write-McpConfig $binaryPath
    $receipt.version = $newVersion
    $receipt.binary_sha256 = $newHash
    $receipt.previous_version = $oldVersion
    $receipt | Add-Member -NotePropertyName previous_binary_sha256 -NotePropertyValue $oldHash -Force
    $receipt.installed_at = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
    Write-Utf8NoBom $receiptPath ($receipt | ConvertTo-Json -Depth 5)
    Write-Host "updated docx2typed -> $newVersion (sha256 $newHash); previous binary kept at $backupPath"
}

function Invoke-Rollback {
    if (-not (Test-Path $receiptPath)) { throw "no receipt at $receiptPath - nothing to roll back" }
    if (-not (Test-Path $backupPath)) { throw "no backup at $backupPath - nothing to roll back" }
    $receipt = Read-Json $receiptPath
    $tempPath = Join-Path $binDir (".docx2typed.exe.tmp-" + [guid]::NewGuid().ToString("N"))
    Copy-Item -Path $backupPath -Destination $tempPath -Force
    # atomic replace of the current binary with the backup; the replaced
    # current binary goes to a discard file (never a null backup: .NET
    # Framework's File.Replace rejects null)
    $discard = Join-Path $binDir (".docx2typed.exe.discard-" + [guid]::NewGuid().ToString("N"))
    [System.IO.File]::Replace($tempPath, $binaryPath, $discard)
    Remove-Item -Path $discard -Force -ErrorAction SilentlyContinue
    $rolledBack = Get-BinaryVersion $binaryPath
    $hash = Get-Sha256 $binaryPath
    if ($receipt.PSObject.Properties.Name -contains "previous_version") {
        $receipt.version = $receipt.previous_version
    }
    $receipt.binary_sha256 = $hash
    if ($receipt.PSObject.Properties.Name -contains "previous_binary_sha256") {
        $receipt.previous_binary_sha256 = $null
    }
    $receipt.installed_at = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
    Write-Utf8NoBom $receiptPath ($receipt | ConvertTo-Json -Depth 5)
    Remove-Item -Path $backupPath -Force
    Write-Host "rolled back to $rolledBack (sha256 $hash); backup consumed"
}

function Invoke-Uninstall {
    if (-not (Test-Path $receiptPath)) {
        Write-Host "no receipt at $receiptPath - nothing to uninstall (receipt-safe: refusing to guess)"
        return
    }
    $receipt = Read-Json $receiptPath
    # Safety: only remove the exact files the receipt records, and only when
    # their current hash still matches what we installed (a file changed
    # since install is user state - never delete it).
    foreach ($prop in @("binary_path", "mcp_config_path")) {
        $path = $receipt.$prop
        if (-not $path) { continue }
        if (Test-Path $path) {
            $recorded = $null
            if ($prop -eq "binary_path") { $recorded = $receipt.binary_sha256 }
            if ($recorded -and ((Get-Sha256 $path) -ne $recorded)) {
                throw "refusing to uninstall: $path changed since install (hash differs from receipt)"
            }
            Remove-Item -Path $path -Force
            Write-Host "removed $path"
        }
    }
    # backup file is ours (created by update); the receipt records its path
    # indirectly, so only remove it when it sits next to the receipt binary.
    if (Test-Path $backupPath) { Remove-Item -Path $backupPath -Force; Write-Host "removed $backupPath" }
    Remove-Item -Path $receiptPath -Force
    Write-Host "removed $receiptPath"
    # remove empty dirs we created (only if empty - never touch shared dirs)
    foreach ($dir in @($binDir, $Prefix)) {
        if ((Test-Path $dir) -and -not (Get-ChildItem -Path $dir -Force)) {
            Remove-Item -Path $dir -Force
            Write-Host "removed empty directory $dir"
        }
    }
}

switch ($Action) {
    "install" { Invoke-Install }
    "update" { Invoke-Update }
    "rollback" { Invoke-Rollback }
    "uninstall" { Invoke-Uninstall }
}
