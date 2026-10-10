//! Read-only workdir inspection and readiness classification (issue #56
//! slice), mirroring `scripts/inspect_migrate.py` (`inventory_assets` /
//! `inventory_sha256` / `inspect_workdir`) for schema-1 workdirs.
//!
//! Inspect never writes to the source: no locks, no state files, no
//! evidence. The inventory walk is link-safe — symlinks and junctions
//! (reparse points) are reported and never dereferenced or hashed through.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use docx2typed_protocol::{resolve_path, semantic_sha256};
use serde_json::{json, Value};

use crate::edit_state::{classify_edit_state, EditState};
use crate::CoreError;

pub const WORKDIR_MANIFEST_SCHEMA: &str = "docx2typed-workdir-manifest-1";
pub const MANIFEST_VERSION: i64 = 1;
pub const MANIFEST_FILE: &str = "workdir.manifest.json";
pub const REVIEW_DIR: &str = ".review";

/// (name, role, read_only): the authoritative schema-1 asset set.
pub const AUTHORITATIVE_ASSETS: [(&str, &str, bool); 4] = [
    ("typed.md", "typed-ast", false),
    ("format.json", "format", false),
    ("styles.json", "styles", false),
    ("_template.docx", "template-baseline", true),
];

/// (name, role): optional assets the engine understands. Presence is
/// preserved exactly; absence is not an error.
///
/// Mirrors the Python Reference table exactly (deep-equal inspect parity).
/// `islands.json` — the Rust-only island-edit sidecar — is deliberately NOT
/// listed: it is engine-private, the frozen reference has no such asset, and
/// both engines report it as an ordinary opaque attachment. Listing it here
/// would drift the inspect payload away from the reference.
pub const OPTIONAL_ASSETS: [(&str, &str); 10] = [
    ("edit.md", "edit-projection"),
    ("edit.state.json", "edit-state"),
    ("edit.state.json.run.json", "edit-evidence"),
    ("revisions.json", "revisions"),
    ("revisions.md", "revisions-view"),
    ("regions.md", "regions"),
    ("decisions.json", "decisions"),
    ("run.evidence.json", "run-evidence"),
    ("operation-ledger.json", "operation-ledger"),
    ("workdir.manifest.json", "workdir-manifest"),
];

/// Private immutable-generation store metadata (issue #50): never part of
/// the ordinary manifest/asset closure, never migrated as an attachment.
const STORE_METADATA: [&str; 2] = [".docx2typed-store", "workdir.json"];

/// Stable inspect reason codes. Blocking ones double as Protocol
/// diagnostic codes; informational ones never flip readiness.
pub const BLOCKING_REASONS: [&str; 5] = [
    "asset-closure",
    "schema-incompatible",
    "required-feature-unsupported",
    "source-drift",
    "symlink-detected",
];

/// Windows `FILE_ATTRIBUTE_REPARSE_POINT` (symlink or junction).
/// Only referenced on Windows; allow the lint elsewhere (CI runs clippy
/// with -D warnings on Linux too).
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
const REPARSE_POINT: u32 = 0x400;

/// One asset-table row (Python `_file_asset` shape).
#[derive(Clone, Debug, PartialEq)]
pub struct AssetEntry {
    pub path: String,
    pub kind: String, // authoritative | optional | opaque
    pub required: bool,
    pub read_only: bool,
    pub role: String,
    pub presence: String, // present | missing
    pub bytes: i64,       // 0 when missing
    pub sha256: Option<String>,
    pub mtime_ns: Option<i64>,
}

impl AssetEntry {
    /// Python `_file_asset` JSON (same keys).
    pub fn to_json(&self) -> Value {
        json!({
            "path": self.path,
            "kind": self.kind,
            "required": self.required,
            "read_only": self.read_only,
            "role": self.role,
            "presence": self.presence,
            "bytes": self.bytes,
            "sha256": self.sha256,
            "mtime_ns": self.mtime_ns,
        })
    }
}

/// Readiness classification payload (Python `inspect_workdir` shape).
#[derive(Clone, Debug)]
pub struct Inspection {
    pub readiness: String, // ready | blocked
    pub next_action: String,
    pub reason_codes: Vec<String>,
    pub symlinks: Vec<String>,
    pub edit_state: Value, // {state, typed_sha256, edit_body_sha256} (+detail)
    pub template_drift: bool,
    pub styles_drift: bool,
    pub revision_count: Option<i64>,
    pub comment_count: Option<i64>,
    pub opaque_attachment_count: usize,
    pub assets: Vec<AssetEntry>,
    pub baseline: Value,
    pub source_snapshot: Value,
    /// The full `data` payload of the inspect Result envelope.
    pub payload: Value,
}

/// True for every link-like entry: POSIX symlinks plus Windows symlinks
/// AND junctions (reparse points). Never dereferences the path.
pub fn is_link(path: &Path) -> bool {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(_) => return false,
    };
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & REPARSE_POINT != 0 {
            return true;
        }
    }
    meta.file_type().is_symlink()
}

/// Every regular file under `top`, never following or dereferencing links
/// (linked directories are not descended into; linked files are skipped).
pub fn walk_files(top: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![top.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if is_link(&path) {
                continue;
            }
            let meta = match fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(_) => continue,
            };
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// Relative paths (POSIX separators) of every link anywhere under `root`
/// (including `.review` and opaque subtrees), sorted. Never dereferences.
pub fn symlink_paths(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if is_link(&path) {
                if let Ok(rel) = path.strip_prefix(root) {
                    found.push(rel.to_string_lossy().replace('\\', "/"));
                }
            } else if path.is_dir() {
                stack.push(path);
            }
        }
    }
    found.sort();
    found
}

fn known_top_level(name: &str) -> bool {
    AUTHORITATIVE_ASSETS.iter().any(|(n, _, _)| *n == name)
        || OPTIONAL_ASSETS.iter().any(|(n, _)| *n == name)
        || name == REVIEW_DIR
        || STORE_METADATA.contains(&name)
}

fn file_asset(
    rel: &str,
    path: &Path,
    kind: &str,
    role: &str,
    required: bool,
    read_only: bool,
) -> Result<AssetEntry, CoreError> {
    if path.is_file() {
        let stat = fs::metadata(path).map_err(CoreError::io)?;
        let mtime_ns = stat
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos() as i64);
        return Ok(AssetEntry {
            path: rel.to_string(),
            kind: kind.to_string(),
            required,
            read_only,
            role: role.to_string(),
            presence: "present".to_string(),
            bytes: stat.len() as i64,
            sha256: Some(docx2typed_protocol::file_sha256(path).map_err(CoreError::io)?),
            mtime_ns,
        });
    }
    Ok(AssetEntry {
        path: rel.to_string(),
        kind: kind.to_string(),
        required,
        read_only,
        role: role.to_string(),
        presence: "missing".to_string(),
        bytes: 0,
        sha256: None,
        mtime_ns: None,
    })
}

/// Content digest of one directory asset, keyed by source-relative paths
/// so same-basename descendants under different opaque subtrees never
/// collide (`_dir_digest`).
pub fn dir_digest(root: &Path, files: &[PathBuf]) -> Result<String, CoreError> {
    let mut map = BTreeMap::new();
    for path in files {
        let rel = path
            .strip_prefix(root)
            .map_err(|_| CoreError::Io(std::io::Error::other("outside root")))?
            .to_string_lossy()
            .replace('\\', "/");
        let hash = docx2typed_protocol::file_sha256(path).map_err(CoreError::io)?;
        map.insert(rel, hash);
    }
    Ok(semantic_sha256(
        &serde_json::to_value(map).expect("map serializes"),
    ))
}

/// Stable asset table for every file under `root` (Python
/// `inventory_assets`): authoritative always listed (missing when absent),
/// optional + opaque when present, `.review` files as optional review
/// state. Links are skipped (never dereferenced).
pub fn inventory_assets(root: &Path) -> Result<Vec<AssetEntry>, CoreError> {
    let mut assets = Vec::new();
    for (name, role, read_only) in AUTHORITATIVE_ASSETS {
        let path = root.join(name);
        if is_link(&path) {
            continue; // rejected by inspect; never dereference the link
        }
        assets.push(file_asset(
            name,
            &path,
            "authoritative",
            role,
            true,
            read_only,
        )?);
    }
    for (name, role) in OPTIONAL_ASSETS {
        let path = root.join(name);
        if is_link(&path) {
            continue;
        }
        assets.push(file_asset(
            name,
            &path,
            "optional",
            role,
            false,
            name == "workdir.manifest.json",
        )?);
    }
    let review = root.join(REVIEW_DIR);
    if review.is_dir() && !is_link(&review) {
        for path in walk_files(&review) {
            let rel = path
                .strip_prefix(root)
                .map_err(|_| CoreError::Io(std::io::Error::other("outside root")))?
                .to_string_lossy()
                .replace('\\', "/");
            let lock = path
                .file_name()
                .map(|name| name.to_string_lossy().ends_with(".lock"))
                .unwrap_or(false);
            assets.push(file_asset(
                &rel,
                &path,
                "optional",
                if lock {
                    "review-lock"
                } else {
                    "review-session"
                },
                false,
                lock,
            )?);
        }
    }
    let mut top: Vec<PathBuf> = match fs::read_dir(root) {
        Ok(entries) => entries.flatten().map(|entry| entry.path()).collect(),
        Err(error) => return Err(CoreError::io(error)),
    };
    top.sort();
    for path in top {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if known_top_level(&name) {
            continue;
        }
        if is_link(&path) {
            continue; // rejected by inspect; never dereference the link
        }
        if path.is_dir() {
            let files = walk_files(&path);
            let bytes: i64 = files
                .iter()
                .filter_map(|file| fs::metadata(file).ok())
                .map(|meta| meta.len() as i64)
                .sum();
            assets.push(AssetEntry {
                path: format!("{name}/"),
                kind: "opaque".to_string(),
                required: false,
                read_only: true,
                role: "attachment".to_string(),
                presence: "present".to_string(),
                bytes,
                sha256: Some(dir_digest(root, &files)?),
                mtime_ns: None,
            });
        } else if path.is_file() {
            assets.push(file_asset(
                &name,
                &path,
                "opaque",
                "attachment",
                false,
                true,
            )?);
        }
    }
    Ok(assets)
}

/// Content identity over every present asset (paths + hashes), including
/// Opaque attachments; stable across retries (`inventory_sha256`).
pub fn inventory_sha256(root: &Path) -> Result<String, CoreError> {
    let mut items = Vec::new();
    for asset in inventory_assets(root)? {
        if asset.presence == "present" {
            items.push(json!({ "path": asset.path, "sha256": asset.sha256 }));
        }
    }
    Ok(semantic_sha256(&Value::Array(items)))
}

/// Parsed `format.json` dict, or None when missing/unreadable/invalid
/// (`_format_data`).
pub fn format_data(root: &Path) -> Option<Value> {
    let path = root.join("format.json");
    if !path.is_file() || is_link(&path) {
        return None;
    }
    let bytes = fs::read(&path).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    if value.is_object() {
        Some(value)
    } else {
        None
    }
}

/// Read-only readiness classification of one schema-1 workdir (Python
/// `inspect_workdir`). Returns Io errors for unreadable sources; the
/// payload is the exact inspect `data` payload.
pub fn inspect_workdir(path: &Path) -> Result<Inspection, CoreError> {
    let root = resolve_path(path);
    let assets = inventory_assets(&root)?;
    let mut present = BTreeMap::new();
    for asset in &assets {
        if asset.presence == "present" {
            present.insert(asset.path.clone(), asset);
        }
    }
    let mut reasons: Vec<String> = Vec::new();

    // Symlinks anywhere under the source block migration with a stable
    // reason; the link itself is never dereferenced by the walk.
    let symlinks = symlink_paths(&root);
    if !symlinks.is_empty() {
        reasons.push("symlink-detected".to_string());
    }

    let missing: Vec<&str> = AUTHORITATIVE_ASSETS
        .iter()
        .filter(|(name, _, _)| !present.contains_key(*name))
        .map(|(name, _, _)| *name)
        .collect();
    if !missing.is_empty() {
        reasons.push("asset-closure".to_string());
    }

    let format_data = format_data(&root);
    let schema_ok = format_data.as_ref().is_some_and(|data| {
        data.get("schema").and_then(Value::as_str) == Some("typed-format-1")
            && data.get("model_version").and_then(Value::as_i64) == Some(1)
            && data.get("canonicalizer_version").and_then(Value::as_i64) == Some(1)
    });
    if !schema_ok {
        reasons.push("schema-incompatible".to_string());
    }

    if let Some(declared) = format_data
        .as_ref()
        .and_then(|data| data.get("required_features"))
    {
        let valid = declared
            .as_array()
            .is_some_and(|list| list.iter().all(|item| item.is_string()));
        if !valid {
            reasons.push("required-feature-unsupported".to_string());
        } else {
            let required: Vec<&str> = docx2typed_protocol::REQUIRED_FEATURES.to_vec();
            let unknown: Vec<String> = declared
                .as_array()
                .map(|list| {
                    list.iter()
                        .filter_map(Value::as_str)
                        .filter(|name| !required.contains(name))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            if !unknown.is_empty() {
                reasons.push("required-feature-unsupported".to_string());
            }
        }
    }

    // Drift hashing consults the symlink-safe inventory: a symlinked
    // template/styles is rejected (never hashed through the link).
    let mut drift: Vec<String> = Vec::new();
    let template = root.join("_template.docx");
    let styles = root.join("styles.json");
    if let Some(data) = &format_data {
        if present.contains_key("_template.docx") {
            if let Some(recorded) = data.get("template_sha256").and_then(Value::as_str) {
                let actual = docx2typed_protocol::file_sha256(&template).map_err(CoreError::io)?;
                if actual != recorded {
                    drift.push("template".to_string());
                }
            }
        }
        if present.contains_key("styles.json") {
            if let Some(recorded) = data.get("styles_sha256").and_then(Value::as_str) {
                let actual = docx2typed_protocol::file_sha256(&styles).map_err(CoreError::io)?;
                if actual != recorded {
                    drift.push("styles".to_string());
                }
            }
        }
    }
    if !drift.is_empty() {
        reasons.push("source-drift".to_string());
    }

    // Edit-state classification must never read through a link.
    let edit_state: Value = if symlinks.is_empty() {
        match classify_edit_state(&root) {
            Ok(EditState {
                state,
                typed_sha256,
                edit_body_sha256,
            }) => json!({
                "state": state,
                "typed_sha256": typed_sha256,
                "edit_body_sha256": edit_body_sha256,
            }),
            Err(error) => {
                if root.join("edit.state.json").exists() {
                    json!({ "state": "incompatible", "detail": error.to_string() })
                } else {
                    json!({ "state": "missing", "detail": error.to_string() })
                }
            }
        }
    } else {
        json!({ "state": "missing" })
    };
    match edit_state.get("state").and_then(Value::as_str) {
        Some("missing") => reasons.push("edit-state-missing".to_string()),
        Some(state @ ("dirty" | "stale-clean" | "conflict")) => {
            let _ = state;
            reasons.push("non-clean-edit".to_string());
        }
        _ => {}
    }

    let opaque_assets: Vec<&AssetEntry> = assets
        .iter()
        .filter(|asset| asset.kind == "opaque")
        .collect();
    if !opaque_assets.is_empty() {
        reasons.push("opaque-attachment".to_string());
    }

    let revision_count: Option<i64> = {
        let path = root.join("revisions.json");
        if path.is_file() && !is_link(&path) {
            match fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            {
                Some(value) if value.is_object() => match value.get("revisions") {
                    Some(Value::Array(list)) => Some(list.len() as i64),
                    Some(Value::String(text)) => Some(text.chars().count() as i64),
                    _ => Some(0), // `get("revisions", []) or []` — falsy => []
                },
                _ => None,
            }
        } else {
            None
        }
    };

    let comment_count: Option<i64> = format_data.as_ref().map(|data| {
        data.get("paragraphs")
            .and_then(Value::as_array)
            .map(|records| {
                records
                    .iter()
                    .filter(|record| {
                        record.get("part_key").and_then(Value::as_str) == Some("comments")
                    })
                    .count() as i64
            })
            .unwrap_or(0)
    });

    let blocking: Vec<String> = reasons
        .iter()
        .filter(|reason| BLOCKING_REASONS.contains(&reason.as_str()))
        .cloned()
        .collect();
    let readiness = if blocking.is_empty() {
        "ready"
    } else {
        "blocked"
    };
    if reasons.is_empty() {
        reasons.push("ok".to_string());
    }

    let baseline: Value = match &format_data {
        Some(data) => json!({
            "template": data.get("template"),
            "template_sha256": data.get("template_sha256"),
            "package_manifest": data.get("package_manifest"),
            "source": data.get("source"),
            "source_sha256": data.get("source_sha256"),
            "styles_sha256": data.get("styles_sha256"),
            "document_xml_sha256": data.get("document_xml_sha256"),
            "source_track_enabled": data.get("source_track_enabled"),
            "uses_date_utc": data.get("uses_date_utc"),
        }),
        None => json!({}),
    };

    let present_files: Vec<&AssetEntry> = assets
        .iter()
        .filter(|asset| asset.presence == "present")
        .collect();
    let source_snapshot = json!({
        "files": present_files.len(),
        "bytes": present_files.iter().map(|asset| asset.bytes).sum::<i64>(),
    });

    let payload = json!({
        "readiness": readiness,
        "workdir": docx2typed_protocol::typed_path_value(&root),
        "next_action": if readiness == "ready" { "migrate" } else { "none" },
        "reason_codes": reasons,
        "symlinks": symlinks,
        "semantic_state": {
            "edit": edit_state,
            "template_drift": drift.iter().any(|item| item == "template"),
            "styles_drift": drift.iter().any(|item| item == "styles"),
            "revision_count": revision_count,
            "comment_count": comment_count,
            "opaque_attachment_count": opaque_assets.len(),
        },
        "assets": assets.iter().map(AssetEntry::to_json).collect::<Vec<Value>>(),
        "features": {
            "supported": docx2typed_protocol::FEATURES,
            "required": docx2typed_protocol::REQUIRED_FEATURES,
        },
        "baseline": baseline,
        "source_snapshot": source_snapshot,
    });

    Ok(Inspection {
        readiness: readiness.to_string(),
        next_action: if readiness == "ready" {
            "migrate"
        } else {
            "none"
        }
        .to_string(),
        reason_codes: reasons,
        symlinks,
        edit_state,
        template_drift: drift.iter().any(|item| item == "template"),
        styles_drift: drift.iter().any(|item| item == "styles"),
        revision_count,
        comment_count,
        opaque_attachment_count: opaque_assets.len(),
        assets,
        baseline,
        source_snapshot,
        payload,
    })
}

/// First blocking reason code in frozen order; `workdir-invalid` when none
/// (`_blocking_reason`).
pub fn blocking_reason(reason_codes: &[String]) -> &'static str {
    for code in BLOCKING_REASONS {
        if reason_codes.iter().any(|reason| reason == code) {
            return code;
        }
    }
    "workdir-invalid"
}
