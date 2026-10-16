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
    for key in ["scope", "limit", "offset"] {
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

    // Count OCCURRENCES, not blocks. The handle a caller sends belongs to an
    // occurrence, and a block's own verdict is only its first occurrence's:
    // measured on a real patent, a page reported `not_editable: 0` while five of
    // its occurrences carried `editable: false`, so the one number a caller acts
    // on said the opposite of the list beside it.
    let mut occurrences = 0usize;
    let mut editable = 0usize;
    for entry in &matches {
        match entry.get("occurrences").and_then(Value::as_array) {
            Some(listed) if !listed.is_empty() => {
                for occurrence in listed {
                    occurrences += 1;
                    if occurrence["verdict"]["editable"] == json!(true) {
                        editable += 1;
                    }
                }
            }
            _ => {
                occurrences += 1;
                if entry["verdict"]["editable"] == json!(true) {
                    editable += 1;
                }
            }
        }
    }
    // The search's own coverage numbers, kept. `limit` bounds matching BLOCKS,
    // not occurrences, and a caller that sees only the page's own count cannot
    // tell a complete answer from the first page of one. That distinction matters
    // most for the thing an agent is usually asked to do to a patent — unify a
    // term across the whole document — where silently seeing the first twenty
    // matching paragraphs looks exactly like having seen them all.
    let total_matches = data.get("total_matches").cloned().unwrap_or(Value::Null);
    let total_blocks = data.get("total_blocks").cloned().unwrap_or(Value::Null);
    let returned_blocks = data.get("returned_blocks").cloned().unwrap_or(Value::Null);
    let offset = data.get("offset").cloned().unwrap_or(json!(0));
    let complete = match (
        returned_blocks.as_u64(),
        total_blocks.as_u64(),
        offset.as_u64(),
    ) {
        (Some(returned), Some(total), Some(offset)) => offset + returned >= total,
        _ => false,
    };
    session.success(
        "document_find",
        json!({
            "query": query,
            "matches": matches,
            "counts": {
                // What the caller acts on: every entry in `matches` carries the
                // handles its occurrences are addressed by.
                "occurrences": occurrences,
                "editable": editable,
                "not_editable": occurrences - editable,
                // The page's shape, so a paging caller can see where it is.
                "blocks": matches.len(),
                // The document-wide numbers, so the page is never mistaken for
                // the whole answer.
                "total_matches": total_matches,
                "total_blocks": total_blocks,
                "returned_blocks": returned_blocks,
                "offset": offset,
                "complete": complete,
            },
            "next": if complete {
                Value::Null
            } else {
                json!({
                    "action": "continue-search",
                    "why": "more matching paragraphs exist than this page returned",
                    "call": { "query": query, "offset": offset.as_u64().unwrap_or(0)
                        + returned_blocks.as_u64().unwrap_or(0) },
                })
            },
        }),
        build_commit,
    )
}
