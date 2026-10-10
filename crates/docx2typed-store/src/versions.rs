//! Version timeline (`docx2typed-history-1`) — the read-only half of the
//! Python Reference's `store.head_version` / `store.history_list` /
//! `store.history_diff` / `store.history_blame` / `store.history_verify`
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

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use docx2typed_protocol::{bytes_sha256, file_sha256, semantic_sha256};
use serde_json::{json, Value};

use crate::store::{POINTER_FILE, POINTER_SCHEMA, STORE_DIR_NAME};
use crate::StoreError;

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
