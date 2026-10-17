//! `document_edit` — change the document; the engine does the rest.
//!
//! Four kinds, one addressing scheme. `replace` (the default), `insert_after`
//! and `delete` change the text; `format` gives the characters a handle covers
//! the character properties of a style the document already carries, named by
//! `style_id`. The ids are the ones `document_find` reports for the span, and the
//! engine — not this file — decides what the request means: which characters it
//! covers, whether it overlaps another request, and how the runs are shaped
//! afterwards. Format edits and text edits are separate transactions, because a
//! format edit's span is measured against the text as it stands.
//!
//! ONE transaction: resolve every handle, apply, sync, and publish — or write
//! nothing at all. The caller says `{handle, replace_with}`, which is the
//! facade's own request shape, so there is no translation to get wrong and no
//! second key spelling to discover. A real session guessed `{old, new}` at a
//! tool that wanted `{handle, replace_with}` and had to try again.
//!
//! Both halves run inside ONE store mutation. That is not decoration: with
//! `generation: true` the closure is handed a COPY of the workdir's current
//! generation, and the store publishes it or discards it as a unit. Applying the
//! edit and then syncing it as two separate calls left a window where the draft
//! had moved and the canonical document had not — a failure the caller was told
//! was "nothing was written". Running them in one closure closes that window: a
//! refusal anywhere leaves the workdir exactly as it was.
//!
//! The same closure also gives the retry property. The store's ledger check runs
//! BEFORE the closure, so a repeat with the same `operation_id` replays the
//! original result and never touches a handle — which matters, because after a
//! successful edit those handles are stale and a naive re-run would refuse
//! instead of replaying. The reference requires a caller-supplied id; here it is
//! optional and minted when absent, returned in the result so a caller that lost
//! the response still has something stable to retry with.

use crate::mcp::McpSession;
use docx2typed_protocol::new_operation_id;
use docx2typed_review::draft::{self, EditTracking};
use docx2typed_review::facade::{self, EditRequest};
use docx2typed_review::{collab, server::MutationError};
use serde_json::{json, Value};
use std::path::Path;

/// Every handle a `selector` names, in document order.
///
/// A caller that wants "every 四氧化三铁 in the body" should say exactly that. The
/// enumeration is deterministic selection — the surface's job — and it used to be
/// the caller's: a blind run transcribed thirty-one handles out of a 968-line
/// reply and rebuilt truncated ones from their base64 structure. `expected_count`
/// is honoured when the caller gives it: a count that disagrees is a refusal, not
/// a surprise edit.
fn handles_for_selector(
    session: &mut McpSession,
    selector: &Value,
    build_commit: &str,
) -> Result<Vec<String>, Value> {
    let Some(text) = selector.get("text").and_then(Value::as_str) else {
        return Err(session.failure(
            "document_edit",
            "invalid-arguments",
            "a selector needs `text`: the text to enumerate. `match` may be `all` (the default) \
             or `first`, and `expected_count` is checked when given",
            build_commit,
        ));
    };
    let match_mode = selector
        .get("match")
        .and_then(Value::as_str)
        .unwrap_or("all");
    if !matches!(match_mode, "all" | "first") {
        return Err(session.failure(
            "document_edit",
            "invalid-arguments",
            &format!("selector `match` is `{match_mode}`; it may be `all` or `first`"),
            build_commit,
        ));
    }
    let mut search_args = json!({ "query": text, "limit": 1000 });
    if let Some(scope) = selector.get("scope") {
        search_args["scope"] = scope.clone();
    }
    // The SURFACE's own find, not the engine's raw search: the raw payload has a
    // different shape, and reading the wrong one is how this first enumerated
    // nothing while reporting a count of zero.
    let searched = super::find::document_find(session, &search_args, build_commit);
    let Some(envelope) = searched.get("structuredContent") else {
        return Err(searched);
    };
    if envelope.get("outcome").and_then(Value::as_str) != Some("success") {
        return Err(searched);
    }
    let data = envelope.get("data").cloned().unwrap_or(Value::Null);
    let mut handles: Vec<String> = Vec::new();
    if let Some(matches) = data.get("matches").and_then(Value::as_array) {
        for entry in matches {
            if let Some(occurrences) = entry.get("occurrences").and_then(Value::as_array) {
                for occurrence in occurrences {
                    if let Some(handle) = occurrence.get("handle").and_then(Value::as_str) {
                        handles.push(handle.to_string());
                    }
                }
            }
        }
    }
    let complete = data
        .get("counts")
        .and_then(|counts| counts.get("complete"))
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if !complete {
        return Err(session.failure(
            "document_edit",
            "selector-incomplete",
            &format!(
                "the search for {text:?} did not cover every match, so a selector that edits \
                 \"all\" of them would be a guess. Narrow the query"
            ),
            build_commit,
        ));
    }
    if match_mode == "first" {
        handles.truncate(1);
    }
    if let Some(expected) = selector.get("expected_count").and_then(Value::as_u64) {
        if handles.len() as u64 != expected {
            return Err(session.failure(
                "document_edit",
                "selector-count-mismatch",
                &format!(
                    "the selector expected {expected} match(es) of {text:?} and the document \
                     holds {}; nothing was written",
                    handles.len()
                ),
                build_commit,
            ));
        }
    }
    if handles.is_empty() {
        return Err(session.failure(
            "document_edit",
            "selector-matched-nothing",
            &format!("no match for {text:?}; nothing was written"),
            build_commit,
        ));
    }
    Ok(handles)
}

/// A format edit: give a span the character properties of a style the document
/// already carries. There is no synthesis here — the style registry holds what
/// the document has, and a style id it does not hold is refused before anything
/// is written.
#[derive(Clone, Debug)]
pub(crate) struct FormatRequest {
    pub(crate) handle: String,
    /// Either a style id the caller named, or empty with `attributes` set.
    pub(crate) style_id: String,
    pub(crate) attributes: Option<Value>,
}

/// One edit, in the shape its kind requires.
///
/// The kinds are a closed set and every one of them addresses its target the same
/// way — a handle from `document_find` — because two addressing schemes is how a
/// caller picks the wrong one. A missing per-kind argument is refused naming that
/// argument, and an unknown kind is refused naming the allowed ones.
pub(crate) enum Edit {
    Replace(EditRequest),
    InsertAfter {
        handle: String,
        text: String,
        format: docx2typed_core::prose::FormatPolicy,
    },
    Delete {
        handle: String,
    },
    Format(FormatRequest),
}

impl Edit {
    #[allow(dead_code)] // the kinds are matched structurally; kept for symmetry
    fn handle(&self) -> &str {
        match self {
            Edit::Replace(request) => &request.handle,
            Edit::InsertAfter { handle, .. } => handle,
            Edit::Delete { handle } => handle,
            Edit::Format(request) => &request.handle,
        }
    }
}

/// The edits, or a refusal that names the shape rather than only the mistake.
fn edits_from(
    session: &mut McpSession,
    args: &Value,
    build_commit: &str,
) -> Result<Vec<Edit>, Value> {
    let Some(edits) = args.get("edits").and_then(Value::as_array) else {
        return Err(session.failure(
            "document_edit",
            "invalid-arguments",
            "document_edit requires `edits`: an array of edits, each with a `handle` from \
             document_find and a `kind` of replace, insert_after, delete or format",
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
    // A selector names its targets by QUERY and the surface enumerates them, so a
    // caller never transcribes handles: "every 四氧化三铁 in the body" is one entry,
    // not thirty-one copied ids. The enumeration is deterministic selection, which
    // is this layer's job — the format engine still decides what each span becomes,
    // and one target that cannot be applied refuses the whole batch.
    let mut expanded: Vec<Value> = Vec::with_capacity(edits.len());
    for edit in edits {
        match edit.get("selector") {
            Some(selector) if !selector.is_null() => {
                let handles = handles_for_selector(session, selector, build_commit)?;
                for handle in handles {
                    let mut concrete = edit.clone();
                    if let Some(object) = concrete.as_object_mut() {
                        object.remove("selector");
                        object.insert("handle".to_string(), json!(handle));
                    }
                    expanded.push(concrete);
                }
            }
            _ => expanded.push(edit.clone()),
        }
    }
    let mut parsed = Vec::with_capacity(expanded.len());
    for (index, edit) in expanded.iter().enumerate() {
        let number = index + 1;
        let keys = || {
            edit.as_object()
                .map(|object| object.keys().cloned().collect::<Vec<_>>().join(", "))
                .unwrap_or_default()
        };
        let Some(handle) = edit.get("handle").and_then(Value::as_str) else {
            return Err(session.failure(
                "document_edit",
                "invalid-arguments",
                &format!(
                    "edit #{number} needs a `handle` from document_find; got keys [{}]",
                    keys()
                ),
                build_commit,
            ));
        };
        let kind = edit
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("replace");
        match kind {
            "replace" => {
                let Some(replace_with) = edit.get("replace_with").and_then(Value::as_str) else {
                    return Err(session.failure(
                        "document_edit",
                        "invalid-arguments",
                        &format!(
                            "edit #{number} is kind `replace` and needs `replace_with` (the new \
                             text); got keys [{}]",
                            keys()
                        ),
                        build_commit,
                    ));
                };
                parsed.push(Edit::Replace(EditRequest {
                    handle: handle.to_string(),
                    replace_with: replace_with.to_string(),
                }));
            }
            "insert_after" => {
                let Some(text) = edit.get("text").and_then(Value::as_str) else {
                    return Err(session.failure(
                        "document_edit",
                        "invalid-arguments",
                        &format!(
                            "edit #{number} is kind `insert_after` and needs `text` (the \
                             paragraph to insert); got keys [{}]",
                            keys()
                        ),
                        build_commit,
                    ));
                };
                let format = match edit.get("format_policy").and_then(Value::as_str) {
                    None | Some("safe_inherit") => {
                        docx2typed_core::prose::FormatPolicy::SafeInherit
                    }
                    Some("none") => docx2typed_core::prose::FormatPolicy::None,
                    Some(other) => {
                        return Err(session.failure(
                            "document_edit",
                            "invalid-arguments",
                            &format!(
                                "edit #{number} has format_policy `{other}`; the allowed values \
                                 are safe_inherit (the default) and none"
                            ),
                            build_commit,
                        ))
                    }
                };
                parsed.push(Edit::InsertAfter {
                    handle: handle.to_string(),
                    text: text.to_string(),
                    format,
                });
            }
            "delete" => parsed.push(Edit::Delete {
                handle: handle.to_string(),
            }),
            "format" => {
                // Two ways to say it, one authority. `style_id` names a style the
                // document carries — the ids document_find reports for the span.
                // `format` names PROPERTIES (`{"bold": true}`) and the engine
                // resolves the style the document already has for them, refusing
                // when it has none. An agent that reads rPr features to pick an id
                // is doing the engine's work; a blind run did exactly that.
                let style_id = edit.get("style_id").and_then(Value::as_str);
                let attributes = edit.get("format").and_then(Value::as_object);
                match (style_id, attributes) {
                    (Some(style_id), _) => parsed.push(Edit::Format(FormatRequest {
                        handle: handle.to_string(),
                        style_id: style_id.to_string(),
                        attributes: None,
                    })),
                    (None, Some(attributes)) => parsed.push(Edit::Format(FormatRequest {
                        handle: handle.to_string(),
                        style_id: String::new(),
                        attributes: Some(Value::Object(attributes.clone())),
                    })),
                    (None, None) => {
                        return Err(session.failure(
                            "document_edit",
                            "invalid-arguments",
                            &format!(
                                "edit #{number} is kind `format` and needs either `style_id` \
                                 (one of the ids document_find reports for the span) or `format` \
                                 (the properties to give the characters, e.g. {{\"bold\": true}}, \
                                 which the engine resolves against the styles this document \
                                 already carries); got keys [{}]",
                                keys()
                            ),
                            build_commit,
                        ))
                    }
                }
            }
            other => {
                return Err(session.failure(
                    "document_edit",
                    "invalid-arguments",
                    &format!(
                        "edit #{number} has kind `{other}`; the allowed kinds are replace, \
                         insert_after, delete and format"
                    ),
                    build_commit,
                ))
            }
        }
    }
    Ok(parsed)
}

/// Apply the edits and settle them, against one generation of the workdir.
///
/// This is the body `commit_sync` runs, composed here rather than called through
/// its handler: the handler opens its own store mutation, and nesting one inside
/// another is not a transaction, it is two.
///
/// Every kind runs inside the SAME closure, so a refusal anywhere discards the
/// generation — inserts and deletes included. That is why the kinds do not each
/// need their own fail-closed staging: the store's generation copy is the
/// staging, and it covers all of them at once.
pub(crate) fn apply_and_sync(
    target: &Path,
    edits: &[Edit],
    tracking: EditTracking<'_>,
) -> Result<Value, MutationError> {
    let mut requests: Vec<EditRequest> = Vec::new();
    let mut structural: Vec<Value> = Vec::new();
    let mut restyles: Vec<Value> = Vec::new();
    for edit in edits {
        match edit {
            Edit::Replace(request) => requests.push(request.clone()),
            Edit::Format(request) => {
                // A format edit's span is measured against the text as it stands,
                // so a batch that also rewrites text would make the span's
                // meaning depend on the order of the entries. One transaction,
                // one kind — the caller edits the text first, then formats what
                // it produced.
                if edits.iter().any(|other| !matches!(other, Edit::Format(_))) {
                    return Err(MutationError::new(
                        "format-and-text-in-one-batch",
                        "a batch cannot mix text edits with format edits: a format edit's span \
                         is measured against the text as it stands, so the two kinds would have \
                         to run in a fixed order to mean anything. Send the text edits, then a \
                         separate document_edit with the format edits",
                    ));
                }
                let resolved =
                    docx2typed_core::document_search::resolve_match_ref(target, &request.handle)
                        .map_err(|error| {
                            let message = error.to_string();
                            match message.split_once(':') {
                                Some((code, detail)) if !code.contains(' ') => {
                                    MutationError::new(code, detail.trim())
                                }
                                _ => MutationError::new("invalid-edit", message),
                            }
                        })?;
                // The surface decides nothing about formatting here. It resolves
                // the handle to a character range and hands the request to the
                // engine, which validates the style, settles overlaps and derives
                // the run structure. A second opinion here is the second authority
                // the split exists to remove.
                let length = resolved.text.chars().count();
                let (start, end) = (resolved.start, resolved.start + length);
                let style_id = match &request.attributes {
                    Some(Value::Object(attributes)) => {
                        docx2typed_review::format_lane::resolve_style(
                            target,
                            &resolved.paragraph_id,
                            start,
                            end,
                            attributes,
                        )
                        .map_err(|error| MutationError::new(error.code, error.detail))?
                    }
                    _ => request.style_id.clone(),
                };
                let (plan, _snapshot, _requests) =
                    docx2typed_review::format_lane::record_format_request(
                        target,
                        docx2typed_review::format_lane::FormatRequest {
                            paragraph_id: resolved.paragraph_id.clone(),
                            start,
                            end,
                            style_id: style_id.clone(),
                            attributes: request.attributes.clone(),
                        },
                    )
                    .map_err(|error| MutationError::new(error.code, error.detail))?;
                let paragraph_plan = plan.paragraph(&resolved.paragraph_id);
                restyles.push(json!({
                    "kind": "format",
                    "paragraph_id": resolved.paragraph_id,
                    "range": {
                        "start": resolved.start,
                        "end": resolved.start + length,
                        "unit": "unicode-scalars",
                    },
                    "style_id": style_id,
                    "segments": paragraph_plan.map(|plan| plan.segments.len()),
                    "runs": paragraph_plan.map(|plan| plan.runs.len()),
                }));
            }
            Edit::InsertAfter {
                handle,
                text,
                format,
            } => {
                let anchor = paragraph_of(target, handle)?;
                // `anchor` is the POSITION. Naming it as the inherit source too
                // made the engine refuse whenever that paragraph carried
                // revisions or anchors — a decision the caller cannot see and
                // cannot act on, since the surface hands out no handle for a
                // paragraph the search did not match. Passing `None` lets the
                // engine choose a source that works and keep the position.
                let inserted = draft::insert_paragraph(target, &anchor, text, None, *format)
                    .map_err(|error| MutationError::new(error.code, error.detail))?;
                structural
                    .push(json!({ "kind": "insert_after", "anchor": anchor, "result": inserted }));
            }
            Edit::Delete { handle } => {
                let paragraph = paragraph_of(target, handle)?;
                let deleted = draft::delete_paragraph(target, &paragraph)
                    .map_err(|error| MutationError::new(error.code, error.detail))?;
                structural.push(
                    json!({ "kind": "delete", "paragraph_id": paragraph, "result": deleted }),
                );
            }
        }
    }

    // No replacements means "settle what is there" — the save path uses it, and
    // it must not open an empty facade batch, which the facade refuses by design.
    let edited = if requests.is_empty() {
        Value::Null
    } else {
        facade::edit(target, &requests, false, tracking)
            .map_err(|error| MutationError::new(error.code, error.detail))?
    };
    if !requests.is_empty() && edited.get("outcome").and_then(Value::as_str) != Some("committed") {
        // The facade stages every target before it applies any, so a blocked
        // batch has written nothing. Report it and let the caller decide; the
        // generation it returns is the one it started from.
        return Ok(json!({
            "edit": edited,
            "structural": structural,
            "restyles": restyles,
            "synced": Value::Null,
        }));
    }

    // Bootstrap the review session BEFORE `apply_projection` rewrites typed.md.
    // The bootstrap pins the current typed.md hash as the C0 baseline; doing it
    // after the rewrite would pin the post-commit hash and `publish_current`
    // would then see `actual == pinned` and refuse `current-not-changed` even
    // though the draft had edits.
    collab::ensure_session(target).map_err(|error| MutationError::new(error.code, error.detail))?;
    let changed = draft::apply_projection_with_options(
        target,
        tracking.tracked,
        tracking.author,
        tracking.date,
    )
    .map_err(|error| MutationError::new(error.code, error.detail))?;
    if changed.is_empty() {
        return Ok(json!({
            "edit": edited,
            "structural": structural,
            "restyles": restyles,
            "synced": {
                "changed_paragraph_ids": [],
                "state": "clean",
                "current_snapshot": Value::Null,
            },
        }));
    }
    let parent = collab::document_state(target)
        .ok()
        .and_then(|state| {
            state
                .get("current_snapshot")
                .and_then(|snapshot| snapshot.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();
    let published = collab::publish_current(
        target,
        &parent,
        tracking.author.unwrap_or("agent"),
        &changed,
        None,
    )
    .map_err(|error| MutationError::new(error.code, error.detail))?;
    Ok(json!({
        "edit": edited,
        "structural": structural,
        "restyles": restyles,
        "synced": {
            "changed_paragraph_ids": changed,
            "state": "clean",
            "current_snapshot": published.get("current_snapshot").cloned().unwrap_or(Value::Null),
        },
    }))
}

/// Rewrite the engine's `next` steps in one reply to the step the surface can
/// take.
///
/// The engine answers in its own vocabulary — `data.edit.engine.next` reads
/// "diff_preview to inspect style ownership, then commit_sync", a synthesised
/// paragraph's result says "commit_sync allocates the formal paragraph ID" — and
/// neither tool is mounted, nor is either step outstanding: the commit already
/// happened inside this call. A blind agent quoted the advice back as a defect,
/// on the replace path as well as the structural one, so the rewrite covers the
/// whole reply instead of the fields anyone remembered. The engine's own reply
/// keeps its wording for the CLI and the tests, where those tools exist.
fn in_surface_vocabulary(mut data: Value) -> Value {
    rewrite_next_steps(&mut data);
    data
}

fn rewrite_next_steps(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, entry) in map.iter_mut() {
                if key == "next" {
                    if let Some(step) = entry.as_str() {
                        if names_an_unmounted_tool(step) {
                            *entry =
                                json!("document_save writes the file when you are done editing");
                            continue;
                        }
                    }
                }
                rewrite_next_steps(entry);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(rewrite_next_steps),
        _ => {}
    }
}

/// Does this engine step name a tool the agent surface does not mount?
fn names_an_unmounted_tool(step: &str) -> bool {
    const ENGINE_ONLY: [&str; 9] = [
        "commit_sync",
        "diff_preview",
        "workdir_open",
        "workdir_status",
        "document_patch",
        "batch_edit",
        "facade_edit",
        "facade_locate",
        "facade_verify",
    ];
    ENGINE_ONLY.iter().any(|tool| step.starts_with(tool))
}

/// The paragraph a handle addresses.
///
/// A handle names a span, and a span lives in exactly one paragraph, so this is
/// how an insert or a delete finds its target without a second addressing scheme.
fn paragraph_of(workdir: &Path, handle: &str) -> Result<String, MutationError> {
    docx2typed_core::document_search::resolve_match_ref(workdir, handle)
        .map(|resolved| resolved.paragraph_id)
        .map_err(|error| {
            let message = error.to_string();
            match message.split_once(':') {
                Some((code, detail)) if !code.contains(' ') => {
                    MutationError::new(code, detail.trim())
                }
                _ => MutationError::new("invalid-edit", message),
            }
        })
}

/// `document_edit` — apply the edits and settle them, in one transaction.
pub fn document_edit(session: &mut McpSession, args: &Value, build_commit: &str) -> Value {
    if session.open_workdir().is_none() {
        return session.failure(
            "document_edit",
            "workdir-not-open",
            "no document is open; call document_open first",
            build_commit,
        );
    }
    let edits = match edits_from(session, args, build_commit) {
        Ok(edits) => edits,
        Err(failure) => return failure,
    };
    // Optional, and minted when absent, so a caller does not have to learn an
    // idempotency protocol before making its first edit. It IS returned, which
    // is what makes it usable: a caller that lost the response can retry with
    // the id it never chose.
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
    let tracked = session.tracking();
    let author = session.authoring().map(str::to_string);
    let date = session.stamp().map(str::to_string);
    let tracking = EditTracking {
        tracked,
        author: author.as_deref(),
        date: date.as_deref(),
    };

    if dry_run {
        // Nothing is written, so nothing needs a transaction or a ledger entry.
        let workdir = Path::new(session.open_workdir().unwrap_or_default());
        // The structural kinds are previewed by resolving their anchor — the
        // first step the real call takes, and the only one that can fail before
        // a byte is written — instead of being dropped on the floor. They used
        // to be filtered out and the survivors handed to the facade, so an
        // insert-only batch arrived there EMPTY and came back `edit-empty`,
        // "at least one edit is required", for a call that gave one. A blind
        // agent probed 20 shapes through `dry_run` (the honest way to try an
        // unknown tool), was told its edit did not exist every time, and
        // concluded that inserting a paragraph was not possible at all.
        let mut replacements: Vec<EditRequest> = Vec::new();
        let mut structural: Vec<Value> = Vec::new();
        let mut blocked: Vec<Value> = Vec::new();
        for edit in &edits {
            let (kind, handle) = match edit {
                Edit::Replace(request) => {
                    replacements.push(request.clone());
                    continue;
                }
                Edit::InsertAfter { handle, .. } => ("insert_after", handle),
                Edit::Delete { handle } => ("delete", handle),
                Edit::Format(request) => ("format", &request.handle),
            };
            match paragraph_of(workdir, handle) {
                Ok(anchor) => structural.push(json!({
                    "kind": kind,
                    "anchor": anchor,
                    "capability": "supported",
                })),
                Err(error) => blocked.push(json!({
                    "kind": kind,
                    "capability": "unsupported",
                    "reason": format!("{}: {}", error.code, error.detail),
                })),
            }
        }
        // An empty replacement batch is not a question the facade can answer, so
        // a structural-only call does not ask it one.
        let preview = if replacements.is_empty() {
            Value::Null
        } else {
            match facade::edit(workdir, &replacements, true, tracking) {
                Ok(value) => value,
                Err(error) => {
                    return session.failure(
                        "document_edit",
                        &error.code,
                        &error.detail,
                        build_commit,
                    )
                }
            }
        };
        let mut data = json!({
            "operation_id": operation_id,
            "dry_run": true,
            "edit": preview,
        });
        if !structural.is_empty() || !blocked.is_empty() {
            data["structural"] = json!(structural);
            // Say what a dry run can and cannot promise about the structural
            // kinds: the anchor is resolved here, and the properties the new
            // paragraph inherits are chosen when the edit is applied.
            data["note"] = json!(
                "structural edits are previewed by resolving their anchor; the properties an \
                 inserted paragraph inherits are chosen when the edit is applied"
            );
        }
        if !blocked.is_empty() {
            data["blocked_targets"] = json!(blocked);
        }
        return session.success("document_edit", in_surface_vocabulary(data), build_commit);
    }

    // The operation id goes through the args so the store's ledger sees it: a
    // repeat replays before the closure runs, so a stale handle cannot make a
    // retry fail where the first attempt succeeded.
    let mut mutation_args = json!({ "operation_id": operation_id, "edits": args.get("edits") });
    mutation_args["edits"] = args.get("edits").cloned().unwrap_or(Value::Null);
    let applied = session.edit_mutation(
        &mutation_args,
        edits,
        tracked,
        author.clone(),
        date.clone(),
        build_commit,
    );

    let edit_count = args
        .get("edits")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let Some(envelope) = applied.get("structuredContent") else {
        return applied;
    };
    if envelope.get("outcome").and_then(Value::as_str) != Some("success") {
        return applied;
    }
    let data = envelope.get("data").cloned().unwrap_or_else(|| json!({}));
    let edit_outcome = data
        .get("edit")
        .and_then(|edit| edit.get("outcome"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if edit_outcome == "blocked" {
        let reasons: Vec<String> = data
            .get("edit")
            .and_then(|edit| edit.get("blocked_targets"))
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
                edit_count,
                reasons.join("; ")
            ),
            build_commit,
        );
    }
    session.success(
        "document_edit",
        in_surface_vocabulary(json!({
            "operation_id": operation_id,
            "dry_run": false,
            // Replacements report through `edit` (the facade's staged targets);
            // inserts and deletes through `structural`; format edits through
            // `restyles`. A caller that asked for one kind gets the report for it
            // and an explicit null for the others, rather than a field that
            // silently is not there.
            "edit": data.get("edit").cloned().unwrap_or(Value::Null),
            "structural": data.get("structural").cloned().unwrap_or(Value::Null),
            "restyles": data.get("restyles").cloned().unwrap_or(Value::Null),
            "synced": data.get("synced").cloned().unwrap_or(Value::Null),
        })),
        build_commit,
    )
}
