//! `document_status` — what is open, what is pending, what to do next.
//!
//! Composes the existing handlers rather than reimplementing them: the state,
//! the version list, and the review state are each already answered elsewhere,
//! and a second implementation of any of them is how two answers start
//! disagreeing. What this adds is the join — one call that tells an agent where
//! it is, so it does not have to assemble that picture from three tools.

use crate::mcp::McpSession;
use serde_json::{json, Value};

/// The `data` of a success envelope, or `None` when the call refused.
///
/// A tool result wraps the envelope one level down: `{content,
/// structuredContent: {outcome, data, ...}, isError}`. Reading `outcome` off
/// the top level silently finds nothing and every call looks like a refusal.
fn data_of(result: &Value) -> Option<Value> {
    let envelope = result.get("structuredContent")?;
    match envelope.get("outcome").and_then(Value::as_str) {
        Some("success") => Some(envelope.get("data").cloned().unwrap_or(Value::Null)),
        _ => None,
    }
}

/// The one thing to do next, or `None` when nothing is pending.
///
/// Every branch reads a field that exists, and reads the RIGHT one. This used to
/// infer "the source document changed outside the engine" from
/// `current_matches_filesystem` — which compares the session snapshot with the
/// workdir's own projection, a different question entirely. Two drifts exist and
/// they have different remedies:
///
/// - `source_drift`: the DOCX on disk differs from the bytes the workdir was
///   extracted from. The session descriptor carries this (`source_drift.changed`
///   plus both hashes); re-opening re-extracts.
/// - `snapshot_drift`: the review snapshot disagrees with the workdir's current
///   projection. Nothing on disk is wrong; the caller's view is stale, so the
///   remedy is to look again, not to rebuild anything.
///
/// Answering one with the other's remedy is the defect class this whole round
/// has been removing, and it is worse here than elsewhere: telling an agent to
/// re-open a workdir that carries committed edits is telling it to hit
/// `workdir-not-at-baseline`.
fn next_step(versions: &Value, review: &Value, session: &Value) -> Option<Value> {
    // The draft's own dirty flag, and NOT the committed snapshot's
    // `changed_paragraph_ids`: that list describes the last commit, which is
    // history. Reading it as pending work reported `document_save` forever —
    // measured, on a document that had just been saved.
    let draft_dirty = versions
        .get("draft_dirty")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if draft_dirty {
        return Some(json!({
            "action": "document_save",
            "why": "the draft carries changes that have not been settled into the \
                    document yet; document_save settles them and writes the file",
        }));
    }
    // The DOCX on disk moved on. Re-opening is the remedy, and it is safe only
    // while the workdir still matches its baseline — which is exactly the case
    // this branch is in, because a workdir with unsaved changes returned above.
    if session
        .pointer("/source_drift/changed")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Some(json!({
            "action": "document_open",
            "why": "the source document changed outside the engine; re-open it to \
                    extract the new baseline before editing again",
            "source_drift": session.get("source_drift").cloned().unwrap_or(Value::Null),
        }));
    }
    // The caller's view is stale. Nothing on disk needs to change, so the remedy
    // is to look again rather than to rebuild: a stale handle is re-located by
    // document_find, and re-opening here would be a baseline reset the workdir
    // does not need.
    if review
        .get("current_matches_filesystem")
        .and_then(Value::as_bool)
        .is_some_and(|matches| !matches)
    {
        return Some(json!({
            "action": "document_find",
            "why": "the review snapshot no longer matches the workdir projection; \
                    re-locate the text you were about to edit and use the fresh handle",
        }));
    }
    None
}

/// `document_status` — the current state, the versions, and the next step.
pub fn document_status(session: &mut McpSession, _args: &Value, build_commit: &str) -> Value {
    // `workdir_open` on the workdir already open returns the live session — the
    // same call `document_open` makes, and idempotent, so it is the honest way to
    // read the session descriptor without a second implementation of it.
    let Some(workdir) = session.open_workdir().map(str::to_string) else {
        return session.failure(
            "document_status",
            "workdir-not-open",
            "no document is open; call document_open first",
            build_commit,
        );
    };
    let opened = session.workdir_open(&json!({ "workdir": workdir }), build_commit);
    if opened.get("isError").and_then(Value::as_bool) == Some(true) {
        return opened;
    }
    let session_descriptor = opened
        .get("structuredContent")
        .and_then(|envelope| envelope.get("data"))
        .and_then(|data| data.get("session"))
        .cloned()
        .unwrap_or(Value::Null);

    let state = session.workdir_status(build_commit);
    let Some(state_data) = data_of(&state) else {
        return state;
    };
    // Neither of these is a reason to fail the whole status: an agent asking
    // where it is should get an answer even when one view is unavailable.
    let versions = session.history_list(&json!({}), build_commit);
    let review = session.review_state(build_commit);
    let review_data = data_of(&review).unwrap_or(Value::Null);
    let versions_data = data_of(&versions).unwrap_or(Value::Null);

    let next = next_step(&versions_data, &review_data, &session_descriptor);
    session.success(
        "document_status",
        json!({
            "state": state_data,
            "session": session_descriptor,
            "versions": versions_data,
            "review": review_data,
            "next": next,
        }),
        build_commit,
    )
}
