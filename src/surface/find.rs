//! `document_find` — find the text you want to change, and get a handle.
//!
//! Composes `document_search` and adds the one thing it left to the caller: a
//! SINGLE verdict about whether each span can be edited as one edit.
//!
//! Two tools disagreeing about one span is the defect this tool exists to
//! remove. A real session was told `patchable_as_single_hunk: true` by the
//! search and then refused `cross-leaf-edit` by the edit, and it spent calls
//! discovering that. The search already carries the boundary analysis — the
//! edit path was fixed to reach the same conclusion from the same analysis — so
//! this tool reports that conclusion as a verdict, per span, instead of leaving
//! the caller to infer it from a field name.

use crate::mcp::McpSession;
use docx2typed_review::draft::EditTracking;
use docx2typed_review::facade::{self, EditRequest};
use serde_json::{json, Value};
use std::path::Path;

/// The verdict for one span, taken from the edit path itself.
///
/// Not inferred from a field on the search result. A real session was told
/// `patchable_as_single_hunk: true` by the search and then refused
/// `cross-leaf-edit` by the edit, and it burned calls discovering that; the
/// search's own boundary analysis turned out not to cover every reason the edit
/// path refuses — text split across runs after an earlier edit is one it never
/// saw. So this stages the span through the same resolution the edit performs
/// (`facade::edit` with `dry_run`, which writes nothing) and reports THAT
/// verdict. One question, one answer, one implementation.
fn verdict_for(workdir: &Path, handle: &str) -> Value {
    let request = EditRequest {
        handle: handle.to_string(),
        // The replacement is never written under `dry_run`; it only has to be a
        // string, because staging validates the TARGET, not the new text.
        replace_with: String::new(),
    };
    // Staging writes nothing, so the tracking choice cannot affect the
    // verdict; an untracked, unattributed preview is the neutral one.
    let preview = EditTracking {
        tracked: false,
        author: None,
        date: None,
    };
    match facade::edit(workdir, &[request], true, preview) {
        Ok(report) => {
            let blocked = report.get("blocked_targets").and_then(Value::as_array);
            match blocked.and_then(|targets| targets.first()) {
                Some(target) => json!({
                    "editable": false,
                    "code": target.get("reason")
                        .and_then(Value::as_str)
                        .and_then(|reason| reason.split(':').next())
                        .unwrap_or("blocked")
                        .to_string(),
                    "why": target.get("reason").cloned().unwrap_or(Value::Null),
                }),
                None => json!({ "editable": true }),
            }
        }
        Err(error) => json!({ "editable": false, "code": error.code, "why": error.detail }),
    }
}

/// `document_find` — matches, each with a handle and one editability verdict.
pub fn document_find(session: &mut McpSession, args: &Value, build_commit: &str) -> Value {
    let Some(query) = args.get("query").and_then(Value::as_str) else {
        return session.failure(
            "document_find",
            "invalid-arguments",
            "document_find requires `query`: the text to look for",
            build_commit,
        );
    };
    if query.is_empty() {
        return session.failure(
            "document_find",
            "invalid-arguments",
            "document_find needs a non-empty `query`; an empty query matches everything and \
             would return the whole document rather than an answer",
            build_commit,
        );
    }

    let Some(workdir) = session.open_workdir().map(str::to_string) else {
        return session.failure(
            "document_find",
            "workdir-not-open",
            "no document is open; call document_open first",
            build_commit,
        );
    };
    let mut search_args = json!({ "query": query });
    for key in ["scope", "limit"] {
        if let Some(value) = args.get(key) {
            if !value.is_null() {
                search_args[key] = value.clone();
            }
        }
    }
    let searched = session.document_search(&search_args, build_commit);
    let Some(envelope) = searched.get("structuredContent") else {
        return searched;
    };
    if envelope.get("outcome").and_then(Value::as_str) != Some("success") {
        // A search that refuses (no workdir open, no projection) already carries
        // the recovery payload naming what to call; pass it through unchanged.
        return searched;
    }
    let data = envelope.get("data").cloned().unwrap_or(Value::Null);

    let workdir = Path::new(&workdir);
    let matches: Vec<Value> = data
        .get("matches")
        .and_then(Value::as_array)
        .map(|matches| {
            matches
                .iter()
                .map(|entry| {
                    let handle = entry
                        .get("match_ref")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let occurrences: Vec<Value> = entry
                        .get("occurrences")
                        .and_then(Value::as_array)
                        .map(|occurrences| {
                            occurrences
                                .iter()
                                .filter_map(|occurrence| occurrence.get("match_ref"))
                                .filter_map(Value::as_str)
                                .map(|handle| {
                                    json!({
                                        "handle": handle,
                                        "verdict": verdict_for(workdir, handle),
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    json!({
                        "paragraph_id": entry.get("id").cloned().unwrap_or(Value::Null),
                        "matched_text": entry.get("matched_text").cloned().unwrap_or(Value::Null),
                        "handle": handle,
                        "verdict": verdict_for(workdir, &handle),
                        "occurrences": occurrences,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let editable = matches
        .iter()
        .filter(|entry| entry["verdict"]["editable"] == json!(true))
        .count();
    session.success(
        "document_find",
        json!({
            "query": query,
            "matches": matches,
            "counts": {
                "matches": matches.len(),
                // The number the caller actually acts on: a match it cannot edit
                // as one span is not a match it can pass to document_edit.
                "editable": editable,
                "not_editable": matches.len() - editable,
            },
        }),
        build_commit,
    )
}
