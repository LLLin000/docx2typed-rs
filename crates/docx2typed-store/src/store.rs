//! Issue #57: the real generation Store — immutable generations, Writer
//! lane, hash-chained transaction journals, Operation ledger, 1 MiB recovery
//! reserve, startup recovery, deterministic fault injection, and external
//! two-phase publication. Mirrors `scripts/store.py` (issues #34/#50/#57)
//! semantics on the real filesystem.
//!
//! Layout of a store-backed workdir (`<root>`)::
//!
//! ```text
//! <root>/workdir.json                     pointer: atomically selects the current
//!                                         immutable Workdir generation
//! <root>/.docx2typed-store/               private; excluded from asset closure
//!     probe.json                          filesystem qualification probe result
//!     lock                                Writer lane (fixed inode, OS advisory lock)
//!     reserve                             1 MiB genuinely allocated recovery reserve
//!     reserve-depleted.json               marker when the reserve was released (ENOSPC)
//!     generations/<gen-id>/               immutable full snapshots (authoritative)
//!         generation.json                 generation manifest (assets + parent + sha256)
//!         <workdir assets>                typed.md, format.json, ..., run.evidence.json
//!     transactions/<operation_id>/        hash-chained phase records:
//!         intent.json                     (prev = pointer hash) operation started
//!         prepared.json                   generation/evidence/externals staged
//!         external-published.json         external outputs atomically published
//!         generation-committed.json       pointer CAS committed
//!         completed.json                  ledger durable; transaction finished
//!     staging/<operation_id>/             prepared external outputs before publish
//!     recovery/<run_id>.json              recovery Run evidence (immutable)
//!     quarantine/<name>/                  ambiguous state, never guessed into repair
//! ```
//!
//! Guarantee boundary: every cut point (kill before/after journal write/
//! flush/rename, external publish, pointer swap, materialize; ENOSPC; short
//! write; flush failure; corruption; CAS race; lock-holder death) yields only
//! the complete old generation, the complete new generation, or explicit
//! `needs-recovery` — never a mixed generation, evidence-free mutation,
//! duplicated Operation-ID effect, or half-published external output.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use docx2typed_protocol::{
    canonical_json, file_sha256, new_operation_id, resolve_path, run_evidence, semantic_sha256,
    ResultEnvelope, RunEvidence, RESULT_SCHEMA,
};
use fs2::FileExt;
use serde_json::{json, Value};

use crate::StoreError;

// ---------------------------------------------------------------------------
// Frozen layout + schema constants (mirror `scripts/store.py`)
// ---------------------------------------------------------------------------

pub const STORE_DIR_NAME: &str = ".docx2typed-store";
pub const POINTER_FILE: &str = "workdir.json";
pub const POINTER_SCHEMA: &str = "docx2typed-workdir-pointer-1";
pub const GENERATION_MANIFEST_SCHEMA: &str = "docx2typed-generation-manifest-1";
pub const JOURNAL_SCHEMA: &str = "docx2typed-transaction-journal-1";
pub const PROBE_SCHEMA: &str = "docx2typed-store-probe-1";
pub const RECOVERY_EVIDENCE_SCHEMA: &str = "docx2typed-recovery-evidence-1";
pub const LEDGER_SCHEMA: &str = "docx2typed-operation-ledger-1";

/// 1 MiB recovery reserve, genuinely allocated.
pub const RESERVE_BYTES: u64 = 1024 * 1024;

/// Canonical journal phase order (recovery reads in this order).
pub const PHASE_ORDER: [&str; 5] = [
    "intent",
    "prepared",
    "external-published",
    "generation-committed",
    "completed",
];

/// Root files that stay mutable Draft ingress: reads take them from the
/// root, mutations overlay them into the generation copy before running.
pub const INGRESS_FILES: [&str; 2] = ["typed.md", "edit.md"];

/// Root sidecars the engine may create and later remove.
///
/// A sidecar that becomes empty is deleted from the generation, but
/// [`Store::materialize_root`] only *copies* files the generation carries —
/// so without this list the root would keep a stale copy, and the engine
/// (which reads sidecars from the root) would keep acting on state the commit
/// had already dropped. Observed end to end: deleting a synthesised paragraph
/// emptied `structure.json`, the generation lost it, the root kept it, and the
/// builder kept re-emitting the deleted paragraph.
///
/// Deliberately an explicit list rather than "anything the generation lacks":
/// the root also holds files the store does not own (`workdir.json` is
/// root-only by design), and a blanket sweep would delete them.
pub const OPTIONAL_ROOT_SIDECARS: [&str; 2] = ["islands.json", "structure.json"];

/// Lock outcomes are stable diagnostic codes (public contract).
pub const WRITER_BUSY: &str = "writer-busy";
pub const WRITER_TIMEOUT: &str = "writer-timeout";
pub const GENERATION_CONFLICT: &str = "generation-conflict";
pub const NEEDS_RECOVERY: &str = "needs-recovery";
pub const RESERVE_DEPLETED: &str = "reserve-depleted";
pub const UNSUPPORTED_BY_DESIGN: &str = "unsupported-by-design";
pub const STORE_INVALID: &str = "store-invalid";
pub const OPERATION_JOURNAL_CONFLICT: &str = "operation-journal-conflict";
pub const OPERATION_ID_REUSED: &str = "operation-id-reused";

/// Recovery acquires the Writer lane with a bounded wait; infinite waits are
/// prohibited by the contract.
const RECOVERY_LOCK_TIMEOUT_MS: u64 = 30_000;

// ---------------------------------------------------------------------------
// Fault injection (deterministic, test-only). A fault fires at one named cut
// point: `Kill` simulates process death (a panic that escapes every error
// handler, leaving the journal exactly as the process died); `Enospc`/
// `Error` fail the cut with an I/O error; `Truncate(n)` truncates the staged
// temp path (short write), then lets the flow continue.
// ---------------------------------------------------------------------------

/// Simulated process death. `panic_any(KillPanic)` unwinds through every
/// error handler — only the test harness raises it; real kills leave the
/// same on-disk state.
#[derive(Debug)]
pub struct KillPanic;

#[derive(Clone, Debug)]
pub enum Fault {
    Kill,
    Enospc,
    Error(String),
    Truncate(usize),
}

static FAULTS: LazyLock<Mutex<BTreeMap<String, Fault>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

fn faults() -> &'static Mutex<BTreeMap<String, Fault>> {
    &FAULTS
}

/// Arm one named cut point (`None` disarms).
pub fn set_fault(name: &str, fault: Option<Fault>) {
    let mut map = faults().lock().expect("fault map poisoned");
    match fault {
        Some(fault) => {
            map.insert(name.to_string(), fault);
        }
        None => {
            map.remove(name);
        }
    }
}

pub fn clear_faults() {
    faults().lock().expect("fault map poisoned").clear();
}

/// Arm `name` to simulate process death (`KillPanic` escapes error handlers).
pub fn kill_at(name: &str) {
    set_fault(name, Some(Fault::Kill));
}

/// Fire one named cut point; `staged` is the temp path a short-write fault
/// may truncate. A `Kill` fault optionally parks the process (marker file +
/// sleep) so a gate can kill it for real, then panics.
fn fire(name: Option<&str>, staged: Option<&Path>) -> Result<(), StoreError> {
    let Some(name) = name else {
        return Ok(());
    };
    let fault = faults()
        .lock()
        .expect("fault map poisoned")
        .get(name)
        .cloned();
    match fault {
        None => Ok(()),
        Some(Fault::Kill) => {
            if let Ok(marker) = std::env::var("DOCX2TYPED_FAULT_MARKER") {
                let _ = fs::write(Path::new(&marker), "ready");
            }
            if let Ok(ms) = std::env::var("DOCX2TYPED_FAULT_SLEEP_MS") {
                if let Ok(ms) = ms.parse::<u64>() {
                    std::thread::sleep(Duration::from_millis(ms));
                }
            }
            std::panic::panic_any(KillPanic);
        }
        Some(Fault::Enospc) => Err(StoreError::Io(io::Error::from_raw_os_error(28))),
        Some(Fault::Error(message)) => Err(StoreError::Io(io::Error::other(message))),
        Some(Fault::Truncate(len)) => {
            if let Some(staged) = staged {
                let file = fs::OpenOptions::new().write(true).open(staged)?;
                file.set_len(len as u64)?;
            }
            Ok(())
        }
    }
}

/// Arm faults from `DOCX2TYPED_FAULT` (`kill:<cut>`, `enospc:<cut>`,
/// `error:<cut>`, `truncate:<n>:<cut>`) — the binary's real-process-kill and
/// qualification seams.
pub fn arm_faults_from_env(spec: &str) {
    let parts: Vec<&str> = spec.splitn(3, ':').collect();
    match (
        parts.first().copied(),
        parts.get(1).copied(),
        parts.get(2).copied(),
    ) {
        (Some("kill"), Some(cut), _) => kill_at(cut),
        (Some("enospc"), Some(cut), _) => set_fault(cut, Some(Fault::Enospc)),
        (Some("error"), Some(cut), _) => {
            set_fault(cut, Some(Fault::Error("injected io error".into())))
        }
        (Some("truncate"), Some(len), Some(cut)) => {
            if let Ok(len) = len.parse::<usize>() {
                set_fault(cut, Some(Fault::Truncate(len)));
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Durability helpers (temp write -> flush -> fsync -> rename -> dir barrier)
// ---------------------------------------------------------------------------

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_path(parent: &Path, name: &str) -> PathBuf {
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(".{name}.{}-{n}.tmp", std::process::id()))
}

/// UTC ISO-8601 with seconds precision, mirroring Python
/// `datetime.now(timezone.utc).isoformat(timespec="seconds")`.
fn now_iso() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (year, month, day) = civil_from_days((seconds / 86_400) as i64);
    let hms = seconds % 86_400;
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}+00:00",
        year,
        month,
        day,
        hms / 3600,
        (hms % 3600) / 60,
        hms % 60
    )
}

/// Days since 1970-01-01 to civil date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn fsync_file(path: &Path) -> io::Result<()> {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .or_else(|_| fs::OpenOptions::new().read(true).open(path))?;
    file.sync_all()
}

/// Directory durability barrier; False on platforms that document an
/// equivalent (Windows: NTFS directory metadata is covered by the file fsync
/// + atomic replace contract), mirroring Python `_fsync_dir`.
fn fsync_dir(path: &Path) -> bool {
    fs::File::open(path)
        .and_then(|file| file.sync_all())
        .is_ok()
}

/// Atomic durable publish: temp write -> flush -> fsync -> rename -> parent
/// directory barrier. Fault points carry the staged temp path so a
/// short-write fault can truncate it before the rename lands; the size check
/// then fails the write instead of letting a torn record break the hash
/// chain.
fn write_durable(
    path: &Path,
    data: &[u8],
    write_fault: Option<&str>,
    flush_fault: Option<&str>,
    rename_fault: Option<&str>,
) -> Result<(), StoreError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let temp = temp_path(parent, &name);
    let result = (|| -> Result<(), StoreError> {
        fire(write_fault, Some(&temp))?;
        {
            let mut handle = fs::File::create(&temp)?;
            handle.write_all(data)?;
            handle.flush()?;
            handle.sync_all()?;
        }
        fire(flush_fault, Some(&temp))?;
        let size = fs::metadata(&temp).map(|meta| meta.len()).unwrap_or(0);
        if size != data.len() as u64 {
            // Short write (fault-injected or real): the staged record would
            // land torn and silently break the hash chain. Fail the write so
            // the mutation rolls back to the complete old state instead of
            // returning success over a corrupt journal.
            return Err(StoreError::Io(io::Error::other(format!(
                "short write detected publishing {name}"
            ))));
        }
        fire(rename_fault, Some(&temp))?;
        fs::rename(&temp, path)?;
        fsync_dir(parent);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// fsync every file under `root` and every directory, so a pointer swap can
/// never outrun its generation's bytes.
fn fsync_tree(root: &Path) -> Result<(), StoreError> {
    for file in walk_files(root) {
        fsync_file(&file)?;
    }
    let mut dirs = vec![root.to_path_buf()];
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path.clone());
                stack.push(path);
            }
        }
    }
    dirs.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for dir in dirs {
        fsync_dir(&dir);
    }
    Ok(())
}

/// Copy one generation snapshot (files + structure, byte-exact).
fn copy_tree(source: &Path, target: &Path) -> Result<(), StoreError> {
    fs::create_dir_all(target)?;
    let mut stack = vec![source.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let rel = path
                .strip_prefix(source)
                .map_err(|_| StoreError::Io(io::Error::other("generation copy escapes source")))?;
            let dst = target.join(rel);
            if path.is_dir() {
                fs::create_dir_all(&dst)?;
                stack.push(path);
            } else if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)?;
                fs::copy(&path, &dst)?;
            }
        }
    }
    Ok(())
}

/// Every file under `root`, deterministic order.
fn walk_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

// ---------------------------------------------------------------------------
// Advisory Writer lane (fixed inode, OS lock; process death releases it)
// ---------------------------------------------------------------------------

/// Writer lane guard: the OS advisory lock lives on the open file
/// description; dropping the guard (or process death) releases it. Nothing
/// deletes, steals, or reclaims the lock by PID/age.
pub struct WriterGuard {
    file: fs::File,
}

impl Drop for WriterGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn acquire_lane(path: &Path, timeout_ms: u64) -> Result<WriterGuard, StoreError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    if file.metadata()?.len() == 0 {
        // Seed the fixed inode once; the lock itself is advisory-only.
        let mut seed = file.try_clone()?;
        seed.write_all(b"0")?;
        seed.sync_all()?;
    }
    let deadline = if timeout_ms == 0 {
        None
    } else {
        Some(Instant::now() + Duration::from_millis(timeout_ms))
    };
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(WriterGuard { file }),
            Err(_) => match deadline {
                None => {
                    return Err(StoreError::store(
                        WRITER_BUSY,
                        format!("another writer holds the workdir lane: {}", path.display()),
                    ))
                }
                Some(deadline) if Instant::now() >= deadline => {
                    return Err(StoreError::store(
                        WRITER_TIMEOUT,
                        format!(
                            "writer lane did not become free within {timeout_ms}ms: {}",
                            path.display()
                        ),
                    ))
                }
                Some(_) => std::thread::sleep(Duration::from_millis(20)),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Filesystem qualification probe
// ---------------------------------------------------------------------------

/// Identity of the host filesystem volume holding `store_dir`. The probe
/// cache is bound to this identity so a workdir moved onto a different
/// volume is re-probed instead of trusting a foreign cache. On Windows the
/// drive/UNC prefix of the resolved path is the stable proxy (Python's
/// `st_dev` is the drive number); on POSIX the device id.
fn volume_identity(store_dir: &Path) -> Option<String> {
    let absolute = std::path::absolute(store_dir).ok()?;
    let component = absolute.components().next()?;
    Some(component.as_os_str().to_string_lossy().into_owned())
}

fn probe_filesystem(store_dir: &Path) -> Result<Value, StoreError> {
    let mut checks: BTreeMap<String, Value> = BTreeMap::new();
    // Unique per-probe file names so concurrent cold opens (e.g. parallel
    // store_mutation calls) never corrupt each other's probe artifacts.
    let salt = &new_operation_id()[..8];
    let mut run =
        |name: &str, check: &dyn Fn() -> Result<bool, Box<dyn std::error::Error>>| match check() {
            Ok(value) => {
                checks.insert(name.to_string(), Value::Bool(value));
            }
            Err(error) => {
                checks.insert(name.to_string(), Value::String(error.to_string()));
            }
        };
    run("atomic_replace", &|| {
        let a = store_dir.join(format!(".probe-a-{salt}"));
        let b = store_dir.join(format!(".probe-b-{salt}"));
        fs::write(&a, b"a")?;
        fs::write(&b, b"b")?;
        fs::rename(&a, &b)?;
        fs::rename(&b, &a)?;
        let _ = fs::remove_file(&a);
        let _ = fs::remove_file(&b);
        Ok(true)
    });
    run("file_durability", &|| {
        let probe = store_dir.join(format!(".probe-fsync-{salt}"));
        let mut handle = fs::File::create(&probe)?;
        handle.write_all(b"x")?;
        handle.flush()?;
        handle.sync_all()?;
        let _ = fs::remove_file(&probe);
        Ok(true)
    });
    // False = documented platform equivalent (Windows: NTFS directory
    // metadata is covered by file fsync + atomic replace).
    run("dir_durability", &|| Ok(fsync_dir(store_dir)));
    run("advisory_lock", &|| {
        acquire_lane(&store_dir.join(format!(".probe-lock-{salt}")), 5_000)
            .map(|_| true)
            .map_err(|error| Box::<dyn std::error::Error>::from(error.to_string()))
    });
    run("stable_identity", &|| {
        let probe = store_dir.join(format!(".probe-id-{salt}"));
        fs::write(&probe, b"id")?;
        let first = fs::metadata(&probe)?;
        let second = fs::metadata(&probe)?;
        let _ = fs::remove_file(&probe);
        Ok(stable_identity(&first, &second))
    });
    let qualified = matches!(checks.get("atomic_replace"), Some(Value::Bool(true)))
        && matches!(checks.get("file_durability"), Some(Value::Bool(true)))
        && matches!(checks.get("advisory_lock"), Some(Value::Bool(true)))
        && matches!(checks.get("stable_identity"), Some(Value::Bool(true)));
    Ok(json!({
        "schema": PROBE_SCHEMA,
        "qualified": qualified,
        "checked_at": now_iso(),
        "os": std::env::consts::OS,
        "python": "n/a (rust)",
        "volume_identity": volume_identity(store_dir),
        "checks": checks,
    }))
}

fn stable_identity(first: &fs::Metadata, second: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Stable metadata identity proxy (Rust's `file_index` is unstable):
        // two stats of the same untouched file give identical tuples, which
        // is exactly what the probe asserts.
        (
            first.file_attributes(),
            first.file_size(),
            first.creation_time(),
            first.last_write_time(),
        ) == (
            second.file_attributes(),
            second.file_size(),
            second.creation_time(),
            second.last_write_time(),
        )
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::MetadataExt;
        (first.dev(), first.ino()) == (second.dev(), second.ino())
    }
}

fn probe_or_reuse(store_dir: &Path) -> Result<Value, StoreError> {
    // Public qualification harness seam: DOCX2TYPED_FORCE_UNQUALIFIED=1 makes
    // every store fail closed as unsupported-by-design, so the capability
    // matrix can exercise the guard through the real CLI.
    if std::env::var("DOCX2TYPED_FORCE_UNQUALIFIED").as_deref() == Ok("1") {
        return Err(StoreError::store(
            UNSUPPORTED_BY_DESIGN,
            "workdir filesystem qualification forced unqualified (DOCX2TYPED_FORCE_UNQUALIFIED=1)",
        ));
    }
    let probe_path = store_dir.join("probe.json");
    let cached = read_json_file(&probe_path).ok();
    let cache_valid = cached.as_ref().is_some_and(|value| {
        value.get("schema").and_then(Value::as_str) == Some(PROBE_SCHEMA)
            && value.get("qualified").and_then(Value::as_bool) == Some(true)
            && value.get("os").and_then(Value::as_str) == Some(std::env::consts::OS)
            // The cache is bound to the host volume: a workdir moved onto a
            // different filesystem must re-probe, never reuse a foreign result.
            && value.get("volume_identity").and_then(Value::as_str)
                == volume_identity(store_dir).as_deref()
    });
    if cache_valid {
        return Ok(cached.expect("validated above"));
    }
    let probe = probe_filesystem(store_dir)?;
    if probe.get("qualified").and_then(Value::as_bool) != Some(true) {
        return Err(StoreError::store(
            UNSUPPORTED_BY_DESIGN,
            format!(
                "workdir filesystem is not qualified for atomic durability: {}",
                canonical_json(&probe)
            ),
        ));
    }
    let mut bytes = canonical_json(&probe).into_bytes();
    bytes.push(b'\n');
    write_durable(&probe_path, &bytes, None, None, None)?;
    Ok(probe)
}

// ---------------------------------------------------------------------------
// Pointer, generation manifest, journal records
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Pointer {
    pub generation: String,
    pub operation_id: Option<String>,
    pub manifest_sha256: Option<String>,
}

/// The version HEAD's fields, spelled exactly as the Python reference spells
/// them (`store.VERSION_POINTER_FIELDS`). Kept in one place because the
/// readers in `versions.rs` address them by name.
const VERSION_POINTER_FIELDS: [&str; 7] = [
    "head_version",
    "head_version_generation",
    "head_tree",
    "head_tree_object",
    "head_commit",
    "baseline_epoch",
    "version_seq",
];

/// Pointer payload = HEAD: the current generation plus the version HEAD names.
///
/// A plain mutation **carries the version fields forward** — canonical may
/// drift ahead of HEAD, and that drift is what `version_dirty` names (ADR
/// 0044) — while only a save boundary replaces them. Writing them off here is
/// not "no opinion": it erases a timeline, and leaves the objects it named in
/// the pool with nothing referencing them.
fn pointer_payload(
    root: &Path,
    generation: &str,
    operation_id: Option<&str>,
    manifest_sha256: &str,
) -> Value {
    let mut payload = json!({
        "schema": POINTER_SCHEMA,
        "generation": generation,
        "operation_id": operation_id,
        "manifest_sha256": manifest_sha256,
        "written_at": now_iso(),
    });
    let previous = read_json_file(&root.join(POINTER_FILE)).ok();
    if let Some(previous) = previous {
        if previous.get("schema").and_then(Value::as_str) == Some(POINTER_SCHEMA) {
            if let Some(object) = payload.as_object_mut() {
                for field in VERSION_POINTER_FIELDS {
                    if let Some(carried) = previous.get(field) {
                        if !carried.is_null() {
                            object.insert(field.to_string(), carried.clone());
                        }
                    }
                }
            }
        }
    }
    payload
}

fn read_pointer(root: &Path) -> Option<Pointer> {
    let data = read_json_file(&root.join(POINTER_FILE)).ok()?;
    if data.get("schema").and_then(Value::as_str) != Some(POINTER_SCHEMA) {
        return None;
    }
    let generation = data.get("generation")?.as_str()?.to_string();
    Some(Pointer {
        generation,
        operation_id: data
            .get("operation_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        manifest_sha256: data
            .get("manifest_sha256")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn read_json_file(path: &Path) -> io::Result<Value> {
    let text = fs::read_to_string(path)?;
    serde_json::from_str(&text).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn generation_manifest(
    gen_dir: &Path,
    generation: &str,
    parent: Option<&str>,
    operation_id: &str,
    input_sha256: &str,
) -> Result<Value, StoreError> {
    let mut assets = Vec::new();
    for path in walk_files(gen_dir) {
        let rel = path
            .strip_prefix(gen_dir)
            .map_err(|_| StoreError::Io(io::Error::other("outside generation dir")))?
            .to_string_lossy()
            .replace('\\', "/");
        if rel == "generation.json" {
            continue;
        }
        assets.push(json!({
            "path": rel,
            "bytes": fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0),
            "sha256": file_sha256(&path)?,
        }));
    }
    let assets_value = Value::Array(assets);
    let assets_sha256 = semantic_sha256(&assets_value);
    Ok(json!({
        "schema": GENERATION_MANIFEST_SCHEMA,
        "generation": generation,
        "parent": parent,
        "operation_id": operation_id,
        "input_sha256": input_sha256,
        "assets": assets_value,
        "assets_sha256": assets_sha256,
        "created_at": now_iso(),
    }))
}

fn write_generation_manifest(gen_dir: &Path, manifest: &Value) -> Result<(), StoreError> {
    let mut bytes = canonical_json(manifest).into_bytes();
    bytes.push(b'\n');
    write_durable(&gen_dir.join("generation.json"), &bytes, None, None, None)
}

/// One hash-chained journal record: `record_sha256` covers exactly the body
/// (schema + phase + prev_hash + payload), mirroring Python.
fn journal_record(phase: &str, payload: Value, prev_hash: &str) -> Value {
    let mut body = serde_json::Map::new();
    body.insert(
        "schema".to_string(),
        Value::String(JOURNAL_SCHEMA.to_string()),
    );
    body.insert("phase".to_string(), Value::String(phase.to_string()));
    body.insert(
        "prev_hash".to_string(),
        Value::String(prev_hash.to_string()),
    );
    for (key, value) in payload.as_object().expect("payload is an object") {
        body.insert(key.clone(), value.clone());
    }
    let body = Value::Object(body);
    let record_hash = semantic_sha256(&body);
    let mut record = body.as_object().expect("object").clone();
    record.insert("record_sha256".to_string(), Value::String(record_hash));
    Value::Object(record)
}

fn journal_path(tx_dir: &Path, phase: &str) -> PathBuf {
    tx_dir.join(format!("{phase}.json"))
}

fn write_journal_record(tx_dir: &Path, record: &Value) -> Result<(), StoreError> {
    let phase = record
        .get("phase")
        .and_then(Value::as_str)
        .unwrap_or("record");
    let mut bytes = canonical_json(record).into_bytes();
    bytes.push(b'\n');
    write_durable(
        &journal_path(tx_dir, phase),
        &bytes,
        Some(&format!("journal-write-{phase}")),
        Some(&format!("journal-flush-{phase}")),
        Some(&format!("journal-rename-{phase}")),
    )
}

/// Read the present phase records in canonical order. Raises `store-invalid`
/// when the chain is broken (missing predecessor, bad hash, bad link).
fn read_phases(tx_dir: &Path) -> Result<Vec<Value>, StoreError> {
    let mut records = Vec::new();
    let mut expected_prev: Option<String> = None;
    for phase in PHASE_ORDER {
        let path = journal_path(tx_dir, phase);
        if !path.exists() {
            continue;
        }
        let record = read_json_file(&path).map_err(|error| {
            StoreError::store(
                STORE_INVALID,
                format!(
                    "transaction journal record is corrupt: {}: {error}",
                    path.display()
                ),
            )
        })?;
        let valid = record.get("schema").and_then(Value::as_str) == Some(JOURNAL_SCHEMA)
            && record.get("phase").and_then(Value::as_str) == Some(phase)
            && record.get("record_sha256").is_some();
        if !valid {
            return Err(StoreError::store(
                STORE_INVALID,
                format!(
                    "transaction journal record is malformed: {}",
                    path.display()
                ),
            ));
        }
        let body = {
            let mut body = record.as_object().expect("object").clone();
            body.remove("record_sha256");
            Value::Object(body)
        };
        if record.get("record_sha256").and_then(Value::as_str)
            != Some(semantic_sha256(&body).as_str())
        {
            return Err(StoreError::store(
                STORE_INVALID,
                format!(
                    "transaction journal record hash mismatch: {}",
                    path.display()
                ),
            ));
        }
        if let Some(expected) = &expected_prev {
            if record.get("prev_hash").and_then(Value::as_str) != Some(expected.as_str()) {
                return Err(StoreError::store(
                    STORE_INVALID,
                    format!(
                        "transaction journal chain broken at {phase}: expected prev {expected}, record has {:?}",
                        record.get("prev_hash").and_then(Value::as_str)
                    ),
                ));
            }
        }
        expected_prev = record
            .get("record_sha256")
            .and_then(Value::as_str)
            .map(str::to_string);
        records.push(record);
    }
    if !records.is_empty()
        && journal_path(tx_dir, "intent").exists()
        && records[0].get("phase").and_then(Value::as_str) != Some("intent")
    {
        return Err(StoreError::store(
            STORE_INVALID,
            "transaction journal starts without intent",
        ));
    }
    Ok(records)
}

/// Like `read_phases` but None for a broken chain (inspection must not throw
/// on corrupt journals).
fn read_phases_soft(tx_dir: &Path) -> Option<Vec<Value>> {
    read_phases(tx_dir).ok()
}

fn phase_files(tx_dir: &Path) -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(entries) = fs::read_dir(tx_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".json") {
                names.push(name);
            }
        }
    }
    names.sort();
    names
}

// ---------------------------------------------------------------------------
// Operation ledger (`docx2typed-operation-ledger-1`), mirroring
// `scripts/protocol.py` `operation_ledger` persisted shape.
// ---------------------------------------------------------------------------

fn ledger_path(anchor: &Path, directory: bool) -> PathBuf {
    if directory || (anchor.exists() && anchor.is_dir()) {
        anchor.join("operation-ledger.json")
    } else {
        PathBuf::from(format!(
            "{}.operation-ledger.json",
            anchor.to_string_lossy()
        ))
    }
}

/// Full `docx2typed-result-1` envelope shape check (mirror of
/// `_result_envelope_ok`).
fn result_envelope_ok(envelope: &Value) -> bool {
    if envelope.get("schema").and_then(Value::as_str) != Some(RESULT_SCHEMA) {
        return false;
    }
    if !envelope
        .get("operation")
        .and_then(Value::as_str)
        .is_some_and(|operation| !operation.is_empty())
    {
        return false;
    }
    if !matches!(
        envelope.get("outcome").and_then(Value::as_str),
        Some("success" | "failure" | "partial")
    ) {
        return false;
    }
    if !envelope.get("data").is_some_and(Value::is_object) {
        return false;
    }
    let list_of_objects = |value: Option<&Value>| {
        value
            .and_then(Value::as_array)
            .is_some_and(|list| list.iter().all(Value::is_object))
    };
    if !list_of_objects(envelope.get("diagnostics")) {
        return false;
    }
    if !list_of_objects(envelope.get("evidence")) {
        return false;
    }
    envelope.get("engine").is_some_and(Value::is_object)
}

/// Shape validation of one persisted ledger record (mirror of
/// `_ledger_record_ok`): a pre-publish row MUST carry `pending: true` (and
/// may carry a full prepared envelope); a completed row carries a full valid
/// Result. Any other shape is corrupt and is reported (never dropped).
fn ledger_record_ok(record: &Value) -> bool {
    if !record
        .get("input_sha256")
        .and_then(Value::as_str)
        .is_some_and(|input| !input.is_empty())
    {
        return false;
    }
    if !matches!(record.get("pending"), None | Some(Value::Bool(_))) {
        return false;
    }
    match record.get("envelope") {
        None => record.get("pending").and_then(Value::as_bool) == Some(true),
        Some(envelope) => result_envelope_ok(envelope),
    }
}

fn read_ledger_file(path: &Path) -> (BTreeMap<String, Value>, BTreeMap<String, Value>) {
    let mut valid = BTreeMap::new();
    let mut corrupt = BTreeMap::new();
    let Ok(data) = read_json_file(path) else {
        return (valid, corrupt);
    };
    let Some(records) = data.get("records").and_then(Value::as_object) else {
        return (valid, corrupt);
    };
    for (operation_id, record) in records {
        if ledger_record_ok(record) {
            valid.insert(operation_id.clone(), record.clone());
        } else {
            corrupt.insert(operation_id.clone(), record.clone());
        }
    }
    (valid, corrupt)
}

fn write_ledger_file(path: &Path, records: &BTreeMap<String, Value>) -> Result<(), StoreError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "ledger".to_string());
    let temp = temp_path(parent, &name);
    let result = (|| -> Result<(), StoreError> {
        let value = json!({ "schema": LEDGER_SCHEMA, "records": records });
        let mut bytes = canonical_json(&value).into_bytes();
        bytes.push(b'\n');
        let mut handle = fs::File::create(&temp)?;
        handle.write_all(&bytes)?;
        handle.sync_all()?;
        fs::rename(&temp, path)?;
        fsync_dir(parent);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn lookup_persisted(operation_id: &str, anchor: &Path, directory: bool) -> Option<Value> {
    read_ledger_file(&ledger_path(anchor, directory))
        .0
        .get(operation_id)
        .cloned()
}

fn corrupt_persisted(operation_id: &str, anchor: &Path, directory: bool) -> Option<PathBuf> {
    let path = ledger_path(anchor, directory);
    read_ledger_file(&path)
        .1
        .contains_key(operation_id)
        .then_some(path)
}

fn write_ledger_record(
    operation_id: &str,
    canonical: &str,
    envelope: &Value,
    anchor: &Path,
    directory: bool,
) -> Result<(), StoreError> {
    let path = ledger_path(anchor, directory);
    let (mut records, corrupt) = read_ledger_file(&path);
    // Preserve corrupt rows: they fail closed on their own operation_id.
    records.extend(corrupt);
    records.insert(
        operation_id.to_string(),
        json!({ "input_sha256": canonical, "envelope": envelope }),
    );
    write_ledger_file(&path, &records)
}

// ---------------------------------------------------------------------------
// Run-evidence publication (indent-2 canonical JSON beside the artifact)
// ---------------------------------------------------------------------------

fn publish_run_evidence(path: &Path, evidence: &Value) -> Result<(), StoreError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "evidence".to_string());
    let temp = temp_path(parent, &name);
    let result = (|| -> Result<(), StoreError> {
        let mut json = serde_json::to_string_pretty(evidence).expect("evidence serializes");
        json.push('\n');
        fs::write(&temp, json)?;
        fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// One store-backed workdir: pointer, generations, transactions, Writer
/// lane, recovery reserve, filesystem qualification.
#[derive(Clone, Debug)]
pub struct Store {
    pub root: PathBuf,
    pub store_dir: PathBuf,
    pub generations_dir: PathBuf,
    pub transactions_dir: PathBuf,
    pub staging_dir: PathBuf,
    pub recovery_dir: PathBuf,
    pub quarantine_dir: PathBuf,
    pub lock_path: PathBuf,
    pub reserve_path: PathBuf,
    pub reserve_marker: PathBuf,
}

impl Store {
    pub fn new(root: &Path) -> Store {
        let root = resolve_path(root);
        let store_dir = root.join(STORE_DIR_NAME);
        Store {
            root: root.clone(),
            store_dir: store_dir.clone(),
            generations_dir: store_dir.join("generations"),
            transactions_dir: store_dir.join("transactions"),
            staging_dir: store_dir.join("staging"),
            recovery_dir: store_dir.join("recovery"),
            quarantine_dir: store_dir.join("quarantine"),
            lock_path: store_dir.join("lock"),
            reserve_path: store_dir.join("reserve"),
            reserve_marker: store_dir.join("reserve-depleted.json"),
        }
    }

    /// Open an existing store-backed workdir (probe + pointer read). Does
    /// not run recovery; `recover()`/`mutate()` do, under the Writer lane.
    pub fn open(root: &Path) -> Result<Store, StoreError> {
        let store = Store::new(root);
        if !store.store_dir.is_dir() {
            return Err(StoreError::store(
                STORE_INVALID,
                format!("not a store-backed workdir: {}", store.root.display()),
            ));
        }
        probe_or_reuse(&store.store_dir)?;
        if read_pointer(&store.root).is_none() {
            return Err(StoreError::store(
                STORE_INVALID,
                format!(
                    "store pointer is missing or corrupt: {}",
                    store.root.join(POINTER_FILE).display()
                ),
            ));
        }
        Ok(store)
    }

    /// Create a fresh store from the current root files: probe, reserve,
    /// snapshot generation 0, journal the birth transaction, pointer. A
    /// failed birth leaves no trace so a retry re-attempts init.
    pub fn init(root: &Path, operation_id: &str, input_sha256: &str) -> Result<Store, StoreError> {
        let store = Store::new(root);
        if !store.root.is_dir() {
            return Err(StoreError::store(
                STORE_INVALID,
                format!("workdir not found: {}", store.root.display()),
            ));
        }
        if store.store_dir.exists() {
            // Already store-backed: recovery decides; never re-init over it.
            return Ok(store);
        }
        fs::create_dir_all(&store.root)?;
        let result = (|| -> Result<(), StoreError> {
            fs::create_dir_all(&store.store_dir)?;
            probe_or_reuse(&store.store_dir)?;
            store.write_reserve()?;
            let generation = new_operation_id();
            let gen_dir = store.generations_dir.join(&generation);
            fs::create_dir_all(&gen_dir)?;
            copy_root_assets(&store.root, &gen_dir)?;
            let manifest =
                generation_manifest(&gen_dir, &generation, None, operation_id, input_sha256)?;
            write_generation_manifest(&gen_dir, &manifest)?;
            fsync_tree(&gen_dir)?;
            let tx_dir = store.transactions_dir.join(operation_id);
            let intent = journal_record(
                "intent",
                json!({
                    "operation_id": operation_id,
                    "input_sha256": input_sha256,
                    "expected_generation": Value::Null,
                    "kind": "birth",
                }),
                "genesis",
            );
            write_journal_record(&tx_dir, &intent)?;
            let prepared = journal_record(
                "prepared",
                json!({
                    "operation_id": operation_id,
                    "generation": generation,
                    "parent": Value::Null,
                    "input_sha256": input_sha256,
                    "manifest_sha256": manifest.get("assets_sha256"),
                    "kind": "birth",
                }),
                intent
                    .get("record_sha256")
                    .and_then(Value::as_str)
                    .expect("record_sha256"),
            );
            write_journal_record(&tx_dir, &prepared)?;
            let pointer = pointer_payload(
                &store.root,
                &generation,
                Some(operation_id),
                manifest
                    .get("assets_sha256")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            );
            let mut bytes = canonical_json(&pointer).into_bytes();
            bytes.push(b'\n');
            write_durable(
                &store.root.join(POINTER_FILE),
                &bytes,
                Some("pointer-write"),
                Some("pointer-flush"),
                Some("pointer-rename"),
            )?;
            let committed = journal_record(
                "generation-committed",
                json!({
                    "operation_id": operation_id,
                    "generation": generation,
                    "parent": Value::Null,
                }),
                prepared
                    .get("record_sha256")
                    .and_then(Value::as_str)
                    .expect("record_sha256"),
            );
            write_journal_record(&tx_dir, &committed)?;
            let completed = journal_record(
                "completed",
                json!({
                    "operation_id": operation_id,
                    "generation": generation,
                    "parent": Value::Null,
                    "outcome": "success",
                    "kind": "birth",
                }),
                committed
                    .get("record_sha256")
                    .and_then(Value::as_str)
                    .expect("record_sha256"),
            );
            write_journal_record(&tx_dir, &completed)?;
            let _ = fs::remove_dir_all(&tx_dir);
            Ok(())
        })();
        if let Err(error) = result {
            let _ = fs::remove_dir_all(&store.store_dir);
            return Err(error);
        }
        Ok(store)
    }

    /// Open an existing store-backed workdir, or upgrade a pre-store workdir
    /// in place (birth generation 0) and open it.
    pub fn ensure(
        root: &Path,
        operation_id: &str,
        input_sha256: &str,
    ) -> Result<Store, StoreError> {
        if root.join(STORE_DIR_NAME).is_dir() {
            Store::open(root)
        } else {
            Store::init(root, operation_id, input_sha256)
        }
    }

    // -- reserve -----------------------------------------------------------

    fn write_reserve(&self) -> Result<(), StoreError> {
        let data = pseudo_random(RESERVE_BYTES as usize);
        let temp = temp_path(&self.store_dir, "reserve");
        let result = (|| -> Result<(), StoreError> {
            let mut handle = fs::File::create(&temp)?;
            handle.write_all(&data)?;
            handle.flush()?;
            handle.sync_all()?;
            fs::rename(&temp, &self.reserve_path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }

    fn require_reserve(&self) -> Result<(), StoreError> {
        if self.reserve_marker.exists() {
            return Err(StoreError::store(
                RESERVE_DEPLETED,
                "workdir recovery reserve is depleted (ENOSPC); replenish it before further mutations",
            ));
        }
        match fs::metadata(&self.reserve_path) {
            Ok(meta) if meta.len() >= RESERVE_BYTES => Ok(()),
            _ => Err(StoreError::store(
                RESERVE_DEPLETED,
                "workdir recovery reserve is depleted (ENOSPC); replenish it before further mutations",
            )),
        }
    }

    /// ENOSPC emergency: release the reserve only to close the journal and
    /// write minimal failure evidence. The workdir becomes read-only
    /// `reserve-depleted` until replenished.
    fn release_reserve(&self) {
        if let Ok(file) = fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&self.reserve_path)
        {
            let _ = file.sync_all();
        }
        let marker = json!({
            "schema": "docx2typed-reserve-depleted-1",
            "released_at": now_iso(),
        });
        let _ = fs::write(
            &self.reserve_marker,
            serde_json::to_string_pretty(&marker).expect("marker serializes") + "\n",
        );
    }

    /// Restore the 1 MiB recovery reserve; clears `reserve-depleted`.
    pub fn replenish_reserve(&self) -> Result<(), StoreError> {
        let _ = fs::remove_file(&self.reserve_marker);
        self.write_reserve()
    }

    // -- read side ---------------------------------------------------------

    /// Pin the current generation for one read-only operation. The
    /// generation directory is immutable, so the pinned path stays
    /// consistent even while a later writer commits. A missing generation
    /// directory or corrupt generation manifest is `needs-recovery` (never
    /// guessed into repair).
    pub fn pin(&self) -> Result<PinnedGeneration, StoreError> {
        let pointer = read_pointer(&self.root).ok_or_else(|| {
            StoreError::store(
                STORE_INVALID,
                format!(
                    "store pointer is missing or corrupt: {}",
                    self.root.join(POINTER_FILE).display()
                ),
            )
        })?;
        let gen_dir = self.generations_dir.join(&pointer.generation);
        if !gen_dir.is_dir() {
            return Err(StoreError::store(
                NEEDS_RECOVERY,
                format!(
                    "pointer selects generation {} but its directory is missing",
                    pointer.generation
                ),
            ));
        }
        let manifest = read_json_file(&gen_dir.join("generation.json")).map_err(|error| {
            StoreError::store(
                NEEDS_RECOVERY,
                format!(
                    "generation manifest is corrupt: {}: {error}",
                    gen_dir.join("generation.json").display()
                ),
            )
        })?;
        if manifest.get("schema").and_then(Value::as_str) != Some(GENERATION_MANIFEST_SCHEMA)
            || manifest.get("generation").and_then(Value::as_str)
                != Some(pointer.generation.as_str())
        {
            return Err(StoreError::store(
                NEEDS_RECOVERY,
                format!(
                    "generation manifest is malformed: {}",
                    gen_dir.join("generation.json").display()
                ),
            ));
        }
        Ok(PinnedGeneration {
            generation: pointer.generation,
            path: gen_dir,
            manifest_sha256: pointer.manifest_sha256,
        })
    }

    pub fn ledger_dir(&self) -> Result<PathBuf, StoreError> {
        Ok(self.pin()?.path)
    }

    /// Replay lookup for one operation under this store. Returns `(record,
    /// corrupt_path)`: the persisted record (or None) and, when a
    /// structurally corrupt row for the operation exists, the exact ledger
    /// file holding it. Workdir mutations (`generation=True`) search every
    /// generation directory, because a record lives under the generation the
    /// operation committed and the pointer may have advanced past it.
    pub fn lookup_ledger(
        &self,
        operation_id: &str,
        generation: bool,
        anchor: Option<&Path>,
        directory: bool,
    ) -> Result<(Option<Value>, Option<PathBuf>), StoreError> {
        if generation {
            let mut gens = Vec::new();
            if let Ok(entries) = fs::read_dir(&self.generations_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        gens.push(entry.file_name().to_string_lossy().into_owned());
                    }
                }
            }
            gens.sort();
            gens.reverse();
            for gen in gens {
                let gen_dir = self.generations_dir.join(gen);
                if let Some(record) = lookup_persisted(operation_id, &gen_dir, true) {
                    return Ok((Some(record), None));
                }
                if let Some(path) = corrupt_persisted(operation_id, &gen_dir, true) {
                    return Ok((None, Some(path)));
                }
            }
            return Ok((None, None));
        }
        let lookup_anchor = anchor.unwrap_or(&self.root);
        if let Some(record) = lookup_persisted(operation_id, lookup_anchor, directory) {
            return Ok((Some(record), None));
        }
        Ok((
            None,
            corrupt_persisted(operation_id, lookup_anchor, directory),
        ))
    }

    /// The committed generation for one completed mutation, resolved from
    /// the store's own ledger: the generation directory whose ledger holds
    /// the operation's record. Race-free (the record is durable before
    /// `mutate` returns) and replay-correct (an idempotent replay resolves
    /// the original generation). Returns `(generation, manifest_sha256)`
    /// where the manifest is that generation's `assets_sha256`. `None`
    /// when no ledger record exists (e.g. a birth transaction, which never
    /// writes a ledger record).
    pub fn committed_generation(&self, operation_id: &str) -> Option<(String, Option<String>)> {
        let mut gens = Vec::new();
        if let Ok(entries) = fs::read_dir(&self.generations_dir) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    gens.push(entry.file_name().to_string_lossy().into_owned());
                }
            }
        }
        for gen in gens {
            let gen_dir = self.generations_dir.join(&gen);
            let manifest = read_json_file(&gen_dir.join("generation.json")).ok();
            let committed_here = manifest
                .as_ref()
                .and_then(|value| value.get("operation_id"))
                .and_then(Value::as_str)
                == Some(operation_id);
            if committed_here && lookup_persisted(operation_id, &gen_dir, true).is_some() {
                return Some((gen.clone(), self.generation_manifest_sha256(&gen)));
            }
        }
        None
    }

    /// Resolve the manifest hash (`assets_sha256`) of one immutable
    /// generation from its own `generation.json`. `None` when the
    /// generation directory or its manifest is missing. Immutable
    /// generations make this race-free; used by the review frame to enrich
    /// generation-bound history records.
    pub fn generation_manifest_sha256(&self, generation: &str) -> Option<String> {
        let gen_dir = self.generations_dir.join(generation);
        if !gen_dir.is_dir() {
            return None;
        }
        let manifest = read_json_file(&gen_dir.join("generation.json")).ok()?;
        manifest
            .get("assets_sha256")
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    /// Lightweight read-only transaction inspection (no lane): one descriptor
    /// per transaction whose journal has not reached `completed` or whose
    /// chain is broken.
    pub fn pending_transactions(&self) -> Vec<Value> {
        let mut pending = Vec::new();
        if !self.transactions_dir.is_dir() {
            return pending;
        }
        let mut tx_dirs = Vec::new();
        if let Ok(entries) = fs::read_dir(&self.transactions_dir) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    tx_dirs.push(entry.path());
                }
            }
        }
        tx_dirs.sort();
        for tx_dir in tx_dirs {
            let name = tx_dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            match read_phases_soft(&tx_dir) {
                None => pending.push(json!({
                    "operation_id": name,
                    "state": "corrupt-journal",
                    "phases": phase_files(&tx_dir),
                })),
                Some(records) => {
                    let phases: Vec<&str> = records
                        .iter()
                        .filter_map(|record| record.get("phase").and_then(Value::as_str))
                        .collect();
                    if !phases.contains(&"completed") {
                        pending.push(json!({
                            "operation_id": name,
                            "state": "incomplete",
                            "phases": phases,
                        }));
                    }
                }
            }
        }
        pending
    }

    pub fn recovery_warning(&self) -> Vec<String> {
        self.pending_transactions()
            .iter()
            .map(|item| {
                format!(
                    "transaction {} is {}",
                    item.get("operation_id")
                        .and_then(Value::as_str)
                        .unwrap_or("?"),
                    item.get("state").and_then(Value::as_str).unwrap_or("?")
                )
            })
            .collect()
    }

    // -- writer ------------------------------------------------------------

    pub fn writer(&self, timeout_ms: u64) -> Result<WriterGuard, StoreError> {
        acquire_lane(&self.lock_path, timeout_ms)
    }

    // -- journal helpers ---------------------------------------------------

    fn tx_dir(&self, operation_id: &str) -> PathBuf {
        self.transactions_dir.join(operation_id)
    }

    fn begin_journal(
        &self,
        operation_id: &str,
        canonical: &str,
        expected_generation: Option<&str>,
        input_sha256: &str,
        kind: &str,
    ) -> Result<(PathBuf, Value), StoreError> {
        let tx_dir = self.tx_dir(operation_id);
        if tx_dir.exists() {
            let records = read_phases_soft(&tx_dir);
            let message = if records.as_ref().is_some_and(|records| {
                records
                    .iter()
                    .any(|r| r.get("phase").and_then(Value::as_str) == Some("completed"))
            }) {
                format!("transaction {operation_id} already completed in its journal")
            } else {
                format!("transaction {operation_id} already exists; run recovery first")
            };
            return Err(StoreError::store(OPERATION_JOURNAL_CONFLICT, message));
        }
        fs::create_dir_all(&tx_dir)?;
        let pointer = read_pointer(&self.root);
        let prev_hash = match &pointer {
            Some(_) => file_sha256(&self.root.join(POINTER_FILE))?,
            None => "genesis".to_string(),
        };
        let intent = journal_record(
            "intent",
            json!({
                "operation_id": operation_id,
                "input_sha256": canonical,
                "expected_generation": expected_generation,
                "input_manifest_sha256": input_sha256,
                "kind": kind,
            }),
            &prev_hash,
        );
        write_journal_record(&tx_dir, &intent)?;
        Ok((tx_dir, intent))
    }

    fn copy_generation(
        &self,
        parent: Option<&str>,
        generation: &str,
    ) -> Result<PathBuf, StoreError> {
        let gen_dir = self.generations_dir.join(generation);
        match parent {
            Some(parent) => {
                fire(Some("generation-copy"), None)?;
                copy_tree(&self.generations_dir.join(parent), &gen_dir)?;
            }
            None => {
                fs::create_dir_all(&gen_dir)?;
                copy_root_assets(&self.root, &gen_dir)?;
            }
        }
        Ok(gen_dir)
    }

    fn overlay_ingress(&self, gen_dir: &Path) -> Result<(), StoreError> {
        for name in INGRESS_FILES {
            let source = self.root.join(name);
            if source.is_file() {
                fs::copy(&source, gen_dir.join(name))?;
            }
        }
        Ok(())
    }

    /// Mirror the committed generation's files onto the root. Recovery
    /// re-runs this to roll forward, so every file must be replaced
    /// independently and idempotently.
    fn materialize_root(&self, gen_dir: &Path) -> Result<(), StoreError> {
        fire(Some("materialize"), None)?;
        for source in walk_files(gen_dir) {
            let rel = source.strip_prefix(gen_dir).map_err(|_| {
                StoreError::Io(io::Error::other("materialize escapes generation dir"))
            })?;
            if rel.to_string_lossy().replace('\\', "/") == "generation.json" {
                continue;
            }
            let target = self.root.join(rel);
            let name = target
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            fire(Some(&format!("materialize-file-{name}")), None)?;
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let temp = temp_path(target.parent().unwrap_or_else(|| Path::new(".")), &name);
            let result = (|| -> Result<(), StoreError> {
                fs::copy(&source, &temp)?;
                fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&temp)?
                    .sync_all()?;
                fs::rename(&temp, &target)?;
                Ok(())
            })();
            if result.is_err() {
                let _ = fs::remove_file(&temp);
            }
            result?;
        }
        // A sidecar the generation no longer carries must not survive at the
        // root: reads take sidecars from the root, so a stale copy would
        // resurrect state the commit dropped. Idempotent (guarded by
        // `is_file`), so recovery may re-run this freely.
        for name in OPTIONAL_ROOT_SIDECARS {
            if gen_dir.join(name).is_file() {
                continue;
            }
            let target = self.root.join(name);
            if target.is_file() {
                fire(Some(&format!("materialize-remove-{name}")), None)?;
                fs::remove_file(&target)?;
            }
        }
        fsync_dir(&self.root);
        Ok(())
    }

    fn commit_pointer(
        &self,
        generation: &str,
        operation_id: &str,
        manifest_sha256: &str,
    ) -> Result<(), StoreError> {
        let pointer = pointer_payload(&self.root, generation, Some(operation_id), manifest_sha256);
        let mut bytes = canonical_json(&pointer).into_bytes();
        bytes.push(b'\n');
        write_durable(
            &self.root.join(POINTER_FILE),
            &bytes,
            Some("pointer-write"),
            Some("pointer-flush"),
            Some("pointer-rename"),
        )
    }

    fn write_evidence(&self, path: &Path, evidence: &Value) -> Result<(), StoreError> {
        publish_run_evidence(path, evidence)?;
        fsync_file(path)?;
        Ok(())
    }

    // -- recovery ----------------------------------------------------------

    /// Startup recovery under the Writer lane. Deterministic roll forward/
    /// back for uniquely provable transactions; ambiguity marks
    /// `needs-recovery` (journal preserved for inspection). Never guesses by
    /// mtime.
    pub fn recover(&self, auto: bool) -> Result<RecoverySummary, StoreError> {
        let _guard = self.writer(RECOVERY_LOCK_TIMEOUT_MS)?;
        let mut result = RecoverySummary::default();
        self.recover_all(&mut result, auto)?;
        Ok(result)
    }

    fn recover_all(&self, result: &mut RecoverySummary, auto: bool) -> Result<(), StoreError> {
        if !self.transactions_dir.is_dir() {
            return Ok(());
        }
        let mut tx_dirs = Vec::new();
        if let Ok(entries) = fs::read_dir(&self.transactions_dir) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    tx_dirs.push(entry.path());
                }
            }
        }
        tx_dirs.sort();
        for tx_dir in tx_dirs {
            self.recover_tx(&tx_dir, result, auto);
        }
        self.gc_abandoned(result);
        Ok(())
    }

    fn recover_tx(&self, tx_dir: &Path, result: &mut RecoverySummary, _auto: bool) {
        let operation_id = tx_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let records = match read_phases_soft(tx_dir) {
            None => {
                result.needs_recovery.push(json!({
                    "operation_id": operation_id,
                    "reason": "corrupt-journal-chain",
                }));
                return;
            }
            Some(records) => records,
        };
        if records.is_empty() {
            // Empty transaction directory (crash before the intent record
            // landed): trivially rolled back.
            self.roll_back_generation(tx_dir, &operation_id, None, result);
            return;
        }
        let last = records.last().expect("non-empty").clone();
        let prepared = records
            .iter()
            .find(|record| record.get("phase").and_then(Value::as_str) == Some("prepared"))
            .cloned();
        let parent = prepared
            .as_ref()
            .and_then(|record| record.get("parent"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let prepared_gen = prepared
            .as_ref()
            .and_then(|record| record.get("generation"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let current = read_pointer(&self.root).map(|pointer| pointer.generation);

        if last.get("phase").and_then(Value::as_str) == Some("completed") {
            // Result may already be durable; ensure ledger + evidence landed.
            self.settle_completed(tx_dir, &last, result);
            return;
        }
        let last_phase = last.get("phase").and_then(Value::as_str).unwrap_or("");
        if prepared_gen.is_none() {
            // External-only transaction (build output): the pointer never
            // moves; recovery decides on the journal + hashes.
            if matches!(last_phase, "external-published" | "generation-committed") {
                self.roll_forward(tx_dir, &records, &last, result);
                return;
            }
            if last_phase == "prepared" {
                let decision = self.external_decision(&records, result);
                match decision.as_str() {
                    "forward" => self.roll_forward(tx_dir, &records, &last, result),
                    "back" => {
                        self.roll_back_externals(tx_dir, &records, result);
                        self.roll_back_generation(tx_dir, &operation_id, None, result);
                    }
                    _ => {
                        self.ambiguous(&operation_id, result, "external output state is ambiguous");
                    }
                }
                return;
            }
            // intent only: nothing was published.
            self.roll_back_generation(tx_dir, &operation_id, None, result);
            return;
        }
        if current.as_deref() == prepared_gen.as_deref() {
            // Pointer already selects the prepared generation: complete the
            // commit (materialize, evidence, ledger, completed journal).
            self.roll_forward(tx_dir, &records, &last, result);
            return;
        }
        if current.as_deref() == parent.as_deref() {
            // Pointer never moved: the mutation did not commit. Restore prior
            // outputs from verified backups; remove staged generation/outputs.
            self.roll_back_externals(tx_dir, &records, result);
            self.roll_back_generation(tx_dir, &operation_id, prepared_gen.as_deref(), result);
            return;
        }
        self.ambiguous(
            &operation_id,
            result,
            "prepared generation differs from pointer",
        );
    }

    /// Deterministic forward/back/ambiguous decision for a prepared (not yet
    /// external-published) external-only transaction: verify hashes, never
    /// mtimes. A target matching the recorded output hash landed (forward);
    /// a target with a verified backup and no matching hash is rolled back;
    /// anything else is ambiguous (quarantine).
    fn external_decision(&self, records: &[Value], result: &mut RecoverySummary) -> String {
        let prepared = records
            .iter()
            .find(|record| record.get("phase").and_then(Value::as_str) == Some("prepared"))
            .expect("prepared present");
        let operation_id = records[0]
            .get("operation_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let mut decision: Option<&str> = None;
        let mut ambiguous = false;
        if let Some(externals) = prepared.get("externals").and_then(Value::as_array) {
            for external in externals {
                let target =
                    PathBuf::from(external.get("target").and_then(Value::as_str).unwrap_or(""));
                let mode = external
                    .get("mode")
                    .and_then(Value::as_str)
                    .unwrap_or("replace");
                let landed = target.is_file()
                    && file_sha256(&target).ok().as_deref()
                        == external.get("sha256").and_then(Value::as_str);
                let backup_ok = match external.get("backup").and_then(Value::as_str) {
                    Some(backup) => {
                        let backup = Path::new(backup);
                        backup.is_file()
                            && file_sha256(backup).ok().as_deref()
                                == external.get("backup_sha256").and_then(Value::as_str)
                    }
                    None => false,
                };
                if landed {
                    decision = Some("forward");
                } else if target.exists() && !backup_ok && mode == "create" {
                    // create-only output exists but does not match: it cannot
                    // have been produced by this transaction (create never
                    // replaces).
                    self.quarantine(&target, &operation_id, result, "unmatched external output");
                    ambiguous = true;
                } else if decision != Some("forward") {
                    decision = Some("back");
                }
            }
        }
        if ambiguous {
            return "ambiguous".to_string();
        }
        decision.unwrap_or("back").to_string()
    }

    /// The commit landed (pointer selected the prepared generation, or the
    /// external outputs were published): complete it — materialize
    /// (generation transactions), repair evidence + ledger, close the
    /// journal with a `completed` record built from the prepared envelope.
    /// Never re-publish externals, never guess.
    fn roll_forward(
        &self,
        tx_dir: &Path,
        records: &[Value],
        _last: &Value,
        result: &mut RecoverySummary,
    ) {
        let operation_id = records[0]
            .get("operation_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let prepared = records
            .iter()
            .find(|record| record.get("phase").and_then(Value::as_str) == Some("prepared"))
            .expect("prepared present");
        let generation = prepared
            .get("generation")
            .and_then(Value::as_str)
            .map(str::to_string);
        let envelope = prepared.get("envelope").cloned();
        if let Some(generation) = &generation {
            let gen_dir = self.generations_dir.join(generation);
            if !gen_dir.is_dir() {
                self.ambiguous(
                    &operation_id,
                    result,
                    "prepared generation directory missing",
                );
                return;
            }
            if let Err(error) = self.materialize_root(&gen_dir) {
                self.ambiguous(
                    &operation_id,
                    result,
                    &format!("roll-forward materialize failed: {error}"),
                );
                return;
            }
        }
        self.finish_commit(
            tx_dir,
            prepared,
            envelope.as_ref(),
            generation.as_deref(),
            result,
        );
    }

    /// Ensure ledger + evidence are durable for one transaction. Returns
    /// False (and records ambiguity) when a required repair fails.
    fn repair_semantic_result(
        &self,
        tx_dir: &Path,
        prepared: Option<&Value>,
        completed: Option<&Value>,
        result: &mut RecoverySummary,
    ) -> bool {
        let operation_id = completed
            .or(prepared)
            .and_then(|record| record.get("operation_id"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| {
                tx_dir
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default()
            });
        let envelope = completed
            .or(prepared)
            .and_then(|record| record.get("envelope"))
            .cloned();
        let canonical = prepared
            .or(completed)
            .and_then(|record| record.get("input_sha256"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let generation = completed
            .or(prepared)
            .and_then(|record| record.get("generation"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let records = read_phases_soft(tx_dir).unwrap_or_default();
        let prepared_rec = records
            .iter()
            .find(|record| record.get("phase").and_then(Value::as_str) == Some("prepared"))
            .cloned();
        let ledger_anchor: Option<PathBuf> = match prepared_rec
            .as_ref()
            .and_then(|record| record.get("ledger_anchor"))
            .and_then(Value::as_str)
        {
            Some(anchor) => Some(PathBuf::from(anchor)),
            None => generation
                .as_ref()
                .map(|generation| self.generations_dir.join(generation))
                .filter(|dir| dir.is_dir())
                .or_else(|| self.pin_path_or_none()),
        };
        let ledger_directory = prepared_rec
            .as_ref()
            .and_then(|record| record.get("ledger_directory"))
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let evidence_path = prepared_rec
            .as_ref()
            .and_then(|record| record.get("evidence_path"))
            .and_then(Value::as_str)
            .map(PathBuf::from);

        if let Some(anchor) = &ledger_anchor {
            if let Some(envelope_value) = &envelope {
                if envelope_value.get("schema").and_then(Value::as_str) == Some(RESULT_SCHEMA) {
                    let record = lookup_persisted(&operation_id, anchor, ledger_directory);
                    let outcome = envelope_value
                        .get("outcome")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if record.is_none() && matches!(outcome, "success" | "partial") {
                        match self.write_ledger_at(
                            envelope_value,
                            &canonical,
                            anchor,
                            ledger_directory,
                        ) {
                            Err(error) => {
                                self.ambiguous(
                                    &operation_id,
                                    result,
                                    &format!("ledger repair failed: {error}"),
                                );
                                return false;
                            }
                            Ok(()) => {
                                result.recovered.push(json!({
                                    "operation_id": operation_id,
                                    "action": "ledger-repaired",
                                }));
                            }
                        }
                    }
                }
            }
        }
        if let Some(evidence_path) = evidence_path {
            if let Some(envelope_value) = &envelope {
                let stored = envelope_value
                    .get("evidence")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                if stored.len() == 1 {
                    let pretty =
                        serde_json::to_string_pretty(&stored[0]).expect("evidence serializes");
                    let needs_repair = match fs::read_to_string(&evidence_path) {
                        Ok(text) => text.trim() != pretty,
                        Err(_) => true,
                    };
                    if needs_repair {
                        match self.write_evidence(&evidence_path, &stored[0]) {
                            Err(error) => {
                                self.ambiguous(
                                    &operation_id,
                                    result,
                                    &format!("evidence repair failed: {error}"),
                                );
                                return false;
                            }
                            Ok(()) => {
                                result.recovered.push(json!({
                                    "operation_id": operation_id,
                                    "action": "evidence-repaired",
                                }));
                            }
                        }
                    }
                }
            }
        }
        true
    }

    /// Close an interrupted commit: repair ledger/evidence, write the
    /// `completed` journal record chained from the last phase, then remove
    /// the transaction directory.
    fn finish_commit(
        &self,
        tx_dir: &Path,
        prepared: &Value,
        envelope: Option<&Value>,
        generation: Option<&str>,
        result: &mut RecoverySummary,
    ) {
        let operation_id = prepared
            .get("operation_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if !self.repair_semantic_result(tx_dir, Some(prepared), None, result) {
            return;
        }
        let records = read_phases_soft(tx_dir).unwrap_or_default();
        let prev_hash = records
            .last()
            .and_then(|record| record.get("record_sha256"))
            .and_then(Value::as_str)
            .unwrap_or("genesis")
            .to_string();
        let completed = journal_record(
            "completed",
            json!({
                "operation_id": operation_id,
                "generation": generation,
                "parent": prepared.get("parent"),
                "outcome": envelope.and_then(|e| e.get("outcome")).and_then(Value::as_str).unwrap_or("success"),
                "input_sha256": prepared.get("input_sha256").and_then(Value::as_str).unwrap_or(""),
                "envelope": envelope,
            }),
            &prev_hash,
        );
        if let Err(error) = write_journal_record(tx_dir, &completed) {
            self.ambiguous(
                &operation_id,
                result,
                &format!("completed journal close failed: {error}"),
            );
            return;
        }
        let _ = fs::remove_dir_all(tx_dir);
        result.recovered.push(json!({
            "operation_id": operation_id,
            "action": "completed",
            "generation": generation,
        }));
    }

    /// A completed record exists: ensure its semantic result (ledger + run
    /// evidence) is durable, then remove the transaction directory.
    fn settle_completed(&self, tx_dir: &Path, completed: &Value, result: &mut RecoverySummary) {
        let operation_id = completed
            .get("operation_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| {
                tx_dir
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default()
            });
        let generation = completed
            .get("generation")
            .and_then(Value::as_str)
            .map(str::to_string);
        if !self.repair_semantic_result(tx_dir, None, Some(completed), result) {
            return;
        }
        let _ = fs::remove_dir_all(tx_dir);
        result.recovered.push(json!({
            "operation_id": operation_id,
            "action": "completed",
            "generation": generation,
        }));
    }

    fn pin_path_or_none(&self) -> Option<PathBuf> {
        let pointer = read_pointer(&self.root)?;
        let gen_dir = self.generations_dir.join(&pointer.generation);
        gen_dir.is_dir().then_some(gen_dir)
    }

    fn roll_back_externals(&self, _tx_dir: &Path, records: &[Value], result: &mut RecoverySummary) {
        let external = records
            .iter()
            .find(|record| {
                record.get("phase").and_then(Value::as_str) == Some("external-published")
            })
            .cloned();
        let operation_id = records[0]
            .get("operation_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if let Some(external) = external {
            if let Some(externals) = external.get("externals").and_then(Value::as_array) {
                for external in externals {
                    let target =
                        PathBuf::from(external.get("target").and_then(Value::as_str).unwrap_or(""));
                    let mode = external
                        .get("mode")
                        .and_then(Value::as_str)
                        .unwrap_or("replace");
                    let backup = external
                        .get("backup")
                        .and_then(Value::as_str)
                        .map(PathBuf::from);
                    let backup_is_file = backup.as_ref().is_some_and(|path| path.is_file());
                    if backup_is_file {
                        let backup_path = backup.expect("checked above");
                        if file_sha256(&backup_path).ok().as_deref()
                            != external.get("backup_sha256").and_then(Value::as_str)
                        {
                            self.quarantine(&target, &operation_id, result, "backup hash mismatch");
                            continue;
                        }
                        match fs::rename(&backup_path, &target) {
                            Err(error) => {
                                self.quarantine(
                                    &target,
                                    &operation_id,
                                    result,
                                    &format!("backup restore failed: {error}"),
                                );
                                continue;
                            }
                            Ok(()) => {
                                fsync_dir(target.parent().unwrap_or_else(|| Path::new(".")));
                            }
                        }
                    } else if mode == "create" && target.exists() {
                        // Create-only output: it did not exist before the
                        // transaction.
                        match fs::remove_file(&target) {
                            Err(error) => {
                                self.quarantine(
                                    &target,
                                    &operation_id,
                                    result,
                                    &format!("output removal failed: {error}"),
                                );
                                continue;
                            }
                            Ok(()) => {
                                fsync_dir(target.parent().unwrap_or_else(|| Path::new(".")));
                            }
                        }
                    }
                    result.rolled_back.push(json!({
                        "operation_id": operation_id,
                        "target": target.to_string_lossy().into_owned(),
                    }));
                }
            }
        }
        let _ = fs::remove_dir_all(self.staging_dir.join(&operation_id));
    }

    fn roll_back_generation(
        &self,
        tx_dir: &Path,
        operation_id: &str,
        prepared_gen: Option<&str>,
        result: &mut RecoverySummary,
    ) {
        if let Some(prepared_gen) = prepared_gen {
            let _ = fs::remove_dir_all(self.generations_dir.join(prepared_gen));
        }
        let _ = fs::remove_dir_all(self.staging_dir.join(operation_id));
        let records = read_phases_soft(tx_dir).unwrap_or_default();
        let prev_hash = records
            .last()
            .and_then(|record| record.get("record_sha256"))
            .and_then(Value::as_str)
            .unwrap_or("genesis")
            .to_string();
        let completed = journal_record(
            "completed",
            json!({
                "operation_id": operation_id,
                "outcome": "rolled-back",
                "generation": prepared_gen,
            }),
            &prev_hash,
        );
        if write_journal_record(tx_dir, &completed).is_err() {
            // Reserve may be depleted; keep the journal inspectable.
            let _ = write_journal_record(tx_dir, &completed);
        }
        result
            .rolled_back
            .push(json!({ "operation_id": operation_id }));
        let _ = fs::remove_dir_all(tx_dir);
    }

    fn ambiguous(&self, operation_id: &str, result: &mut RecoverySummary, reason: &str) {
        result.needs_recovery.push(json!({
            "operation_id": operation_id,
            "reason": reason,
        }));
    }

    fn quarantine(
        &self,
        target: &Path,
        operation_id: &str,
        result: &mut RecoverySummary,
        reason: &str,
    ) {
        let name = target
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "output".to_string());
        let destination = self.quarantine_dir.join(format!(
            "{operation_id}-{}-{name}",
            &new_operation_id()[..8]
        ));
        if fs::create_dir_all(&self.quarantine_dir).is_err() {
            result.needs_recovery.push(json!({
                "operation_id": operation_id,
                "reason": "quarantine move failed",
            }));
            return;
        }
        match fs::rename(target, &destination) {
            Err(error) => {
                result.needs_recovery.push(json!({
                    "operation_id": operation_id,
                    "reason": format!("quarantine move failed: {error}"),
                }));
            }
            Ok(()) => {
                result.needs_recovery.push(json!({
                    "operation_id": operation_id,
                    "reason": reason,
                    "quarantined": destination.to_string_lossy().into_owned(),
                }));
            }
        }
    }

    /// Delete abandoned temp generations and staging not referenced by the
    /// pointer or any transaction journal. No speculative GC beyond that.
    fn gc_abandoned(&self, result: &mut RecoverySummary) {
        let mut referenced: BTreeSet<String> = BTreeSet::new();
        if let Some(pointer) = read_pointer(&self.root) {
            referenced.insert(pointer.generation);
        }
        if self.transactions_dir.is_dir() {
            if let Ok(entries) = fs::read_dir(&self.transactions_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if !path.is_dir() {
                        continue;
                    }
                    for record in read_phases_soft(&path).unwrap_or_default() {
                        if let Some(generation) = record.get("generation").and_then(Value::as_str) {
                            referenced.insert(generation.to_string());
                        }
                    }
                }
            }
        }
        if self.generations_dir.is_dir() {
            if let Ok(entries) = fs::read_dir(&self.generations_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        let name = entry.file_name().to_string_lossy().into_owned();
                        if !referenced.contains(&name) {
                            let _ = fs::remove_dir_all(&path);
                            result.cleaned.push(format!("generation/{name}"));
                        }
                    }
                }
            }
        }
        if self.staging_dir.is_dir() {
            let tx_names: BTreeSet<String> = match fs::read_dir(&self.transactions_dir) {
                Ok(entries) => entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect(),
                Err(_) => BTreeSet::new(),
            };
            if let Ok(entries) = fs::read_dir(&self.staging_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if !tx_names.contains(&name) {
                        let _ = fs::remove_dir_all(&path);
                        result.cleaned.push(format!("staging/{name}"));
                    }
                }
            }
        }
    }

    // -- mutation ----------------------------------------------------------

    /// Run one mutation under the Writer lane with journaled phases.
    ///
    /// `generation=True` mutates a fresh immutable generation snapshot and
    /// commits the pointer (workdir mutations); `target` is the snapshot.
    /// `generation=False` only publishes external outputs (build): `target`
    /// is the workdir root (reads pin the current generation), the pointer
    /// never moves, and the ledger/evidence live at the caller-provided
    /// external anchors.
    ///
    /// Commit ordering (deterministic recovery): intent -> prepared ->
    /// external-published -> pointer CAS -> ledger -> materialize ->
    /// completed journal. Every cut point yields only old/new/needs-recovery.
    /// Returns the committed Result envelope (journal + ledger durable).
    pub fn mutate(&self, mut request: StoreMutateRequest) -> Result<Value, StoreError> {
        let _guard = self.writer(request.lock_timeout_ms)?;
        self.require_reserve()?;
        let has_pending = self.transactions_dir.is_dir()
            && fs::read_dir(&self.transactions_dir)
                .map(|mut entries| entries.next().is_some())
                .unwrap_or(false);
        if has_pending {
            // Startup recovery: settle/roll-back any journal left by a
            // crashed or completed run, and GC abandoned generations.
            let mut summary = RecoverySummary::default();
            self.recover_all(&mut summary, true)?;
            if !summary.needs_recovery.is_empty() {
                let reasons: Vec<String> = summary
                    .needs_recovery
                    .iter()
                    .map(|item| {
                        format!(
                            "{} ({})",
                            item.get("operation_id")
                                .and_then(Value::as_str)
                                .unwrap_or("?"),
                            item.get("reason")
                                .and_then(Value::as_str)
                                .unwrap_or("ambiguous")
                        )
                    })
                    .collect();
                return Err(StoreError::store(
                    NEEDS_RECOVERY,
                    format!("workdir needs recovery: {}", reasons.join("; ")),
                ));
            }
        }
        let pointer = read_pointer(&self.root).ok_or_else(|| {
            StoreError::store(
                NEEDS_RECOVERY,
                format!(
                    "workdir pointer is missing or corrupt: {}",
                    self.root.join(POINTER_FILE).display()
                ),
            )
        })?;
        let current = pointer.generation.clone();
        if current != request.expected_generation {
            return Err(StoreError::store(
                GENERATION_CONFLICT,
                format!(
                    "expected parent generation {}, current is {}",
                    request.expected_generation, current
                ),
            ));
        }
        if let Some(manifest_sha256) = &pointer.manifest_sha256 {
            if !request.input_sha256.is_empty() && *manifest_sha256 != request.input_sha256 {
                return Err(StoreError::store(
                    GENERATION_CONFLICT,
                    format!(
                        "generation content changed since planning (expected manifest {}, current {})",
                        request.input_sha256, manifest_sha256
                    ),
                ));
            }
        }
        // Operation-ID semantics are part of the store contract: identical
        // op-id + canonical input replays the original envelope without a
        // second effect; changed input is rejected before any journaling.
        let (prior, _corrupt_path) = self.lookup_ledger(
            &request.operation_id,
            request.generation,
            request.ledger_anchor.as_deref(),
            request.ledger_directory,
        )?;
        if let Some(prior) = prior {
            let prior_envelope = prior.get("envelope").cloned();
            if prior.get("input_sha256").and_then(Value::as_str) == Some(request.canonical.as_str())
            {
                if let Some(prior_envelope) = prior_envelope {
                    return Ok(prior_envelope);
                }
            }
            return Err(StoreError::store(
                OPERATION_ID_REUSED,
                format!(
                    "operation_id {:?} was already used with different canonical input",
                    request.operation_id
                ),
            ));
        }
        let generation_id = new_operation_id();
        let (tx_dir, intent) = self.begin_journal(
            &request.operation_id,
            &request.canonical,
            Some(&request.expected_generation),
            &request.input_sha256,
            &request.kind,
        )?;
        let pointer_committed = std::cell::Cell::new(false);
        let result = (|| -> Result<Value, StoreError> {
            let gen_dir: PathBuf;
            let target: PathBuf;
            if request.generation {
                let copied = self.copy_generation(Some(&current), &generation_id)?;
                self.overlay_ingress(&copied)?;
                gen_dir = copied;
                target = gen_dir.clone();
            } else {
                gen_dir = PathBuf::new();
                target = self.root.clone();
            }
            let mut transaction = Transaction {
                staging_dir: self.staging_dir.clone(),
                operation_id: request.operation_id.clone(),
                generation_dir: request.generation.then(|| gen_dir.clone()),
                root_dir: self.root.clone(),
                evidence_path: request.evidence_path.clone(),
                externals: Vec::new(),
            };
            if request.evidence_path.is_some() {
                transaction.set_evidence_path(request.evidence_path.as_deref().expect("checked"));
            }
            let run_outcome = (request.run)(&target, &mut transaction)?;
            let outcome = run_outcome.outcome;
            let evidence = run_evidence(
                &request.operation,
                &outcome,
                &run_outcome.kind,
                &request.operation_id,
                run_outcome.payload,
            );
            let evidence_value = serde_json::to_value(&evidence).expect("evidence serializes");
            let mut data = run_outcome.data.as_object().cloned().unwrap_or_default();
            data.insert(
                "operation_id".to_string(),
                Value::String(request.operation_id.clone()),
            );
            let envelope_value = build_envelope(
                &request.operation,
                &outcome,
                Value::Object(data),
                run_outcome.diagnostics,
                vec![evidence],
            );
            let externals = transaction.externals();
            let manifest_sha = if request.generation {
                let manifest = generation_manifest(
                    &gen_dir,
                    &generation_id,
                    Some(&current),
                    &request.operation_id,
                    &request.canonical,
                )?;
                write_generation_manifest(&gen_dir, &manifest)?;
                fsync_tree(&gen_dir)?;
                manifest
                    .get("assets_sha256")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            } else {
                pointer
                    .manifest_sha256
                    .clone()
                    .unwrap_or_else(|| request.canonical.clone())
            };
            transaction.write_evidence(&evidence_value)?;
            let evidence_target = transaction.evidence_path.clone().unwrap_or_else(|| {
                if request.generation {
                    gen_dir.join("run.evidence.json")
                } else {
                    self.root.join("run.evidence.json")
                }
            });
            let ledger_anchor_path = request.ledger_anchor.clone().unwrap_or_else(|| {
                if request.generation {
                    gen_dir.clone()
                } else {
                    self.root.clone()
                }
            });
            let prepared = journal_record(
                "prepared",
                json!({
                    "operation_id": request.operation_id,
                    "generation": if request.generation { Value::String(generation_id.clone()) } else { Value::Null },
                    "parent": current,
                    "input_sha256": request.canonical,
                    "manifest_sha256": manifest_sha,
                    "evidence_path": evidence_target.to_string_lossy().into_owned(),
                    "evidence_sha256": semantic_sha256(&evidence_value),
                    "envelope": envelope_value,
                    "envelope_sha256": semantic_sha256(&envelope_value),
                    "ledger_anchor": ledger_anchor_path.to_string_lossy().into_owned(),
                    "ledger_directory": request.ledger_directory,
                    "externals": externals,
                }),
                intent
                    .get("record_sha256")
                    .and_then(Value::as_str)
                    .expect("record_sha256"),
            );
            write_journal_record(&tx_dir, &prepared)?;
            self.publish_externals(
                &tx_dir,
                &prepared,
                &mut transaction.externals,
                &request.operation_id,
            )?;
            let committed = journal_record(
                "generation-committed",
                json!({
                    "operation_id": request.operation_id,
                    "generation": if request.generation { Value::String(generation_id.clone()) } else { Value::Null },
                    "parent": current,
                }),
                prepared
                    .get("record_sha256")
                    .and_then(Value::as_str)
                    .expect("record_sha256"),
            );
            if request.generation {
                self.commit_pointer(&generation_id, &request.operation_id, &manifest_sha)?;
            }
            pointer_committed.set(request.generation);
            write_journal_record(&tx_dir, &committed)?;
            self.write_ledger_at(
                &envelope_value,
                &request.canonical,
                &ledger_anchor_path,
                request.ledger_directory,
            )?;
            if request.generation {
                self.materialize_root(&gen_dir)?;
            }
            let completed = journal_record(
                "completed",
                json!({
                    "operation_id": request.operation_id,
                    "generation": if request.generation { Value::String(generation_id.clone()) } else { Value::Null },
                    "parent": current,
                    "outcome": outcome,
                    "input_sha256": request.canonical,
                    "envelope": envelope_value,
                }),
                committed
                    .get("record_sha256")
                    .and_then(Value::as_str)
                    .expect("record_sha256"),
            );
            write_journal_record(&tx_dir, &completed)?;
            let _ = fs::remove_dir_all(&tx_dir);
            let _ = fs::remove_dir_all(self.staging_dir.join(&request.operation_id));
            Ok(envelope_value)
        })();
        match result {
            Ok(envelope) => Ok(envelope),
            Err(error) => {
                self.abort(
                    &tx_dir,
                    &request.operation_id,
                    &generation_id,
                    &error,
                    Some(&current),
                    pointer_committed.get(),
                );
                if let StoreError::Io(io_error) = &error {
                    if io_error.raw_os_error() == Some(28) {
                        return Err(StoreError::store(
                            RESERVE_DEPLETED,
                            "workdir recovery reserve was depleted by ENOSPC; replenish it before further mutations",
                        ));
                    }
                }
                Err(error)
            }
        }
    }

    fn write_ledger_at(
        &self,
        envelope: &Value,
        canonical: &str,
        anchor: &Path,
        directory: bool,
    ) -> Result<(), StoreError> {
        fire(Some("ledger-write"), None)?;
        let operation_id = envelope
            .get("data")
            .and_then(|data| data.get("operation_id"))
            .and_then(Value::as_str)
            .or_else(|| envelope.get("operation").and_then(Value::as_str))
            .unwrap_or("mutation");
        write_ledger_record(operation_id, canonical, envelope, anchor, directory)
    }

    fn publish_externals(
        &self,
        tx_dir: &Path,
        prepared: &Value,
        externals: &mut Vec<Value>,
        operation_id: &str,
    ) -> Result<(), StoreError> {
        if externals.is_empty() {
            return Ok(());
        }
        for external in externals.iter_mut() {
            let target =
                PathBuf::from(external.get("target").and_then(Value::as_str).unwrap_or(""));
            let staged =
                PathBuf::from(external.get("staged").and_then(Value::as_str).unwrap_or(""));
            let target_name = target
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "output".to_string());
            fire(
                Some(&format!("external-publish-{target_name}")),
                Some(&staged),
            )?;
            let mode = external
                .get("mode")
                .and_then(Value::as_str)
                .unwrap_or("replace");
            if mode == "replace" && target.exists() {
                fire(Some("external-backup"), None)?;
                let backup = self
                    .staging_dir
                    .join(operation_id)
                    .join(format!("backup-{target_name}"));
                if let Some(parent) = backup.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(&target, &backup)?;
                fsync_file(&backup)?;
                external["backup"] = Value::String(backup.to_string_lossy().into_owned());
                external["backup_sha256"] = Value::String(file_sha256(&backup)?);
            }
            fs::rename(&staged, &target)?;
            fsync_dir(target.parent().unwrap_or_else(|| Path::new(".")));
        }
        let published = journal_record(
            "external-published",
            json!({
                "operation_id": operation_id,
                "externals": externals,
            }),
            prepared
                .get("record_sha256")
                .and_then(Value::as_str)
                .expect("record_sha256"),
        );
        write_journal_record(tx_dir, &published)
    }

    /// Roll back an uncommitted mutation: remove the prepared generation and
    /// external staging, restore the pointer when it was already swapped,
    /// journal the failure. ENOSPC releases the reserve and closes the
    /// journal with minimal failure evidence.
    fn abort(
        &self,
        tx_dir: &Path,
        operation_id: &str,
        generation: &str,
        error: &StoreError,
        parent: Option<&str>,
        pointer_committed: bool,
    ) {
        let enospc =
            matches!(error, StoreError::Io(io_error) if io_error.raw_os_error() == Some(28));
        if pointer_committed {
            // The pointer moved before the failure: restore the parent
            // generation so the workdir is the complete old state (never a
            // mixed generation). We hold the Writer lane, so this CAS is safe.
            if let Some(parent) = parent {
                let dir = self.generations_dir.join(parent);
                let parent_manifest = if dir.is_dir() {
                    read_json_file(&dir.join("generation.json"))
                        .ok()
                        .and_then(|manifest| {
                            manifest
                                .get("assets_sha256")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        })
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                let _ = self.commit_pointer(parent, "", &parent_manifest);
            }
        }
        let _ = fs::remove_dir_all(self.generations_dir.join(generation));
        let _ = fs::remove_dir_all(self.staging_dir.join(operation_id));
        let records = read_phases_soft(tx_dir).unwrap_or_default();
        let prev_hash = records
            .last()
            .and_then(|record| record.get("record_sha256"))
            .and_then(Value::as_str)
            .unwrap_or("genesis")
            .to_string();
        let error_code = match error {
            StoreError::Store { code, .. } => code.clone(),
            StoreError::Io(io_error) => format!("{:?}", io_error.kind()),
        };
        let completed = journal_record(
            "completed",
            json!({
                "operation_id": operation_id,
                "generation": generation,
                "parent": parent,
                "outcome": "failed",
                "error_code": error_code,
                "error_message": error.message().chars().take(400).collect::<String>(),
                "reserve_released": enospc,
            }),
            &prev_hash,
        );
        match write_journal_record(tx_dir, &completed) {
            Err(_) if enospc => {
                self.release_reserve();
                let _ = write_journal_record(tx_dir, &completed);
            }
            Err(_) => {}
            Ok(()) => {}
        }
        if enospc {
            self.release_reserve();
        }
        let _ = fs::remove_dir_all(tx_dir);
    }
}

/// The pinned immutable generation for one read-only operation.
#[derive(Clone, Debug)]
pub struct PinnedGeneration {
    pub generation: String,
    pub path: PathBuf,
    pub manifest_sha256: Option<String>,
}

/// One mutation's outcome, produced by the caller-supplied `run`.
#[derive(Clone, Debug)]
pub struct RunOutcome {
    pub outcome: String,
    pub data: Value,
    pub kind: String,
    pub payload: Value,
    pub diagnostics: Vec<docx2typed_protocol::Diagnostic>,
}

/// Per-mutation journal context handed to `run(target, tx)`: external output
/// staging and evidence path registration.
#[derive(Debug)]
pub struct Transaction {
    staging_dir: PathBuf,
    operation_id: String,
    generation_dir: Option<PathBuf>,
    root_dir: PathBuf,
    evidence_path: Option<PathBuf>,
    externals: Vec<Value>,
}

impl Transaction {
    /// A prepared staging path for an external output (parent created).
    pub fn staging(&self, name: &str) -> PathBuf {
        let path = self.staging_dir.join(&self.operation_id).join(name);
        let _ = fs::create_dir_all(path.parent().unwrap_or_else(|| Path::new(".")));
        path
    }

    /// Register one external output for journaled publication. `mode` is
    /// `"create"` (target must not exist) or `"replace"` (prior output is
    /// backed up). `staged` must already be flushed.
    pub fn stage_external(
        &mut self,
        target: &Path,
        staged: &Path,
        mode: &str,
    ) -> Result<(), StoreError> {
        let sha256 = file_sha256(staged)?;
        self.externals.push(json!({
            "target": resolve_path(target).to_string_lossy().into_owned(),
            "staged": resolve_path(staged).to_string_lossy().into_owned(),
            "mode": mode,
            "sha256": sha256,
        }));
        Ok(())
    }

    pub fn externals(&self) -> Vec<Value> {
        self.externals.clone()
    }

    fn set_evidence_path(&mut self, path: &Path) {
        self.evidence_path = Some(path.to_path_buf());
    }

    /// Default evidence location for workdir mutations is the fresh
    /// generation directory (authoritative, fsynced before the pointer
    /// moves); callers pin an external sidecar for build outputs.
    pub fn write_evidence(&self, evidence: &Value) -> Result<(), StoreError> {
        let target = match &self.evidence_path {
            Some(path) if path.is_dir() => path.join("run.evidence.json"),
            Some(path) => path.clone(),
            None => match &self.generation_dir {
                Some(dir) => dir.join("run.evidence.json"),
                None => self.root_dir.join("run.evidence.json"),
            },
        };
        publish_run_evidence(&target, evidence)?;
        fsync_file(&target)?;
        Ok(())
    }
}

/// The mutation body handed to the Store: `(generation snapshot, tx) ->
/// outcome`. Mirrors Python's `run(gen_dir, tx)` callable.
pub type MutateRun = Box<dyn FnMut(&Path, &mut Transaction) -> Result<RunOutcome, StoreError>>;

/// One mutation request (mirror of Python `Store.mutate` keyword contract).
pub struct StoreMutateRequest {
    /// Store-backed workdir root the mutation runs against.
    pub workdir: PathBuf,
    pub operation: String,
    pub operation_id: String,
    pub canonical: String,
    pub input_sha256: String,
    pub expected_generation: String,
    /// `true` mutates a fresh immutable generation snapshot and commits the
    /// pointer; `false` only publishes external outputs (build).
    pub generation: bool,
    pub ledger_anchor: Option<PathBuf>,
    pub ledger_directory: bool,
    pub evidence_path: Option<PathBuf>,
    pub kind: String,
    /// 0 = fail immediately with `writer-busy`; a bounded positive value
    /// fails with `writer-timeout`.
    pub lock_timeout_ms: u64,
    pub run: MutateRun,
}

/// One `recover()` run's summary (recovered / rolled_back /
/// needs_recovery / cleaned).
#[derive(Debug, Default, Clone)]
pub struct RecoverySummary {
    pub recovered: Vec<Value>,
    pub rolled_back: Vec<Value>,
    pub needs_recovery: Vec<Value>,
    pub cleaned: Vec<String>,
}

impl RecoverySummary {
    pub fn to_json(&self) -> Value {
        json!({
            "recovered": self.recovered,
            "rolled_back": self.rolled_back,
            "needs_recovery": self.needs_recovery,
            "cleaned": self.cleaned,
        })
    }
}

// ---------------------------------------------------------------------------
// Module-level seams (read pinning + store discovery), mirroring Python
// `has_store` / `read_root` / `pending_recovery` / `state`.
// ---------------------------------------------------------------------------

pub fn has_store(root: &Path) -> bool {
    root.join(STORE_DIR_NAME).is_dir()
}

/// Read root of the current generation for one read-only operation: the
/// pinned immutable generation directory for store-backed workdirs, the
/// workdir itself otherwise (schema-1 compatibility). Never takes the Writer
/// lane and never mutates anything.
pub fn read_root(root: &Path) -> PathBuf {
    let root_path = resolve_path(root);
    if !has_store(&root_path) {
        return root_path;
    }
    match read_pointer(&root_path) {
        Some(pointer) => {
            let gen_dir = root_path
                .join(STORE_DIR_NAME)
                .join("generations")
                .join(&pointer.generation);
            if gen_dir.is_dir() {
                gen_dir
            } else {
                root_path // degenerate; recovery repairs on the next mutation
            }
        }
        None => root_path,
    }
}

/// Read-only recovery warning for read-only operations (they may pin the
/// last committed generation but must report the warning).
pub fn pending_recovery(root: &Path) -> Vec<String> {
    if !has_store(root) {
        return Vec::new();
    }
    Store::new(root).recovery_warning()
}

/// Stable diagnostics descriptor for `inspect`/store inspection: generation,
/// pending recovery warnings, per-transaction phase descriptors, reserve
/// state, and filesystem qualification.
pub fn state(root: &Path) -> Value {
    let root_path = resolve_path(root);
    if !has_store(&root_path) {
        return json!({ "schema": "docx2typed-store-state-1", "backed": false });
    }
    let store = Store::new(&root_path);
    let pointer = read_pointer(&root_path);
    json!({
        "schema": "docx2typed-store-state-1",
        "backed": true,
        "generation": pointer.map(|pointer| pointer.generation),
        "pending_recovery": store.recovery_warning(),
        "pending_transactions": store.pending_transactions(),
        "reserve_depleted": store.reserve_marker.exists(),
        "filesystem_qualified": true,
    })
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Deterministic pseudo-random reserve bytes (content is not security).
fn pseudo_random(len: usize) -> Vec<u8> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let mut state = (std::process::id() as u64)
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407)
        ^ nanos;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// Copy every root workdir file into a generation snapshot. The private
/// store directory and pointer are never part of a generation.
fn copy_root_assets(root: &Path, gen_dir: &Path) -> Result<(), StoreError> {
    for file in walk_files(root) {
        let rel = file
            .strip_prefix(root)
            .map_err(|_| StoreError::Io(io::Error::other("root asset copy escapes root")))?;
        let rel_posix = rel.to_string_lossy().replace('\\', "/");
        if rel_posix.starts_with(&format!("{STORE_DIR_NAME}/")) || rel_posix == POINTER_FILE {
            continue;
        }
        let dst = gen_dir.join(rel);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&file, &dst)?;
    }
    Ok(())
}

/// `docx2typed-result-1` envelope with the live engine descriptor.
fn build_envelope(
    operation: &str,
    outcome: &str,
    data: Value,
    diagnostics: Vec<docx2typed_protocol::Diagnostic>,
    evidence: Vec<RunEvidence>,
) -> Value {
    let build_commit = std::env::var("DOCX2TYPED_BUILD_COMMIT").unwrap_or_default();
    let envelope = ResultEnvelope::new(
        operation,
        outcome,
        data,
        diagnostics,
        evidence,
        &build_commit,
    );
    serde_json::to_value(envelope).expect("envelope serializes")
}
