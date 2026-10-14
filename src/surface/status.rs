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
/// Named explicitly because an agent that has to infer its next step from a
/// status blob will sometimes infer wrong, and then retry the call that just
/// failed. The engine knows the answer; saying it costs one field. The names it
/// uses are the agent surface's, since those are the calls an agent can make.
///
/// Every branch reads a field that exists in the payloads this composes — the
/// draft's dirty flag and the changed-paragraph list, and the source-drift
/// flag. A next step inferred from a field that is not there is worse than no
/// next step: it names a call the agent does not need.
fn next_step(versions: &Value, review: &Value) -> Option<Value> {
    let draft_dirty = versions
        .get("draft_dirty")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let changed = review
        .pointer("/current_snapshot/changed_paragraph_ids")
        .and_then(Value::as_array)
        .is_some_and(|ids| !ids.is_empty());
    if draft_dirty || changed {
        return Some(json!({
            "action": "document_save",
            "why": "the workdir carries changes that are not in the source document yet;                     save writes them, or keep editing and save once",
        }));
    }
    if review
        .get("current_matches_filesystem")
        .and_then(Value::as_bool)
        .is_some_and(|matches| !matches)
    {
        return Some(json!({
            "action": "document_open",
            "why": "the source document changed outside the engine; re-open it before editing",
        }));
    }
    None
}

/// `document_status` — the current state, the versions, and the next step.
pub fn document_status(session: &mut McpSession, _args: &Value, build_commit: &str) -> Value {
    let state = session.workdir_status(build_commit);
    let Some(state_data) = data_of(&state) else {
        // Not open, or not a workdir: the refusal already carries the recovery
        // payload naming what to call, so pass it through unchanged.
        return state;
    };

    // Neither of these is a reason to fail the whole status: an agent asking
    // where it is should get an answer even when one view is unavailable.
    let versions = session.history_list(&json!({}), build_commit);
    let review = session.review_state(build_commit);
    let review_data = data_of(&review).unwrap_or(Value::Null);

    let next = next_step(&data_of(&versions).unwrap_or(Value::Null), &review_data);
    session.success(
        "document_status",
        json!({
            "state": state_data,
            "versions": data_of(&versions).unwrap_or(Value::Null),
            "review": review_data,
            "next": next,
        }),
        build_commit,
    )
}
