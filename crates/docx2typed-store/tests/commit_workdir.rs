//! Atomicity and delta semantics of `WorkdirStore::commit_workdir`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use docx2typed_core::{Asset, ChangeSet};
use docx2typed_store::WorkdirStore;

struct ScratchGuard(PathBuf);

impl std::ops::Deref for ScratchGuard {
    type Target = std::path::Path;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<std::path::Path> for ScratchGuard {
    fn as_ref(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for ScratchGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn scratch(tag: &str) -> ScratchGuard {
    let dir = std::env::temp_dir().join(format!("d2t-commit-{}-{}", tag, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    ScratchGuard(dir)
}

/// Recursive snapshot of every file under `root` (relative path -> bytes).
fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    fn walk(root: &Path, cur: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        if let Ok(entries) = fs::read_dir(cur) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(root, &path, out);
                } else if path.is_file() {
                    let rel = path
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/");
                    out.insert(rel, fs::read(&path).unwrap());
                }
            }
        }
    }
    walk(root, root, &mut out);
    out
}

#[test]
fn failed_apply_leaves_previous_workdir_intact() {
    let root = scratch("atomic");
    fs::write(root.join("keep.txt"), b"keep-bytes").unwrap();

    let missing_source = root.join("does-not-exist.bin");
    let change_set = ChangeSet {
        assets: vec![
            Asset::Bytes("added.txt".to_string(), b"new-bytes".to_vec()),
            Asset::CopySource {
                path: "missing.txt".to_string(),
                source: missing_source,
            },
        ],
    };

    let before = snapshot(&root);
    let result = WorkdirStore::new().commit_workdir(&root, &change_set);
    assert!(result.is_err(), "apply should fail on the missing source");
    let after = snapshot(&root);
    assert_eq!(before, after, "previous workdir must be byte-identical");
}

#[test]
fn delta_change_set_preserves_unnamed_assets() {
    let root = scratch("delta");
    fs::write(root.join("keep.txt"), b"keep-bytes").unwrap();

    let change_set = ChangeSet {
        assets: vec![Asset::Bytes("added.txt".to_string(), b"new-bytes".to_vec())],
    };

    WorkdirStore::new()
        .commit_workdir(&root, &change_set)
        .expect("commit succeeds");

    let snap = snapshot(&root);
    assert_eq!(snap.get("keep.txt").unwrap(), b"keep-bytes");
    assert_eq!(snap.get("added.txt").unwrap(), b"new-bytes");
}

#[test]
fn commit_does_not_touch_sibling_workdirs_sharing_a_parent() {
    // Windows CI reported `.../.docx2typed-commit-<pid>-<n>.tmp/format.json`
    // as `workdir-invalid`, a different test each run. The commit lifecycle
    // itself was sound; the fault was a test tail that removed the SHARED
    // temp parent, sweeping sibling workdirs and in-flight `.tmp` staging
    // mid-commit. This pins the real invariant at the `commit_workdir` seam:
    // a commit stages, swaps and cleans up INSIDE its own workdir's parent
    // while a sibling workdir under the same parent keeps every byte.
    let parent = std::env::temp_dir().join(format!(
        "d2t-commit-siblings-{}-{}",
        std::process::id(),
        crate_seq()
    ));
    let _ = fs::remove_dir_all(&parent);
    fs::create_dir_all(&parent).unwrap();
    let left = parent.join("left");
    let right = parent.join("right");
    fs::create_dir_all(&left).unwrap();
    fs::create_dir_all(&right).unwrap();
    fs::write(left.join("keep.txt"), b"left-bytes").unwrap();
    fs::write(right.join("keep.txt"), b"right-bytes").unwrap();
    fs::write(left.join("format.json"), b"{\"left\":true}\n").unwrap();
    fs::write(right.join("format.json"), b"{\"right\":true}\n").unwrap();

    let change_set = ChangeSet {
        assets: vec![Asset::Bytes("added.txt".to_string(), b"new-bytes".to_vec())],
    };
    WorkdirStore::new()
        .commit_workdir(&left, &change_set)
        .expect("commit succeeds");

    assert_eq!(
        fs::read(right.join("keep.txt")).unwrap(),
        b"right-bytes",
        "sibling workdir bytes survive a neighbour commit"
    );
    assert_eq!(
        fs::read(right.join("format.json")).unwrap(),
        b"{\"right\":true}\n",
        "sibling format.json survives a neighbour commit — the Windows CI symptom"
    );
    assert!(
        parent.is_dir(),
        "the shared parent survives the commit lifecycle"
    );
    for entry in fs::read_dir(&parent).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        assert!(
            !name.starts_with(".docx2typed-commit"),
            "staging is removed after commit: {name}"
        );
        assert!(
            !name.starts_with(".docx2typed-commit-old"),
            "backup is removed after commit: {name}"
        );
    }
    assert_eq!(
        fs::read(left.join("added.txt")).unwrap(),
        b"new-bytes",
        "the committed workdir carries the applied change"
    );
    let _ = fs::remove_dir_all(&parent);
}

fn crate_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}
