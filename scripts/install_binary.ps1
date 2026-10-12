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
    [ValidateSet("install", "update", "rollback", "uninstall", "configure-mcp", "unconfigure-mcp")]
    [string]$Action = "install",
    [string]$Bin = "",
    [string]$Prefix = "",
    # The HOST config to merge the MCP entry into. Defaults to omp's agent
    # config; pass another path for a different host, or "" with -NoMcp to
    # install the binary alone. Named -HostMcpConfig because PowerShell
    # variable names are case-INSENSITIVE: -McpConfigPath would be the SAME
    # variable as this script's own $mcpConfigPath (the snippet it writes under
    # the prefix), and the snippet would silently win.
    [string]$HostMcpConfig = "",
    # Install the binary without touching any host configuration.
    [switch]$NoMcp
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

# ---------------------------------------------------------------------------
# Host MCP configuration
#
# A binary MCP server has no `npx`/`uvx` one-liner and the host offers no
# `mcp add` command, so the installer merges the entry itself. The contract
# that makes that safe rather than a footgun:
#
#   1. read the host config (or start from {"mcpServers":{}});
#   2. back it up, timestamped, before anything is written;
#   3. merge exactly ONE key, mcpServers.docx2typed, leaving every other
#      server untouched;
#   4. re-parse the merged text - a host config that does not parse breaks the
#      whole agent, so a bad merge restores the backup and fails;
#   5. publish atomically (temp in the same directory, then rename);
#   6. record what was added, so uninstall removes exactly that and nothing
#      else;
#   7. VERIFY by launching the server and listing its tools - proof the entry
#      works, not merely that JSON was written.
# ---------------------------------------------------------------------------

function Get-DefaultMcpConfigPath {
    # omp keeps its MCP servers in ~/.omp/agent/mcp.json. Honour an explicit
    # -McpConfigPath first; this is only the default.
    $ompHome = Join-Path $HOME ".omp"
    return (Join-Path $ompHome "agent\mcp.json")
}

function Read-HostMcpConfig([string]$path) {
    if (-not (Test-Path $path)) { return [pscustomobject]@{ mcpServers = [pscustomobject]@{} } }
    $raw = Get-Content -Path $path -Raw
    if ([string]::IsNullOrWhiteSpace($raw)) { return [pscustomobject]@{ mcpServers = [pscustomobject]@{} } }
    try {
        return ($raw | ConvertFrom-Json)
    } catch {
        # Refuse, and say so plainly. This file belongs to the host, not to us:
        # a config that does not parse already breaks the agent, and rewriting
        # it would discard whatever the user was editing. Nothing is touched.
        throw ("the host config at '$path' is not valid JSON, so it was left exactly as it " +
               "is (nothing was changed). Fix it, or point -HostMcpConfig at another file, or " +
               "install the binary alone with -NoMcp. Parse error: $($_.Exception.Message)")
    }
}

function Backup-HostConfig([string]$path) {
    if (-not (Test-Path $path)) { return $null }
    $stamp = (Get-Date).ToUniversalTime().ToString("yyyyMMddTHHmmssZ")
    $backup = "$path.bak-$stamp"
    Copy-Item -Path $path -Destination $backup -Force
    return $backup
}

function Publish-HostConfig([string]$path, $config, [string]$backup) {
    # Serialize, then PROVE the bytes parse before they reach the host. A host
    # config that does not parse takes the whole agent down, so the merge is
    # validated on the text, not on the object it came from.
    $json = $config | ConvertTo-Json -Depth 20
    $null = $json | ConvertFrom-Json
    $dir = Split-Path -Parent $path
    if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }
    $temp = Join-Path $dir (".mcp.json.tmp-" + [guid]::NewGuid().ToString("N"))
    Write-Utf8NoBom $temp $json
    try {
        if (Test-Path $path) {
            [System.IO.File]::Replace($temp, $path, "$path.prev")
            Remove-Item "$path.prev" -Force -ErrorAction SilentlyContinue
        } else {
            [System.IO.File]::Move($temp, $path)
        }
    } catch {
        if ($backup -and (Test-Path $backup)) { Copy-Item $backup $path -Force }
        throw "could not publish the host config at '$path': $($_.Exception.Message). The previous file is intact."
    }
}

function Test-McpEntryWorks([string]$binaryPath) {
    # Launch the server and list its tools. An entry that is present but does
    # not serve is worse than no entry: the host reports a dead server forever.
    $request = '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
    try {
        $out = $request | & $binaryPath "mcp" 2>$null | Out-String
    } catch {
        return 0
    }
    $count = 0
    foreach ($line in ($out -split "`n")) {
        if ($line -match '"tools"') {
            try { $count = ($line | ConvertFrom-Json).result.tools.Count } catch { $count = 0 }
            break
        }
    }
    return $count
}

function Invoke-ConfigureMcp {
    $target = if ($HostMcpConfig) { $HostMcpConfig } else { Get-DefaultMcpConfigPath }
    if (-not (Test-Path $binaryPath)) {
        throw "no installed binary at $binaryPath - run install/update first"
    }
    $config = Read-HostMcpConfig $target
    # `@(...)` around `.Name` is load-bearing, not style: on an object with NO
    # properties `.Name` returns $null rather than an empty array, and calling
    # `.Contains()` on that is an error. A host config that has an `mcpServers`
    # key but no servers yet hits exactly that, which is the common case on a
    # fresh install. `-contains` on a forced array is the safe form.
    if (@($config.PSObject.Properties.Name) -notcontains "mcpServers") {
        $config | Add-Member -NotePropertyName mcpServers -NotePropertyValue ([pscustomobject]@{}) -Force
    }
    $backup = Backup-HostConfig $target
    $entry = [pscustomobject]@{ command = $binaryPath; args = @("mcp") }
    if (@($config.mcpServers.PSObject.Properties.Name) -contains "docx2typed") {
        $config.mcpServers.docx2typed = $entry          # idempotent: replace, never duplicate
    } else {
        $config.mcpServers | Add-Member -NotePropertyName docx2typed -NotePropertyValue $entry -Force
    }
    Publish-HostConfig $target $config $backup

    $tools = Test-McpEntryWorks $binaryPath
    if ($tools -le 0) {
        throw "configured '$target' but the server did not answer tools/list - the entry is present and dead. Restore from '$backup'."
    }

    # Record the path so uninstall removes exactly what this added.
    if (Test-Path $receiptPath) {
        $receipt = Read-Json $receiptPath
        $receipt | Add-Member -NotePropertyName mcp_host_config_path -NotePropertyValue $target -Force
        $receipt | Add-Member -NotePropertyName mcp_host_config_backup -NotePropertyValue $backup -Force
        Write-Utf8NoBom $receiptPath ($receipt | ConvertTo-Json -Depth 5)
    }
    Write-Host "MCP: configured '$target' -> $binaryPath mcp ($tools tools served)"
    if ($backup) { Write-Host "MCP: previous host config backed up at $backup" }
}

function Invoke-UnconfigureMcp {
    $target = if ($HostMcpConfig) { $HostMcpConfig } else { Get-DefaultMcpConfigPath }
    if (-not (Test-Path $target)) { Write-Host "MCP: no host config at '$target' - nothing to remove"; return }
    $config = Read-HostMcpConfig $target
    if (@($config.PSObject.Properties.Name) -notcontains "mcpServers" -or
        @($config.mcpServers.PSObject.Properties.Name) -notcontains "docx2typed") {
        Write-Host "MCP: '$target' has no docx2typed entry - nothing to remove"
        return
    }
    $backup = Backup-HostConfig $target
    $config.mcpServers.PSObject.Properties.Remove("docx2typed")
    Publish-HostConfig $target $config $backup
    Write-Host "MCP: removed the docx2typed entry from '$target' (other servers untouched)"
}

switch ($Action) {
    "install" {
        Invoke-Install
        if (-not $NoMcp) {
            try { Invoke-ConfigureMcp }
            catch {
                # The binary is already in place; only the host entry failed.
                # Say which half landed so the user is not left guessing.
                Write-Host "MCP: NOT configured - $($_.Exception.Message)"
                Write-Host "MCP: the binary itself is installed and working; re-run with -Action configure-mcp once the config is usable."
                throw
            }
        }
        else { Write-Host "MCP: skipped (-NoMcp); configure later with -Action configure-mcp" }
    }
    "update" {
        Invoke-Update
        if (-not $NoMcp) { Invoke-ConfigureMcp }
        else { Write-Host "MCP: skipped (-NoMcp)" }
    }
    "rollback" { Invoke-Rollback }
    "uninstall" {
        Invoke-UnconfigureMcp
        Invoke-Uninstall
    }
    "configure-mcp" { Invoke-ConfigureMcp }
    "unconfigure-mcp" { Invoke-UnconfigureMcp }
}
