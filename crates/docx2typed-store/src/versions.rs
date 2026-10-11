//! Version timeline (`docx2typed-history-1`) — the Python Reference's
//! `store.head_version` / `store.history_list` / `store.history_diff` /
//! `store.history_blame` / `store.history_verify` reads (ADR 0043/0044) plus
//! the retention/restore half (`store.history_gc` and the `history_restore`
//! mutation): HEAD names the current version and the commit graph in the
//! content-addressed object pool *is* the history, so listing it needs no
//! index file. A workdir saved before the pool existed falls back to the
//! `version` record its generation manifests carry, and the two chains are
//! joined where the newest commit names a legacy parent.
//!
//! Reads never write the pointer, a generation, or an object. `history_gc`
//! writes only the trim ledger and sweeps unreferenced pool objects;
//! `history_restore` copies an earlier state FORWARD into a new version (history
//! never rewinds, ADR 0039) through the normal mutation pipeline. Both stay
//! faithful to the Python layout, so a Python-written workdir behaves
//! identically.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use docx2typed_protocol::{
    bytes_sha256, canonical_json, canonical_operation_input, file_sha256, new_operation_id,
    resolve_path, semantic_sha256,
};
use serde_json::{json, Value};

use crate::objects;
use crate::store::{
    has_store, now_iso, read_root, SaveBoundary, Store, StoreMutateRequest, POINTER_FILE,
    POINTER_SCHEMA, STORE_DIR_NAME,
};
use crate::{RunOutcome, StoreError};

pub const HISTORY_SCHEMA: &str = "docx2typed-history-1";
pub const HISTORY_DIFF_SCHEMA: &str = "docx2typed-history-diff-1";
pub const HISTORY_BLAME_SCHEMA: &str = "docx2typed-history-blame-1";
pub const HISTORY_VERIFY_SCHEMA: &str = "docx2typed-history-verify-1";

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
    Ok(read_trim_records(root)?
        .iter()
        .filter_map(|record| {
            record
                .get("version")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect())
}

/// The full trim ledger records (`store._read_trim_records`): one JSON object
/// per line, each naming the version and the tree it released.
fn read_trim_records(root: &Path) -> Result<Vec<Value>, StoreError> {
    let path = store_dir(root).join(TRIM_LOG);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
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
    let mut records = Vec::new();
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
            Some(version) if !version.is_empty() => records.push(record),
            _ => {
                return Err(StoreError::store(
                    "store-invalid",
                    format!("history trim ledger line {} has no version", index + 1),
                ))
            }
        }
    }
    Ok(records)
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

// ---------------------------------------------------------------------------
// Paragraph identity — the unit history reports on
// ---------------------------------------------------------------------------

const MAP_SCHEMA: &str = "docx2typed-map-1";
const BUCKET_SCHEMA: &str = "docx2typed-bucket-1";
/// `objectstore.BUCKET_ENTRIES`: a key's bucket follows from the sorted key
/// list, so blaming one paragraph is one top-map read plus one bucket read per
/// version, never a whole-document scan.
const BUCKET_ENTRIES: usize = 64;
/// Python `_paragraph_preview` truncates to 80 characters.
const PREVIEW_LIMIT: usize = 80;
/// Python `history_diff.preview_limit`: previews cover the first 20 touched ids.
const PREVIEW_LIMIT_COUNT: usize = 20;

fn read_bytes(root: &Path, kind: &str, digest: &str) -> Option<Vec<u8>> {
    fs::read(object_path(root, kind, digest)).ok()
}

/// Python `objectstore.read_map`: a bucketed `{key -> blob id}` map, flattened.
/// `None` where Python raises `KeyError` (missing object, unknown schema).
fn read_map(root: &Path, digest: &str) -> Option<BTreeMap<String, String>> {
    let raw = read_bytes(root, "map", digest)?;
    let data: Value = serde_json::from_slice(&raw).ok()?;
    if data.get("schema").and_then(Value::as_str) == Some(MAP_SCHEMA) {
        let mut merged = BTreeMap::new();
        for bucket in data
            .get("buckets")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            merged.extend(read_map(root, bucket.as_str()?)?);
        }
        return Some(merged);
    }
    if data.get("schema").and_then(Value::as_str) == Some(BUCKET_SCHEMA) {
        return Some(
            data.get("entries")
                .and_then(Value::as_object)
                .map(|entries| {
                    entries
                        .iter()
                        .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_string())))
                        .collect()
                })
                .unwrap_or_default(),
        );
    }
    None
}

/// Python `_typed_part`: one asset's part descriptor in a version's tree, or
/// `None` when the version keeps its content in a pre-pool generation.
fn typed_part(root: &Path, tree_id: Option<&str>, name: &str) -> Option<Value> {
    let tree = read_object(root, "tree", tree_id?)?;
    let part = tree.get("parts")?.get(name)?.clone();
    part.is_object().then_some(part)
}

/// Python `objectstore.read_chunk_id`: the blob id of one chunk without
/// loading every bucket.
fn read_chunk_id(root: &Path, part: &Value, key: &str) -> Option<String> {
    if let Some(whole) = part.get("whole").and_then(Value::as_str) {
        return Some(whole.to_string());
    }
    let order_digest = part.get("order").and_then(Value::as_str)?;
    let chunks_digest = part.get("chunks").and_then(Value::as_str)?;
    let mut order: Vec<String> =
        serde_json::from_slice(&read_bytes(root, "blob", order_digest)?).ok()?;
    order.sort();
    let index = order.iter().position(|candidate| candidate == key)?;
    let top: Value = serde_json::from_slice(&read_bytes(root, "map", chunks_digest)?).ok()?;
    let bucket = top
        .get("buckets")
        .and_then(Value::as_array)?
        .get(index / BUCKET_ENTRIES)?
        .as_str()?;
    read_map(root, bucket)?.get(key).cloned()
}

/// Python `objectstore.read_chunk`: one chunk's bytes.
fn read_chunk(root: &Path, part: &Value, key: &str) -> Option<Vec<u8>> {
    read_bytes(root, "blob", &read_chunk_id(root, part, key)?)
}

/// Python `_generation_typed_text`: the content a workdir saved before the
/// object pool keeps in its generation directory.
fn generation_typed_text(root: &Path, generation: Option<&str>) -> Option<String> {
    let generation = generation.filter(|generation| !generation.is_empty())?;
    fs::read_to_string(
        store_dir(root)
            .join("generations")
            .join(generation)
            .join("typed.md"),
    )
    .ok()
}

/// Python `_PARAGRAPH_MARKER` at line start:
/// `<!--@(p|new|delete) (id|temp)="…"` — returns the captured id.
fn paragraph_marker(line: &str) -> Option<String> {
    let rest = line.strip_prefix("<!--@")?;
    let (kind, rest) = rest.split_once(' ')?;
    if !matches!(kind, "p" | "new" | "delete") {
        return None;
    }
    let rest = rest
        .strip_prefix("id=\"")
        .or_else(|| rest.strip_prefix("temp=\""))?;
    Some(rest.split('"').next()?.to_string())
}

/// Python `_split_paragraph_blocks`: `{paragraph id -> block}` by marker line.
/// A reader cannot insist on the pool splitter's blank-line separation — a
/// typed.md written without it still has paragraph markers, and reading it as
/// "no paragraphs" would report a version as empty.
fn split_paragraph_blocks(text: &str) -> BTreeMap<String, String> {
    /// Python `"\n".join(buffer).rstrip("\n")`: the blank line that separates
    /// blocks belongs to the one before it and must not enter the block.
    fn join_block(buffer: &[&str]) -> String {
        buffer.join("\n").trim_end_matches('\n').to_string()
    }

    let mut blocks = BTreeMap::new();
    let mut current: Option<String> = None;
    let mut buffer: Vec<&str> = Vec::new();
    for line in text.lines() {
        match paragraph_marker(line) {
            Some(id) => {
                if let Some(previous) = current.take() {
                    blocks.insert(previous, join_block(&buffer));
                }
                current = Some(id);
                buffer.clear();
            }
            None if current.is_none() => continue,
            None => {}
        }
        buffer.push(line);
    }
    if let Some(id) = current {
        blocks.insert(id, join_block(&buffer));
    }
    blocks
}

/// Python `_paragraph_block`: one paragraph's canonical block in one version.
///
/// Three storage shapes exist — a tree with per-paragraph chunks (one bucket
/// lookup), a tree holding `typed.md` whole (split on read), and a version
/// saved before the object pool (generation only). Comparing a blob id against
/// a whole-file offset would call every paragraph changed, so the identity is
/// derived per shape by the caller.
fn paragraph_block(root: &Path, record: &Value, paragraph_id: &str) -> Option<Vec<u8>> {
    let part = typed_part(
        root,
        record.get("tree_object").and_then(Value::as_str),
        "typed.md",
    );
    let text = match &part {
        Some(part) => {
            if part.get("chunks").is_some() {
                return read_chunk(root, part, paragraph_id);
            }
            let whole = part.get("whole").and_then(Value::as_str)?;
            String::from_utf8_lossy(&read_bytes(root, "blob", whole)?).into_owned()
        }
        None => generation_typed_text(root, record.get("generation").and_then(Value::as_str))?,
    };
    split_paragraph_blocks(&text)
        .remove(paragraph_id)
        .map(String::into_bytes)
}

/// Python `_block_digest`: one content hash per paragraph — the identity
/// comparable across the storage shapes.
fn block_digest(root: &Path, record: &Value, paragraph_id: &str) -> String {
    bytes_sha256(&paragraph_block(root, record, paragraph_id).unwrap_or_default())
}

/// Python `_typed_paragraph_digests`: `({paragraph id -> identity}, chunked?)`.
/// The identity is the pooled blob id when the tree carries per-paragraph
/// chunks, a content hash otherwise; callers comparing two versions must agree
/// on the shape.
fn typed_paragraph_digests(
    root: &Path,
    record: &Value,
) -> Option<(BTreeMap<String, String>, bool)> {
    let part = typed_part(
        root,
        record.get("tree_object").and_then(Value::as_str),
        "typed.md",
    );
    let text = match &part {
        Some(part) if part.get("chunks").is_some() => {
            let map = read_map(root, part.get("chunks")?.as_str()?)?;
            return Some((map, true));
        }
        Some(part) => {
            let whole = part.get("whole").and_then(Value::as_str)?;
            String::from_utf8_lossy(&read_bytes(root, "blob", whole)?).into_owned()
        }
        None => generation_typed_text(root, record.get("generation").and_then(Value::as_str))?,
    };
    let digests = split_paragraph_blocks(&text)
        .into_iter()
        .map(|(key, block)| (key, bytes_sha256(block.as_bytes())))
        .collect();
    Some((digests, false))
}

/// Python `_paragraph_preview`: a block's visible text, trimmed for a report.
fn paragraph_preview(block: Option<&[u8]>) -> Value {
    let Some(block) = block else {
        return Value::Null;
    };
    let decoded = String::from_utf8_lossy(block);
    let text = decoded
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with("<!--"))
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(" ");
    let clipped: String = text.chars().take(PREVIEW_LIMIT).collect();
    Value::String(if text.chars().count() > PREVIEW_LIMIT {
        format!("{clipped}…")
    } else {
        clipped
    })
}

/// Python `_paragraph_sort_key`: P2 before P10 — a diff report is read by a
/// human.
fn paragraph_sort_key(paragraph_id: &str) -> (String, u64, String) {
    let prefix_end = paragraph_id
        .find(|character: char| character.is_ascii_digit())
        .unwrap_or(paragraph_id.len());
    let (prefix, rest) = paragraph_id.split_at(prefix_end);
    let digits_end = rest
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(rest.len());
    let (digits, tail) = rest.split_at(digits_end);
    (
        prefix.to_string(),
        digits.parse().unwrap_or(0),
        tail.to_string(),
    )
}

fn sort_paragraph_ids(ids: &mut [String]) {
    ids.sort_by_key(|id| paragraph_sort_key(id));
}

fn version_of(record: &Value) -> String {
    match record.get("version") {
        Some(Value::String(name)) => name.clone(),
        Some(other) => other.to_string(),
        None => "None".to_string(),
    }
}

fn version_not_found(name: &str) -> StoreError {
    StoreError::store(
        "version-not-found",
        format!("{name} is not in this document's history; call history_list"),
    )
}

/// One side of a diff, or the code that says why it cannot be read: a version
/// whose content retention dropped on purpose is `version-trimmed`, never
/// corruption.
fn readable_side(
    root: &Path,
    record: &Value,
    trimmed: &BTreeSet<String>,
) -> Result<(BTreeMap<String, String>, bool), StoreError> {
    match typed_paragraph_digests(root, record) {
        Some(side) => Ok(side),
        None => {
            let name = version_of(record);
            if trimmed.contains(&name) {
                Err(StoreError::store(
                    "version-trimmed",
                    format!("{name} was trimmed by retention; its content is gone on purpose"),
                ))
            } else {
                Err(StoreError::store(
                    "version-content-missing",
                    format!("{name} has no readable pooled content"),
                ))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// history_diff / history_blame / history_verify
// ---------------------------------------------------------------------------

/// Python `store.history_diff`: which paragraphs one version changed, against
/// its parent by default. Keyed by paragraph identity, so an unchanged
/// paragraph costs one map lookup and the report stays in document order.
pub fn history_diff(
    root: &Path,
    version: &str,
    against: Option<&str>,
) -> Result<Value, StoreError> {
    let chain = version_chain(root)?;
    let position = chain
        .iter()
        .position(|record| record.get("version").and_then(Value::as_str) == Some(version));
    let Some(position) = position else {
        return Err(version_not_found(version));
    };
    let newer = chain[position].clone();
    let parent = match against {
        Some(against) => match chain
            .iter()
            .find(|record| record.get("version").and_then(Value::as_str) == Some(against))
        {
            Some(record) => Some(record.clone()),
            None => return Err(version_not_found(against)),
        },
        None => chain.get(position + 1).cloned(),
    };

    let trimmed = trimmed_versions(root)?;
    let (mut new_map, new_chunked) = readable_side(root, &newer, &trimmed)?;
    let (mut old_map, old_chunked) = match &parent {
        Some(parent) => readable_side(root, parent, &trimmed)?,
        None => (BTreeMap::new(), true),
    };
    if new_chunked != old_chunked {
        // blob ids and content hashes are not comparable identities
        new_map = new_map
            .keys()
            .map(|key| (key.clone(), block_digest(root, &newer, key)))
            .collect();
        old_map = match &parent {
            Some(parent) => old_map
                .keys()
                .map(|key| (key.clone(), block_digest(root, parent, key)))
                .collect(),
            None => BTreeMap::new(),
        };
    }

    let mut added: Vec<String> = Vec::new();
    let mut changed: Vec<String> = Vec::new();
    let mut shared = 0_usize;
    for (key, digest) in &new_map {
        match old_map.get(key) {
            None => added.push(key.clone()),
            Some(previous) => {
                shared += 1;
                if previous != digest {
                    changed.push(key.clone());
                }
            }
        }
    }
    let mut removed: Vec<String> = old_map
        .keys()
        .filter(|key| !new_map.contains_key(*key))
        .cloned()
        .collect();
    let unchanged = shared - changed.len();
    sort_paragraph_ids(&mut added);
    sort_paragraph_ids(&mut changed);
    sort_paragraph_ids(&mut removed);

    let touched: Vec<&String> = added.iter().chain(&changed).chain(&removed).collect();
    let mut previews = serde_json::Map::new();
    for key in touched.into_iter().take(PREVIEW_LIMIT_COUNT) {
        let before = paragraph_preview(
            parent
                .as_ref()
                .and_then(|parent| paragraph_block(root, parent, key))
                .as_deref(),
        );
        let after = paragraph_preview(paragraph_block(root, &newer, key).as_deref());
        previews.insert((*key).clone(), json!({"before": before, "after": after}));
    }

    Ok(json!({
        "schema": HISTORY_DIFF_SCHEMA,
        "version": version,
        "against": parent
            .as_ref()
            .and_then(|parent| parent.get("version"))
            .cloned()
            .unwrap_or(Value::Null),
        "label": newer.get("label").cloned().unwrap_or(Value::Null),
        "origin": newer.get("origin").cloned().unwrap_or(Value::Null),
        "created_at": newer.get("created_at").cloned().unwrap_or(Value::Null),
        "added": added,
        "changed": changed,
        "removed": removed,
        "unchanged": unchanged,
        "previews": previews,
        "preview_limit": PREVIEW_LIMIT_COUNT,
    }))
}

/// Python `store.history_blame`: the version that last changed one paragraph.
/// Walks the chain comparing the paragraph's own object — one bucket lookup
/// per version, never document text it does not report. `state: absent` means
/// the paragraph is not in the current state.
pub fn history_blame(root: &Path, paragraph_id: &str) -> Result<Value, StoreError> {
    let chain = version_chain(root)?;
    let Some(head) = chain.first() else {
        return Err(StoreError::store(
            "version-not-found",
            "the workdir has no versions",
        ));
    };
    let current = paragraph_block(root, head, paragraph_id);
    if current.is_none() {
        return Ok(json!({
            "schema": HISTORY_BLAME_SCHEMA,
            "paragraph_id": paragraph_id,
            "state": "absent",
            "version": Value::Null,
            "versions": chain.len(),
        }));
    }
    for (index, record) in chain.iter().enumerate() {
        let block = if index == 0 {
            current.clone()
        } else {
            paragraph_block(root, record, paragraph_id)
        };
        let parent = chain.get(index + 1);
        let parent_block = parent.and_then(|parent| paragraph_block(root, parent, paragraph_id));
        if parent_block != block {
            return Ok(json!({
                "schema": HISTORY_BLAME_SCHEMA,
                "paragraph_id": paragraph_id,
                "state": if parent_block.is_none() { "added" } else { "modified" },
                "version": record.get("version").cloned().unwrap_or(Value::Null),
                "label": record.get("label").cloned().unwrap_or(Value::Null),
                "origin": record.get("origin").cloned().unwrap_or(Value::Null),
                "created_at": record.get("created_at").cloned().unwrap_or(Value::Null),
                "text_preview": paragraph_preview(block.as_deref()),
                "previous": match (&parent, &parent_block) {
                    (Some(parent), Some(parent_block)) => json!({
                        "version": parent.get("version").cloned().unwrap_or(Value::Null),
                        "text_preview": paragraph_preview(Some(parent_block.as_slice())),
                    }),
                    _ => Value::Null,
                },
                "versions": chain.len(),
            }));
        }
    }
    // chain[0] always differs from its parent or is itself the first version.
    Ok(json!({
        "schema": HISTORY_BLAME_SCHEMA,
        "paragraph_id": paragraph_id,
        "state": "unknown",
        "version": chain
            .last()
            .and_then(|record| record.get("version"))
            .cloned()
            .unwrap_or(Value::Null),
        "versions": chain.len(),
    }))
}

/// Python `objectstore.verify`: every object a tree needs, present? A missing
/// blob is detected and named, never silent.
fn verify_tree(root: &Path, tree_id: &str) -> Value {
    let Some(tree) = read_object(root, "tree", tree_id) else {
        return json!({"ok": false, "missing": ["tree"], "checked": 0});
    };
    let mut missing: Vec<Value> = Vec::new();
    let mut checked = 0_usize;
    let parts = tree
        .get("parts")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for (name, part) in parts {
        if let Some(whole) = part.get("whole").and_then(Value::as_str) {
            checked += 1;
            if !object_exists(root, "blob", whole) {
                missing.push(json!(format!("{name}:blob")));
            }
            continue;
        }
        checked += 1;
        let order_present = part
            .get("order")
            .and_then(Value::as_str)
            .is_some_and(|order| object_exists(root, "blob", order));
        if !order_present {
            missing.push(json!(format!("{name}:order")));
        }
        let entries = part
            .get("chunks")
            .and_then(Value::as_str)
            .and_then(|chunks| read_map(root, chunks));
        let Some(entries) = entries else {
            missing.push(json!(format!("{name}:map")));
            continue;
        };
        for (key, digest) in entries {
            checked += 1;
            if !object_exists(root, "blob", &digest) {
                missing.push(json!(format!("{name}:{key}")));
            }
        }
    }
    json!({
        "schema": HISTORY_VERIFY_SCHEMA,
        "tree": tree_id,
        "checked": checked,
        "missing": missing,
        "ok": missing.is_empty(),
    })
}

/// Python `store.history_verify`: check retained content and report deliberate
/// trims separately. Content retention released is `content: trimmed`, never
/// counted as damage.
pub fn history_verify(root: &Path) -> Result<Value, StoreError> {
    let trimmed = trimmed_versions(root)?;
    let mut versions: Vec<Value> = Vec::new();
    let mut ok = true;
    for record in version_chain(root)? {
        let name = version_of(&record);
        if trimmed.contains(&name) {
            versions.push(json!({"version": name, "content": "trimmed", "missing": []}));
            continue;
        }
        let tree_object = record
            .get("tree_object")
            .and_then(Value::as_str)
            .filter(|tree| !tree.is_empty());
        let Some(tree_object) = tree_object else {
            let generation = store_dir(root).join("generations").join(
                record
                    .get("generation")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            );
            let present = generation.is_dir();
            versions.push(json!({
                "version": name,
                "content": if present { "retained" } else { "missing" },
                "missing": if present { json!([]) } else { json!(["generation"]) },
            }));
            ok = ok && present;
            continue;
        };
        let report = verify_tree(root, tree_object);
        let intact = report.get("ok").and_then(Value::as_bool).unwrap_or(false);
        let missing: Vec<Value> = report
            .get("missing")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        versions.push(json!({
            "version": name,
            "content": if intact { "retained" } else { "missing" },
            "missing": missing.into_iter().take(5).collect::<Vec<_>>(),
        }));
        ok = ok && intact;
    }
    Ok(json!({
        "schema": HISTORY_VERIFY_SCHEMA,
        "ok": ok,
        "versions": versions,
    }))
}

// ---------------------------------------------------------------------------
// history_gc — retention planning and application
// ---------------------------------------------------------------------------

/// Python `store._RETENTION_SCHEMA`.
pub const HISTORY_GC_SCHEMA: &str = "docx2typed-history-gc-1";

fn read_json_file(path: &Path) -> Option<Value> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Every file under `dir`, recursively, sorted — pool kinds bucket by digest
/// (`ab/cdef…`), so a sweep walks three directory trees instead of an index.
fn collect_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// Python `objectstore._map_objects`: mark a bucketed map and every bucket and
/// blob it references, so `sweep` never drops a referenced object.
fn mark_map_objects(root: &Path, map_id: &str, reachable: &mut BTreeSet<(String, String)>) {
    reachable.insert(("map".to_string(), map_id.to_string()));
    let Some(data) = read_object(root, "map", map_id) else {
        return;
    };
    if data.get("schema").and_then(Value::as_str) == Some(MAP_SCHEMA) {
        for bucket in data
            .get("buckets")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            if let Some(bucket) = bucket.as_str() {
                mark_map_objects(root, bucket, reachable);
            }
        }
        return;
    }
    if let Some(entries) = data.get("entries").and_then(Value::as_object) {
        for digest in entries.values() {
            if let Some(digest) = digest.as_str() {
                reachable.insert(("blob".to_string(), digest.to_string()));
            }
        }
    }
}

/// Python `objectstore.sweep`: drop blobs/trees/maps no retained version
/// references; commits are never swept (the chain survives the content).
/// Returns `(removed, freed_bytes)`.
fn sweep_objects(root: &Path, keep_trees: &BTreeSet<String>) -> Result<(usize, u64), StoreError> {
    let mut reachable: BTreeSet<(String, String)> = BTreeSet::new();
    for tree_id in keep_trees {
        let Some(tree) = read_object(root, "tree", tree_id) else {
            continue;
        };
        reachable.insert(("tree".to_string(), tree_id.clone()));
        let parts = tree
            .get("parts")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        for part in parts.values() {
            if let Some(whole) = part.get("whole").and_then(Value::as_str) {
                reachable.insert(("blob".to_string(), whole.to_string()));
                continue;
            }
            for key in ["order", "header", "global"] {
                if let Some(digest) = part.get(key).and_then(Value::as_str) {
                    reachable.insert(("blob".to_string(), digest.to_string()));
                }
            }
            if let Some(chunks) = part.get("chunks").and_then(Value::as_str) {
                mark_map_objects(root, chunks, &mut reachable);
            }
        }
    }
    let mut removed = 0_usize;
    let mut freed = 0_u64;
    let base = store_dir(root).join(OBJECT_DIR);
    for kind in ["blob", "tree", "map"] {
        let kind_dir = base.join(kind);
        if !kind_dir.is_dir() {
            continue;
        }
        for path in collect_files(&kind_dir) {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let bucket = path
                .parent()
                .and_then(|parent| parent.file_name())
                .and_then(|name| name.to_str())
                .unwrap_or("");
            let digest = format!("{bucket}{name}");
            if reachable.contains(&(kind.to_string(), digest)) {
                continue;
            }
            freed += fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
            if fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
    }
    Ok((removed, freed))
}

/// Python `_append_trim_log`: durably mark content trims before sweeping their
/// objects. Append-only logically; rewritten atomically so a retry is
/// idempotent and a torn tail never becomes state.
fn append_trim_log(root: &Path, versions: &[Value]) -> Result<(), StoreError> {
    if versions.is_empty() {
        return Ok(());
    }
    let path = store_dir(root).join(TRIM_LOG);
    let existing = read_trim_records(root)?;
    let mut known: BTreeMap<String, Value> = existing
        .iter()
        .filter_map(|record| {
            record
                .get("version")
                .and_then(Value::as_str)
                .map(|version| (version.to_string(), record.clone()))
        })
        .collect();
    let mut merged = existing.clone();
    for record in versions {
        let version = record
            .get("version")
            .and_then(Value::as_str)
            .filter(|version| !version.is_empty())
            .ok_or_else(|| {
                StoreError::store("store-invalid", "history trim record has no version")
            })?;
        if let Some(recorded) = known.get(version) {
            let recorded_tree = recorded.get("tree_object").and_then(Value::as_str);
            let requested_tree = record.get("tree_object").and_then(Value::as_str);
            if recorded_tree.is_some()
                && requested_tree.is_some()
                && recorded_tree != requested_tree
            {
                return Err(StoreError::store(
                    "store-invalid",
                    format!("history trim tree mismatch for {version}"),
                ));
            }
            continue;
        }
        let entry = json!({
            "version": version,
            "tree_object": record.get("tree_object").cloned().unwrap_or(Value::Null),
            "trimmed_at": now_iso(),
        });
        known.insert(version.to_string(), entry.clone());
        merged.push(entry);
    }
    if merged.len() == existing.len() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(StoreError::Io)?;
    }
    let payload: String = merged
        .iter()
        .map(|record| format!("{}\n", canonical_json(record)))
        .collect();
    let temp = path.with_extension("jsonl.tmp");
    fs::write(&temp, payload).map_err(StoreError::Io)?;
    fs::rename(&temp, &path).map_err(StoreError::Io)?;
    Ok(())
}

/// Generations a live transaction still references (`_retention_plan`'s
/// transaction scan): a prepared or completed journal names the generation its
/// commit publishes, so retention must not reclaim it.
fn transaction_generations(root: &Path) -> BTreeSet<String> {
    let transactions = store_dir(root).join("transactions");
    let mut generations = BTreeSet::new();
    if !transactions.is_dir() {
        return generations;
    }
    for transaction in fs::read_dir(&transactions).into_iter().flatten().flatten() {
        let path = transaction.path();
        if !path.is_dir() {
            continue;
        }
        for phase in collect_files(&path) {
            if phase.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            if let Some(record) = read_json_file(&phase) {
                if let Some(generation) = record.get("generation").and_then(Value::as_str) {
                    generations.insert(generation.to_string());
                }
            }
        }
    }
    generations
}

struct RetentionPlan {
    report: Value,
    keep_trees: BTreeSet<String>,
    reclaimable: Vec<String>,
    trimmed: Vec<Value>,
    newly_trimmed: Vec<Value>,
}

/// Python `_retention_plan`: keep the last `keep_last` versions plus pinned
/// ones; a version sharing a retained tree is retained with it.
fn retention_plan(root: &Path, keep_last: i64, dry_run: bool) -> Result<RetentionPlan, StoreError> {
    let chain = version_chain(root)?;
    let trimmed_names = trimmed_versions(root)?;
    let keep_boundary = keep_last.max(0) as usize;

    let mut retained_names: BTreeSet<String> = BTreeSet::new();
    for (index, record) in chain.iter().enumerate() {
        let version = record.get("version").and_then(Value::as_str).unwrap_or("");
        let pinned = record.get("pin").and_then(Value::as_bool).unwrap_or(false);
        if !trimmed_names.contains(version) && (index < keep_boundary || pinned) {
            retained_names.insert(version.to_string());
        }
    }

    let mut keep_trees: BTreeSet<String> = BTreeSet::new();
    for record in &chain {
        let version = record.get("version").and_then(Value::as_str).unwrap_or("");
        if !retained_names.contains(version) {
            continue;
        }
        if let Some(tree) = record
            .get("tree_object")
            .and_then(Value::as_str)
            .filter(|tree| !tree.is_empty())
        {
            keep_trees.insert(tree.to_string());
        }
    }

    let mut trimmed: Vec<Value> = Vec::new();
    for record in &chain {
        let version = record.get("version").and_then(Value::as_str).unwrap_or("");
        if trimmed_names.contains(version) {
            trimmed.push(record.clone());
            continue;
        }
        if retained_names.contains(version) {
            continue;
        }
        // A version sharing a retained tree still has restorable content.
        let shared = record
            .get("tree_object")
            .and_then(Value::as_str)
            .is_some_and(|tree| keep_trees.contains(tree));
        if !shared {
            trimmed.push(record.clone());
        }
    }

    let trimmed_version_names: BTreeSet<String> = trimmed
        .iter()
        .filter_map(|record| {
            record
                .get("version")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();

    let mut keep_generations: BTreeSet<String> = BTreeSet::new();
    for record in &chain {
        let version = record.get("version").and_then(Value::as_str).unwrap_or("");
        let has_tree = record
            .get("tree_object")
            .and_then(Value::as_str)
            .is_some_and(|tree| !tree.is_empty());
        if !has_tree && !trimmed_version_names.contains(version) {
            if let Some(generation) = record.get("generation").and_then(Value::as_str) {
                keep_generations.insert(generation.to_string());
            }
        }
    }
    if let Some(generation) = pointer(root).and_then(|pointer| {
        pointer
            .get("generation")
            .and_then(Value::as_str)
            .map(str::to_string)
    }) {
        keep_generations.insert(generation);
    }
    keep_generations.extend(transaction_generations(root));

    let mut reclaimable: Vec<String> = Vec::new();
    let generations_dir = store_dir(root).join("generations");
    if generations_dir.is_dir() {
        let mut names: Vec<String> = fs::read_dir(&generations_dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        names.sort();
        for name in names {
            if keep_generations.contains(&name) {
                continue;
            }
            let Some(manifest) =
                read_json_file(&generations_dir.join(&name).join("generation.json"))
            else {
                continue;
            };
            let version = manifest.get("version").and_then(Value::as_object);
            let tree_object = version
                .and_then(|version| version.get("tree_object"))
                .and_then(Value::as_str);
            let version_name = version
                .and_then(|version| version.get("version"))
                .and_then(Value::as_str);
            if tree_object.is_some_and(|tree| keep_trees.contains(tree))
                || version_name.is_some_and(|name| trimmed_version_names.contains(name))
            {
                reclaimable.push(name);
            }
        }
    }

    let report = json!({
        "schema": HISTORY_GC_SCHEMA,
        "versions_total": chain.len(),
        "versions_retained": chain.len() - trimmed.len(),
        "trees_kept": keep_trees.len(),
        "generations_reclaimable": reclaimable.len(),
        "dry_run": dry_run,
        "swept_objects": 0,
        "freed_bytes": 0,
        "versions_trimmed": trimmed
            .iter()
            .map(|record| record.get("version").cloned().unwrap_or(Value::Null))
            .collect::<Vec<_>>(),
    });
    let newly_trimmed: Vec<Value> = trimmed
        .iter()
        .filter(|record| {
            !record
                .get("version")
                .and_then(Value::as_str)
                .is_some_and(|version| trimmed_names.contains(version))
        })
        .cloned()
        .collect();
    Ok(RetentionPlan {
        report,
        keep_trees,
        reclaimable,
        trimmed,
        newly_trimmed,
    })
}

/// Python `store.history_gc`: reclaim history content past retention; commit
/// metadata is never dropped. Keeps the most recent `keep_last` versions plus
/// pinned versions. `dry_run` reports without deleting or recording a trim.
///
/// The decision (which versions are trimmed, which trees are kept, which
/// generations are reclaimable) is exactly Python's. The applied path writes
/// the trim ledger, sweeps unreferenced pool objects, and reclaims the
/// generations retention released. It runs under the Writer lane so it never
/// races a mutation; the Python reference additionally journals the decision
/// (`retention-marked` / `retention-swept` / `completed`) — that recovery
/// ledger is not part of this store's recovery yet.
pub fn history_gc(root: &Path, keep_last: i64, dry_run: bool) -> Result<Value, StoreError> {
    let root = resolve_path(root);
    let store = Store::new(&root);
    // Dry runs still take the lane, so the plan never observes a half-written
    // mutation (Python takes `writer(timeout_ms=0)` for both).
    let _guard = if has_store(&root) {
        Some(store.writer(0)?)
    } else {
        None
    };
    let plan = retention_plan(&root, keep_last, dry_run)?;
    if dry_run || (plan.newly_trimmed.is_empty() && plan.reclaimable.is_empty()) {
        return Ok(plan.report);
    }
    append_trim_log(&root, &plan.trimmed)?;
    let (removed, freed) = sweep_objects(&root, &plan.keep_trees)?;
    let generations_dir = store_dir(&root).join("generations");
    for generation in &plan.reclaimable {
        let _ = fs::remove_dir_all(generations_dir.join(generation));
    }
    let mut report = plan.report;
    report["swept_objects"] = json!(removed);
    report["freed_bytes"] = json!(freed);
    report["generations_reclaimed"] = json!(plan.reclaimable.len());
    Ok(report)
}

// ---------------------------------------------------------------------------
// history_restore — copy an earlier state FORWARD into a new version
// ---------------------------------------------------------------------------

/// Paragraph ids that carry table/SDT/text-box topology (`_cherry_pick_guard`'s
/// `(?:^|\.)[TSB]\d+\.`), which a selective restore never moves.
fn coupled_to_topology(paragraph_id: &str) -> bool {
    let bytes = paragraph_id.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if !matches!(*byte, b'T' | b'S' | b'B') {
            continue;
        }
        if index > 0 && bytes[index - 1] != b'.' {
            continue;
        }
        let mut cursor = index + 1;
        while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
            cursor += 1;
        }
        if cursor > index + 1 && cursor < bytes.len() && bytes[cursor] == b'.' {
            return true;
        }
    }
    false
}

/// `format.json`'s paragraph records keyed by id (`_paragraph_records`).
fn paragraph_records(dir: &Path) -> BTreeMap<String, Value> {
    let Some(data) = read_json_file(&dir.join("format.json")) else {
        return BTreeMap::new();
    };
    data.get("paragraphs")
        .and_then(Value::as_array)
        .map(|records| {
            records
                .iter()
                .filter_map(|record| {
                    record
                        .get("id")
                        .and_then(Value::as_str)
                        .map(|id| (id.to_string(), record.clone()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `path -> sha256` as recorded in a generation manifest (`_recorded_assets`);
/// `{}` for a pooled source, which has no generation manifest to read.
fn recorded_assets(dir: &Path) -> BTreeMap<String, String> {
    let Some(manifest) = read_json_file(&dir.join("generation.json")) else {
        return BTreeMap::new();
    };
    manifest
        .get("assets")
        .and_then(Value::as_array)
        .map(|assets| {
            assets
                .iter()
                .filter_map(|entry| {
                    let path = entry.get("path").and_then(Value::as_str)?;
                    let sha = entry.get("sha256").and_then(Value::as_str)?;
                    Some((path.to_string(), sha.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Python `_cherry_pick_guard`: why a selective restore is refused, or `None`
/// when the selection is dependency-free. A paragraph's format record carries
/// `token_ids` into a GLOBAL token table, so moving one across versions can
/// dangle OOXML bytes or split an anchor pair.
fn cherry_pick_guard(source: &Path, target: &Path, selection: &[String]) -> Option<String> {
    let source_records = paragraph_records(source);
    let current_records = paragraph_records(target);
    let mut problems: Vec<String> = Vec::new();
    let mut ids: Vec<String> = selection.to_vec();
    ids.sort();
    ids.dedup();
    for paragraph_id in ids {
        if coupled_to_topology(&paragraph_id) {
            problems.push(format!(
                "{paragraph_id} is coupled to table/SDT/text-box topology"
            ));
            continue;
        }
        let old = source_records.get(&paragraph_id);
        let new = current_records.get(&paragraph_id);
        let (Some(old), Some(new)) = (old, new) else {
            problems.push(format!(
                "{paragraph_id} is missing from one of the two states"
            ));
            continue;
        };
        for (label, record) in [("the version", old), ("the current state", new)] {
            let tokens = record
                .get("token_ids")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if tokens.is_empty() {
                continue;
            }
            let names: Vec<String> = tokens
                .iter()
                .take(4)
                .filter_map(|token| token.get(0).and_then(Value::as_str).map(str::to_string))
                .collect();
            problems.push(format!(
                "{paragraph_id} references {} token(s) in {label} ({}…)",
                tokens.len(),
                names.join(", ")
            ));
        }
    }
    if problems.is_empty() {
        return None;
    }
    Some(format!(
        "selective restore moves dependency-free paragraphs only: {}. Restore the whole version, \
         or patch the paragraph explicitly with its target text.",
        problems.join("; ")
    ))
}

/// Paragraph ids in the order they appear in `typed.md` (dedup keeps first).
fn paragraph_order(text: &str) -> Vec<String> {
    let mut order: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for line in text.lines() {
        if let Some(id) = paragraph_marker(line) {
            if seen.insert(id.clone()) {
                order.push(id);
            }
        }
    }
    order
}

/// The non-paragraph head of `typed.md` (the `@typed`/`@part` preamble), which
/// `split_paragraph_blocks` skips.
fn paragraph_preamble(text: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    for line in text.lines() {
        if paragraph_marker(line).is_some() {
            break;
        }
        lines.push(line);
    }
    lines.join("\n").trim_end_matches('\n').to_string()
}

/// Python `_apply_cherry_pick`: splice the selected paragraphs (text AND format
/// record) from a version into the current canonical state. Returns the ids
/// actually replaced. The store crate carries no typed-node model, so the
/// splice is at block granularity (`split_paragraph_blocks`), which is the same
/// unit the pool splitter writes.
fn cherry_pick(
    source: &Path,
    target: &Path,
    selection: &[String],
) -> Result<Vec<String>, StoreError> {
    let chosen: BTreeSet<&str> = selection.iter().map(String::as_str).collect();
    let source_text = fs::read_to_string(source.join("typed.md")).map_err(StoreError::Io)?;
    let target_text = fs::read_to_string(target.join("typed.md")).map_err(StoreError::Io)?;
    let source_blocks = split_paragraph_blocks(&source_text);
    let mut target_blocks = split_paragraph_blocks(&target_text);
    let mut replaced: Vec<String> = Vec::new();
    for id in &chosen {
        let Some(block) = source_blocks.get(*id) else {
            return Err(StoreError::store(
                "version-content-missing",
                format!("{id} not found in the restored version"),
            ));
        };
        if !target_blocks.contains_key(*id) {
            return Err(StoreError::store(
                "version-content-missing",
                format!("{id} not found in the restored version"),
            ));
        }
        target_blocks.insert((*id).to_string(), block.clone());
        replaced.push((*id).to_string());
    }
    // Reassemble in document order, preserving the `@typed` preamble.
    let mut parts: Vec<String> = Vec::new();
    let preamble = paragraph_preamble(&target_text);
    if !preamble.is_empty() {
        parts.push(preamble);
    }
    for id in paragraph_order(&target_text) {
        if let Some(block) = target_blocks.get(&id) {
            parts.push(block.clone());
        }
    }
    let mut typed = parts.join("\n\n");
    typed.push('\n');
    fs::write(target.join("typed.md"), typed).map_err(StoreError::Io)?;

    if let Some(mut format_data) = read_json_file(&target.join("format.json")) {
        let source_records = paragraph_records(source);
        if let Some(paragraphs) = format_data
            .get_mut("paragraphs")
            .and_then(Value::as_array_mut)
        {
            for record in paragraphs.iter_mut() {
                let Some(id) = record.get("id").and_then(Value::as_str).map(str::to_string) else {
                    continue;
                };
                if !chosen.contains(id.as_str()) {
                    continue;
                }
                if let Some(source_record) = source_records.get(&id) {
                    // text and the record that describes it move together
                    *record = source_record.clone();
                }
            }
        }
        let mut bytes = serde_json::to_string_pretty(&format_data)
            .map_err(|error| StoreError::Io(std::io::Error::other(error)))?;
        bytes.push('\n');
        fs::write(target.join("format.json"), bytes).map_err(StoreError::Io)?;
    }
    replaced.sort();
    Ok(replaced)
}

/// Paragraph id -> visible text (`_changed_paragraph_texts`): the summary line
/// of a restore. Unreadable sides compare as "no information".
fn visible_paragraph_texts(text: &str) -> BTreeMap<String, String> {
    split_paragraph_blocks(text)
        .into_iter()
        .map(|(id, block)| {
            let visible = block
                .lines()
                .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with("<!--"))
                .map(str::trim)
                .collect::<Vec<_>>()
                .join(" ");
            (id, visible)
        })
        .collect()
}

fn changed_paragraph_texts(before: Option<&str>, after: &str) -> Vec<String> {
    let before = before.map(visible_paragraph_texts).unwrap_or_default();
    let after = visible_paragraph_texts(after);
    let mut ids: BTreeSet<String> = before.keys().cloned().collect();
    ids.extend(after.keys().cloned());
    let mut changed: Vec<String> = ids
        .into_iter()
        .filter(|id| before.get(id) != after.get(id))
        .collect();
    sort_paragraph_ids(&mut changed);
    changed
}

/// Python `_version_state_dir`: a version's canonical state, materialised from
/// the pool when the version has a tree there (ADR 0043), else the generation
/// that still carries it. `None` when neither exists (trimmed).
fn version_state_dir(
    root: &Path,
    record: &Value,
    scratch: &Path,
) -> Result<Option<PathBuf>, StoreError> {
    let tree_object = record
        .get("tree_object")
        .and_then(Value::as_str)
        .filter(|tree| !tree.is_empty());
    if let Some(tree_object) = tree_object {
        if object_exists(root, "tree", tree_object) {
            let report = verify_tree(root, tree_object);
            if !report.get("ok").and_then(Value::as_bool).unwrap_or(false) {
                let missing: Vec<String> = report
                    .get("missing")
                    .and_then(Value::as_array)
                    .map(|missing| {
                        missing
                            .iter()
                            .take(3)
                            .filter_map(|item| item.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                return Err(StoreError::store(
                    "version-content-missing",
                    format!(
                        "{} content is incomplete: {}",
                        version_of(record),
                        missing.join(", ")
                    ),
                ));
            }
            objects::materialize(root, tree_object, scratch)
                .map_err(|error| StoreError::store("version-content-missing", error.message()))?;
            return Ok(Some(scratch.to_path_buf()));
        }
    }
    let generation = store_dir(root).join("generations").join(
        record
            .get("generation")
            .and_then(Value::as_str)
            .unwrap_or(""),
    );
    if generation.is_dir() {
        return Ok(Some(generation));
    }
    Ok(None)
}

/// The draft lane's dirty check: a dirty or conflicting draft would be silently
/// discarded by a restore, so it is refused. Rust keeps no `edit.md` projection
/// for a plain workdir, so a missing/unreadable sidecar counts as not-dirty.
fn draft_is_dirty(root: &Path) -> bool {
    match docx2typed_core::edit_state::classify_edit_state(root) {
        Ok(state) => matches!(state.state.as_str(), "dirty" | "conflict"),
        Err(_) => false,
    }
}

struct ScratchDir(PathBuf);

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Python `history_restore`: restore an earlier version by copying its state
/// FORWARD into a new version (ADR 0039 — history never rewinds). The restore
/// is a normal mutation, so the caller's next `commit_sync` sees a consistent
/// workspace; the new version records `restored_from` and its content.
///
/// `paragraphs = Some([...])` is the guarded selective form (ADR 0045): it
/// moves only dependency-free, zero-token paragraphs, refusing coupling with
/// `partial-restore-needs-dependent-state`. Refused when the version is unknown
/// (`version-not-found`), trimmed (`version-trimmed`), a recorded hash does not
/// match (`version-content-missing`), the draft has unsaved edits
/// (`restore-draft-dirty`), or the selection is empty
/// (`restore-empty-selection`).
pub fn history_restore(
    root: &Path,
    version: &str,
    paragraphs: Option<&[String]>,
) -> Result<Value, StoreError> {
    let root = resolve_path(root);
    let selection: Option<Vec<String>> = match paragraphs {
        Some(list) => {
            if list.is_empty() {
                return Err(StoreError::store(
                    "restore-empty-selection",
                    "paragraphs must name at least one paragraph; omit it to restore the whole \
                     version",
                ));
            }
            let unique: BTreeSet<String> = list.iter().cloned().collect();
            Some(unique.into_iter().collect())
        }
        None => None,
    };

    let chain = version_chain(&root)?;
    let record = chain
        .iter()
        .find(|record| record.get("version").and_then(Value::as_str) == Some(version))
        .cloned()
        .ok_or_else(|| version_not_found(version))?;
    if trimmed_versions(&root)?.contains(version) {
        return Err(StoreError::store(
            "version-trimmed",
            format!(
                "{version} was trimmed by retention (its content is gone on purpose); restore a \
                 version that is still retained"
            ),
        ));
    }

    let scratch = std::env::temp_dir().join(format!(
        "docx2typed-version-{}-{}",
        std::process::id(),
        new_operation_id()
    ));
    let _scratch = ScratchDir(scratch.clone());
    let Some(source) = version_state_dir(&root, &record, &scratch)? else {
        return Err(StoreError::store(
            "version-trimmed",
            format!(
                "{version} content has been trimmed by retention and can no longer be restored"
            ),
        ));
    };

    // Verify the restored state against the hashes recorded with the version.
    let recorded = recorded_assets(&source);
    for name in CANONICAL_ASSETS {
        let origin = source.join(name);
        if !origin.is_file() {
            return Err(StoreError::store(
                "version-content-missing",
                format!("{version} has no recorded {name}"),
            ));
        }
        if let Some(expected) = recorded.get(name) {
            let actual = file_sha256(&origin).map_err(StoreError::Io)?;
            if &actual != expected {
                return Err(StoreError::store(
                    "version-content-missing",
                    format!(
                        "{version} content for {name} does not match the hash recorded with it"
                    ),
                ));
            }
        }
    }

    if draft_is_dirty(&root) {
        return Err(StoreError::store(
            "restore-draft-dirty",
            "the draft has unsaved edits; commit_sync or revert them before restoring, otherwise \
             the restore would silently discard them",
        ));
    }

    let head_tree_before = head_version(&root)
        .get("tree")
        .cloned()
        .unwrap_or(Value::Null);
    let before_typed = fs::read_to_string(read_root(&root).join("typed.md")).ok();
    let version_owned = version.to_string();
    let closure_root = root.clone();
    let closure_source = source.clone();
    let closure_selection = selection.clone();
    let closure_head_tree = head_tree_before.clone();
    let closure_before_typed = before_typed.clone();

    let run = Box::new(move |target: &Path, _tx: &mut crate::Transaction| {
        for name in CANONICAL_ASSETS {
            fs::copy(closure_source.join(name), target.join(name)).map_err(StoreError::Io)?;
        }
        let mut picked: Vec<String> = Vec::new();
        if let Some(selection) = &closure_selection {
            // selective restore: start from the CURRENT state and move only the
            // dependency-free paragraphs out of the version
            let current_root = read_root(&closure_root);
            fs::copy(current_root.join("typed.md"), target.join("typed.md"))
                .map_err(StoreError::Io)?;
            fs::copy(current_root.join("format.json"), target.join("format.json"))
                .map_err(StoreError::Io)?;
            if let Some(refusal) = cherry_pick_guard(&closure_source, target, selection) {
                return Err(StoreError::store(
                    "partial-restore-needs-dependent-state",
                    refusal,
                ));
            }
            picked = cherry_pick(&closure_source, target, selection)?;
        }
        let after_typed = fs::read_to_string(target.join("typed.md")).ok();
        let changed = changed_paragraph_texts(
            closure_before_typed.as_deref(),
            after_typed.as_deref().unwrap_or(""),
        );
        let digest = crate::objects::canonical_tree_digest(target)?;
        let unchanged = closure_head_tree.as_str() == Some(digest.as_str());
        let data = json!({
            "restored_from": version_owned,
            "cherry_picked": if picked.is_empty() { Value::Null } else { json!(picked) },
            "changed_paragraph_ids": changed,
            "noop": unchanged,
            "state": "clean",
        });
        Ok(RunOutcome {
            outcome: "success".to_string(),
            data,
            kind: "mutation".to_string(),
            payload: json!({
                "checks": [{"name": "history-restore", "status": "pass", "restored_from": version_owned}],
            }),
            diagnostics: vec![],
        })
    });

    let operation_id = new_operation_id();
    let canonical = canonical_operation_input(
        "history_restore",
        &json!({
            "workdir": root.to_string_lossy(),
            "version": version,
            "paragraphs": selection,
        }),
    );
    let store = Store::ensure(&root, &operation_id, &canonical)?;
    let pin = store.pin()?;
    let selective = selection.is_some();
    let boundary = SaveBoundary {
        origin: if selective { "cherry-pick" } else { "restore" }.to_string(),
        label: Some(if selective {
            format!("selective restore {version}")
        } else {
            format!("restore {version}")
        }),
        pin: false,
        baseline_epoch: None,
        restored_from: Some(version.to_string()),
    };
    let request = StoreMutateRequest {
        workdir: root.clone(),
        operation: "history_restore".to_string(),
        operation_id,
        canonical,
        input_sha256: pin.manifest_sha256.clone().unwrap_or_default(),
        expected_generation: pin.generation.clone(),
        generation: true,
        save_boundary: true,
        ledger_anchor: None,
        ledger_directory: true,
        evidence_path: None,
        kind: "mutation".to_string(),
        lock_timeout_ms: 0,
        run,
    };
    let envelope = store.mutate_with_boundary(request, Some(boundary))?;
    Ok(envelope.get("data").cloned().unwrap_or(Value::Null))
}
