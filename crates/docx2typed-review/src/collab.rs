//! Document/session collaboration state machine — mirror of
//! `scripts/review_collab.py` (issue #50/#51/#60).
//!
//! Session state lives in `<workdir>/.review/session.json`
//! (`docx2typed-review-session-1`), append-only history in
//! `.review/history.jsonl`, persisted renderable snapshots in
//! `.review/snapshots/C<n>.json`, and the event queue in
//! `.review/inbox` (see `queue`).
//!
//! Rust canonical note: the canonical artifact in this fork is
//! `_template.docx` (island/governed edits commit generations against it);
//! `typed.md` is the typed record the session snapshots pin. Settlement
//! applies governed revision decisions to the template inside one store
//! generation mutation; the C snapshot id does not advance because
//! `typed.md` itself does not change (the store generation is the CAS
//! identity). `current_matches_filesystem` still compares the session's
//! pinned `typed_sha256` against the live file, mirroring Python.

use std::fs;
use std::path::{Path, PathBuf};

use docx2typed_core::govern;
use docx2typed_protocol::{bytes_sha256, file_sha256, new_operation_id, utc_now_iso};
use serde_json::{json, Value};

use crate::queue;

pub const COLLAB_DIR: &str = ".review";
pub const SESSION_FILE: &str = "session.json";
pub const HISTORY_FILE: &str = "history.jsonl";
pub const COLLAB_SCHEMA: &str = "docx2typed-review-session-1";
pub const SNAPSHOT_DIR: &str = "snapshots";
pub const SNAPSHOT_SCHEMA: &str = "docx2typed-review-snapshot-1";
pub const PATCH_SCHEMA: &str = "docx2typed-document-patch-1";

/// A fail-closed collaboration contract violation.
#[derive(Clone, Debug)]
pub struct CollaborationError {
    pub code: String,
    pub detail: String,
}

impl CollaborationError {
    pub fn new(code: impl Into<String>, detail: impl Into<String>) -> Self {
        CollaborationError {
            code: code.into(),
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for CollaborationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for CollaborationError {}

impl From<String> for CollaborationError {
    fn from(detail: String) -> Self {
        CollaborationError::new("workdir-unreadable", detail)
    }
}

fn now() -> String {
    utc_now_iso()
}

fn root(workdir: &Path) -> PathBuf {
    let path = workdir.join(COLLAB_DIR);
    let _ = fs::create_dir_all(&path);
    path
}

fn session_path(workdir: &Path) -> PathBuf {
    root(workdir).join(SESSION_FILE)
}

fn snapshot_root(workdir: &Path) -> PathBuf {
    let path = root(workdir).join(SNAPSHOT_DIR);
    let _ = fs::create_dir_all(&path);
    path
}

fn snapshot_path(workdir: &Path, snapshot_id: &str) -> Result<PathBuf, CollaborationError> {
    if snapshot_id.is_empty() || !snapshot_id.starts_with('C') {
        return Err(CollaborationError::new(
            "invalid-snapshot-id",
            "invalid snapshot id",
        ));
    }
    Ok(snapshot_root(workdir).join(format!("{snapshot_id}.json")))
}

fn sha256_file(path: &Path) -> Result<String, std::io::Error> {
    file_sha256(path)
}

fn sha256_json(value: &Value) -> String {
    let mut bytes = serde_json::to_vec(value).expect("value serializes");
    bytes.push(b'\n');
    bytes_sha256(&bytes)
}

fn atomic_json(path: &Path, value: &Value) -> Result<(), String> {
    let parent = path.parent().ok_or("session path has no parent")?;
    let temp = parent.join(format!(
        ".{}-{}.tmp",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "session".to_string()),
        &new_operation_id()[..8]
    ));
    let result = (|| -> Result<(), String> {
        let mut bytes = serde_json::to_vec(value).map_err(|error| error.to_string())?;
        bytes.push(b'\n');
        fs::write(&temp, &bytes).map_err(|error| error.to_string())?;
        fs::rename(&temp, path).map_err(|error| error.to_string())?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Append one generation-bound history record (see `crate::history`): the
/// record carries the store generation this mutation runs against, so the
/// trail is traceable across generations that share one C snapshot.
fn append_history(workdir: &Path, event: &Value) {
    let (generation, manifest) = crate::history::generation_binding(workdir);
    crate::history::append(workdir, event, generation.as_deref(), manifest.as_deref());
}

fn snapshot_id(prefix: &str, number: usize) -> String {
    format!("{prefix}{number}")
}

fn empty_staged(current: &Value) -> Value {
    let id = current
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("C0")
        .to_string();
    json!({
        "id": format!("H{}", &id[1..]),
        "parent_snapshot": id,
        "base_snapshot": id,
        "patch_ids": [],
        "patch_chain_sha256": sha256_json(&json!([])),
    })
}

/// The hash of the baseline a session belongs to, when the workdir has one.
///
/// `_template.docx` is the one file only `extract` replaces, so it is the only
/// honest anchor for "which document is this session about". The typed
/// projection's hash cannot serve: it advances on every commit, and
/// `publish_current` gates on it HAVING advanced, so treating a difference as
/// staleness would make that gate a tautology and refuse every commit with
/// `current-not-changed`.
fn baseline_sha256(workdir: &Path) -> Option<String> {
    sha256_file(&workdir.join("_template.docx")).ok()
}

/// A session reopened on a document that replaced the one it knew.
fn rebase_session(typed_sha256: String, baseline_sha256: &str) -> Value {
    let mut fresh = new_session(typed_sha256);
    fresh["baseline_sha256"] = json!(baseline_sha256);
    fresh
}

fn new_session(typed_sha256: String) -> Value {
    let now = now();
    let current = json!({
        "id": snapshot_id("C", 0),
        "typed_sha256": typed_sha256,
        "parent_snapshot": null,
        "origin": "session-bootstrap",
        "changed_paragraph_ids": [],
        "batch_id": null,
        "published_at": now,
    });
    json!({
        "schema": COLLAB_SCHEMA,
        "review_base": {
            "id": "S0",
            "typed_sha256": typed_sha256,
            "parent_snapshot": null,
            "origin": "session-bootstrap",
            "created_at": now,
        },
        "current_snapshot": current,
        "staged_snapshot": empty_staged(&current),
        "writer": {"state": "idle", "batch_id": null},
    })
}

fn read_session(workdir: &Path) -> Option<Value> {
    let bytes = fs::read(session_path(workdir)).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    if value.is_object() {
        Some(value)
    } else {
        None
    }
}

fn validate_session(value: &Value) -> Result<Value, CollaborationError> {
    if value.get("schema").and_then(Value::as_str) != Some(COLLAB_SCHEMA) {
        return Err(CollaborationError::new(
            "session-schema",
            "unexpected collaboration session schema",
        ));
    }
    for key in [
        "review_base",
        "current_snapshot",
        "staged_snapshot",
        "writer",
    ] {
        if value.get(key).and_then(Value::as_object).is_none() {
            return Err(CollaborationError::new(
                "session-schema",
                format!("session is missing {key}"),
            ));
        }
    }
    Ok(value.clone())
}

/// Read or create the session (mutation path: creates session, its C0
/// snapshot, and the first history record).
pub fn ensure_session(workdir: &Path) -> Result<Value, CollaborationError> {
    let typed_path = workdir.join("typed.md");
    if !typed_path.is_file() {
        return Err(CollaborationError::new(
            "workdir-not-found",
            format!("typed.md not found in {}", workdir.to_string_lossy()),
        ));
    }
    let typed_sha256 = sha256_file(&typed_path)
        .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))?;
    let state = match read_session(workdir) {
        Some(state) => {
            let state = validate_session(&state)?;
            let current_baseline = baseline_sha256(workdir);
            match (
                state.get("baseline_sha256").and_then(Value::as_str),
                current_baseline.as_deref(),
            ) {
                // The workdir's baseline is not the one this session was
                // bootstrapped against: extraction replaced it (ADR 0048). The
                // session then describes a document that is not here any more,
                // and it used to be reused verbatim — measured: after a reset it
                // still reported the old origin and a changed paragraph the new
                // document does not have, and the next edit was refused with
                // `agent-preflight-required`, leaving the workdir uneditable
                // with nothing explaining why. Re-bootstrap, and say so.
                (Some(bound), Some(current)) if bound != current => {
                    let rebased = rebase_session(typed_sha256.clone(), current);
                    atomic_json(&session_path(workdir), &rebased)
                        .map_err(|error| CollaborationError::new("workdir-unreadable", error))?;
                    persist_snapshot(workdir, &rebased["current_snapshot"])
                        .map_err(|error| CollaborationError::new("workdir-unreadable", error))?;
                    append_history(
                        workdir,
                        &json!({
                            "event": "session-rebound",
                            "previous_baseline_sha256": bound,
                            "baseline_sha256": current,
                        }),
                    );
                    return Ok(rebased);
                }
                // A session recorded before this anchor existed: stamp it and
                // keep its chain, so the migration costs nothing and only a real
                // replacement rebinds.
                (None, Some(current)) => {
                    let mut stamped = state.clone();
                    stamped["baseline_sha256"] = json!(current);
                    atomic_json(&session_path(workdir), &stamped)
                        .map_err(|error| CollaborationError::new("workdir-unreadable", error))?;
                    stamped
                }
                _ => state,
            }
        }
        None => {
            // Anchor a fresh session immediately, so the very first commit does
            // not have to stamp it on a later touch.
            let state = match baseline_sha256(workdir) {
                Some(current) => rebase_session(typed_sha256, &current),
                None => new_session(typed_sha256),
            };
            atomic_json(&session_path(workdir), &state)
                .map_err(|error| CollaborationError::new("workdir-unreadable", error))?;
            persist_snapshot(workdir, &state["current_snapshot"])
                .map_err(|error| CollaborationError::new("workdir-unreadable", error))?;
            append_history(
                workdir,
                &json!({
                    "event": "session-created",
                    "snapshot": state["current_snapshot"],
                }),
            );
            return Ok(state);
        }
    };
    let current_id = state
        .get("current_snapshot")
        .and_then(|snapshot| snapshot.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if !snapshot_path(workdir, current_id)
        .map(|path| path.is_file())
        .unwrap_or(false)
    {
        persist_snapshot(workdir, &state["current_snapshot"])
            .map_err(|error| CollaborationError::new("workdir-unreadable", error))?;
    }
    Ok(state)
}
fn persist_snapshot(workdir: &Path, snapshot: &Value) -> Result<(), String> {
    let snapshot_id = snapshot.get("id").and_then(Value::as_str).unwrap_or("");
    if snapshot_id.is_empty() {
        return Err("snapshot id is required".to_string());
    }
    let path = snapshot_path(workdir, snapshot_id).map_err(|error| error.to_string())?;
    atomic_json(
        &path,
        &json!({
            "schema": SNAPSHOT_SCHEMA,
            "snapshot": snapshot,
        }),
    )
}

/// Document/session state (mutation path: creates the session if absent).
pub fn document_state(workdir: &Path) -> Result<Value, CollaborationError> {
    let state = ensure_session(workdir)?;
    let actual = sha256_file(&workdir.join("typed.md"))
        .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))?;
    let mut result = state;
    result["filesystem_typed_sha256"] = json!(actual);
    let current_hash = result
        .get("current_snapshot")
        .and_then(|snapshot| snapshot.get("typed_sha256"))
        .and_then(Value::as_str)
        .unwrap_or("");
    result["current_matches_filesystem"] = json!(actual == current_hash);
    Ok(result)
}

/// Document/session state without any side effect.
pub fn document_state_readonly(workdir: &Path) -> Value {
    let state = read_session(workdir);
    let state = match state {
        Some(state) => match validate_session(&state) {
            Ok(state) => state,
            Err(_) => {
                return json!({
                    "schema": COLLAB_SCHEMA,
                    "review_base": null,
                    "current_snapshot": null,
                    "staged_snapshot": null,
                    "writer": {"state": "idle", "batch_id": null},
                    "filesystem_typed_sha256": null,
                    "current_matches_filesystem": false,
                })
            }
        },
        None => {
            let typed_sha256 = sha256_file(&workdir.join("typed.md")).ok();
            return json!({
                "schema": COLLAB_SCHEMA,
                "review_base": null,
                "current_snapshot": null,
                "staged_snapshot": null,
                "writer": {"state": "idle", "batch_id": null},
                "filesystem_typed_sha256": typed_sha256,
                "current_matches_filesystem": false,
            });
        }
    };
    let actual = sha256_file(&workdir.join("typed.md")).ok();
    let mut result = state;
    result["filesystem_typed_sha256"] = match actual {
        Some(ref hash) => json!(hash),
        None => Value::Null,
    };
    let current_hash = result
        .get("current_snapshot")
        .and_then(|snapshot| snapshot.get("typed_sha256"))
        .and_then(Value::as_str)
        .unwrap_or("");
    result["current_matches_filesystem"] = json!(actual.as_deref() == Some(current_hash));
    result
}

/// Raise the frozen agent gate when the workdir is not ready for an agent
/// write (mirror of Python `_agent_preflight`): current-snapshot-drift or
/// queued human patches block.
pub fn ensure_agent_ready(workdir: &Path) -> Result<(), CollaborationError> {
    let result = preflight(workdir);
    if result.get("ready").and_then(Value::as_bool) != Some(true) {
        let reasons = result.get("reasons").cloned().unwrap_or_default();
        let queued = result.get("queued_events").cloned().unwrap_or_default();
        return Err(CollaborationError::new(
            "agent-preflight-required",
            serde_json::to_string(&json!({
                "reasons": reasons,
                "queued_events": queued,
            }))
            .expect("serializes"),
        ));
    }
    Ok(())
}

/// The gate an agent must pass before changing the canonical AST.
pub fn preflight(workdir: &Path) -> Value {
    let state = match document_state(workdir) {
        Ok(state) => state,
        Err(error) => {
            return json!({
                "ready": false,
                "reasons": [error.code],
                "current_snapshot": null,
                "review_base": null,
                "staged_snapshot": null,
                "queued_events": [],
                "blocked_patches": [],
            })
        }
    };
    let events = queue::snapshot(workdir);
    let events = events
        .get("events")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let queued: Vec<Value> = events
        .iter()
        .filter(|event| {
            event.get("status").and_then(Value::as_str) == Some("queued")
                && !matches!(
                    event.get("delivery_state").and_then(Value::as_str),
                    Some("in_progress" | "applied" | "acknowledged")
                )
        })
        .cloned()
        .collect();
    let blocked_patches: Vec<Value> = queued
        .iter()
        .filter(|event| event.get("type").and_then(Value::as_str) == Some("patch"))
        .cloned()
        .collect();
    let mut reasons = Vec::new();
    if state
        .get("current_matches_filesystem")
        .and_then(Value::as_bool)
        != Some(true)
    {
        reasons.push("current-snapshot-drift".to_string());
    }
    if !blocked_patches.is_empty() {
        reasons.push("queued-human-patch".to_string());
    }
    json!({
        "ready": reasons.is_empty(),
        "reasons": reasons,
        "current_snapshot": state.get("current_snapshot"),
        "review_base": state.get("review_base"),
        "staged_snapshot": state.get("staged_snapshot"),
        "queued_events": queued,
        "blocked_patches": blocked_patches,
    })
}

/// Partition one queued batch into canonical decisions and human patches.
pub fn settlement_plan(workdir: &Path, event_ids: Option<&[String]>) -> Value {
    let state = document_state_readonly(workdir);
    let wanted: Vec<String> = event_ids.map(|ids| ids.to_vec()).unwrap_or_default();
    let events = queue::snapshot_readonly(workdir);
    let events = events
        .get("events")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let selected: Vec<Value> = events
        .iter()
        .filter(|event| {
            let id = event.get("event_id").and_then(Value::as_str).unwrap_or("");
            (wanted.is_empty() || wanted.iter().any(|wanted| wanted == id))
                && matches!(
                    event.get("status").and_then(Value::as_str),
                    Some("queued" | "acknowledged")
                )
        })
        .cloned()
        .collect();
    let decisions: Vec<Value> = selected
        .iter()
        .filter(|event| event.get("type").and_then(Value::as_str) == Some("decision"))
        .cloned()
        .collect();
    let patches: Vec<Value> = selected
        .iter()
        .filter(|event| event.get("type").and_then(Value::as_str) == Some("patch"))
        .cloned()
        .collect();
    let current_id = state
        .get("current_snapshot")
        .and_then(|snapshot| snapshot.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let carry_forward: Vec<Value> = selected
        .iter()
        .filter(|event| {
            event.get("review_decision").and_then(Value::as_str) == Some("defer")
                || (event.get("type").and_then(Value::as_str) == Some("patch")
                    && event.get("delivery_state").and_then(Value::as_str) != Some("applied")
                    && event.get("parent_snapshot").and_then(Value::as_str) != Some(current_id))
        })
        .cloned()
        .collect();
    let ready_for_agent_write = carry_forward.is_empty()
        && state
            .get("current_matches_filesystem")
            .and_then(Value::as_bool)
            == Some(true);
    json!({
        "schema": "docx2typed-review-settlement-1",
        "review_base": state.get("review_base"),
        "current_snapshot": state.get("current_snapshot"),
        "staged_snapshot": state.get("staged_snapshot"),
        "decisions": decisions,
        "patches": patches,
        "carry_forward": carry_forward,
        "ready_for_agent_write": ready_for_agent_write,
    })
}

/// Validate an import/rollback caller before it touches the workdir.
pub fn external_write_guard(
    workdir: &Path,
    expected_parent_snapshot: &str,
    operation: &str,
) -> Result<Value, CollaborationError> {
    if !matches!(operation, "import" | "rollback") {
        return Err(CollaborationError::new(
            "external-operation",
            "operation must be import or rollback",
        ));
    }
    let state = document_state(workdir)?;
    let current = state.get("current_snapshot").cloned().unwrap_or_default();
    let current_id = current.get("id").and_then(Value::as_str).unwrap_or("");
    if current_id != expected_parent_snapshot {
        return Err(CollaborationError::new(
            "current-parent-mismatch",
            format!("expected {expected_parent_snapshot}, current is {current_id}"),
        ));
    }
    if state
        .get("current_matches_filesystem")
        .and_then(Value::as_bool)
        != Some(true)
    {
        return Err(CollaborationError::new(
            "current-snapshot-drift",
            "typed.md differs from the canonical snapshot",
        ));
    }
    Ok(json!({
        "schema": "docx2typed-review-external-guard-1",
        "operation": operation,
        "expected_parent_snapshot": current_id,
        "typed_sha256": current.get("typed_sha256"),
        "issued_at": now(),
    }))
}

fn bounded_text(
    value: Option<&Value>,
    name: &str,
    limit: usize,
) -> Result<String, CollaborationError> {
    let text = value.and_then(Value::as_str).unwrap_or("").to_string();
    if text.len() > limit {
        return Err(CollaborationError::new(
            "patch-too-large",
            format!("{name} exceeds {limit} characters"),
        ));
    }
    Ok(text)
}

/// Validate + normalize a collaboration patch event.
pub fn validate_patch(patch: &Value) -> Result<Value, CollaborationError> {
    if patch.get("type").and_then(Value::as_str) != Some("patch") {
        return Err(CollaborationError::new(
            "patch-type",
            "collaboration event must have type=patch",
        ));
    }
    let mut normalized = patch.clone();
    // `generation` is server-authoritative (the store generation the event
    // is written against); a client-supplied value is never trusted.
    normalized.as_object_mut().unwrap().remove("generation");
    normalized["schema"] = json!(PATCH_SCHEMA);
    let event_id = patch
        .get("event_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(new_operation_id);
    normalized["event_id"] = json!(event_id);
    let client_id = bounded_text(patch.get("client_id"), "client_id", 200)?;
    normalized["client_id"] = json!(if client_id.is_empty() {
        event_id.clone()
    } else {
        client_id
    });
    let origin = bounded_text(patch.get("origin"), "origin", 32)?;
    if !matches!(origin.as_str(), "human_ui" | "human_external" | "agent") {
        return Err(CollaborationError::new(
            "patch-origin",
            "origin must be human_ui, human_external, or agent",
        ));
    }
    normalized["origin"] = json!(origin);
    normalized["author"] = json!(bounded_text(patch.get("author"), "author", 200)?);
    let parent_snapshot = bounded_text(patch.get("parent_snapshot"), "parent_snapshot", 120)?;
    let paragraph_id = bounded_text(patch.get("paragraph_id"), "paragraph_id", 120)?;
    if parent_snapshot.is_empty() || paragraph_id.is_empty() {
        return Err(CollaborationError::new(
            "patch-target",
            "parent_snapshot and paragraph_id are required",
        ));
    }
    normalized["parent_snapshot"] = json!(parent_snapshot);
    normalized["paragraph_id"] = json!(paragraph_id);
    if patch
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("replace")
        != "replace"
    {
        return Err(CollaborationError::new(
            "patch-kind",
            "only semantic text replacement patches are supported",
        ));
    }
    let target = patch
        .get("target")
        .and_then(Value::as_object)
        .ok_or_else(|| CollaborationError::new("patch-target", "target must be an object"))?;
    let start = target.get("start_offset").and_then(Value::as_u64);
    let end = target.get("end_offset").and_then(Value::as_u64);
    let (Some(start), Some(end)) = (start, end) else {
        return Err(CollaborationError::new(
            "patch-range",
            "target offsets must be an ordered non-negative range",
        ));
    };
    if start > end {
        return Err(CollaborationError::new(
            "patch-range",
            "target offsets must be an ordered non-negative range",
        ));
    }
    let before = bounded_text(patch.get("before"), "before", 8_000)?;
    let after = bounded_text(patch.get("after"), "after", 8_000)?;
    let expected = bounded_text(target.get("expected_text"), "target.expected_text", 8_000)?;
    if before != expected {
        return Err(CollaborationError::new(
            "patch-precondition",
            "before must equal target.expected_text",
        ));
    }
    if start == end && !before.is_empty() {
        return Err(CollaborationError::new(
            "patch-range",
            "insertions must use an empty before text",
        ));
    }
    if start != end && before.is_empty() {
        return Err(CollaborationError::new(
            "patch-range",
            "deletions or replacements require non-empty before text",
        ));
    }
    normalized["kind"] = json!("replace");
    normalized["target"] = json!({
        "start_offset": start,
        "end_offset": end,
        "expected_text": expected,
        "left_context": bounded_text(target.get("left_context"), "target.left_context", 2_000)?,
        "right_context": bounded_text(target.get("right_context"), "target.right_context", 2_000)?,
        "paragraph_fingerprint": bounded_text(target.get("paragraph_fingerprint"), "target.paragraph_fingerprint", 200)?,
        "region_fingerprint": bounded_text(target.get("region_fingerprint"), "target.region_fingerprint", 200)?,
        "style_region_ids": target.get("style_region_ids").cloned().unwrap_or_else(|| json!([])),
    });
    normalized["before"] = json!(before);
    normalized["after"] = json!(after);
    let empty_review_item = json!("");
    let review_item_id = bounded_text(
        patch.get("review_item_id").or(Some(&empty_review_item)),
        "review_item_id",
        200,
    )?;
    normalized["review_item_id"] = json!(if review_item_id.is_empty() {
        format!("review-{event_id}")
    } else {
        review_item_id
    });
    normalized["delivery_state"] = json!("staged");
    normalized["review_decision"] = json!("pending");
    Ok(normalized)
}

/// Recompute the document fingerprints for one patch against the canonical
/// Core projection of the document it targets (fail-closed, item 7): at
/// least paragraph identity, the Unicode-scalar range, `expected_text`, the
/// paragraph fingerprint, and the region fingerprint of the covering style
/// region must match (recomputed via the Core fingerprint functions over
/// the projection of the generation snapshot the mutation targets). Runs
/// before any apply/stage work.
///
/// `style_region_ids` is never silently accepted as "validated" while
/// actually deferred: the projection contract addresses style coverage by
/// (paragraph_id, start, end) — there are no stable region ids — so a
/// non-empty list is rejected loudly instead of being ignored.
pub fn verify_patch_fingerprints(workdir: &Path, patch: &Value) -> Result<(), CollaborationError> {
    let paragraph_id = patch
        .get("paragraph_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let target = patch.get("target").cloned().unwrap_or_default();
    let start = target
        .get("start_offset")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let end = target
        .get("end_offset")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let expected = target
        .get("expected_text")
        .and_then(Value::as_str)
        .unwrap_or("");
    let claimed_paragraph_fp = target
        .get("paragraph_fingerprint")
        .and_then(Value::as_str)
        .unwrap_or("");
    let claimed_region_fp = target
        .get("region_fingerprint")
        .and_then(Value::as_str)
        .unwrap_or("");
    match target.get("style_region_ids") {
        None => {}
        Some(Value::Array(ids)) if ids.is_empty() => {}
        Some(Value::Array(_)) => {
            return Err(CollaborationError::new(
                "style-region-unsupported",
                "style_region_ids is not part of the projection contract; coverage is validated by region_fingerprint over (paragraph_id, start, end)",
            ));
        }
        Some(_) => {
            return Err(CollaborationError::new(
                "invalid-arguments",
                "style_region_ids must be a string array",
            ));
        }
    }
    crate::frame::verify_document_fingerprints(
        workdir,
        paragraph_id,
        start,
        end,
        expected,
        claimed_paragraph_fp,
        claimed_region_fp,
    )
    .map_err(|(code, detail)| CollaborationError::new(code, detail))
}

/// Stage one human patch as a collaboration event with session coordinates.
pub fn stage_patch(workdir: &Path, patch: &Value) -> Result<Value, CollaborationError> {
    let state = ensure_session(workdir)?;
    let normalized = validate_patch(patch)?;
    // Core fingerprint gate: recompute paragraph/region fingerprints over
    // the projection of the generation this patch targets before any
    // queue/session write (fail-closed, never check-then-act deferred).
    verify_patch_fingerprints(workdir, &normalized)?;
    let staged = state.get("staged_snapshot").cloned().unwrap_or_default();
    let staged_id = staged
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let has_patches = staged
        .get("patch_ids")
        .and_then(Value::as_array)
        .map(|ids| !ids.is_empty())
        .unwrap_or(false);
    let current_id = state
        .get("current_snapshot")
        .and_then(|snapshot| snapshot.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let expected_parent = if has_patches {
        staged_id.clone()
    } else {
        current_id.clone()
    };
    let parent = normalized
        .get("parent_snapshot")
        .and_then(Value::as_str)
        .unwrap_or("");
    if parent != expected_parent {
        return Err(CollaborationError::new(
            "staged-parent-mismatch",
            format!("patch parent {parent} does not match {expected_parent}"),
        ));
    }
    let record = queue::upsert_event(workdir, &normalized)
        .map_err(|error| CollaborationError::new("workdir-unreadable", error))?;
    let event_id = record.get("event_id").and_then(Value::as_str).unwrap_or("");
    let mut patch_ids = staged
        .get("patch_ids")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    patch_ids.push(json!(event_id));
    let current_base = state
        .get("current_snapshot")
        .and_then(|snapshot| snapshot.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let staged_snapshot = format!("H{}", &current_base[1..]);
    let staged_snapshot = format!("{staged_snapshot}.{}", patch_ids.len());
    let mut state = state;
    state["staged_snapshot"] = json!({
        "id": staged_snapshot,
        "parent_snapshot": expected_parent,
        "base_snapshot": current_id,
        "patch_ids": patch_ids,
        "patch_chain_sha256": sha256_json(&json!(patch_ids)),
    });
    queue::update_event(
        workdir,
        event_id,
        &json!({
            "staged_parent_snapshot": expected_parent,
            "staged_snapshot": staged_snapshot,
            "review_item_id": record.get("review_item_id"),
        }),
    )
    .map_err(|error| CollaborationError::new("workdir-unreadable", error))?;
    atomic_json(&session_path(workdir), &state)
        .map_err(|error| CollaborationError::new("workdir-unreadable", error))?;
    append_history(
        workdir,
        &json!({
            "event": "patch-staged",
            "event_id": event_id,
            "review_item_id": record.get("review_item_id"),
            "parent_snapshot": expected_parent,
            "staged_snapshot": staged_snapshot,
            "origin": record.get("origin"),
        }),
    );
    Ok(record)
}

/// Publish a human canonical change as a new C snapshot (CAS: the expected
/// parent must be the current snapshot id and the live typed.md must differ
/// from the pinned hash).
pub fn publish_current(
    workdir: &Path,
    expected_parent_snapshot: &str,
    origin: &str,
    changed_paragraph_ids: &[String],
    batch_id: Option<&str>,
) -> Result<Value, CollaborationError> {
    let mut state = ensure_session(workdir)?;
    let current = state.get("current_snapshot").cloned().unwrap_or_default();
    let current_id = current.get("id").and_then(Value::as_str).unwrap_or("");
    if current_id != expected_parent_snapshot {
        return Err(CollaborationError::new(
            "current-parent-mismatch",
            format!("expected {expected_parent_snapshot}, current is {current_id}"),
        ));
    }
    let actual_hash = sha256_file(&workdir.join("typed.md"))
        .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))?;
    let pinned = current
        .get("typed_sha256")
        .and_then(Value::as_str)
        .unwrap_or("");
    if actual_hash == pinned {
        return Err(CollaborationError::new(
            "current-not-changed",
            "canonical typed.md has not changed",
        ));
    }
    let new_number: usize = current_id[1..].parse().unwrap_or(0) + 1;
    let published_at = now();
    let mut changed = changed_paragraph_ids.to_vec();
    changed.sort();
    changed.dedup();
    let new_current = json!({
        "id": snapshot_id("C", new_number),
        "typed_sha256": actual_hash,
        "parent_snapshot": current_id,
        "origin": origin,
        "changed_paragraph_ids": changed,
        "batch_id": batch_id,
        "published_at": published_at,
    });
    state["current_snapshot"] = new_current.clone();
    state["staged_snapshot"] = empty_staged(&new_current);
    state["writer"] = json!({"state": "idle", "batch_id": null});
    atomic_json(&session_path(workdir), &state)
        .map_err(|error| CollaborationError::new("workdir-unreadable", error))?;
    append_history(
        workdir,
        &json!({
            "event": "current-published",
            "previous_snapshot": current_id,
            "current_snapshot": new_current,
            "batch_id": batch_id,
        }),
    );
    persist_snapshot(workdir, &new_current)
        .map_err(|error| CollaborationError::new("workdir-unreadable", error))?;
    Ok(json!({
        "previous_snapshot": current_id,
        "current_snapshot": new_current,
    }))
}

/// Parse `<part>|<kind>|<w_id>|<fingerprint>`.
pub fn parse_revision_key(revision_key: &str) -> Option<(String, String, String, String)> {
    let parts: Vec<&str> = revision_key.split('|').collect();
    if parts.len() != 4 {
        return None;
    }
    Some((
        parts[0].to_string(),
        parts[1].to_string(),
        parts[2].to_string(),
        parts[3].to_string(),
    ))
}

/// The frozen diagnostic carried by a core settle failure.
fn settle_code(message: &str) -> String {
    let candidate = message.split(':').next().unwrap_or("").trim().to_string();
    if !candidate.is_empty()
        && candidate
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch == '-')
    {
        candidate
    } else {
        "workdir-invalid".to_string()
    }
}

/// Atomically settle native revision decisions and carry deferred items.
///
/// Runs against one generation snapshot (`workdir`): governed decisions are
/// applied to `_template.docx` via `govern::settle_one_revision`, the
/// regenerated format record and a fresh `decisions.json` are written, the
/// review base advances, and events are marked applied with their settled
/// snapshot. Deferred events carry forward.
pub fn settle_decisions(
    workdir: &Path,
    event_ids: Option<&[String]>,
) -> Result<Value, CollaborationError> {
    let state = document_state(workdir)?;
    if state
        .get("current_matches_filesystem")
        .and_then(Value::as_bool)
        != Some(true)
    {
        return Err(CollaborationError::new(
            "current-snapshot-drift",
            "typed.md differs from the canonical snapshot",
        ));
    }
    // Draft freshness: missing edit state means no draft (Rust-extracted
    // workdirs) and counts as clean; a present dirty draft blocks settlement.
    if workdir.join("edit.state.json").is_file() {
        match docx2typed_core::edit_state::classify_edit_state(workdir) {
            Ok(edit_state) if edit_state.state == "clean" => {}
            Ok(_) => {
                return Err(CollaborationError::new(
                    "draft-not-clean",
                    "settlement requires a clean canonical workdir",
                ))
            }
            Err(_) => {
                return Err(CollaborationError::new(
                    "draft-not-clean",
                    "settlement requires a clean canonical workdir",
                ))
            }
        }
    }
    let wanted: Vec<String> = event_ids.map(|ids| ids.to_vec()).unwrap_or_default();
    let events = queue::list_events(workdir);
    let selected: Vec<Value> = events
        .iter()
        .filter(|event| {
            let id = event.get("event_id").and_then(Value::as_str).unwrap_or("");
            (wanted.is_empty() || wanted.iter().any(|wanted| wanted == id))
                && matches!(
                    event.get("status").and_then(Value::as_str),
                    Some("queued" | "acknowledged")
                )
                && event.get("type").and_then(Value::as_str) == Some("decision")
                && !matches!(
                    event.get("delivery_state").and_then(Value::as_str),
                    Some("applied" | "acknowledged")
                )
        })
        .cloned()
        .collect();
    let actionable: Vec<Value> = selected
        .iter()
        .filter(|event| {
            matches!(
                event.get("review_decision").and_then(Value::as_str),
                Some("accept" | "reject" | "defer")
            )
        })
        .cloned()
        .collect();
    if actionable.is_empty() {
        let settled: Vec<String> = events
            .iter()
            .filter(|event| {
                let id = event.get("event_id").and_then(Value::as_str).unwrap_or("");
                wanted.iter().any(|wanted| wanted == id) && event.get("settled_snapshot").is_some()
            })
            .filter_map(|event| {
                event
                    .get("event_id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect();
        if !settled.is_empty() {
            let current = document_state(workdir)?
                .get("current_snapshot")
                .cloned()
                .unwrap_or_default();
            return Ok(json!({
                "schema": "docx2typed-review-settlement-1",
                "state": "already-settled",
                "review_base": document_state(workdir)?.get("review_base"),
                "current_snapshot": current,
                "settled_event_ids": settled,
            }));
        }
        return Err(CollaborationError::new(
            "settlement-empty",
            "no accept, reject, or defer decisions are ready",
        ));
    }

    let template_path = workdir.join("_template.docx");
    let package = fs::read(&template_path)
        .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))?;
    let document_xml = govern::document_xml_bytes(&package)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    let mut decisions = Vec::new();
    let mut changed_ids = Vec::new();
    let mut deferred = Vec::new();
    let mut seen_wids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut current_xml = document_xml;
    let current_package = package;
    for event in &actionable {
        let action = event
            .get("review_decision")
            .and_then(Value::as_str)
            .unwrap_or("");
        if action == "defer" {
            deferred.push(event.clone());
            continue;
        }
        let revision_key = event
            .get("revision_key")
            .and_then(Value::as_str)
            .unwrap_or("");
        let (part, kind, w_id, fingerprint) =
            parse_revision_key(revision_key).ok_or_else(|| {
                CollaborationError::new(
                    "malformed-revision-key",
                    format!("malformed revision key: {revision_key}"),
                )
            })?;
        if part != "word/document.xml" {
            return Err(CollaborationError::new(
                "revision-outside-editable-surface",
                format!("{revision_key} can only be viewed"),
            ));
        }
        if seen_wids.contains(&w_id) {
            return Err(CollaborationError::new(
                "duplicate-decision",
                format!("revision {w_id} has more than one settlement decision"),
            ));
        }
        seen_wids.insert(w_id.clone());
        let settled = govern::settle_one_revision(&current_xml, &w_id, action)
            .map_err(|(_code, message)| CollaborationError::new(settle_code(&message), message))?;
        if settled.revision.kind != kind {
            return Err(CollaborationError::new(
                "workdir-invalid",
                format!(
                    "revision kind mismatch: key says {kind}, node is {}",
                    settled.revision.kind
                ),
            ));
        }
        if settled.revision.fingerprint() != fingerprint {
            return Err(CollaborationError::new(
                "revision-text-fingerprint-mismatch",
                format!(
                    "revision-text-fingerprint-mismatch: expected {fingerprint}, got {}",
                    settled.revision.fingerprint()
                ),
            ));
        }
        let decision = json!({
            "w_id": settled.revision.w_id,
            "kind": settled.revision.kind,
            "action": action,
            "fingerprint": settled.revision.fingerprint(),
            "paragraph_id": settled.revision.paragraph_id,
            "operation": if (action == "accept"
                && (settled.revision.kind == "insert" || settled.revision.kind == "move_to"))
                || (action == "reject"
                    && (settled.revision.kind == "delete" || settled.revision.kind == "move_from"))
            {
                "unwrap"
            } else {
                "remove"
            },
            "review_item_id": event.get("review_item_id"),
            "event_id": event.get("event_id"),
        });
        decisions.push(decision);
        changed_ids.push(settled.revision.paragraph_id.clone());
        current_xml = settled.part_xml;
    }
    if decisions.is_empty() {
        // Only deferred events: nothing to apply.
    }
    let new_package = govern::patch_document_xml(&current_package, &current_xml)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    fs::write(&template_path, &new_package)
        .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))?;
    let format_path = workdir.join("format.json");
    let format: Value = serde_json::from_slice(
        &fs::read(&format_path)
            .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))?,
    )
    .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    let regenerated = docx2typed_core::regenerate_workdir_format(&format, &new_package);
    let mut format_bytes = serde_json::to_vec_pretty(&regenerated).expect("format serializes");
    format_bytes.push(b'\n');
    fs::write(&format_path, format_bytes)
        .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))?;
    let decisions_json = json!({
        "schema": "typed-decisions-1",
        "action": "settle",
        "decisions": decisions,
    });
    let mut decisions_bytes =
        serde_json::to_vec_pretty(&decisions_json).expect("decisions serializes");
    decisions_bytes.push(b'\n');
    fs::write(workdir.join("decisions.json"), decisions_bytes)
        .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))?;

    let mut state = ensure_session(workdir)?;
    let previous_base = state.get("review_base").cloned().unwrap_or_default();
    let base_number: usize = previous_base
        .get("id")
        .and_then(Value::as_str)
        .and_then(|id| id.strip_prefix('S'))
        .and_then(|id| id.parse().ok())
        .unwrap_or(0);
    let current_snapshot = state.get("current_snapshot").cloned().unwrap_or_default();
    let next_base = json!({
        "id": format!("S{}", base_number + 1),
        "typed_sha256": current_snapshot.get("typed_sha256"),
        "parent_snapshot": previous_base.get("id"),
        "origin": "settlement",
        "created_at": now(),
    });
    state["review_base"] = next_base.clone();
    atomic_json(&session_path(workdir), &state)
        .map_err(|error| CollaborationError::new("workdir-unreadable", error))?;
    let snapshot_id = current_snapshot
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    for event in &actionable {
        let event_id = event.get("event_id").and_then(Value::as_str).unwrap_or("");
        let is_deferred = event.get("review_decision").and_then(Value::as_str) == Some("defer");
        queue::update_event(
            workdir,
            event_id,
            &json!({
                "delivery_state": "applied",
                "settled_snapshot": snapshot_id,
                "carry_forward": is_deferred,
            }),
        )
        .map_err(|error| CollaborationError::new("workdir-unreadable", error))?;
    }
    append_history(
        workdir,
        &json!({
            "event": "settlement-completed",
            "previous_review_base": previous_base,
            "review_base": next_base,
            "current_snapshot": current_snapshot,
            "decisions": decisions,
            "deferred_event_ids": deferred.iter().filter_map(|event| event.get("event_id").and_then(Value::as_str)).collect::<Vec<_>>(),
        }),
    );
    Ok(json!({
        "schema": "docx2typed-review-settlement-1",
        "review_base": next_base,
        "current_snapshot": current_snapshot,
        "decisions": decisions,
        "deferred": deferred,
        "carry_forward": deferred,
        "settled_event_ids": actionable.iter().filter_map(|event| event.get("event_id").and_then(Value::as_str)).collect::<Vec<_>>(),
    }))
}

/// Minimal edit.md patch application: locate the paragraph block, prove the
/// target range still holds the expected text, replace it, write the draft
/// back, and mark the event applied. Style-region bookkeeping is a declared
/// deferral (no regions.md rewrite in this tracer).
pub fn apply_patch_to_draft(workdir: &Path, event: &Value) -> Result<Value, CollaborationError> {
    let paragraph_id = event
        .get("paragraph_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    apply_patch_to_draft_untracked(workdir, event)?;
    let event_id = event.get("event_id").and_then(Value::as_str).unwrap_or("");
    let updated = queue::update_event(
        workdir,
        event_id,
        &json!({
            "delivery_state": "applied",
            "status": "acknowledged",
            "applied_at": now(),
        }),
    )
    .map_err(|error| CollaborationError::new("workdir-unreadable", error))?;
    append_history(
        workdir,
        &json!({
            "event": "patch-applied",
            "event_id": event_id,
            "paragraph_id": paragraph_id,
        }),
    );
    Ok(updated)
}

/// Apply one semantic patch to the draft without touching the event record
/// (used by the batch lane, which owns event states itself — mirror of
/// Python `_apply_patch_to_draft(validate=False)`).
pub fn apply_patch_to_draft_untracked(
    workdir: &Path,
    event: &Value,
) -> Result<(), CollaborationError> {
    let paragraph_id = event
        .get("paragraph_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let target = event.get("target").cloned().unwrap_or_default();
    let start = target
        .get("start_offset")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let end = target
        .get("end_offset")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let expected = target
        .get("expected_text")
        .and_then(Value::as_str)
        .unwrap_or("");
    let after = event.get("after").and_then(Value::as_str).unwrap_or("");
    let path = workdir.join("edit.md");
    let text = fs::read_to_string(&path)
        .map_err(|error| CollaborationError::new("draft-unreadable", error.to_string()))?;
    let mut blocks = paragraph_blocks(&text)?;
    let index = find_block(&blocks, "p", paragraph_id).ok_or_else(|| {
        CollaborationError::new(
            "paragraph-not-found",
            format!("paragraph {paragraph_id} not found in the draft"),
        )
    })?;
    let marker = blocks[index].split('\n').next().unwrap_or("").to_string();
    let body = block_body(&blocks[index]);
    // Patch offsets are CHAR offsets (the browser console and the Python
    // mirror index strings by character); convert to byte offsets before
    // slicing so CJK bodies never panic and always compare correctly.
    let chars: Vec<(usize, usize)> = body
        .char_indices()
        .map(|(position, ch)| (position, position + ch.len_utf8()))
        .collect();
    let byte_start = chars
        .get(start)
        .map(|&(position, _)| position)
        .unwrap_or(body.len());
    // Exclusive char-end: the byte offset of char `end` (body[start..end]
    // in Python characters) — mirror of the Python `body[start:end]` slice.
    let byte_end = chars
        .get(end)
        .map(|&(position, _)| position)
        .unwrap_or(body.len());
    if byte_start > byte_end || &body[byte_start..byte_end] != expected {
        return Err(CollaborationError::new(
            "patch-precondition",
            "target text no longer matches the draft; re-read the document and restage the patch",
        ));
    }
    let mut new_body = String::with_capacity(body.len() - (byte_end - byte_start) + after.len());
    new_body.push_str(&body[..byte_start]);
    new_body.push_str(after);
    new_body.push_str(&body[byte_end..]);
    let replacement = if new_body.is_empty() {
        marker
    } else {
        format!("{marker}\n{new_body}")
    };
    blocks[index] = replacement;
    // Preserve the @edit header: `paragraph_blocks` returns only the
    // blocks, and a batch applies several patches in sequence, so the
    // header must survive every intermediate write.
    let header = text
        .lines()
        .find(|line| line.trim_start().starts_with("<!--@edit"))
        .unwrap_or("")
        .to_string();
    let mut out = String::new();
    out.push_str(&header);
    out.push('\n');
    for (i, block) in blocks.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(block);
    }
    fs::write(&path, out)
        .map_err(|error| CollaborationError::new("draft-unreadable", error.to_string()))?;
    Ok(())
}

/// Split an edit.md body into (header-line, paragraph blocks), mirroring
/// `scripts/mcp_server.py` `_paragraph_blocks`: a block starts at a
/// `<!--@p` / `<!--@new` / `<!--@delete` marker line; blank lines are
/// skipped; the header must be an `@edit` comment line.
pub fn paragraph_blocks(text: &str) -> Result<Vec<String>, CollaborationError> {
    let lines: Vec<&str> = text.lines().collect();
    let mut index = 0;
    while index < lines.len() && lines[index].trim().is_empty() {
        index += 1;
    }
    if index >= lines.len() || !lines[index].trim_start().starts_with("<!--@edit") {
        return Err(CollaborationError::new(
            "edit-header-missing",
            "edit.md must start with an @edit header",
        ));
    }
    let mut blocks: Vec<String> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for line in &lines[index + 1..] {
        let stripped = line.trim();
        if stripped.starts_with("<!--@p")
            || stripped.starts_with("<!--@new")
            || stripped.starts_with("<!--@delete")
        {
            if !current.is_empty() {
                blocks.push(current.join("\n"));
                current.clear();
            }
            current.push(line);
        } else if !stripped.is_empty() {
            current.push(line);
        }
    }
    if !current.is_empty() {
        blocks.push(current.join("\n"));
    }
    Ok(blocks)
}

/// Index of the block whose marker line is `<!--@<prefix> id="<id>"`, or
/// None (Python `_find_block`).
pub fn find_block(blocks: &[String], prefix: &str, paragraph_id: &str) -> Option<usize> {
    let expected = format!("<!--@{prefix} id=\"{paragraph_id}\"");
    blocks.iter().position(|block| block.starts_with(&expected))
}

/// Body of one block: every line after the marker line (Python
/// `_block_body`).
pub fn block_body(block: &str) -> String {
    match block.split_once('\n') {
        Some((_, body)) => body.to_string(),
        None => String::new(),
    }
}

/// Apply one queued human patch batch as one canonical transaction (mirror
/// of Python `_review_apply_batch`): claim in_progress, apply each patch to
/// the draft (descending start offsets per paragraph), commit the draft to
/// typed.md, publish one CAS snapshot, mark the batch applied. On any
/// failure the draft is discarded and the claimed events are requeued with
/// the failure recorded.
pub fn review_apply_batch(
    workdir: &Path,
    batch_id: Option<&str>,
    requested_event_id: Option<&str>,
) -> Result<Value, CollaborationError> {
    let events = queue::snapshot(workdir);
    let events = events
        .get("events")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let requested = match requested_event_id {
        Some(id) => events
            .iter()
            .find(|event| event.get("event_id").and_then(Value::as_str) == Some(id))
            .cloned(),
        None => None,
    };
    if requested_event_id.is_some() && requested.is_none() {
        return Err(CollaborationError::new(
            "review-event-not-found",
            format!(
                "review event {} not found",
                requested_event_id.unwrap_or("")
            ),
        ));
    }
    if let Some(event) = &requested {
        if event.get("type").and_then(Value::as_str) != Some("patch") {
            return Err(CollaborationError::new(
                "not-a-patch",
                format!(
                    "review event {} is not a semantic patch",
                    event.get("event_id").and_then(Value::as_str).unwrap_or("")
                ),
            ));
        }
        if event.get("delivery_state").and_then(Value::as_str) == Some("applied") {
            return Ok(json!({ "event": event, "state": "already-applied" }));
        }
    }
    let mut batch_events: Vec<Value> = events
        .iter()
        .filter(|event| {
            event.get("type").and_then(Value::as_str) == Some("patch")
                && event.get("status").and_then(Value::as_str) == Some("queued")
                && (batch_id.is_none() || event.get("batch_id").and_then(Value::as_str) == batch_id)
        })
        .cloned()
        .collect();
    batch_events.sort_by_key(|event| {
        let staged = event
            .get("staged_snapshot")
            .and_then(Value::as_str)
            .unwrap_or("H0.0");
        staged
            .rsplit('.')
            .next()
            .and_then(|tail| tail.parse::<i64>().ok())
            .unwrap_or(0)
    });
    if let Some(requested) = &requested {
        if !batch_events.iter().any(|event| event == requested) {
            return Err(CollaborationError::new(
                "patch-not-queued",
                format!(
                    "review event {} is not queued",
                    requested
                        .get("event_id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                ),
            ));
        }
    }
    if batch_events.is_empty() {
        return Err(CollaborationError::new(
            "patch-batch-empty",
            format!(
                "no queued patches in batch {}",
                batch_id.unwrap_or("<none>")
            ),
        ));
    }
    let state = document_state(workdir)?;
    if state
        .get("current_matches_filesystem")
        .and_then(Value::as_bool)
        != Some(true)
    {
        return Err(CollaborationError::new(
            "current-snapshot-drift",
            "typed.md differs from the canonical snapshot",
        ));
    }
    // Rust-extracted workdirs initialize the draft projection lazily;
    // the human patch lane materializes it before applying (Python
    // workdirs always carry edit.md).
    crate::draft::ensure_projection(workdir)?;
    let current_id = state
        .get("current_snapshot")
        .and_then(|snapshot| snapshot.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut expected_parent = current_id.clone();
    for patch in &batch_events {
        let parent = patch
            .get("parent_snapshot")
            .and_then(Value::as_str)
            .unwrap_or("");
        if parent != expected_parent {
            return Err(CollaborationError::new(
                "patch-parent-mismatch",
                format!("patch parent {parent} does not match {expected_parent}"),
            ));
        }
        expected_parent = patch
            .get("staged_snapshot")
            .and_then(Value::as_str)
            .unwrap_or(&expected_parent)
            .to_string();
    }
    // Overlap guard per paragraph (Python `patch-overlap`).
    let mut ranges: std::collections::BTreeMap<String, Vec<(i64, i64)>> =
        std::collections::BTreeMap::new();
    for patch in &batch_events {
        let paragraph_id = patch
            .get("paragraph_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let target = patch.get("target").cloned().unwrap_or_default();
        let start = target
            .get("start_offset")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let end = target
            .get("end_offset")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        ranges.entry(paragraph_id).or_default().push((start, end));
    }
    for (paragraph_id, paragraph_ranges) in &ranges {
        let mut sorted = paragraph_ranges.clone();
        sorted.sort();
        let mut previous_end = -1i64;
        let mut previous_start = -1i64;
        for (start, end) in sorted {
            if start < previous_end || (start == previous_start && start == end) {
                return Err(CollaborationError::new(
                    "patch-overlap",
                    format!("{paragraph_id}: overlapping patches require a new selection"),
                ));
            }
            previous_start = start;
            previous_end = end;
        }
    }
    let claimed: Vec<String> = batch_events
        .iter()
        .filter_map(|patch| {
            patch
                .get("event_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    let result = (|| -> Result<Value, CollaborationError> {
        for event_id in &claimed {
            queue::update_event(
                workdir,
                event_id,
                &json!({ "delivery_state": "in_progress" }),
            )
            .map_err(|error| CollaborationError::new("workdir-unreadable", error))?;
        }
        let mut ordered = batch_events.clone();
        ordered.sort_by(|left, right| {
            let left_id = left
                .get("paragraph_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            let right_id = right
                .get("paragraph_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            let left_start = left
                .get("target")
                .and_then(|target| target.get("start_offset"))
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let right_start = right
                .get("target")
                .and_then(|target| target.get("start_offset"))
                .and_then(Value::as_i64)
                .unwrap_or(0);
            left_id.cmp(right_id).then(right_start.cmp(&left_start))
        });
        for patch in &ordered {
            apply_patch_to_draft_untracked(workdir, patch)?;
        }
        let changed = crate::draft::apply_projection(workdir)?;
        if changed.is_empty() {
            return Err(CollaborationError::new(
                "patch-noop",
                "human patch batch produced no canonical change",
            ));
        }
        let published = publish_current(workdir, &current_id, "human_ui", &changed, batch_id)?;
        let applied_snapshot = published
            .get("current_snapshot")
            .and_then(|snapshot| snapshot.get("id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let mut updated: Vec<Value> = Vec::new();
        for patch in &batch_events {
            let event_id = patch.get("event_id").and_then(Value::as_str).unwrap_or("");
            let record = queue::update_event(
                workdir,
                event_id,
                &json!({
                    "delivery_state": "applied",
                    "review_decision": "adjusted",
                    "applied_snapshot": applied_snapshot,
                }),
            )
            .map_err(|error| CollaborationError::new("workdir-unreadable", error))?;
            updated.push(record);
        }
        let mut result = json!({
            "events": updated,
            "commit": published,
            "state": "applied",
        });
        if let Some(requested) = &requested {
            let event_id = requested
                .get("event_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            if let Some(record) = updated
                .iter()
                .find(|record| record.get("event_id").and_then(Value::as_str) == Some(event_id))
            {
                result["event"] = record.clone();
            }
        }
        Ok(result)
    })();
    match result {
        Ok(value) => Ok(value),
        Err(error) => {
            // Restore the draft and keep the batch queued with the failure.
            let _ = crate::draft::discard_projection(workdir);
            for event_id in &claimed {
                let _ = queue::update_event(
                    workdir,
                    event_id,
                    &json!({
                        "delivery_state": "queued",
                        "last_error": error.detail.clone(),
                    }),
                );
            }
            Err(CollaborationError::new("patch-apply-failed", error.detail))
        }
    }
}

/// Accept or reject every tracked revision and clear every comment in
/// `_template.docx` (mirror of Python `_decide_all`), returning the decided
/// package bytes. The original workdir is never mutated; the caller builds
/// the new DOCX and re-extracts the clean-baseline workdir.
pub fn decide_all_package(workdir: &Path, action: &str) -> Result<Vec<u8>, CollaborationError> {
    if !matches!(action, "accept" | "reject") {
        return Err(CollaborationError::new(
            "invalid-action",
            "action must be accept or reject",
        ));
    }
    if workdir.join("edit.state.json").is_file() {
        match docx2typed_core::edit_state::classify_edit_state(workdir) {
            Ok(edit_state) if edit_state.state == "clean" => {}
            _ => {
                return Err(CollaborationError::new(
                    "draft-not-clean",
                    "decide_all requires a clean canonical workdir",
                ))
            }
        }
    }
    let template_path = workdir.join("_template.docx");
    let package = fs::read(&template_path)
        .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))?;
    let mut current_xml = govern::document_xml_bytes(&package)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    // Settle every revision in document.xml by w:id (settling never creates
    // new revision ids, so the initial scan is stable).
    let mut revisions = govern::scan_revisions_bytes(&package)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    revisions.sort_by_key(|revision| revision.w_id.clone());
    let mut settled_count = 0usize;
    for revision in &revisions {
        match govern::settle_one_revision(&current_xml, &revision.w_id, action) {
            Ok(settled) => {
                current_xml = settled.part_xml;
                settled_count += 1;
            }
            Err((_code, message)) => {
                return Err(CollaborationError::new(settle_code(&message), message))
            }
        }
    }
    let mut new_package = govern::patch_document_xml(&package, &current_xml)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    // Clear every comment part (commentRangeStart/End + references are
    // removed with the comments themselves).
    let comments = govern::scan_comments_bytes(&new_package)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    for comment in comments {
        new_package = govern::delete_comment_bytes(&new_package, &comment.id)
            .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    }
    // decisions.json report for the new workdir (revision count).
    let decisions_report = json!({
        "schema": "typed-decisions-1",
        "action": "decide-all",
        "revision_count": settled_count,
        "decisions": [],
    });
    let _ = decisions_report;
    Ok(new_package)
}
