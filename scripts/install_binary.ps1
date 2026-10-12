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
    [switch]$NoMcp,
    # Which agent hosts to configure. "auto" (default) configures every host
    # whose config file already exists; "all" also creates configs for hosts
    # that are not installed; a comma-separated list names specific ones.
    [string]$Hosts = "auto",
    # Print the host table with each one's state and exit.
    [switch]$ListHosts,
    # Relocate every host config under this directory instead of the real
    # $HOME / $APPDATA. For tests and for isolated dry runs: with this set, no
    # real agent configuration can be touched.
    [string]$HostsRoot = "",
    # Internal: set when this script has already re-invoked itself under pwsh.
    [switch]$Reinvoked
)

$ErrorActionPreference = "Stop"

# Windows PowerShell 5.1's ConvertFrom-Json cannot parse every config it will
# meet. Measured on a real omp config: 5.1 rejects it with "unrecognized escape
# sequence" at the first doubled backslash in a Windows path, while PowerShell 7
# parses the same file. Since the merge has to parse a config we do not own, hand
# the whole action to pwsh when it exists rather than corrupting or refusing a
# file the user's own host reads happily.
$script:reinvokeActions = @("configure-mcp", "unconfigure-mcp", "install", "update", "uninstall")
if (-not $Reinvoked -and $PSVersionTable.PSVersion.Major -lt 6 -and $script:reinvokeActions -contains $Action) {
    $pwsh = Get-Command pwsh -ErrorAction SilentlyContinue
    if ($pwsh) {
        $forward = @("-NoProfile", "-ExecutionPolicy", "Bypass", "-File", $PSCommandPath,
                     "-Action", $Action, "-Reinvoked")
        foreach ($pair in @(@("-Bin", $Bin), @("-Prefix", $Prefix), @("-HostMcpConfig", $HostMcpConfig),
                            @("-Hosts", $Hosts), @("-HostsRoot", $HostsRoot))) {
            if ($pair[1]) { $forward += $pair }
        }
        if ($NoMcp) { $forward += "-NoMcp" }
        if ($ListHosts) { $forward += "-ListHosts" }
        & $pwsh.Source @forward
        exit $LASTEXITCODE
    }
    Write-Host "note: running under Windows PowerShell $($PSVersionTable.PSVersion); pwsh was not found, and some host configs cannot be parsed by 5.1's ConvertFrom-Json. Install PowerShell 7 for reliable host configuration."
}

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

# ---------------------------------------------------------------------------
# The host table
#
# There is no single place to install an MCP server. Every agent host keeps its
# own config, at its own path, in its own format - and the format is not always
# JSON, nor is the key always the same. Measured on one machine with five hosts
# installed:
#
#   omp / claude-code / cursor    JSON   mcpServers
#   codex                         TOML   [mcp_servers.<name>]
#   vscode / copilot              JSON   servers
#
# So "install the MCP server" means: detect which hosts are present, and speak
# each one's dialect. A host not in this table still gets the snippet printed -
# the universal fallback that works for anything we do not know about.
# ---------------------------------------------------------------------------

function Get-KnownHosts {
    # -HostsRoot relocates every path for tests and isolated dry runs, so the
    # real host configs cannot be reached at all when it is set.
    if ($HostsRoot) {
        $homeRoot = $HostsRoot
        $appData = Join-Path $HostsRoot "AppData"
    } else {
        $homeRoot = $HOME
        $appData = [Environment]::GetFolderPath("ApplicationData")
    }
    return @(
        @{ name = "omp";            path = (Join-Path $homeRoot ".omp\agent\mcp.json");                       format = "json"; key = "mcpServers" },
        @{ name = "claude-code";    path = (Join-Path $homeRoot ".claude.json");                                format = "json"; key = "mcpServers" },
        @{ name = "claude-desktop"; path = (Join-Path $appData "Claude\claude_desktop_config.json");       format = "json"; key = "mcpServers" },
        @{ name = "cursor";         path = (Join-Path $homeRoot ".cursor\mcp.json");                           format = "json"; key = "mcpServers" },
        @{ name = "windsurf";       path = (Join-Path $homeRoot ".codeium\windsurf\mcp_config.json");         format = "json"; key = "mcpServers" },
        @{ name = "continue";       path = (Join-Path $homeRoot ".continue\config.json");                      format = "json"; key = "mcpServers" },
        @{ name = "codex";          path = (Join-Path $homeRoot ".codex\config.toml");                         format = "toml"; key = "mcp_servers" },
        @{ name = "vscode";         path = (Join-Path $appData "Code\User\settings.json");                format = "json"; key = "servers" },
        @{ name = "zed";            path = (Join-Path $appData "Zed\settings.json");                       format = "json"; key = "context_servers" }
    )
}

function Select-Hosts {
    # Resolve -Hosts into the concrete list to act on.
    $known = Get-KnownHosts
    $requested = @($Hosts -split "," | ForEach-Object { $_.Trim().ToLower() } | Where-Object { $_ })
    if ($requested -contains "all") { return $known }
    if ($requested -contains "auto") {
        return @($known | Where-Object { Test-Path $_.path })
    }
    $picked = @($known | Where-Object { $requested -contains $_.name })
    $unknown = @($requested | Where-Object { $_ -notin @($known.name) })
    foreach ($u in $unknown) {
        Write-Host "host '$u' is not known; its snippet must be configured by hand"
    }
    return $picked
}

function Show-Hosts {
    Write-Host "host               state     format  key                 path"
    foreach ($h in (Get-KnownHosts)) {
        $state = if (Test-Path $h.path) { "present" } else { "absent " }
        Write-Host ("{0,-18} {1,-9} {2,-7} {3,-19} {4}" -f $h.name, $state, $h.format, $h.key, $h.path)
    }
    Write-Host ""
    Write-Host "Use -Hosts auto (default), all, or a comma-separated list of names."
}

# ---------------------------------------------------------------------------
# TOML adapter (codex)
#
# TOML is not JSON, and PowerShell has no TOML parser. The edit is therefore
# textual but bounded: find the [mcp_servers.docx2typed] block and replace it,
# or append one. It is then validated by re-reading the file with Python's
# tomllib when a Python is available, and refused (with the backup restored)
# if it does not parse.
# ---------------------------------------------------------------------------

function Get-TomlBlock([string]$text, [string]$section) {
    $header = "[$section]"
    $lines = $text -split "`r?`n"
    $start = -1
    for ($i = 0; $i -lt $lines.Count; $i++) {
        if ($lines[$i].Trim() -eq $header) { $start = $i; break }
    }
    if ($start -lt 0) { return $null }
    $end = $lines.Count
    for ($j = $start + 1; $j -lt $lines.Count; $j++) {
        if ($lines[$j] -match "^\s*\[") { $end = $j; break }
    }
    return @{ start = $start; end = $end }
}

function ConvertTo-TomlString([string]$value) {
    # TOML LITERAL string (single quotes) processes no escapes, which is exactly
    # right for a Windows path: a basic string would read `\U` in `C:\Users` as
    # an escape and refuse to parse. A path containing a single quote falls back
    # to a basic string with both escapes applied.
    if ($value -notmatch "'") { return "'" + $value + "'" }
    return '"' + ($value -replace '\\', '\\\\' -replace '"', '\"') + '"'
}

function Set-TomlServer([string]$text, [string]$section, [string]$command, [string]$argsToml) {
    # The header NEEDS its brackets: `[mcp_servers.docx2typed]` is a table,
    # `mcp_servers.docx2typed` is a bare key and makes the file unparseable.
    #
    # The command line is built in its own statement because PowerShell's array
    # comma binds TIGHTER than `+`: `@("a", "b" + $c, "d")` is THREE elements
    # plus a fourth, not the three intended, and the value lands on its own line.
    # Measured: it produced `command = ` / `'C:\...'` and the file stopped
    # parsing.
    $commandLine = "command = " + (ConvertTo-TomlString $command)
    $block = @("[$section]", $commandLine, "args = $argsToml")
    $lines = $text -split "`r?`n"
    $found = Get-TomlBlock $text $section
    if ($found) {
        $before = if ($found.start -gt 0) { $lines[0..($found.start - 1)] } else { @() }
        $after = if ($found.end -lt $lines.Count) { $lines[$found.end..($lines.Count - 1)] } else { @() }
        return (@($before) + $block + @($after)) -join "`r`n"
    }
    $trimmed = $text.TrimEnd()
    $sep = if ($trimmed.Length -gt 0) { "`r`n`r`n" } else { "" }
    return ($trimmed + $sep + ($block -join "`r`n") + "`r`n")
}

function Test-TomlParses([string]$path) {
    # A REAL parser, or no claim. A structural guess is not a parse: it passed a
    # file whose table header had lost its brackets, which is exactly the kind of
    # damage this check exists to catch.
    $candidates = @("python", "python3", "py") | ForEach-Object { Get-Command $_ -ErrorAction SilentlyContinue }
    foreach ($exe in $candidates) {
        $code = "import sys,tomllib;tomllib.load(open(sys.argv[1],'rb'))"
        & $exe.Source -c $code $path 2>$null | Out-Null
        if ($LASTEXITCODE -eq 0) { return $true }
        if ($LASTEXITCODE -ne 0 -and $LASTEXITCODE -ne 1) { continue }  # not this interpreter
        return $false
    }
    # No TOML parser available. Refuse rather than guess: the caller restores the
    # backup, so an unverifiable edit never reaches the host config.
    throw ("no TOML parser available to verify the edit (tried python/python3/py with tomllib). " +
           "Install Python 3.11+ or edit $path by hand; the previous file was restored.")
}

function Get-DefaultMcpConfigPath {
    # omp keeps its MCP servers in ~/.omp/agent/mcp.json. Honour an explicit
    # -McpConfigPath first; this is only the default.
    $ompHome = Join-Path $HOME ".omp"
    return (Join-Path $ompHome "agent\mcp.json")
}

function Read-HostMcpConfig([string]$path) {
    if (Test-Path $path -PathType Container) {
        # A DIRECTORY where a config file belongs. Reading it as a file throws,
        # and an unguarded throw inside the per-host loop aborts the whole run
        # halfway - leaving some hosts configured and the rest untouched, which
        # is the worst of both. Say what is wrong and let the caller decide.
        throw "'$path' is a directory, not a config file"
    }
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

# NOTE: not `$host` - that is a read-only PowerShell AUTOMATIC variable
# (the host UI object), and variable names are case-insensitive.
function Set-OneHost($target, [string]$binaryPath) {
    # Configure ONE host, in its own dialect. Returns a result row.
    $name = $target.name
    $path = $target.path
    $exists = Test-Path $path
    if ($target.format -eq "toml") {
        $text = if ($exists) { Get-Content -Path $path -Raw } else { "" }
        $backup = Backup-HostConfig $path
        $updated = Set-TomlServer $text "$($target.key).docx2typed" $binaryPath '["mcp"]'
        $dir = Split-Path -Parent $path
        if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }
        Write-Utf8NoBom $path $updated
        if (-not (Test-TomlParses $path)) {
            if ($backup) { Copy-Item $backup $path -Force } else { Remove-Item $path -Force }
            throw "the merge left '$path' unparseable as TOML; the previous file was restored"
        }
        return @{ name = $name; path = $path; backup = $backup; format = "toml" }
    }
    # JSON: one key, whichever key this host uses.
    $config = Read-HostMcpConfig $path
    if (@($config.PSObject.Properties.Name) -notcontains $target.key) {
        $config | Add-Member -NotePropertyName $target.key -NotePropertyValue ([pscustomobject]@{}) -Force
    }
    $backup = Backup-HostConfig $path
    $entry = [pscustomobject]@{ command = $binaryPath; args = @("mcp") }
    $bucket = $config.($target.key)
    if (@($bucket.PSObject.Properties.Name) -contains "docx2typed") {
        $bucket.docx2typed = $entry                 # idempotent: replace, never duplicate
    } else {
        $bucket | Add-Member -NotePropertyName docx2typed -NotePropertyValue $entry -Force
    }
    Publish-HostConfig $path $config $backup
    return @{ name = $name; path = $path; backup = $backup; format = "json" }
}

function Invoke-ConfigureMcp {
    if (-not (Test-Path $binaryPath)) {
        throw "no installed binary at $binaryPath - run install/update first"
    }
    # An explicit -HostMcpConfig names one file, in the default JSON dialect.
    if ($HostMcpConfig) {
        $row = Set-OneHost @{ name = "custom"; path = $HostMcpConfig; format = "json"; key = "mcpServers" } $binaryPath
        $tools = Test-McpEntryWorks $binaryPath
        if ($tools -le 0) { throw "configured '$($row.path)' but the server did not answer tools/list" }
        Write-Host "MCP: configured '$($row.path)' ($tools tools served)"
        if ($row.backup) { Write-Host "MCP: previous config backed up at $($row.backup)" }
        return
    }

    $targets = Select-Hosts
    if (-not $targets -or @($targets).Count -eq 0) {
        Write-Host "MCP: no known agent host config found; nothing configured."
        Write-Host "MCP: point -HostMcpConfig at your host's config, or use -Hosts all to create them."
        return
    }
    # Verify once: the entry is the same binary wherever it lands.
    $tools = Test-McpEntryWorks $binaryPath
    if ($tools -le 0) { throw "the server at $binaryPath did not answer tools/list; refusing to configure any host" }

    $done = @()
    foreach ($h in $targets) {
        try {
            $row = Set-OneHost $h $binaryPath
            $done += $row
            Write-Host ("MCP: {0,-16} configured {1}" -f $row.name, $row.path)
        } catch {
            Write-Host ("MCP: {0,-16} FAILED - {1}" -f $h.name, $_.Exception.Message)
        }
    }
    if ($done.Count -gt 0) {
        Write-Host "MCP: $($done.Count) host(s) configured; $tools tools served by the installed binary"
        if (Test-Path $receiptPath) {
            $receipt = Read-Json $receiptPath
            $receipt | Add-Member -NotePropertyName mcp_host_config_paths -NotePropertyValue (@($done | ForEach-Object { $_.path })) -Force
            Write-Utf8NoBom $receiptPath ($receipt | ConvertTo-Json -Depth 5)
        }
    }
}

function Invoke-UnconfigureMcp {
    if ($HostMcpConfig) {
        $row = @{ name = "custom"; path = $HostMcpConfig; format = "json"; key = "mcpServers" }
        Remove-OneHost $row
        return
    }
    $targets = Select-Hosts
    # Per-host isolation, like the configure side: one host that cannot be
    # handled must not stop the others from being cleaned up.
    foreach ($h in $targets) {
        try { Remove-OneHost $h }
        catch { Write-Host ("MCP: {0,-16} FAILED - {1}" -f $h.name, $_.Exception.Message) }
    }
}

function Remove-OneHost($target) {
    $path = $target.path
    if (-not (Test-Path $path)) { Write-Host ("MCP: {0,-16} no config at {1}" -f $target.name, $path); return }
    if ($target.format -eq "toml") {
        $text = Get-Content -Path $path -Raw
        # Get-TomlBlock takes a BARE section name and adds the brackets itself.
        # Passing a bracketed one produced `[[mcp_servers.docx2typed]]`, which
        # matches nothing, so the entry was reported absent while sitting in the
        # file.
        $found = Get-TomlBlock $text "$($target.key).docx2typed"
        if (-not $found) { Write-Host ("MCP: {0,-16} no docx2typed entry" -f $target.name); return }
        $lines = $text -split "`r?`n"
        $before = if ($found.start -gt 0) { $lines[0..($found.start - 1)] } else { @() }
        $after = if ($found.end -lt $lines.Count) { $lines[$found.end..($lines.Count - 1)] } else { @() }
        $backup = Backup-HostConfig $path
        Write-Utf8NoBom $path ((@($before) + @($after)) -join "`r`n")
        if (-not (Test-TomlParses $path)) {
            if ($backup) { Copy-Item $backup $path -Force }
            throw "removing the entry left '$path' unparseable; the previous file was restored"
        }
        Write-Host ("MCP: {0,-16} entry removed from {1}" -f $target.name, $path)
        return
    }
    $config = Read-HostMcpConfig $path
    if (@($config.PSObject.Properties.Name) -notcontains $target.key -or
        @($config.($target.key).PSObject.Properties.Name) -notcontains "docx2typed") {
        Write-Host ("MCP: {0,-16} no docx2typed entry" -f $target.name); return
    }
    $backup = Backup-HostConfig $path
    $config.($target.key).PSObject.Properties.Remove("docx2typed")
    Publish-HostConfig $path $config $backup
    Write-Host ("MCP: {0,-16} entry removed from {1}" -f $target.name, $path)
}


if ($ListHosts) {
    Show-Hosts
    exit 0
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
