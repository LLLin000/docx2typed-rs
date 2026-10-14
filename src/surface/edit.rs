//! `document_edit` — change the document; the engine does the rest.
//!
//! One transaction: resolve every handle, apply, sync, and write NOTHING if any
//! edit cannot be applied.
//!
//! The caller says `{handle, replace_with}`, which is the facade's own request
//! shape — so there is no translation layer to get wrong, and no second key
//! spelling to discover. A real session guessed `{old, new}` at a tool that
//! wanted `{handle, replace_with}` and had to try again.
//!
//! The sync is not the caller's job. A real session hit
//! `paragraph-has-uncommitted-length-change` twice: it edited a paragraph, then
//! edited it again, and the second edit was refused because the first had not
//! been synced. There is exactly one correct moment to sync, the engine knows
//! it, so the engine takes it — and the facade's own `next` field, which told
//! the caller to run `commit_sync` themselves, is not something an agent should
//! ever have to read.

use crate::mcp::McpSession;
use docx2typed_protocol::new_operation_id;
use docx2typed_review::draft::EditTracking;
use docx2typed_review::facade::{self, EditRequest};
use serde_json::{json, Value};
use std::path::Path;

/// The edits, or a refusal that names the shape rather than only the mistake.
fn requests_from(
    session: &McpSession,
    args: &Value,
    build_commit: &str,
) -> Result<Vec<EditRequest>, Value> {
    let Some(edits) = args.get("edits").and_then(Value::as_array) else {
        return Err(session.failure(
            "document_edit",
            "invalid-arguments",
            "document_edit requires `edits`: an array of {handle, replace_with}",
            build_commit,
        ));
    };
    if edits.is_empty() {
        return Err(session.failure(
            "document_edit",
            "invalid-arguments",
            "document_edit needs at least one edit; an empty list would report success while \
             changing nothing",
            build_commit,
        ));
    }
    let mut requests = Vec::with_capacity(edits.len());
    for (index, edit) in edits.iter().enumerate() {
        let number = index + 1;
        let handle = edit.get("handle").and_then(Value::as_str);
        let replace_with = edit.get("replace_with").and_then(Value::as_str);
        let (Some(handle), Some(replace_with)) = (handle, replace_with) else {
            // Name the right keys, not just the wrong ones: this is the exact
            // place a session guessed `{old, new}` and had to try again.
            return Err(session.failure(
                "document_edit",
                "invalid-arguments",
                &format!(
                    "edit #{number} must be exactly {{\"handle\": <a handle from \
                     document_find>, \"replace_with\": <the new text>}}; got keys [{}]",
                    edit.as_object()
                        .map(|object| object.keys().cloned().collect::<Vec<_>>().join(", "))
                        .unwrap_or_default()
                ),
                build_commit,
            ));
        };
        requests.push(EditRequest {
            handle: handle.to_string(),
            replace_with: replace_with.to_string(),
        });
    }
    Ok(requests)
}

/// `document_edit` — apply the edits, sync them, and report what happened.
pub fn document_edit(session: &mut McpSession, args: &Value, build_commit: &str) -> Value {
    let Some(workdir) = session.open_workdir().map(str::to_string) else {
        return session.failure(
            "document_edit",
            "workdir-not-open",
            "no document is open; call document_open first",
            build_commit,
        );
    };
    let requests = match requests_from(session, args, build_commit) {
        Ok(requests) => requests,
        Err(failure) => return failure,
    };
    // Optional, and minted when absent. The reference requires a caller-supplied
    // id; requiring it here made an agent learn an idempotency protocol before
    // it could make its first edit. Minting one and returning it keeps the retry
    // property — a repeat replays byte-exactly — without the ceremony.
    let operation_id = args
        .get("operation_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .unwrap_or_else(new_operation_id);
    let dry_run = args
        .get("dry_run")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let tracking = EditTracking {
        tracked: session.tracking(),
        author: session.authoring(),
        date: session.stamp(),
    };
    let edited = match facade::edit(Path::new(&workdir), &requests, dry_run, tracking) {
        Ok(value) => value,
        Err(error) => {
            return session.failure("document_edit", &error.code, &error.detail, build_commit)
        }
    };
    // A blocked batch wrote nothing — the facade stages every target before it
    // applies any, so one unusable handle leaves C0 intact — and it must NOT be
    // reported as success. Reporting it as success is the exact lie this surface
    // exists to remove: the caller reads `success`, believes the document
    // changed, and carries on from a state that never happened.
    let edit_outcome = edited.get("outcome").and_then(Value::as_str).unwrap_or("");
    if edit_outcome == "blocked" {
        let reasons: Vec<String> = edited
            .get("blocked_targets")
            .and_then(Value::as_array)
            .map(|targets| {
                targets
                    .iter()
                    .filter_map(|target| target.get("reason").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        return session.failure(
            "document_edit",
            "edit-blocked",
            &format!(
                "{} of {} edit(s) cannot be applied, so nothing was written: {}",
                reasons.len().max(1),
                requests.len(),
                reasons.join("; ")
            ),
            build_commit,
        );
    }
    if edit_outcome != "committed" {
        // `staged` under `dry_run`: validated, nothing written, and the caller
        // asked for exactly that.
        return session.success(
            "document_edit",
            json!({
                "operation_id": operation_id,
                "dry_run": dry_run,
                "edit": edited,
            }),
            build_commit,
        );
    }

    // Sync now, so a second edit to the same paragraph does not need a manual
    // commit between them. The facade's `next` tells the caller to do this
    // themselves; on this surface that step does not exist.
    let synced = session.commit_sync(
        &json!({ "operation_id": format!("{operation_id}-sync") }),
        build_commit,
    );
    let sync_envelope = synced.get("structuredContent").cloned().unwrap_or(Value::Null);
    let sync_outcome = sync_envelope
        .get("outcome")
        .and_then(Value::as_str)
        .unwrap_or("failure");
    if sync_outcome != "success" {
        // `current-not-changed` from the sync is only honest when the draft
        // really matches the canonical document. When it does not, the engine
        // has dropped this edit and is about to let the caller believe it
        // landed — the one failure this surface exists to remove. Measured on a
        // second edit to the same paragraph: `edit.md` read
        // `前缀中文区块甲区块区域内容后缀` while `typed.md` still read
        // `前缀中文区块区域内容`, and the caller was told "success".
        if sync_refused_because_unchanged(&sync_envelope)
            && draft_differs_from_canonical(&workdir)
        {
            return session.failure(
                "document_edit",
                "edit-not-synced",
                "the edit was applied to the draft but the canonical document did not change, \
                 so it was dropped rather than saved. The usual cause is a handle from before \
                 the previous edit: re-run document_find and edit with a fresh handle",
                build_commit,
            );
        }
        return synced;
    }

    session.success(
        "document_edit",
        json!({
            "operation_id": operation_id,
            "dry_run": dry_run,
            "edit": edited,
            "synced": synced
                .get("structuredContent")
                .and_then(|envelope| envelope.get("data"))
                .cloned()
                .unwrap_or_else(|| json!({})),
        }),
        build_commit,
    )
}

/// Did the sync refuse on the grounds that the canonical text was unchanged?
fn sync_refused_because_unchanged(envelope: &Value) -> bool {
    envelope
        .get("diagnostics")
        .and_then(Value::as_array)
        .is_some_and(|diagnostics| {
            diagnostics.iter().any(|diagnostic| {
                diagnostic.get("code").and_then(Value::as_str) == Some("current-not-changed")
            })
        })
}

/// Does the draft carry changes the canonical projection does not?
///
/// A byte comparison of the two rendered files, which is the question the sync
/// claims to have answered. Cheap, and it is the only thing that distinguishes
/// "nothing to sync" from "the sync lost the edit".
fn draft_differs_from_canonical(workdir: &str) -> bool {
    let read = |name: &str| std::fs::read(Path::new(workdir).join(name)).ok();
    match (read("edit.md"), read("typed.md")) {
        (Some(draft), Some(canonical)) => draft != canonical,
        // Unreadable is not evidence of agreement; do not manufacture a refusal.
        _ => false,
    }
}
