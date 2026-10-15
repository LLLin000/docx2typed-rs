//! Deep module: workdir commit, byte-copy build publish, run-evidence and
//! manifest publication (issue #55 slice). Owns asset roles and hashes and
//! atomic JSON publication, but no Word/edit/review meaning.
//!
//! The no-op publish path is a pure byte copy (`fs::copy`) of the template
//! package — copy-if-unchanged per frozen PRD decisions 13/14/15. It never
//! recompresses.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use docx2typed_core::Asset;
use docx2typed_protocol::{bytes_sha256, file_sha256, resolve_path, semantic_sha256, RunEvidence};

/// Issue #57: the real generation Store (immutable generations, Writer
/// lane, journals, ledger, recovery reserve, startup recovery, fault
/// injection, external two-phase publication). Mirror of `scripts/store.py`.
pub mod store;

/// Issue #63: the read-only version timeline (`history_list`) and the
/// per-paragraph history reads (`history_diff`, `history_blame`,
/// `history_verify`) — HEAD, the commit graph in the object pool, and the
/// legacy generation-manifest fallback (mirror of `scripts/store.py`
/// `head_version`/`history_list`/`history_diff`/`history_blame`/`history_verify`).
pub mod objects;
pub mod versions;

pub use objects::{build_tree, canonical_tree_digest, materialize, write_commit};
pub use versions::{
    history_blame, history_diff, history_gc, history_list, history_restore, history_verify,
};

pub use store::{
    has_store, pending_recovery, read_root, state, MutateRun, PinnedGeneration, RecoverySummary,
    RunOutcome, Store, StoreMutateRequest, Transaction, STORE_DIR_NAME,
};

/// Workdir asset set from the Python Reference (`_WORKDIR_ASSETS`).
pub const WORKDIR_ASSETS: [&str; 6] = [
    "_template.docx",
    "edit.md",
    "edit.state.json",
    "format.json",
    "styles.json",
    "typed.md",
];

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub enum StoreError {
    Io(std::io::Error),
    /// Store-contract failure carrying a frozen diagnostic code
    /// (`writer-busy`, `writer-timeout`, `generation-conflict`,
    /// `needs-recovery`, `reserve-depleted`, `unsupported-by-design`,
    /// `store-invalid`, `operation-id-reused`, `operation-journal-conflict`).
    Store {
        code: String,
        message: String,
    },
}

impl StoreError {
    pub fn store(code: impl Into<String>, message: impl Into<String>) -> Self {
        StoreError::Store {
            code: code.into(),
            message: message.into(),
        }
    }

    /// The frozen diagnostic code, when this is a Store-contract failure.
    pub fn code(&self) -> Option<&str> {
        match self {
            StoreError::Io(_) => None,
            StoreError::Store { code, .. } => Some(code),
        }
    }

    pub fn message(&self) -> String {
        match self {
            StoreError::Io(error) => error.to_string(),
            StoreError::Store { message, .. } => message.clone(),
        }
    }
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Io(error) => formatter.write_str(&error.to_string()),
            StoreError::Store { code, message } => {
                formatter.write_str(code)?;
                formatter.write_str(": ")?;
                formatter.write_str(message)
            }
        }
    }
}

impl std::error::Error for StoreError {}

impl From<std::io::Error> for StoreError {
    fn from(error: std::io::Error) -> Self {
        StoreError::Io(error)
    }
}

/// Production store: real filesystem workdirs and atomic JSON publication.
pub struct WorkdirStore;

impl Default for WorkdirStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Rename a directory, retrying a transient sharing violation.
///
/// On Windows a directory rename fails with "Access is denied" while anything
/// holds a handle inside it — a scanner, an indexer, another process briefly
/// opening a file. That is transient by nature, and it is why the publish below
/// cannot be a single unguarded `fs::rename`: measured on CI, the extract in
/// `review_crate` failed this way on windows-latest, a different test each run,
/// with 47 of 48 passing — and never locally, where nothing scans the directory.
///
/// The retry is bounded and the last error is returned, so a genuine permission
/// problem still fails loudly rather than being papered over.
fn rename_with_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    const ATTEMPTS: u32 = 10;
    let mut last = None;
    for attempt in 0..ATTEMPTS {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(error) => {
                // Only a sharing/permission failure is worth retrying; anything
                // else (a missing source, a cross-device move) will not improve.
                let transient = matches!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::WouldBlock
                );
                if !transient {
                    return Err(error);
                }
                last = Some(error);
                std::thread::sleep(std::time::Duration::from_millis(20 * (attempt as u64 + 1)));
            }
        }
    }
    Err(last.expect("a transient failure was recorded"))
}

impl WorkdirStore {
    pub fn new() -> Self {
        WorkdirStore
    }

    /// Materialize a (possibly partial) `ChangeSet` into `dir`.
    ///
    /// Staging contract (Gap 1): the ChangeSet is treated as a DELTA applied on
    /// top of a seeded copy of the existing workdir. The staging directory is
    /// first populated byte-for-byte from the current workdir (via `copy_workdir`)
    /// and only then has the ChangeSet applied to it; the populated staging dir
    /// is renamed over `dir` in one step. Consequences:
    ///   * a ChangeSet that names one asset leaves every other existing asset
    ///     in place (no silent deletion);
    ///   * a ChangeSet that fails partway through leaves the previous workdir
    ///     untouched, because nothing is published until the whole apply
    ///     succeeds.
    ///
    /// Copy-over-empty is deliberate: the empty-staging alternative would force
    /// every caller to pass a COMPLETE set or risk wiping the workdir.
    pub fn commit_workdir(
        &self,
        dir: &Path,
        change_set: &docx2typed_core::ChangeSet,
    ) -> Result<(), StoreError> {
        let root = resolve_path(dir);
        let parent = root.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let staging = parent.join(format!(
            ".docx2typed-commit-{}-{}.tmp",
            std::process::id(),
            counter
        ));
        let _ = fs::remove_dir_all(&staging);
        fs::create_dir(&staging)?;

        if root.exists() {
            self.copy_workdir(&root, &staging)?;
        }

        let apply_result = (|| {
            for asset in &change_set.assets {
                let target = staging.join(asset_path(asset));
                match asset {
                    Asset::Bytes(_, bytes) => {
                        if let Some(parent) = target.parent() {
                            fs::create_dir_all(parent)?;
                        }
                        fs::write(&target, bytes)?;
                    }
                    Asset::CopySource { source, .. } => {
                        if let Some(parent) = target.parent() {
                            fs::create_dir_all(parent)?;
                        }
                        fs::copy(source, &target)?;
                    }
                    Asset::Remove(_) => {
                        let result = if target.is_dir() {
                            fs::remove_dir_all(&target)
                        } else {
                            fs::remove_file(&target)
                        };
                        match result {
                            Ok(()) => {}
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                            Err(error) => return Err(error.into()),
                        }
                    }
                }
            }
            Ok(())
        })();
        if let Err(error) = apply_result {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
        let identity_path = root.join("workspace.json");
        if identity_path.is_file() {
            let _ = fs::copy(&identity_path, staging.join("workspace.json"));
        }

        if root.exists() {
            let backup = parent.join(format!(
                ".docx2typed-commit-old-{}-{}.tmp",
                std::process::id(),
                counter
            ));
            let _ = fs::remove_dir_all(&backup);
            if let Err(error) = rename_with_retry(&root, &backup) {
                let _ = fs::remove_dir_all(&staging);
                return Err(error.into());
            }
            if let Err(error) = rename_with_retry(&staging, &root) {
                let _ = rename_with_retry(&backup, &root);
                return Err(error.into());
            }
            let _ = fs::remove_dir_all(&backup);
        } else {
            if let Err(error) = rename_with_retry(&staging, &root) {
                let _ = fs::remove_dir_all(&staging);
                return Err(error.into());
            }
        }
        Ok(())
    }

    /// Publish a build: byte-copy `template` to `output` (copy-if-unchanged,
    /// never recompression). Writes to a same-directory temp file first so a
    /// failed build never leaves a partial output, then renames into place.
    pub fn publish_build(&self, template: &Path, output: &Path) -> Result<PathBuf, StoreError> {
        let output = resolve_path(output);
        let parent = output.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp = parent.join(format!(
            ".docx2typed-build-{}-{}.tmp",
            std::process::id(),
            counter
        ));
        fs::copy(template, &temp)?;
        publish_replaced(&temp, &output)
    }

    /// Issue #58: publish an edited build whose bytes were produced by the
    /// island-edit surgery (temp write + atomic replace, same contract as
    /// `publish_build`).
    pub fn publish_bytes(&self, bytes: &[u8], output: &Path) -> Result<PathBuf, StoreError> {
        let output = resolve_path(output);
        let parent = output.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp = parent.join(format!(
            ".docx2typed-build-{}-{}.tmp",
            std::process::id(),
            counter
        ));
        fs::write(&temp, bytes)?;
        publish_replaced(&temp, &output)
    }

    /// `docx2typed-derived-workdir-manifest-1` for the workdir (assets that
    /// exist, with bytes + sha256), mirroring Python's
    /// `derived_workdir_manifest`.
    pub fn derived_workdir_manifest(&self, dir: &Path) -> serde_json::Value {
        let root = resolve_path(dir);
        let mut assets = Vec::new();
        for name in WORKDIR_ASSETS {
            let path = root.join(name);
            if path.is_file() {
                let bytes = fs::read(&path).unwrap_or_default();
                assets.push(serde_json::json!({
                    "path": name,
                    "bytes": bytes.len(),
                    "sha256": bytes_sha256(&bytes),
                }));
            }
        }
        serde_json::json!({
            "schema": "docx2typed-derived-workdir-manifest-1",
            "assets": assets,
        })
    }

    pub fn manifest_sha256(&self, dir: &Path) -> String {
        semantic_sha256(&self.derived_workdir_manifest(dir))
    }

    /// Atomically persist one run-evidence record beside the operation's
    /// artifact (temp write + rename), mirroring `publish_run_evidence`.
    pub fn publish_run_evidence(
        &self,
        path: &Path,
        evidence: &RunEvidence,
    ) -> Result<(), StoreError> {
        let target = resolve_path(path);
        let parent = target.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp = parent.join(format!(
            ".{}.{}-{}.tmp",
            target
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "evidence".to_string()),
            std::process::id(),
            counter
        ));
        let mut json = serde_json::to_string_pretty(evidence).expect("evidence serializes");
        json.push('\n');
        fs::write(&temp, json)?;
        match fs::rename(&temp, &target) {
            Ok(()) => {}
            Err(_) => {
                let _ = fs::remove_file(&target);
                fs::rename(&temp, &target)?;
            }
        }
        Ok(())
    }

    /// Per-asset hashes of the workdir assets (for evidence payloads and
    /// the gate script).
    pub fn asset_hashes(&self, dir: &Path) -> BTreeMap<String, String> {
        let root = resolve_path(dir);
        let mut hashes = BTreeMap::new();
        for name in WORKDIR_ASSETS {
            let path = root.join(name);
            if path.is_file() {
                if let Ok(sha) = file_sha256(&path) {
                    hashes.insert(name.to_string(), sha);
                }
            }
        }
        hashes
    }

    /// Byte-for-byte copy of every present asset into `staging` (file
    /// mtimes preserved, mirroring `shutil.copy2`). The inventory walk is
    /// link-safe, so migration can never read through a symlink/junction.
    pub fn copy_workdir(&self, source: &Path, staging: &Path) -> Result<(), StoreError> {
        let source = resolve_path(source);
        let staging = resolve_path(staging);
        fs::create_dir_all(&staging)?;
        let assets = docx2typed_core::inspect::inventory_assets(&source)
            .map_err(|error| StoreError::Io(std::io::Error::other(error.to_string())))?;
        for asset in assets {
            if asset.presence != "present" {
                continue;
            }
            if asset.path.ends_with('/') {
                copy_dir_preserving_mtimes(&source.join(&asset.path), &staging.join(&asset.path))?;
                continue;
            }
            let src = source.join(&asset.path);
            let dst = staging.join(&asset.path);
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&src, &dst)?;
            preserve_mtime(&src, &dst)?;
        }
        Ok(())
    }

    /// Write the versioned workdir manifest into the staging workdir
    /// (`workdir.manifest.json`, indent-2 JSON + trailing newline).
    pub fn write_workdir_manifest(
        &self,
        staging: &Path,
        manifest: &serde_json::Value,
    ) -> Result<(), StoreError> {
        let mut json = serde_json::to_string_pretty(manifest).expect("manifest serializes");
        json.push('\n');
        fs::write(staging.join("workdir.manifest.json"), json)?;
        Ok(())
    }

    /// Atomically publish the staged workdir onto `target` (directory
    /// rename; the staging dir must be a sibling of `target` so the rename
    /// stays on one volume). A failed publish leaves no target.
    pub fn publish_workdir(&self, staging: &Path, target: &Path) -> Result<PathBuf, StoreError> {
        let target = resolve_path(target);
        fs::rename(staging, &target)?;
        Ok(target)
    }
}

/// Atomic replace of `temp` onto `output` (Windows-safe: rename fails when
/// the destination exists, so replace it first — atomic replace is a #50
/// recovery concern for the store journal, not for build publication).
fn publish_replaced(temp: &Path, output: &Path) -> Result<PathBuf, StoreError> {
    match fs::rename(temp, output) {
        Ok(()) => {}
        Err(_) => {
            let _ = fs::remove_file(output);
            fs::rename(temp, output)?;
        }
    }
    Ok(output.to_path_buf())
}

fn preserve_mtime(src: &Path, dst: &Path) -> Result<(), StoreError> {
    let mtime = fs::metadata(src)?.modified()?;
    let file = fs::OpenOptions::new().write(true).open(dst)?;
    file.set_modified(mtime)?;
    Ok(())
}

fn copy_dir_preserving_mtimes(src: &Path, dst: &Path) -> Result<(), StoreError> {
    fs::create_dir_all(dst)?;
    for file in docx2typed_core::inspect::walk_files(src) {
        let rel = file
            .strip_prefix(src)
            .map_err(|_| StoreError::Io(std::io::Error::other("outside root")))?;
        let target = dst.join(rel);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&file, &target)?;
        preserve_mtime(&file, &target)?;
    }
    Ok(())
}

fn asset_path(asset: &Asset) -> String {
    match asset {
        Asset::Bytes(path, _) => path.clone(),
        Asset::CopySource { path, .. } => path.clone(),
        Asset::Remove(path) => path.clone(),
    }
}
