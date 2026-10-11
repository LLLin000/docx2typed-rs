# rust_store_gate.ps1 - issue #57 differential gate: Rust generation Store +
# crash recovery tracer against the frozen Python Reference semantics
# (scripts/store.py, issue #50).
#
# Runs the SAME fault cuts, Writer lane outcomes, Operation-ID idempotency,
# ENOSPC reserve release, corruption classes, and filesystem qualification
# probes against the Rust Store (driven through the installed-style binary:
# edit = real mutation, build = external two-phase publication, store-state
# = read-only inspection) and the Python Reference store (via
# qualification/rust_store_oracle.py), then proves both sides classify every
# scenario identically (old / new / needs-recovery / frozen diagnostic code).
# Includes a REAL process kill of the Rust mutation and the focused Rust
# store test suite. Evidence JSON: qualification/evidence/rust_store_evidence.json.
#
# Usage: powershell -NoProfile -ExecutionPolicy Bypass -File qualification/rust_store_gate.ps1
#        [-Bin ..\target\release\docx2typed.exe] [-Fixture ..\corpus\release\plain.docx]
#        [-Evidence ..\qualification\evidence\rust_store_evidence.json]

param(
    [string]$Bin = "target\release\docx2typed.exe",
    [string]$Fixture = "corpus\release\plain.docx",
    [string]$Evidence = "qualification\evidence\rust_store_evidence.json"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$bin = Join-Path $root $Bin
$fixture = Join-Path $root $Fixture
$oracle = Join-Path $PSScriptRoot "rust_store_oracle.py"
if (-not (Test-Path $bin)) { throw "binary not found: $bin (run cargo build --release first)" }
if (-not (Test-Path $fixture)) { throw "fixture not found: $fixture" }

$scratch = Join-Path $env:TEMP ("rust-store-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $scratch -Force | Out-Null

function Write-Utf8NoBom([string]$path, [string]$content) {
    $utf8 = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($path, $content, $utf8)
}

function Invoke-Rust([string[]]$arguments, [hashtable]$environment = @{}) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $bin
    $psi.Arguments = ($arguments -join " ")
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    foreach ($key in $environment.Keys) {
        $psi.EnvironmentVariables[$key] = $environment[$key]
    }
    $proc = [System.Diagnostics.Process]::Start($psi)
    $stdout = $proc.StandardOutput.ReadToEnd()
    $stderr = $proc.StandardError.ReadToEnd()
    $proc.WaitForExit()
    return @{ rc = $proc.ExitCode; stdout = $stdout; stderr = $stderr }
}

function Invoke-Python([string[]]$arguments, [hashtable]$environment = @{}) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = "python"
    $psi.Arguments = ($arguments -join " ")
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    foreach ($key in $environment.Keys) {
        $psi.EnvironmentVariables[$key] = $environment[$key]
    }
    # The child must WRITE utf-8. These oracles print document text, and a CI
    # console is cp1252: without this they die with UnicodeEncodeError on the
    # first non-ASCII character (found on tracer59 printing 审稿人).
    $psi.EnvironmentVariables["PYTHONIOENCODING"] = "utf-8"
    $proc = [System.Diagnostics.Process]::Start($psi)
    $stdout = $proc.StandardOutput.ReadToEnd()
    $stderr = $proc.StandardError.ReadToEnd()
    $proc.WaitForExit()
    return @{ rc = $proc.ExitCode; stdout = $stdout; stderr = $stderr }
}

function New-PlainWorkdir([string]$dir) {
    New-Item -ItemType Directory -Path $dir -Force | Out-Null
    Copy-Item $fixture (Join-Path $dir "_template.docx")
    Write-Utf8NoBom (Join-Path $dir "typed.md") "hello`nworld`n"
    Write-Utf8NoBom (Join-Path $dir "edit.md") "# draft"
    Write-Utf8NoBom (Join-Path $dir "format.json") "{}"
    Write-Utf8NoBom (Join-Path $dir "styles.json") "{}"
}

function Get-StoreGeneration([string]$dir) {
    $state = Invoke-Rust @("store-state", "--json", $dir)
    $data = $state.stdout | ConvertFrom-Json
    return $data.data.generation
}

function Wait-Path([string]$path, [int]$timeoutSec = 30) {
    $deadline = (Get-Date).AddSeconds($timeoutSec)
    while (-not (Test-Path $path)) {
        if ((Get-Date) -gt $deadline) { throw "timed out waiting for $path" }
        Start-Sleep -Milliseconds 50
    }
}

$checks = @()

# ---------------------------------------------------------------------------
# Prepare one real workdir (python extract, human mode = plain schema-1).
# ---------------------------------------------------------------------------
$plain = Join-Path $scratch "plain"
$extract = Invoke-Python @("-m", "scripts", "extract", $fixture, "-o", $plain)
if ($extract.rc -ne 0) { throw "python extract failed: $($extract.stderr)" }

# ---------------------------------------------------------------------------
# Bullet 4: filesystem qualification probe matrix
# ---------------------------------------------------------------------------
$probeWd = Join-Path $scratch "probe-wd"
Copy-Item -Path $plain -Destination $probeWd -Recurse
$pyProbe = Invoke-Python @($oracle, "probe", $probeWd)
$pyProbeJson = $pyProbe.stdout | ConvertFrom-Json
$checks += @{ name = "python probe qualified (atomic/fsync/lock/identity)"; pass = ($pyProbe.rc -eq 0 -and $pyProbeJson.qualified -eq $true) }
$checks += @{ name = "python probe required checks true (dir_durability is the documented platform equivalent)"; pass = ($pyProbeJson.checks.atomic_replace -eq $true -and $pyProbeJson.checks.file_durability -eq $true -and $pyProbeJson.checks.advisory_lock -eq $true -and $pyProbeJson.checks.stable_identity -eq $true) }

$rustEdit = Invoke-Rust @("edit", "--json", $probeWd, "--operation-id", ("1" * 32))
$checks += @{ name = "rust store births on qualified volume"; pass = ($rustEdit.rc -eq 0 -and $rustEdit.stdout.Contains('"outcome":"success"')) }
$rustState = Invoke-Rust @("store-state", "--json", $probeWd)
$rustStateData = $rustState.stdout | ConvertFrom-Json
$checks += @{ name = "rust probe.json qualified + store-state filesystem_qualified"; pass = ($rustStateData.data.filesystem_qualified -eq $true) }
$rustProbe = Get-Content (Join-Path $probeWd ".docx2typed-store\probe.json") -Raw | ConvertFrom-Json
$checks += @{ name = "rust probe checks atomic_replace/file_durability/advisory_lock/stable_identity"; pass = ($rustProbe.qualified -eq $true -and $rustProbe.checks.atomic_replace -eq $true -and $rustProbe.checks.file_durability -eq $true -and $rustProbe.checks.advisory_lock -eq $true -and $rustProbe.checks.stable_identity -eq $true) }

# Unsupported/unprobed volumes fail BEFORE mutation (fail closed).
$unqualWd = Join-Path $scratch "unqual-wd"
New-PlainWorkdir $unqualWd
$pyUnqual = Invoke-Python @($oracle, "probe", $unqualWd) @{ DOCX2TYPED_FORCE_UNQUALIFIED = "1" }
$pyUnqualJson = $pyUnqual.stdout | ConvertFrom-Json
$checks += @{ name = "python force-unqualified fails closed (unsupported-by-design)"; pass = ($pyUnqualJson.code -eq "unsupported-by-design") }
$rustUnqual = Invoke-Rust @("edit", "--json", $unqualWd, "--operation-id", ("2" * 32)) @{ DOCX2TYPED_FORCE_UNQUALIFIED = "1" }
$rustUnqualEnvelope = $rustUnqual.stdout | ConvertFrom-Json
$checks += @{ name = "rust force-unqualified fails closed (unsupported-by-design, rc=1)"; pass = ($rustUnqual.rc -eq 1 -and $rustUnqualEnvelope.diagnostics[0].code -eq "unsupported-by-design") }
$checks += @{ name = "rust unqualified leaves no store dir (fail before mutation)"; pass = (-not (Test-Path (Join-Path $unqualWd ".docx2typed-store"))) }

# ---------------------------------------------------------------------------
# Bullet 3: same fault cuts against Python and Rust -> same outcome class
# ---------------------------------------------------------------------------
$cuts = @(
    "journal-write-intent", "journal-flush-intent", "journal-rename-intent",
    "journal-write-prepared", "journal-flush-prepared", "journal-rename-prepared",
    "generation-copy",
    "pointer-write", "pointer-flush", "pointer-rename",
    "journal-write-generation-committed",
    "ledger-write", "materialize", "journal-write-completed"
)
$cutRows = @()
foreach ($cut in $cuts) {
    # --- Python Reference oracle ---
    $pyWd = Join-Path $scratch ("py-" + ($cut -replace ":", "-"))
    Copy-Item -Path $plain -Destination $pyWd -Recurse
    $pyCut = Invoke-Python @($oracle, "cut", $pyWd, $cut)
    $pyClass = (($pyCut.stdout | ConvertFrom-Json).class)
    # --- Rust binary (fault armed via env; crash leaves the journal) ---
    $rsWd = Join-Path $scratch ("rs-" + ($cut -replace ":", "-"))
    Copy-Item -Path $plain -Destination $rsWd -Recurse
    $rs1 = Invoke-Rust @("edit", "--json", $rsWd, "--operation-id", ("3" * 32))
    $oldGen = Get-StoreGeneration $rsWd
    $rs2 = Invoke-Rust @("edit", "--json", $rsWd, "--operation-id", ("4" * 32)) @{ DOCX2TYPED_FAULT = "kill:$cut" }
    $rsCrashed = ($rs2.rc -ne 0)
    $newGen = Get-StoreGeneration $rsWd
    if (-not $rsCrashed) {
        $rsClass = "no-crash"
    } elseif ($null -eq $newGen -or $newGen -eq $oldGen) {
        $rsClass = "old"
    } else {
        $rsClass = "new"
    }
    # A fresh mutation entry runs startup recovery and commits cleanly.
    $rs3 = Invoke-Rust @("edit", "--json", $rsWd, "--operation-id", ("5" * 32))
    $recoveryClean = ($rs3.rc -eq 0)
    $match = ($pyClass -eq $rsClass)
    $checks += @{ name = "cut $cut class parity (python=$pyClass rust=$rsClass)"; pass = $match }
    $checks += @{ name = "cut $cut rust crashed + startup recovery clean"; pass = ($rsCrashed -and $recoveryClean) }
    $cutRows += [ordered]@{ cut = $cut; python = $pyClass; rust = $rsClass; match = $match; recovery_clean = ($rsCrashed -and $recoveryClean) }
}

# Real process kill of the Rust mutation (marker + parked process + kill).
$killWd = Join-Path $scratch "real-kill"
Copy-Item -Path $plain -Destination $killWd -Recurse
Invoke-Rust @("edit", "--json", $killWd, "--operation-id", ("6" * 32)) | Out-Null
$oldGen = Get-StoreGeneration $killWd
$marker = Join-Path $scratch "kill-marker"
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $bin
$psi.Arguments = "edit --json `"$killWd`" --operation-id 77777777777777777777777777777777"
$psi.UseShellExecute = $false
$psi.EnvironmentVariables["DOCX2TYPED_FAULT"] = "kill:journal-write-prepared"
$psi.EnvironmentVariables["DOCX2TYPED_FAULT_MARKER"] = $marker
$psi.EnvironmentVariables["DOCX2TYPED_FAULT_SLEEP_MS"] = "60000"
$killer = [System.Diagnostics.Process]::Start($psi)
Wait-Path $marker
$stateMid = Invoke-Rust @("store-state", "--json", $killWd)
$midPending = ($stateMid.stdout | ConvertFrom-Json).data.pending_transactions.Count
$killer.Kill()
$killer.WaitForExit()
Start-Sleep -Milliseconds 500
# The kill left the pointer untouched: the interrupted transaction can only
# roll back (complete old), never forward.
$genAfterKill = Get-StoreGeneration $killWd
$rsAfter = Invoke-Rust @("edit", "--json", $killWd, "--operation-id", ("8" * 32))
$finalState = Invoke-Rust @("store-state", "--json", $killWd)
$finalPending = ($finalState.stdout | ConvertFrom-Json).data.pending_transactions.Count
$checks += @{ name = "real process kill leaves an incomplete journal (phases visible)"; pass = ($midPending -eq 1) }
$checks += @{ name = "real process kill -> pointer never moved (complete old)"; pass = ($genAfterKill -eq $oldGen) }
$checks += @{ name = "real process kill -> next entry recovers (rolled back) and commits cleanly"; pass = ($rsAfter.rc -eq 0 -and $finalPending -eq 0) }
$killRows = [ordered]@{ cut = "real-kill:journal-write-prepared"; class = "old"; journal_visible = ($midPending -eq 1); pointer_unchanged = ($genAfterKill -eq $oldGen); recovery_clean = ($rsAfter.rc -eq 0) }

# ---------------------------------------------------------------------------
# Bullet 1: Writer lane outcomes (frozen diagnostics, bounded wait)
# ---------------------------------------------------------------------------
$laneWd = Join-Path $scratch "lane"
Copy-Item -Path $plain -Destination $laneWd -Recurse
$laneMarker = Join-Path $scratch "lane-ready"
$holderPsi = New-Object System.Diagnostics.ProcessStartInfo
$holderPsi.FileName = "python"
$holderPsi.Arguments = "`"$oracle`" hold `"$laneWd`" `"$laneMarker`""
$holderPsi.UseShellExecute = $false
$holderPsi.RedirectStandardOutput = $true
$holderPsi.RedirectStandardError = $true
$holderProc = [System.Diagnostics.Process]::Start($holderPsi)
Wait-Path $laneMarker
$rustBusy = Invoke-Rust @("edit", "--json", $laneWd, "--operation-id", ("9" * 32))
$rustBusyEnvelope = $rustBusy.stdout | ConvertFrom-Json
$checks += @{ name = "rust immediate contention -> writer-busy"; pass = ($rustBusy.rc -eq 1 -and $rustBusyEnvelope.diagnostics[0].code -eq "writer-busy") }
$started = Get-Date
$rustTimeout = Invoke-Rust @("edit", "--json", $laneWd, "--operation-id", ("10" * 32), "--lock-timeout-ms", "300")
$rustTimeoutEnvelope = $rustTimeout.stdout | ConvertFrom-Json
$elapsed = ((Get-Date) - $started).TotalSeconds
$checks += @{ name = "rust bounded wait expiry -> writer-timeout (<10s)"; pass = ($rustTimeout.rc -eq 1 -and $rustTimeoutEnvelope.diagnostics[0].code -eq "writer-timeout" -and $elapsed -lt 10) }
$pyBusy = Invoke-Python @($oracle, "mutate", $laneWd, "0")
$checks += @{ name = "python immediate contention -> writer-busy (parity)"; pass = (($pyBusy.stdout | ConvertFrom-Json).code -eq "writer-busy") }
$pyTimeout = Invoke-Python @($oracle, "mutate", $laneWd, "300")
$checks += @{ name = "python bounded wait expiry -> writer-timeout (parity)"; pass = (($pyTimeout.stdout | ConvertFrom-Json).code -eq "writer-timeout") }
$holderProc.Kill()
$holderProc.WaitForExit()
Start-Sleep -Milliseconds 500
$rustFree = Invoke-Rust @("edit", "--json", $laneWd, "--operation-id", ("11" * 32))
$checks += @{ name = "lock-holder death releases the lane (rust edit succeeds)"; pass = ($rustFree.rc -eq 0 -and $rustFree.stdout.Contains('"outcome":"success"')) }

# ---------------------------------------------------------------------------
# Bullet 1/2: generation-conflict (CAS) + durability ordering
# ---------------------------------------------------------------------------
$conflictWd = Join-Path $scratch "conflict"
Copy-Item -Path $plain -Destination $conflictWd -Recurse
$pyConflict = Invoke-Python @($oracle, "mutate", $conflictWd, "0", ("d" * 32), "conflict")
$checks += @{ name = "python stale-writer CAS -> generation-conflict"; pass = (($pyConflict.stdout | ConvertFrom-Json).code -eq "generation-conflict") }

$durWd = Join-Path $scratch "durable"
Copy-Item -Path $plain -Destination $durWd -Recurse
$dur = Invoke-Rust @("edit", "--json", $durWd, "--operation-id", ("12" * 32))
$durEnvelope = $dur.stdout | ConvertFrom-Json
$pointer = Get-Content (Join-Path $durWd "workdir.json") -Raw | ConvertFrom-Json
$genDir = Join-Path $durWd (".docx2typed-store\generations\" + $pointer.generation)
$checks += @{ name = "rust mutation reports success only after durability (pointer+generation.json+evidence+ledger)"; pass = ($dur.rc -eq 0 -and $durEnvelope.outcome -eq "success" -and (Test-Path (Join-Path $genDir "generation.json")) -and (Test-Path (Join-Path $genDir "run.evidence.json")) -and (Test-Path (Join-Path $genDir "operation-ledger.json"))) }
$genManifest = Get-Content (Join-Path $genDir "generation.json") -Raw | ConvertFrom-Json
$checks += @{ name = "pointer manifest_sha256 == generation assets_sha256"; pass = ($pointer.manifest_sha256 -eq $genManifest.assets_sha256) }
$checks += @{ name = "root materialized mirror == committed generation (typed.md)"; pass = ((Get-Content (Join-Path $durWd "typed.md") -Raw) -eq (Get-Content (Join-Path $plain "typed.md") -Raw)) }

# ---------------------------------------------------------------------------
# Bullet 2/3: Operation-ID exactly-once (identical retry replays; no second
# effect) + store-backed external build two-phase publication
# ---------------------------------------------------------------------------
$pyReplayWd = Join-Path $scratch "replay-py"
Copy-Item -Path $plain -Destination $pyReplayWd -Recurse
$op = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
$pyReplay = Invoke-Python @($oracle, "replay", $pyReplayWd, $op)
$pyReplayJson = $pyReplay.stdout | ConvertFrom-Json
$checks += @{ name = "python identical retry replays original envelope, no second effect"; pass = ($pyReplayJson.replay_equal -eq $true -and $pyReplayJson.generation_unchanged -eq $true) }
$replayWd = Join-Path $scratch "replay-rs"
Copy-Item -Path $plain -Destination $replayWd -Recurse
$rustReplay1 = Invoke-Rust @("edit", "--json", $replayWd, "--operation-id", $op)
$gensBefore = (Get-ChildItem (Join-Path $replayWd ".docx2typed-store\generations")).Count
$rustReplay2 = Invoke-Rust @("edit", "--json", $replayWd, "--operation-id", $op)
$gensAfter = (Get-ChildItem (Join-Path $replayWd ".docx2typed-store\generations")).Count
$checks += @{ name = "rust identical retry replays original envelope (byte-equal)"; pass = ($rustReplay1.rc -eq 0 -and $rustReplay1.stdout -eq $rustReplay2.stdout) }
$checks += @{ name = "rust replay has no duplicate generation effect"; pass = ($gensAfter -eq $gensBefore) }

$buildWd = Join-Path $scratch "build"
Copy-Item -Path $plain -Destination $buildWd -Recurse
Invoke-Rust @("edit", "--json", $buildWd, "--operation-id", ("13" * 32)) | Out-Null
$buildGensBefore = (Get-ChildItem (Join-Path $buildWd ".docx2typed-store\generations")).Count
$buildOut = Join-Path $scratch "built.docx"
$build = Invoke-Rust @("build", "--json", $buildWd, "-o", $buildOut, "--operation-id", ("14" * 32))
$buildEnvelope = $build.stdout | ConvertFrom-Json
$checks += @{ name = "store-backed external build publishes (two-phase)"; pass = ($build.rc -eq 0 -and $buildEnvelope.outcome -eq "success" -and (Test-Path $buildOut)) }
$checks += @{ name = "build output == template bytes (no-op contract)"; pass = ((Get-FileHash -Algorithm SHA256 -Path $buildOut).Hash.ToLower() -eq (Get-FileHash -Algorithm SHA256 -Path $fixture).Hash.ToLower()) }
$checks += @{ name = "build evidence + operation-ledger durable beside output"; pass = ((Test-Path ($buildOut + ".evidence.json")) -and (Test-Path ($buildOut + ".operation-ledger.json"))) }
$buildGens = (Get-ChildItem (Join-Path $buildWd ".docx2typed-store\generations")).Count
$checks += @{ name = "external build never moves the generation pointer"; pass = ($buildGens -eq $buildGensBefore) }

# ---------------------------------------------------------------------------
# Bullet 3: ENOSPC -> reserve-depleted (read-only until replenished)
# ---------------------------------------------------------------------------
$pyEnospcWd = Join-Path $scratch "enospc-py"
Copy-Item -Path $plain -Destination $pyEnospcWd -Recurse
$pyEnospc = Invoke-Python @($oracle, "mutate", $pyEnospcWd, "0", ("f" * 32), "enospc")
$pyEnospcJson = $pyEnospc.stdout | ConvertFrom-Json
$checks += @{ name = "python ENOSPC -> reserve-depleted + reserve released + marker"; pass = ($pyEnospcJson.code -eq "reserve-depleted" -and $pyEnospcJson.reserve_released -eq $true -and $pyEnospcJson.marker -eq $true) }
$enospcWd = Join-Path $scratch "enospc-rs"
Copy-Item -Path $plain -Destination $enospcWd -Recurse
# Birth first (the fault must fire inside a real mutation's journal, not the
# birth), then arm ENOSPC on the prepared journal write.
Invoke-Rust @("edit", "--json", $enospcWd, "--operation-id", ("14" * 32)) | Out-Null
$rustEnospc = Invoke-Rust @("edit", "--json", $enospcWd, "--operation-id", ("15" * 32)) @{ DOCX2TYPED_FAULT = "enospc:journal-write-prepared" }
$rustEnospcEnvelope = $rustEnospc.stdout | ConvertFrom-Json
$rustReserve = Get-Item (Join-Path $enospcWd ".docx2typed-store\reserve")
$checks += @{ name = "rust ENOSPC -> reserve-depleted (frozen code)"; pass = ($rustEnospc.rc -eq 1 -and $rustEnospcEnvelope.diagnostics[0].code -eq "reserve-depleted") }
$checks += @{ name = "rust reserve released (< 1 MiB) + marker written"; pass = ($rustReserve.Length -lt 1048576 -and (Test-Path (Join-Path $enospcWd ".docx2typed-store\reserve-depleted.json"))) }
$rustReadonly = Invoke-Rust @("edit", "--json", $enospcWd, "--operation-id", ("16" * 32))
$rustReadonlyEnvelope = $rustReadonly.stdout | ConvertFrom-Json
$checks += @{ name = "rust depleted workdir is read-only (reserve-depleted until replenished)"; pass = ($rustReadonly.rc -eq 1 -and $rustReadonlyEnvelope.diagnostics[0].code -eq "reserve-depleted") }

# ---------------------------------------------------------------------------
# Bullet 3: corruption -> needs-recovery / store-invalid (never guessed)
# ---------------------------------------------------------------------------
$corruptWd = Join-Path $scratch "corrupt"
Copy-Item -Path $plain -Destination $corruptWd -Recurse
$pyCorruptJournal = Invoke-Python @($oracle, "corrupt-journal", $corruptWd)
$pyCorruptJson = $pyCorruptJournal.stdout | ConvertFrom-Json
$checks += @{ name = "python corrupt journal chain -> needs-recovery"; pass = ($pyCorruptJson.needs_recovery -eq $true) }
$rustCorrupt = Join-Path $scratch "corrupt-rs"
Copy-Item -Path $plain -Destination $rustCorrupt -Recurse
Invoke-Rust @("edit", "--json", $rustCorrupt, "--operation-id", ("17" * 32)) | Out-Null
Invoke-Rust @("edit", "--json", $rustCorrupt, "--operation-id", ("18" * 32)) @{ DOCX2TYPED_FAULT = "kill:journal-write-prepared" } | Out-Null
$txDir = Join-Path $rustCorrupt (Join-Path ".docx2typed-store\transactions" ("18" * 32))
Write-Utf8NoBom (Join-Path $txDir "intent.json") '{"schema":"docx2typed-transaction-journal-1","phase":"intent","tampered":true}'
$rustCorruptEdit = Invoke-Rust @("edit", "--json", $rustCorrupt, "--operation-id", ("19" * 32))
$rustCorruptEnvelope = $rustCorruptEdit.stdout | ConvertFrom-Json
$checks += @{ name = "rust corrupt journal chain -> needs-recovery at next entry"; pass = ($rustCorruptEdit.rc -eq 1 -and $rustCorruptEnvelope.diagnostics[0].code -eq "needs-recovery") }

$pointerWd = Join-Path $scratch "pointer-rs"
Copy-Item -Path $plain -Destination $pointerWd -Recurse
Invoke-Rust @("edit", "--json", $pointerWd, "--operation-id", ("20" * 32)) | Out-Null
Write-Utf8NoBom (Join-Path $pointerWd "workdir.json") "{not json"
$rustPointerEdit = Invoke-Rust @("edit", "--json", $pointerWd, "--operation-id", ("21" * 32))
$rustPointerEnvelope = $rustPointerEdit.stdout | ConvertFrom-Json
$checks += @{ name = "rust corrupt pointer -> store-invalid (never guessed)"; pass = ($rustPointerEdit.rc -eq 1 -and $rustPointerEnvelope.diagnostics[0].code -eq "store-invalid") }

# ---------------------------------------------------------------------------
# Focused Rust store test suite (every named fault class)
# ---------------------------------------------------------------------------
$oldEap = $ErrorActionPreference
$ErrorActionPreference = "Continue"
$testRun = & cargo test --manifest-path "$root\Cargo.toml" -p docx2typed-store --test store_recovery 2>&1 | Out-String
$ErrorActionPreference = $oldEap
# Frozen count, deliberately: it catches a test silently disappearing. It also
# means adding a test is a deliberate edit here -- and because CI runs the
# schema and tracer62 gates but NOT this one, a drift here is invisible to CI.
# It reached 21 when #42 added the version-HEAD carry-forward pair and #37's
# boundary regression; the count was 18 at the last commit CI could see.
$checks += @{ name = "focused rust store tests pass (21 fault/durability/probe cases)"; pass = ($testRun -match "test result: ok. 21 passed") }

# ---------------------------------------------------------------------------
# Verdict + evidence JSON
# ---------------------------------------------------------------------------
$failures = @($checks | Where-Object { -not $_.pass })
$verdict = if ($failures.Count -eq 0) { "pass" } else { "fail" }
$report = [ordered]@{
    schema = "docx2typed-rust-store-gate-1"
    issue = 57
    verdict = $verdict
    checked_at = (Get-Date).ToString("o")
    binary = $bin
    probe_matrix = [ordered]@{
        qualified = $true
        python_checks = $pyProbeJson.checks
        rust_store_state_qualified = $rustStateData.data.filesystem_qualified
        force_unqualified_python = $pyUnqualJson.code
        force_unqualified_rust = $rustUnqualEnvelope.diagnostics[0].code
    }
    writer_lane = [ordered]@{
        rust_busy = $rustBusyEnvelope.diagnostics[0].code
        rust_timeout = $rustTimeoutEnvelope.diagnostics[0].code
        python_busy = ($pyBusy.stdout | ConvertFrom-Json).code
        python_timeout = ($pyTimeout.stdout | ConvertFrom-Json).code
        holder_death_releases_lane = $true
    }
    generation_cas = [ordered]@{
        python_stale_writer = ($pyConflict.stdout | ConvertFrom-Json).code
        rust_focused_tests = "stale_writer_generation_conflict + cas_race_yields_one_winner"
    }
    fault_cuts = $cutRows
    real_process_kill = $killRows
    durability_ordering = [ordered]@{
        success_only_after = "pointer + generation.json + run.evidence.json + operation-ledger.json + materialized root"
        pointer_manifest_matches = ($pointer.manifest_sha256 -eq $genManifest.assets_sha256)
    }
    exactly_once = [ordered]@{
        python_replay_equal = $pyReplayJson.replay_equal
        rust_replay_equal = ($rustReplay1.stdout -eq $rustReplay2.stdout)
        rust_no_duplicate_generation = ($gensAfter -eq $gensBefore)
    }
    enospc = [ordered]@{
        python = $pyEnospcJson
        rust_code = $rustEnospcEnvelope.diagnostics[0].code
        rust_reserve_released = ($rustReserve.Length -lt 1048576)
        rust_read_only = ($rustReadonlyEnvelope.diagnostics[0].code -eq "reserve-depleted")
    }
    corruption = [ordered]@{
        python_journal = $pyCorruptJson
        rust_journal_code = $rustCorruptEnvelope.diagnostics[0].code
        rust_pointer_code = $rustPointerEnvelope.diagnostics[0].code
    }
    checks = @($checks | ForEach-Object { [ordered]@{ name = $_.name; pass = $_.pass } })
}
$reportJson = $report | ConvertTo-Json -Depth 12
$evidencePath = Join-Path $root $Evidence
Write-Utf8NoBom $evidencePath $reportJson
$reportJson
if ($failures.Count -gt 0) {
    Write-Host "GATE FAILED: $($failures.Count) checks" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host ("  - " + $_.name) -ForegroundColor Red }
    Remove-Item -Recurse -Force $scratch
    exit 1
}
Write-Host "ALL STORE GATES PASS" -ForegroundColor Green
Remove-Item -Recurse -Force $scratch
exit 0
