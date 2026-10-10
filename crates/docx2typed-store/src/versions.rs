//! Version timeline (`docx2typed-history-1`) — the read-only half of the
//! Python Reference's `store.head_version` / `store.history_list`
//! (ADR 0043/0044): HEAD names the current version and the commit graph in
//! the content-addressed object pool *is* the history, so listing it needs
//! no index file. A workdir saved before the pool existed falls back to the
//! `version` record its generation manifests carry, and the two chains are
//! joined where the newest commit names a legacy parent.
//!
//! Read-only by construction: this module never writes the pointer, a
//! generation, or an object. Rust mutations do not yet mark a save boundary
//! (no version commit is created), so a Rust-only workdir reports an empty
//! timeline — exactly like a Python workdir that has never been saved. The
//! reader stays faithful to the Python layout so a Python-written workdir
//! lists identically.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use docx2typed_protocol::{file_sha256, semantic_sha256};
use serde_json::{json, Value};

use crate::store::{POINTER_FILE, POINTER_SCHEMA, STORE_DIR_NAME};
use crate::StoreError;

pub const HISTORY_SCHEMA: &str = "docx2typed-history-1";

/// The canonical assets `head_version.dirty` compares (`store.CANONICAL_ASSETS`):
/// the tree digest covers canonical content only, so an evidence or derived
/// view changing never invents a version.
pub const CANONICAL_ASSETS: [&str; 4] =
    ["typed.md", "format.json", "styles.json", "_template.docx"];

const OBJECT_DIR: &str = "objects";
const TRIM_LOG: &str = "history-trim.jsonl";

// ---------------------------------------------------------------------------
// Object pool (read-only)
// ---------------------------------------------------------------------------

fn store_dir(root: &Path) -> PathBuf {
    root.join(STORE_DIR_NAME)
}

fn object_path(root: &Path, kind: &str, digest: &str) -> PathBuf {
    let split = 2.min(digest.len());
    let (bucket, name) = digest.split_at(split);
    store_dir(root)
        .join(OBJECT_DIR)
        .join(kind)
        .join(bucket)
        .join(name)
}

fn read_object(root: &Path, kind: &str, digest: &str) -> Option<Value> {
    let text = fs::read_to_string(object_path(root, kind, digest)).ok()?;
    serde_json::from_str(&text).ok()
}

fn object_exists(root: &Path, kind: &str, digest: &str) -> bool {
    object_path(root, kind, digest).is_file()
}

// ---------------------------------------------------------------------------
// HEAD (pointer)
// ---------------------------------------------------------------------------

/// The raw pointer JSON: the Rust `Pointer` models the generation lane only,
/// while the timeline also reads the `VERSION_POINTER_FIELDS` HEAD carries.
/// `None` when absent or not a valid pointer.
pub fn pointer(root: &Path) -> Option<Value> {
    let text = fs::read_to_string(root.join(POINTER_FILE)).ok()?;
    let data: Value = serde_json::from_str(&text).ok()?;
    let valid = data.get("schema").and_then(Value::as_str) == Some(POINTER_SCHEMA)
        && data.get("generation").and_then(Value::as_str).is_some();
    valid.then_some(data)
}

fn field(pointer: &Value, key: &str) -> Value {
    pointer.get(key).cloned().unwrap_or(Value::Null)
}

// ---------------------------------------------------------------------------
// Retention ledger
// ---------------------------------------------------------------------------

/// The version names retention has trimmed (`history-trim.jsonl`). An
/// unreadable or malformed ledger is `store-invalid` — never silently empty,
/// which would report deliberately trimmed content as retained.
pub fn trimmed_versions(root: &Path) -> Result<BTreeSet<String>, StoreError> {
    let path = store_dir(root).join(TRIM_LOG);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(error) => {
            return Err(StoreError::store(
                "store-invalid",
                format!(
                    "history trim ledger cannot be read: {}: {error}",
                    path.display()
                ),
            ))
        }
    };
    let mut versions = BTreeSet::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record: Value = serde_json::from_str(line).map_err(|_| {
            StoreError::store(
                "store-invalid",
                format!("history trim ledger line {} is invalid JSON", index + 1),
            )
        })?;
        match record.get("version").and_then(Value::as_str) {
            Some(version) => {
                versions.insert(version.to_string());
            }
            None => {
                return Err(StoreError::store(
                    "store-invalid",
                    format!("history trim ledger line {} has no version", index + 1),
                ))
            }
        }
    }
    Ok(versions)
}

// ---------------------------------------------------------------------------
// Tree digests
// ---------------------------------------------------------------------------

/// Python `_canonical_tree_digest`: the semantic digest of the four
/// canonical assets at `dir` (a missing asset counts as null).
fn canonical_digest(dir: &Path) -> String {
    let mut assets = serde_json::Map::new();
    for name in CANONICAL_ASSETS {
        let value = match file_sha256(&dir.join(name)) {
            Ok(hash) => Value::String(hash),
            Err(_) => Value::Null,
        };
        assets.insert(name.to_string(), value);
    }
    semantic_sha256(&Value::Object(assets))
}

/// Python `canonical_tree_digest`: the CURRENT generation's digest for a
/// store-backed workdir, the root's otherwise.
fn current_tree_digest(root: &Path) -> String {
    if !crate::store::has_store(root) {
        return canonical_digest(root);
    }
    let generation = pointer(root)
        .and_then(|p| {
            p.get("generation")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();
    let generation_dir = store_dir(root).join("generations").join(generation);
    if generation_dir.is_dir() {
        canonical_digest(&generation_dir)
    } else {
        canonical_digest(root)
    }
}

/// Python `draft_tree_digest`: the canonical assets the workdir ROOT presents
/// — the working state a save would commit. It differs from the current
/// generation's digest exactly while a hand edit to a canonical file is
/// unsaved.
fn draft_tree_digest(root: &Path) -> String {
    if CANONICAL_ASSETS
        .iter()
        .any(|name| root.join(name).is_file())
    {
        return canonical_digest(root);
    }
    current_tree_digest(root)
}

/// Python `head_version`: the version HEAD names plus whether the workdir
/// still matches it. No recorded tree yet means the document has never been
/// saved, which counts as dirty so the first commit creates V1.
pub fn head_version(root: &Path) -> Value {
    let pointer = pointer(root).unwrap_or_else(|| json!({}));
    let canonical_tree = draft_tree_digest(root);
    let tree = field(&pointer, "head_tree");
    let dirty = canonical_tree != tree;
    json!({
        "version": field(&pointer, "head_version"),
        "generation": field(&pointer, "head_version_generation"),
        "seq": field(&pointer, "version_seq"),
        "commit": field(&pointer, "head_commit"),
        "tree": tree,
        "tree_object": field(&pointer, "head_tree_object"),
        "baseline_epoch": field(&pointer, "baseline_epoch"),
        "canonical_tree": canonical_tree,
        "dirty": dirty,
    })
}

// ---------------------------------------------------------------------------
// The chain
// ---------------------------------------------------------------------------

/// Python `_legacy_chain`: version records that live in generation manifests
/// (workdirs saved before the object pool). Content counts as retained — a
/// legacy version keeps it in its generation.
fn legacy_chain(root: &Path, generation: Option<String>, trimmed: &BTreeSet<String>) -> Vec<Value> {
    let mut chain: Vec<Value> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut current = generation;
    while let Some(id) = current {
        if id.is_empty() || !seen.insert(id.clone()) {
            break;
        }
        let manifest_path = store_dir(root)
            .join("generations")
            .join(&id)
            .join("generation.json");
        let Ok(text) = fs::read_to_string(&manifest_path) else {
            break;
        };
        let Ok(manifest) = serde_json::from_str::<Value>(&text) else {
            break;
        };
        let Some(record) = manifest.get("version").and_then(Value::as_object) else {
            break;
        };
        if record.is_empty() {
            break;
        }
        let mut record = Value::Object(record.clone());
        let version = record.get("version").and_then(Value::as_str).unwrap_or("");
        let content = if trimmed.contains(version) {
            "trimmed"
        } else {
            "retained"
        };
        if let Some(object) = record.as_object_mut() {
            object.insert("generation".to_string(), Value::String(id.clone()));
            object.insert("content".to_string(), Value::String(content.to_string()));
        }
        current = record
            .get("parent_generation")
            .and_then(Value::as_str)
            .map(str::to_string);
        chain.push(record);
    }
    chain
}

/// Python `_version_chain`: version records from HEAD backwards, newest
/// first. Each record carries the commit id, its retention `content`, and
/// the enrichment the chain walk derives.
fn version_chain(root: &Path) -> Result<Vec<Value>, StoreError> {
    let trimmed = trimmed_versions(root)?;
    let pointer = pointer(root).unwrap_or_else(|| json!({}));
    let mut chain: Vec<Value> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    let mut commit = match field(&pointer, "head_commit") {
        Value::String(id) if !id.is_empty() => Some(id),
        _ => None,
    };
    while let Some(id) = commit {
        if !seen.insert(id.clone()) {
            break;
        }
        let Some(mut record) = read_object(root, "version", &id) else {
            break;
        };
        let version = record.get("version").and_then(Value::as_str).unwrap_or("");
        let content = match record.get("tree_object").and_then(Value::as_str) {
            Some(tree) if trimmed.contains(version) || !object_exists(root, "tree", tree) => {
                "trimmed"
            }
            // Retained, or no tree at all: the content still rides in the
            // version's own generation.
            _ => "retained",
        };
        if let Some(object) = record.as_object_mut() {
            object.insert("commit".to_string(), Value::String(id.clone()));
            object.insert("content".to_string(), Value::String(content.to_string()));
        }
        commit = record
            .get("parent_commit")
            .and_then(Value::as_str)
            .map(str::to_string);
        chain.push(record);
    }

    // Join the legacy tail: the newest commit names the generation the
    // pre-pool chain continues from; an empty pool chain starts at HEAD.
    let legacy_generation = if chain.is_empty() {
        match field(&pointer, "head_version_generation") {
            Value::String(generation) if !generation.is_empty() => Some(generation),
            _ => None,
        }
    } else {
        chain
            .last()
            .and_then(|record| record.get("parent_generation"))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    chain.extend(legacy_chain(root, legacy_generation, &trimmed));
    Ok(chain)
}

// ---------------------------------------------------------------------------
// The tool payload
// ---------------------------------------------------------------------------

/// Python `store.history_list`: walk the version chain from HEAD — the commit
/// graph is the history. `limit`/`offset` window the returned `versions`
/// (Python clamps to `max(1, limit)` and `max(0, offset)`); the counts always
/// describe the whole chain, not the window.
pub fn history_list(root: &Path, limit: i64, offset: i64) -> Result<Value, StoreError> {
    let chain = version_chain(root)?;
    let head = head_version(root);
    let offset = offset.max(0) as usize;
    let limit = limit.max(1) as usize;
    let versions: Vec<Value> = chain.iter().skip(offset).take(limit).cloned().collect();
    let count = |wanted: &str| {
        chain
            .iter()
            .filter(|item| item.get("content").and_then(Value::as_str) == Some(wanted))
            .count()
    };
    Ok(json!({
        "schema": HISTORY_SCHEMA,
        "current": head.get("version").cloned().unwrap_or(Value::Null),
        "current_tree": head.get("tree").cloned().unwrap_or(Value::Null),
        "version_dirty": head.get("dirty").cloned().unwrap_or(Value::Null),
        "total": chain.len(),
        "retained": count("retained"),
        "trimmed": count("trimmed"),
        "offset": offset,
        "versions": versions,
    }))
}
