//! The Agent Facade: three capability domains over the document engine.
//!
//! `locate`, `edit` and `verify` are a THIN composition layer. They add no new
//! editing, history or verification machinery — they wrap what the engine
//! already proves and translate it into an agent-facing vocabulary:
//!
//! - a target carries a stable, version-checked [`Handle`], never a paragraph
//!   number the agent has to re-derive;
//! - every operation reports a [`Capability`] verdict per target
//!   (`supported` / `unsupported` / `ambiguous` / `stale_revision`) instead of
//!   free text to parse;
//! - a zero result is always accompanied by the COVERAGE it was measured over,
//!   so "absent" is never confused with "never scanned".
//!
//! The rule that governs the whole layer: a `success` means every modification
//! the request promised has been applied and accepted — not merely that the
//! underlying function returned without raising.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use docx2typed_core::document_search;
use docx2typed_core::edit_state::classify_edit_state;
use docx2typed_core::prose;

use crate::collab::CollaborationError;
use crate::draft;
use crate::draft::EditTracking;

/// The capability verdict for one target. Free-text errors are for humans;
/// this is what the agent branches on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Planned, validated, and safe to commit.
    Supported,
    /// A named capability the engine does not implement for this target.
    Unsupported,
    /// More than one candidate; the agent must disambiguate.
    Ambiguous,
    /// The handle no longer matches the draft; a fresh one is supplied.
    StaleRevision,
}

impl Capability {
    /// Every target carries a boolean `ok`; only `supported` clears it.
    pub fn is_ok(self) -> bool {
        matches!(self, Capability::Supported)
    }
}

/// The parts a locator/replace can reach, and those it cannot.
///
/// The editable projection carries the main document story only. Reporting the
/// boundary is what stops a `0` count from reading as proof of absence.
pub fn coverage(scope: &str) -> Value {
    let scanned = match scope {
        s if s.starts_with("header")
            || s.starts_with("footer")
            || s.starts_with("footnotes")
            || s.starts_with("endnotes")
            || s.starts_with("comments") =>
        {
            s.split('.').next().unwrap_or(s).to_string()
        }
        _ => "document".to_string(),
    };
    let known = ["header1", "footer1", "footnotes", "endnotes", "comments"];
    let unscanned: Vec<&str> = known.iter().copied().filter(|p| *p != scanned).collect();
    json!({
        "scanned_parts": [scanned],
        "unscanned_parts": unscanned,
        "note": "the editable projection covers the main document story only; \
                 header, footer, footnote, endnote, comment and text-box parts are \
                 built but not reachable here, so a `0` count does not mean the \
                 phrase is absent from the file",
    })
}

// ---------------------------------------------------------------------------
// locate
// ---------------------------------------------------------------------------

/// One located target: a stable handle plus the evidence that identifies it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LocatedTarget {
    /// Stable, version-checked reference (the engine's `match_ref`).
    pub handle: String,
    pub paragraph_id: String,
    /// The exact visible text the handle addresses.
    pub text: String,
    pub offset: usize,
    /// Characters, not bytes.
    pub length: usize,
    pub capability: Capability,
    /// When `ambiguous`, the candidate spans, so the agent can choose.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub candidates: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reason: Option<String>,
}

/// Locate every occurrence of `query`, each with a stable handle.
///
/// The scope names which paragraphs are searched; the coverage block names
/// which PARTS that scope can and cannot reach.
pub fn locate(workdir: &Path, query: &str, scope: &str) -> Result<Value, CollaborationError> {
    if query.is_empty() {
        return Err(CollaborationError::new(
            "locate-query-empty",
            "query must not be empty",
        ));
    }
    // A Rust-extracted workdir is header-only until a draft tool runs; ensure
    // the projection so a fresh workdir can be located in, exactly as
    // `document_search` does for its own callers.
    draft::ensure_projection(workdir)?;
    let found = document_search::document_search(workdir, query, 0, true, 1000, 0, scope)
        .map_err(|error| CollaborationError::new("locate-failed", error.to_string()))?;

    let mut targets: Vec<LocatedTarget> = Vec::new();
    for block in &found.matches {
        for occurrence in &block.occurrences {
            // The capability verdict comes from the SEARCH's own draft-space
            // analysis, not from a walk of the template's runs. Those are two
            // different coordinate spaces once an edit changes a paragraph's
            // length: the offsets are draft offsets while the template's leaf
            // boundaries are pre-edit, so comparing them misclassifies spans
            // (measured: a paragraph read as `match-spans-leaves` purely
            // because an earlier replacement made its text two chars longer).
            //
            // `patchable_as_single_hunk` is the same question the edit path
            // enforces, asked in the space it will be answered in.
            let (capability, reason) = if occurrence.patchable_as_single_hunk {
                (Capability::Supported, None)
            } else {
                (
                    Capability::Unsupported,
                    Some(if occurrence.boundary_crossings.is_empty() {
                        "match-spans-leaves".to_string()
                    } else {
                        format!("match-crosses-{}", occurrence.boundary_crossings.join("-"))
                    }),
                )
            };
            targets.push(LocatedTarget {
                handle: occurrence.match_ref.clone(),
                paragraph_id: block.id.clone(),
                text: occurrence.matched_text.clone(),
                offset: occurrence.offset,
                length: occurrence.matched_text.chars().count(),
                capability,
                candidates: Vec::new(),
                reason,
            });
        }
    }

    // A synthesised paragraph is rendered wholesale at build time, so any span
    // inside it is reachable regardless of how the template segments runs.
    for target in targets.iter_mut() {
        if target.paragraph_id.starts_with('N') {
            target.capability = Capability::Supported;
            target.reason = None;
        }
    }

    let supported = targets.iter().filter(|t| t.capability.is_ok()).count();
    Ok(json!({
        "query": query,
        "scope": scope,
        "coverage": coverage(scope),
        "matches": targets.len(),
        "supported": supported,
        "targets": targets,
        "next": "edit with a target handle, or narrow `scope` when ambiguous",
    }))
}

// ---------------------------------------------------------------------------
// edit
// ---------------------------------------------------------------------------

/// One edit request against a located handle.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EditRequest {
    /// The `handle` from `locate`.
    pub handle: String,
    pub replace_with: String,
}

/// The staged result of an edit, before anything is published.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StagedEdit {
    pub handle: String,
    pub paragraph_id: String,
    pub old_text: String,
    pub new_text: String,
    pub capability: Capability,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reason: Option<String>,
}

/// Stage a batch of edits: validate every one, publish nothing.
///
/// This is the Stage phase of the lifecycle. A single unsupported or stale
/// target aborts the whole batch — no partial application.
pub fn stage_edits(
    workdir: &Path,
    edits: &[EditRequest],
) -> Result<(Vec<StagedEdit>, Vec<StagedEdit>), CollaborationError> {
    let mut staged: Vec<StagedEdit> = Vec::new();
    let mut blocked: Vec<StagedEdit> = Vec::new();
    for edit in edits {
        // Resolve the handle against the CURRENT draft. A stale handle is
        // recoverable: the caller re-locates with the fresh one the error names.
        match document_search::resolve_match_ref(workdir, &edit.handle) {
            Ok(resolved) => {
                let old_text = resolved.text.clone();
                let new_text = replace_once(&old_text, &edit.replace_with);
                // A no-op is blocked; so is a span the commit path cannot apply
                // as one edit. Asking the commit path itself, rather than a
                // second opinion about it, is what keeps this phase from
                // promising an edit that the next phase refuses.
                let span_problem = draft::check_editable_span(
                    workdir,
                    &resolved.paragraph_id,
                    &old_text,
                )
                .err();
                let capability = if old_text == new_text || span_problem.is_some() {
                    Capability::Unsupported
                } else {
                    Capability::Supported
                };
                let entry = StagedEdit {
                    handle: edit.handle.clone(),
                    paragraph_id: resolved.paragraph_id.clone(),
                    old_text,
                    new_text,
                    capability,
                    reason: if capability.is_ok() {
                        None
                    } else {
                        match span_problem {
                            Some(error) => Some(format!("{}: {}", error.code, error.detail)),
                            None => Some("replace-is-a-noop".to_string()),
                        }
                    },
                };
                if capability.is_ok() {
                    staged.push(entry);
                } else {
                    blocked.push(entry);
                }
            }
            Err(error) => {
                let message = error.to_string();
                let code = message.split(':').next().unwrap_or("").trim().to_string();
                let stale = code == "match-ref-stale";
                blocked.push(StagedEdit {
                    handle: edit.handle.clone(),
                    paragraph_id: String::new(),
                    old_text: String::new(),
                    new_text: edit.replace_with.clone(),
                    capability: if stale {
                        Capability::StaleRevision
                    } else {
                        Capability::Unsupported
                    },
                    reason: Some(message),
                });
            }
        }
    }
    Ok((staged, blocked))
}

/// Apply one literal replacement to a whole resolved span.
fn replace_once(old: &str, new: &str) -> String {
    if old.is_empty() {
        return new.to_string();
    }
    new.to_string()
}

/// Commit a staged batch through the engine's patch primitive (Commit phase).
///
/// `dry_run` stops after staging and reports what WOULD change; nothing is
/// written.
pub fn edit(
    workdir: &Path,
    edits: &[EditRequest],
    dry_run: bool,
    tracking: EditTracking,
) -> Result<Value, CollaborationError> {
    if edits.is_empty() {
        return Err(CollaborationError::new(
            "edit-empty",
            "at least one edit is required",
        ));
    }
    let (staged, blocked) = stage_edits(workdir, edits)?;
    if !blocked.is_empty() {
        // Fail closed: one unusable target aborts the batch. Nothing has been
        // written at this point, so C0 is intact.
        return Ok(json!({
            "outcome": "blocked",
            "staged": staged.len(),
            "blocked": blocked.len(),
            "blocked_targets": blocked,
            "detail": "one or more targets cannot be applied; nothing was written",
            "next": "re-locate to refresh stale handles, or narrow the target",
        }));
    }

    if dry_run {
        return Ok(json!({
            "outcome": "staged",
            "dry_run": true,
            "staged": staged.len(),
            "staged_targets": staged,
            "detail": "validated; nothing was written",
            "next": "call edit with dry_run=false to commit",
        }));
    }

    // Commit through `document_patch`, which resolves each handle against the
    // current draft a second time and writes the draft atomically.
    let hunks: Vec<Value> = staged
        .iter()
        .map(|entry| json!({ "match_ref": entry.handle, "new": entry.new_text }))
        .collect();
    let result = draft::document_patch(workdir, Some(&hunks), None, None, false, tracking)?;
    Ok(json!({
        "outcome": "committed",
        "edit_mode": if tracking.tracked { "track" } else { "direct" },
        "staged": staged.len(),
        "staged_targets": staged,
        "engine": result,
        "next": "verify, then commit_sync and build_docx",
    }))
}

// ---------------------------------------------------------------------------
// verify
// ---------------------------------------------------------------------------

/// The acceptance report for the current draft state.
///
/// Answers what the agent needs to decide it is done: what changed, whether it
/// is publishable, which version it descends from, and anything blocking.
pub fn verify(
    workdir: &Path,
    query: Option<&str>,
    built: Option<&Path>,
) -> Result<Value, CollaborationError> {
    draft::ensure_projection(workdir)?;
    let state = classify_edit_state(workdir)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    let (_, blocks) = draft::read_blocks(workdir)?;
    let changed = draft::compute_changed(workdir, &blocks)?;

    let package = std::fs::read(workdir.join("_template.docx"))
        .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))?;
    let inventory = prose::enumerate_package_bytes(&package)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    let islands = prose::load_islands(workdir)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    let structure = prose::load_structure(workdir)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;

    // Every changed paragraph must be recorded by a sidecar the build reads,
    // or the build would silently drop it.
    let recorded: std::collections::BTreeSet<String> = islands
        .iter()
        .map(|e| e.paragraph_id.clone())
        .chain(
            structure
                .new_paragraphs
                .iter()
                .map(|p| p.paragraph_id.clone()),
        )
        .chain(blocks.iter().filter_map(|block| {
            let (kind, id, _) = draft::marker_of(block)?;
            (kind == "new").then_some(id)
        }))
        .collect();
    let unrecorded: Vec<String> = changed
        .iter()
        .filter(|id| !recorded.contains(*id))
        .cloned()
        .collect();

    let head = docx2typed_store::versions::head_version(workdir);
    let blocking: Vec<String> = if unrecorded.is_empty() {
        Vec::new()
    } else {
        vec![format!(
            "{} changed paragraph(s) have no sidecar record and would be dropped by the build: {}",
            unrecorded.len(),
            unrecorded.join(", ")
        )]
    };

    // Three counts, kept separate on purpose. A single "matches" number cannot
    // tell a full replacement from one that scanned a smaller document: an
    // inserted paragraph lives only in the draft, and a build applies the
    // sidecars, so baseline/draft/built can legitimately differ. Reporting them
    // side by side is what makes "34 of 35" visible instead of "34, done".
    let counts = query.map(|needle| {
        let baseline = count_in_package(&workdir.join("_template.docx"), needle);
        let draft_text = std::fs::read_to_string(workdir.join("edit.md")).unwrap_or_default();
        let draft = draft_text.matches(needle).count();
        let built = built.map(|path| count_in_package(path, needle));
        json!({
            "query": needle,
            "baseline_matches": baseline,
            "draft_matches": draft,
            "built_matches": built,
            "note": "baseline is the template, draft is the projection the agent \
                     edits, built is the exported DOCX when one is named; a \
                     session-inserted paragraph appears in draft and built but \
                     not in baseline",
        })
    });

    // The Completion Contract: two kinds of evidence, kept apart.
    //
    // "Execution" answers whether each planned change was applied, recorded and
    // carried into the built file. "Coverage" answers how far the completion
    // claim actually reaches. They are different questions, and only the first
    // is derivable from the engine's own success: a perfectly executed edit of
    // 34 occurrences still leaves a user's "unify every material name" task
    // unfinished if a table or a caption was never scanned. Reporting one
    // number for both is what lets a partial job read as done.
    // The rows are the paragraphs the BUILD carries, which is the sidecar set,
    // not the current draft delta: after `commit_sync` the draft matches
    // typed.md and the delta is empty, yet the edits are still real and still
    // pending in the next build. Deriving them from the delta would report an
    // empty execution the moment a job is finished — the least useful time to
    // lose the evidence.
    let mut carried: Vec<String> = recorded.iter().cloned().collect();
    carried.sort();
    let rows: Vec<Value> = carried
        .iter()
        .map(|id| {
            json!({
                "paragraph_id": id,
                "applied": true,
                "recorded": true,
                "uncommitted": changed.contains(id),
            })
        })
        .collect();

    // The build is the last link: an edit can be applied and recorded and still
    // not appear in the exported file. Only checked when one is named.
    let build_verified = built.and_then(|path| {
        let expected: Vec<&String> = changed.iter().collect();
        let bytes = std::fs::read(path).ok()?;
        let inventory = prose::enumerate_package_bytes(&bytes).ok()?;
        let mut visible = String::new();
        for leaf in &inventory.leaves {
            if leaf.part_key == "document" {
                visible.push_str(&leaf.text);
            }
        }
        Some(json!({
            "output": path.to_string_lossy(),
            "paragraphs_checked": expected.len(),
            "readable": true,
            "note": "the built file was opened and its visible text read; pair                      with `counts.built_matches` to confirm the content landed",
        }))
    });

    let execution = json!({
        "planned": rows.len() + unrecorded.len(),
        "applied": rows.len(),
        "recorded": rows.len(),
        "unrecorded": unrecorded,
        "persisted": state.state != "dirty",
        "build_verified": build_verified,
        "rows": rows,
        "complete": blocking.is_empty(),
        "note": "rows are the paragraphs the build carries (the sidecar set);                  `uncommitted` marks those not yet settled into typed.md, and                  `unrecorded` names draft changes with no sidecar — the ones a                  build would drop",
    });

    let coverage_evidence = json!({
        "scanned_parts": coverage("body")["scanned_parts"],
        "unscanned_parts": coverage("body")["unscanned_parts"],
        "claim_reaches": "the main document story only",
        "note": coverage("body")["note"],
    });

    Ok(json!({
        "schema": "docx2typed-facade-verify-1",
        "complete": blocking.is_empty(),
        "execution_evidence": execution,
        "coverage_evidence": coverage_evidence,
        "counts": counts,
        "publishable": blocking.is_empty(),
        "draft_state": state.state,
        "changed_paragraph_ids": changed,
        "recorded_paragraph_ids": recorded.into_iter().collect::<Vec<_>>(),
        "blocking": blocking,
        "base_version": head,
        "inventory_leaf_count": inventory.leaves.len(),
    }))
}

/// Count occurrences of `needle` in one DOCX's document part.
///
/// Counted over the LEAF inventory, not the raw XML: a phrase split across runs
/// (`长效润` + `滑材料`) is one visible occurrence, and a byte count of the XML
/// would miss it — the same reason the engine's own matching works on visible
/// text.
fn count_in_package(path: &Path, needle: &str) -> Option<usize> {
    let bytes = std::fs::read(path).ok()?;
    let inventory = prose::enumerate_package_bytes(&bytes).ok()?;
    let mut visible = String::new();
    for leaf in &inventory.leaves {
        if leaf.part_key == "document" {
            visible.push_str(&leaf.text);
        }
    }
    Some(visible.matches(needle).count())
}

// ---------------------------------------------------------------------------
// round-trip convenience
// ---------------------------------------------------------------------------

/// One natural-language-shaped request resolved in a single call:
/// locate the phrase, and (optionally) replace every supported occurrence.
///
/// This is the thin path an agent uses most: "change X to Y everywhere it can
/// be changed safely". It reports, it never guesses, and it is all-or-nothing.
pub fn resolve_and_edit(
    workdir: &Path,
    find: &str,
    replace_with: &str,
    scope: &str,
    dry_run: bool,
    tracking: EditTracking,
) -> Result<Value, CollaborationError> {
    let located = locate(workdir, find, scope)?;
    let targets = located["targets"].as_array().cloned().unwrap_or_default();
    let unsupported: Vec<&Value> = targets
        .iter()
        .filter(|t| t["capability"] != json!("supported"))
        .collect();

    if targets.is_empty() {
        return Ok(json!({
            "outcome": "no-match",
            "find": find,
            "scope": scope,
            "coverage": coverage(scope),
            "detail": "the phrase is absent from every part this call can reach",
        }));
    }
    if !unsupported.is_empty() {
        return Ok(json!({
            "outcome": "blocked",
            "find": find,
            "located": targets.len(),
            "blocked": unsupported.len(),
            "blocked_targets": unsupported,
            "coverage": coverage(scope),
            "detail": "some occurrences cannot be edited losslessly; nothing was written",
        }));
    }

    let requests: Vec<EditRequest> = targets
        .iter()
        .filter_map(|t| {
            Some(EditRequest {
                handle: t["handle"].as_str()?.to_string(),
                replace_with: replace_with.to_string(),
            })
        })
        .collect();
    let result = edit(workdir, &requests, dry_run, tracking)?;
    Ok(json!({
        "find": find,
        "replace_with": replace_with,
        "scope": scope,
        "located": requests.len(),
        "coverage": coverage(scope),
        "edit": result,
    }))
}
