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