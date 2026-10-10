//! The object pool's WRITE side (ADR 0043).
//!
//! `versions.rs` reads this pool and documents itself as read-only; this module
//! is the other half. A Version is a tree object plus a commit object, and the
//! pointer's fields name them — see `docs/specs/03_version_writer.md`.
//!
//! ## Whole-blob trees, deliberately
//!
//! The reference splits `typed.md` and `format.json` into content-addressed
//! chunks so an edit rewrites one bucket, and falls back to storing every
//! asset whole when a split would not round-trip (`objectstore._parts` returns
//! `None`). The fallback is a first-class part shape — `materialize` reads it —
//! so this writer emits it directly.
//!
//! Spec check 7 has since run, and it answers the question rather than leaving
//! it open. On a real extracted workdir the two writers produce the **same
//! tree object id**, `6745fdc7f656f2700e095edff5bc3a69515300754100f53680bf0ca2b7b1b6cc`,
//! because the reference's own splitter fails its round-trip guard on the
//! `typed.md` this pipeline writes:
//!
//! ```text
//! split_typed  -> header + 3 chunks, but join_typed(...) != text
//! _parts       -> None  =>  EVERY asset stored whole
//! ```
//!
//! So chunk-level dedup is not merely unnecessary for compatibility, it is
//! **not what the reference does either** for this input. Note the trap: the
//! fallback is all-or-nothing, so `format.json` — which *does* round-trip — is
//! also stored whole.
//!
//! Re-open condition, stated so it is not discovered by surprise: if the
//! pipeline ever emits a `typed.md` that survives `split_typed`'s round-trip,
//! the reference will chunk and this writer will not, and the tree ids will
//! diverge. The digest would still agree, because that is computed over asset
//! bytes rather than over the tree.
//!
//! ## Ordering
//!
//! Callers MUST write objects before the pointer that names them. A pointer
//! naming a missing object is unrecoverable; an unreferenced object is merely
//! garbage, which `sweep` exists to collect.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use docx2typed_protocol::canonical_json;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::store::STORE_DIR_NAME;
use crate::StoreError;

/// Pool directory name, and the kind buckets under it.
pub const OBJECT_DIR: &str = "objects";
pub const TREE_SCHEMA: &str = "docx2typed-tree-1";
pub const COMMIT_SCHEMA: &str = "docx2typed-version-1";

/// The assets a tree carries. Keep in sync with the reference's `TREE_ASSETS`,
/// which is `store.CANONICAL_ASSETS` plus `revisions.json`.
pub const TREE_ASSETS: [&str; 5] = [
    "typed.md",
    "format.json",
    "revisions.json",
    "styles.json",
    "_template.docx",
];

/// What `head_tree` digests: the authoritative inputs of one generation. The
/// reference is explicit that evidence and derived views are excluded, so a
/// `build_docx` run can never invent a version.
pub const CANONICAL_ASSETS: [&str; 4] =
    ["typed.md", "format.json", "styles.json", "_template.docx"];

fn objects_dir(root: &Path) -> PathBuf {
    root.join(STORE_DIR_NAME).join(OBJECT_DIR)
}

pub fn object_path(root: &Path, kind: &str, digest: &str) -> PathBuf {
    let split = 2.min(digest.len());
    let (bucket, name) = digest.split_at(split);
    objects_dir(root).join(kind).join(bucket).join(name)
}

/// `sha256(kind || 0x00 || data)` — kind-scoped, so a blob and a tree with the
/// same bytes are different objects.
pub fn object_id(kind: &str, data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(kind.as_bytes());
    hasher.update([0u8]);
    hasher.update(data);
    hex::encode(hasher.finalize())
}

/// Store one object. Returns `(id, was_new)`; identical content is stored once.
pub fn put(root: &Path, kind: &str, data: &[u8]) -> Result<(String, bool), StoreError> {
    let digest = object_id(kind, data);
    let path = object_path(root, kind, &digest);
    if path.is_file() {
        return Ok((digest, false));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(StoreError::Io)?;
    }
    // Stage then rename, so a torn write cannot leave a short object that
    // hashes to nothing anyone will ever compute.
    let staged = path.with_file_name(format!(
        "{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    fs::write(&staged, data).map_err(StoreError::Io)?;
    fs::rename(&staged, &path).map_err(StoreError::Io)?;
    Ok((digest, true))
}

pub fn read_object(root: &Path, kind: &str, digest: &str) -> Option<Vec<u8>> {
    fs::read(object_path(root, kind, digest)).ok()
}

/// The tree digest of one generation: `semantic_sha256` over the canonical
/// assets' file hashes, with a missing asset hashed as `null`.
///
/// This is what a save boundary compares to decide whether a Version is
/// created, so it must cover exactly `CANONICAL_ASSETS` — see the module note.
pub fn canonical_tree_digest(state_dir: &Path) -> Result<String, StoreError> {
    let mut assets = serde_json::Map::new();
    for name in CANONICAL_ASSETS {
        let path = state_dir.join(name);
        let digest = if path.is_file() {
            Value::String(docx2typed_protocol::bytes_sha256(
                &fs::read(&path).map_err(StoreError::Io)?,
            ))
        } else {
            Value::Null
        };
        assets.insert(name.to_string(), digest);
    }
    Ok(docx2typed_protocol::semantic_sha256(&Value::Object(assets)))
}

/// Store one canonical state as objects; returns `{tree, digest, parts}`,
/// shaped exactly like the reference's `build_tree`.
pub fn build_tree(
    root: &Path,
    state_dir: &Path,
    digest: Option<&str>,
) -> Result<Value, StoreError> {
    let mut parts = BTreeMap::new();
    for name in TREE_ASSETS {
        let path = state_dir.join(name);
        if !path.is_file() {
            continue;
        }
        let bytes = fs::read(&path).map_err(StoreError::Io)?;
        let (blob, _) = put(root, "blob", &bytes)?;
        parts.insert(name.to_string(), json!({ "whole": blob }));
    }
    let mut tree = json!({
        "schema": TREE_SCHEMA,
        "parts": Value::Object(parts.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
    });
    if let Some(digest) = digest {
        tree["digest"] = json!(digest);
    }
    let (tree_id, _) = put(root, "tree", canonical_json(&tree).as_bytes())?;
    Ok(json!({
        "tree": tree_id,
        "digest": digest,
        "parts": tree["parts"].clone(),
    }))
}

pub fn read_tree(root: &Path, tree_id: &str) -> Option<Value> {
    let raw = read_object(root, "tree", tree_id)?;
    serde_json::from_slice(&raw).ok()
}

/// Write a commit object; returns its id. The commit names its parent, which
/// is what makes the chain walkable without a separate index.
pub fn write_commit(root: &Path, commit: &Value) -> Result<String, StoreError> {
    let mut payload = commit.clone();
    if let Some(object) = payload.as_object_mut() {
        object.insert("schema".to_string(), json!(COMMIT_SCHEMA));
    }
    let (digest, _) = put(root, "version", canonical_json(&payload).as_bytes())?;
    Ok(digest)
}

pub fn read_commit(root: &Path, commit_id: &str) -> Option<Value> {
    let raw = read_object(root, "version", commit_id)?;
    serde_json::from_slice(&raw).ok()
}

/// Materialise a tree's assets into `destination`; returns the names written.
/// Mirrors the reference's `materialize`, whole-blob parts included.
pub fn materialize(
    root: &Path,
    tree_id: &str,
    destination: &Path,
) -> Result<Vec<String>, StoreError> {
    let tree = read_tree(root, tree_id).ok_or_else(|| {
        StoreError::store(
            "store-invalid",
            format!(
                "tree object {} is missing",
                &tree_id[..12.min(tree_id.len())]
            ),
        )
    })?;
    fs::create_dir_all(destination).map_err(StoreError::Io)?;
    let mut written = Vec::new();
    let parts = tree
        .get("parts")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for (name, part) in parts {
        let Some(blob) = part.get("whole").and_then(Value::as_str) else {
            // A chunked part is valid pool content but not something this
            // writer produces; refusing beats materialising half an asset.
            return Err(StoreError::store(
                "store-invalid",
                format!("tree part {name} is chunked, which this writer does not read"),
            ));
        };
        let bytes = read_object(root, "blob", blob)
            .ok_or_else(|| StoreError::store("store-invalid", format!("{name} blob is missing")))?;
        fs::write(destination.join(&name), bytes).map_err(StoreError::Io)?;
        written.push(name);
    }
    Ok(written)
}
