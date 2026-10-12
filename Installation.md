# Installing docx2typed — the agent procedure

Read this when the user asks you to install, configure, upgrade, or remove
`docx2typed`. It is written for **you**, not for a human operator: every step
names the command, what it changes, and what to check before moving on.

Two things get installed, and they are separate:

| | what | why it is separate |
|---|---|---|
| **the engine** | the Rust binary | runs the document work; usable from a shell on its own |
| **the wiring** | the MCP entry + the skill | makes the engine reachable from the agent you are running in |

The wiring is host-specific. There is no single place to install an MCP server,
so **you configure the host you are running in, and you ask before touching any
other.** A machine can have several agents installed at once, each with its own
config file and its own format.

---

## Step 0 — Preflight (change nothing yet)

Find out what already exists, and report it before acting.

```powershell
# Is an engine already installed, and is it the Rust one?
Get-Command docx2typed -ErrorAction SilentlyContinue | Select-Object Source
& docx2typed --version --json    # must say "name": "docx2typed-rust"
```

```powershell
# Which agent hosts are present, and where does each keep its MCP config?
pwsh -NoProfile -File scripts/install_binary.ps1 -ListHosts
```

That prints the host table. As of this writing:

| host | config | format | key |
|---|---|---|---|
| omp | `~/.omp/agent/mcp.json` | JSON | `mcpServers` |
| claude-code | `~/.claude.json` | JSON | `mcpServers` |
| claude-desktop | `%APPDATA%\Claude\claude_desktop_config.json` | JSON | `mcpServers` |
| cursor | `~/.cursor/mcp.json` | JSON | `mcpServers` |
| windsurf | `~/.codeium/windsurf/mcp_config.json` | JSON | `mcpServers` |
| continue | `~/.continue/config.json` | JSON | `mcpServers` |
| **codex** | `~/.codex/config.toml` | **TOML** | `[mcp_servers.<name>]` |
| **vscode / copilot** | `%APPDATA%\Code\User\settings.json` | JSON | **`servers`** |
| zed | `%APPDATA%\Zed\settings.json` | JSON | `context_servers` |

**Report what you found, including anything stale.** A host pointing at an old
binary is a real problem worth naming: the engine changes, and an entry left on
an earlier build silently runs the earlier behaviour.

## Step 1 — Confirm the scope with the user

Ask, in one message:

1. **Which host(s)** should be wired? The default is the one you are running in.
   Do not silently edit another agent's configuration.
2. **Which binary source** (Step 2), if more than one is available.

If the user has already said "install it", wiring the host you are running in is
the reasonable reading. Wiring *every* host is not — say what you would change
and let them choose.

## Step 2 — Obtain the binary

**A. From crates.io (preferred when the user has Rust).** The crate is published
as `0.1.0-rc1`; this builds and installs in one step:

```text
cargo install docx2typed --version 0.1.0-rc1
```

Result: `~/.cargo/bin/docx2typed[.exe]`. Skip to Step 3 and pass that path.

**B. A release bundle**, if one is offered. Verify before using it — the bundle
carries `SHA256SUMS.txt` and a detached signature:

```text
sha256sum -c SHA256SUMS.txt
```

No bundle is published by this repository at the time of writing; do not invent
a download URL.

**C. Build from a checkout** (development, or when neither of the above is
available):

```text
cargo build --release --locked
```

Result: `target/release/docx2typed[.exe]`.

Whichever source you use, check it before installing:

```text
docx2typed --version --json
```

`"name": "docx2typed-rust"` and a `target` matching the machine. If it says
anything else, stop and report — you have the wrong artifact.

## Step 3 — Install the engine

The Windows installer is receipt-safe: it records every path it owns, so
uninstall removes exactly those and nothing else.

```powershell
powershell -NoProfile -ExecutionPolicy Bypass `
  -File scripts\install_binary.ps1 `
  -Action install `
  -Bin <path to the binary from Step 2>
```

Layout:

```text
%LOCALAPPDATA%\docx2typed\
├── bin\docx2typed.exe       installed Rust binary
├── receipt.json              version, SHA-256, owned paths
└── mcp.config.json           MCP snippet with the absolute binary path
```

`-Action update` replaces an existing install and keeps one backup;
`-Action rollback` consumes it.

On macOS/Linux there is no installer: copy the binary to a user-owned directory
and use its absolute path in Step 4.

## Step 4 — Wire the MCP entry

This is the host-specific step. **The entry must point at the installed binary's
absolute path** and pass `mcp` as its only argument.

### If the host is in the table

One command configures every host whose config already exists:

```powershell
pwsh -NoProfile -File scripts\install_binary.ps1 -Action configure-mcp
```

Or name them:

```powershell
pwsh -NoProfile -File scripts\install_binary.ps1 -Action configure-mcp -Hosts omp,codex
```

**`-Prefix` must match what Step 3 used.** `configure-mcp` wires the binary it
finds under the prefix, defaulting to `%LOCALAPPDATA%\docx2typed`. If you
installed with a custom `-Prefix`, pass the same one here or you will wire the
default install — or nothing, if there is no default. The single-command flow
(`-Action install` without `-NoMcp`) does not have this problem: it wires the
binary it just installed.

What it does to each config, in order: read → **back up** (timestamped, beside
the file) → merge **only** `docx2typed` → re-parse the merged text → publish
atomically → **launch the server and count `tools/list`**. A config that does
not parse is refused and left byte-identical.

### If the host is NOT in the table

Do not guess the format. Print the snippet, and let the user place it:

```json
{
  "mcpServers": {
    "docx2typed": {
      "command": "<absolute path to the installed binary>",
      "args": ["mcp"]
    }
  }
}
```

Adapt the key to the host's own convention (`servers` for VS Code,
`[mcp_servers.docx2typed]` for TOML hosts) — and say which you adapted, so the
user can check it.

## Step 5 — Install the skill

Enable this repository's `SKILL.md` through the host's own skill mechanism. Do
not create a second copy if the host manager already has one, and do not remove
the user's other skills.

The skill is what tells you *how* to use the engine — the facade, the completion
rules, the refusal paths. The MCP entry is what gives you the tools. Both are
needed; neither substitutes for the other.

## Step 6 — Verify, and report only what you observed

```text
1. the binary reports "docx2typed-rust"                docx2typed --version --json
2. extract succeeds on a representative DOCX           docx2typed extract <doc> -o <wd>
3. build and independent verify succeed                docx2typed build <wd> -o <out>
                                                       docx2typed verify <wd> <out>
4. the MCP entry points at the installed absolute
   binary with args ["mcp"]                            read the config back
5. the server answers tools/list                       launch it and count
6. existing user configuration is unchanged except
   for the additions the user authorized                diff against the backup
```

Office save/reopen is a **separate, host-dependent** gate. If no real
Word/LibreOffice is available, report `not-run-no-host`. Do not claim it, and do
not build Office automation to fake it.

## Step 7 — Upgrade, rollback, remove

```powershell
# upgrade to new bytes (keeps one backup)
powershell -File scripts\install_binary.ps1 -Action update -Bin <new binary>

# go back to the previous bytes
powershell -File scripts\install_binary.ps1 -Action rollback

# remove the MCP entry only
pwsh -File scripts\install_binary.ps1 -Action unconfigure-mcp

# remove the entry AND the binary (receipt-scoped)
powershell -File scripts\install_binary.ps1 -Action uninstall
```

---

## What not to do

- **Do not edit another agent's configuration without asking.** Several hosts on
  one machine is normal; each config belongs to its host.
- **Do not hand-edit a config you cannot validate.** JSON is re-parsed before it
  is written; TOML is checked with a real parser or refused.
- **Do not claim a verification you did not run.** Step 6 is a checklist of
  observations, not a formality.
- **Do not replace the entry's command with `python`, `uvx`, or a wrapper.** The
  engine is the binary; a Python interpreter in that field is a different,
  unsupported runtime.
- **Do not hand-edit the DOCX package.** Once installed, all document work goes
  through the engine.

## Known environment limits

- **Windows PowerShell 5.1 cannot parse every host config.** Measured on a real
  `~/.omp/agent/mcp.json`: 5.1 rejects it with *unrecognized escape sequence* at
  the first doubled backslash in a Windows path, while PowerShell 7 parses the
  same file. The installer detects this and hands the action to `pwsh` when it
  exists; if `pwsh` is absent it says so rather than corrupting or refusing the
  file.
- **TOML validation needs a real parser.** Python 3.11+ (`tomllib`) is used when
  present; without it the installer refuses the TOML edit and restores the
  backup, because a structural guess is not a parse.

## Test the installer

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\mcp_merge_tests.ps1
```

27 checks over temp prefixes and temp host configs — other servers survive,
re-running is idempotent, every "no entry yet" shape works, a corrupt config is
refused untouched, both dialects round-trip, uninstall removes only our entry,
and `-NoMcp` touches nothing. No signing key, no release bundle, and it never
reads a real host configuration.
