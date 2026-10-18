# real_workflow_word_handoff.ps1 - Word bidirectional handoff lane entry point.
#
# Owns qualification/real_workflow_word_handoff.ps1 (companion to the .py lane).
# Thin wrapper: resolves defaults (binary/manifest/out), enforces a fresh COM
# instance per Word call (owned by the .py lane scripts), and runs the lane.
# No Zotero, no core changes, no XML simulation: Word work goes through real
# Word COM; a missing Word host yields not-run-no-host, never a PASS.
#
# Usage:
#   powershell -NoProfile -ExecutionPolicy Bypass `
#     -File qualification/real_workflow_word_handoff.ps1 `
#     [-Bin <path>] [-Manifest <path>] [-Out <path>] [-TimeoutSec <n>] [-InputSha <sha>]

param(
    [string]$Bin = "",
    [string]$Manifest = "",
    [string]$Out = "",
    [int]$TimeoutSec = 240,
    [string]$InputSha = ""
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
if ([string]::IsNullOrWhiteSpace($Bin)) {
    $release = Join-Path $root "target\release\docx2typed.exe"
    $Bin = if (Test-Path $release) { $release } else { Join-Path $root "target\release\docx2typed" }
}
if ([string]::IsNullOrWhiteSpace($Manifest)) { $Manifest = "C:\tmp\real-docx-corpus-v1\manifest.json" }
if ([string]::IsNullOrWhiteSpace($Out)) { $Out = "C:\tmp\real-workflow-qualification-v1\word-handoff" }

if (Test-Path $Out) { throw "refusing to reuse output dir (fresh dir required): $Out" }
New-Item -ItemType Directory -Path $Out -Force | Out-Null

$lane = Join-Path $PSScriptRoot "real_workflow_word_handoff.py"
if (-not (Test-Path $Bin)) { throw "binary not found: $Bin (build release first)" }
if (-not (Test-Path $Manifest)) { throw "manifest not found: $Manifest" }

$pyArgs = @($lane, "--bin", $Bin, "--manifest", $Manifest, "--out", $Out, "--timeout", "$TimeoutSec")
if (-not [string]::IsNullOrWhiteSpace($InputSha)) { $pyArgs += @("--input-sha", $InputSha) }

# python from PATH (same interpreter family the repo gates use); stdout is the
# lane summary, report.json + private logs land under $Out.
& python @pyArgs
exit $LASTEXITCODE
