# docx2typed

[中文版本](https://github.com/LLLin000/docx2typed-rs/blob/main/README.zh-CN.md) · [Installation and collaboration](https://github.com/LLLin000/docx2typed-rs/blob/main/Installation.md)

> Structure-preserving DOCX editing from a self-contained Rust binary, with MCP review workflows and independent verification.

`docx2typed` edits `.docx` text without flattening the document into lossy plain
text or HTML. Existing formatting, tracked revisions, comments, tables, content
controls, anchors, and untouched package parts are protected by the typed
workdir and byte-preserving build pipeline.

## Which repository is this?

This is the **Rust production repository**:

- CLI: `docx2typed <command>`
- MCP: `docx2typed mcp` over clean stdio
- Browser review server: `docx2typed review <workdir>`
- Python: a separate offline reference implementation, never a runtime fallback

The Python reference remains at
[`LLLin000/docx2typed`](https://github.com/LLLin000/docx2typed). Rust production
does not require Python, `uvx`, a source checkout, or a Python MCP launcher.

## Installation paths

There is no universal one-line installer yet. Use exactly one of these paths:

| Situation | Supported path |
|---|---|
| Windows user with a local binary | `scripts/install_binary.ps1` |
| macOS/Linux user with a local binary | Copy the binary to a user-owned `bin` directory and configure MCP with its absolute path |
| Developer | `cargo build --release --locked`, then use the resulting binary |
| Release operator | Produce a signed target bundle with `scripts/package_release.ps1` |

The PowerShell installer is **Windows-only** and does not download a binary. The
repository does not provide `install.sh`, Homebrew, apt, winget, or a PyPI
installation path for the Rust runtime.

## Windows: build and install

From a Rust checkout:

```powershell
cargo build --release --locked
$source = (Resolve-Path .\target\release\docx2typed.exe).Path
& $source --version --json
```

Install the verified binary with the receipt-safe lifecycle installer:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\install_binary.ps1 `
  -Action install `
  -Bin $source
```

The default installation is:

```text
%LOCALAPPDATA%\docx2typed\
├── bin\docx2typed.exe       installed Rust binary
├── receipt.json              version, SHA-256, and owned paths
└── mcp.config.json           MCP snippet using the absolute binary path
```

The installer does not modify `PATH`. Use the absolute path, or opt into the
current PowerShell session explicitly:

```powershell
$bin = Join-Path $env:LOCALAPPDATA 'docx2typed\bin\docx2typed.exe'
$env:Path = "$(Split-Path $bin);$env:Path"
& $bin --version --json
```

Lifecycle operations require the existing receipt:

```powershell
powershell -File .\scripts\install_binary.ps1 -Action update -Bin $source
powershell -File .\scripts\install_binary.ps1 -Action rollback
powershell -File .\scripts\install_binary.ps1 -Action uninstall
```

`update` keeps one verified backup. `rollback` consumes that backup. `uninstall`
removes only receipt-owned files whose recorded hashes still match; it refuses
to guess after user changes.

## macOS/Linux: use a built or released binary

The repository currently has no cross-platform installer. After building or
extracting a target-matched release bundle, place the binary in a directory you
own:

```bash
cargo build --release --locked
mkdir -p "$HOME/.local/bin"
cp target/release/docx2typed "$HOME/.local/bin/docx2typed"
chmod 755 "$HOME/.local/bin/docx2typed"
export PATH="$HOME/.local/bin:$PATH"
docx2typed --version --json
```

For a release bundle, verify `SHA256SUMS.txt` and its detached signature before
copying the binary. A bundle contains the binary, checksums, provenance, SBOM,
licenses, and signature; it does not contain a Python runtime or a background
service.

## Configure MCP

The canonical MCP configuration points directly to the installed Rust binary:

```json
{
  "mcpServers": {
    "docx2typed": {
      "command": "C:\\Users\\<you>\\AppData\\Local\\docx2typed\\bin\\docx2typed.exe",
      "args": ["mcp"]
    }
  }
}
```

On macOS/Linux, replace `command` with the absolute path to the copied binary.
On Windows, the installer writes the same shape to
`%LOCALAPPDATA%\docx2typed\mcp.config.json`. Copy that object into the host's
MCP configuration only after authorization; preserve existing servers.

Do not replace the command with `python`, `uvx`, `cargo run`, a relative path,
or a repository import. The MCP process writes protocol replies to stdout and
logs to stderr.

Smoke the installed server directly:

```text
{"tool":"engine_info","args":{}}
{"tool":"tools/list","args":{}}
```

Expected result: one `OK` line per request and 36 frozen tools in `tools/list`.

## First document

The source DOCX is never overwritten. A typed workdir contains the extracted
state, immutable template, fingerprints, and generation store.

```powershell
$bin = Join-Path $env:LOCALAPPDATA 'docx2typed\bin\docx2typed.exe'
& $bin extract input.docx -o workdir --json
& $bin enumerate workdir --json
& $bin edit text workdir P0.0 "old text" "new text" --json
& $bin build workdir -o output.docx --json
& $bin verify workdir output.docx --json
```

Use `docx2typed revisions`, `decide`, `comment`, and the MCP review tools for
tracked revisions, comments, table structure, and human-agent handoff. See
[`capabilities.md`](https://github.com/LLLin000/docx2typed-rs/blob/main/capabilities.md) for the complete command surface.

## Browser review

Start the local review server after extracting a workdir:

```powershell
& $bin review workdir --host 127.0.0.1 --port 8876
```

Open <http://127.0.0.1:8876/>. The browser queues decisions and patches; it
does not write the DOCX behind the agent's back. The agent applies the queue,
builds a new output, and runs independent verification.

The Rust binary has no `--tailscale` mode and does not silently broaden a local
bind to `0.0.0.0`. Remote access requires an explicitly controlled private
interface and host ACLs.

## Delivery gate

Every delivery follows:

```text
extract → inspect/enumerate → edit or review → build → independent verify → optional Office check
```

`verify` independently re-derives the baseline and checks text, styles,
protected structures, revisions/comments, and package-part identity. A clean
build is not a substitute for verification. Office save/reopen is an optional
host-dependent check; without a real host, report `not-run-no-host`.

## Further reading

- [Installation and collaboration guide](https://github.com/LLLin000/docx2typed-rs/blob/main/Installation.md)
- [CLI and MCP capabilities](https://github.com/LLLin000/docx2typed-rs/blob/main/capabilities.md)
- [End-to-end workflows](https://github.com/LLLin000/docx2typed-rs/blob/main/composites.md)
- [Verification guarantees](https://github.com/LLLin000/docx2typed-rs/blob/main/verification.md)
- [Agent skill](https://github.com/LLLin000/docx2typed-rs/blob/main/SKILL.md)
