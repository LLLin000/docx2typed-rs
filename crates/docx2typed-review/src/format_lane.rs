//! The adapter between the workdir and the format engine.
//!
//! Everything the engine needs, and nothing it does not: a snapshot (the draft
//! text, the runs that text lives in, the style registry, the baseline digest)
//! and, at build time, the byte ranges of the paragraphs a plan names.
//!
//! Two boundaries this module exists to hold:
//!
//! - **Paragraph identity is decided here and nowhere else.** The engine plans
//!   characters; the materialiser writes bytes; neither knows what `P9` means.
//!   This module maps an id to a byte range, once, and the mapping is used for
//!   the plan, the build and the verification — never re-paired by ordinal after
//!   a build has moved paragraphs around.
//! - **The draft is the coordinate space.** A plan's ranges address the draft:
//!   the template with the text islands and the structural edits applied. The
//!   build applies the plan to that same package, which is what makes the
//!   coordinates mean the same thing on both sides.

use docx2typed_core::prose::{self, ProseLeaf};
use docx2typed_format::materialize::materialize;
use docx2typed_format::{
    plan, FormatIntent, FormatPlan, OriginalRun, ParagraphSnapshot, RangeUnit, Snapshot,
};
use std::collections::BTreeMap;
use std::path::Path;

use crate::{CollaborationError, FORMAT_INTENTS_FILE, FORMAT_PLAN_FILE};

/// Each planned paragraph and the byte range it occupies in a package.
pub type ParagraphRanges = Vec<(String, (usize, usize))>;

/// One format request as the surface resolved it: a paragraph, a character range
/// in the draft, and a style the document carries.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FormatRequest {
    pub paragraph_id: String,
    pub start: usize,
    pub end: usize,
    /// The style the characters take, resolved by the engine. A caller may name
    /// one directly (an id `document_find` reported) or name PROPERTIES and let
    /// the engine find the style the document already carries for them —
    /// `{"bold": true}`. Deriving that from the registry is the engine's work;
    /// an agent reading rPr features to pick an id is doing the engine's job.
    pub style_id: String,
    /// What the caller asked for, when they asked by properties. Audit only: the
    /// plan is derived from `style_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attributes: Option<serde_json::Value>,
}

/// True when every run the span covers already carries the style's properties.
///
/// A whole-set request ("make all of these bold") is satisfied by the ones that
/// already are. Refusing the batch over them made the ordinary case impossible:
/// one blind run could not bold twelve phrases because the first was bold in the
/// source, and the refusal named the attributes rather than the occurrence.
pub fn already_holds(
    workdir: &Path,
    paragraph_id: &str,
    start: usize,
    end: usize,
    style_id: &str,
) -> Result<bool, CollaborationError> {
    let styles = prose::load_style_rprs_for(workdir)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    let Some(style_rpr) = styles.get(style_id) else {
        return Err(CollaborationError::new(
            "format-style-unavailable",
            format!("{style_id} is not a style this document carries"),
        ));
    };
    let package = draft_package(workdir)?;
    let inventory = prose::enumerate_package_bytes(&package)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    let leaves: Vec<&ProseLeaf> = inventory
        .leaves
        .iter()
        .filter(|leaf| leaf.paragraph_id == paragraph_id)
        .collect();
    let mut cursor = 0usize;
    let mut covered = 0usize;
    for leaf in leaves {
        if leaf.text_start == leaf.text_end {
            continue;
        }
        let length = leaf.text.chars().count();
        let (leaf_start, leaf_end) = (cursor, cursor + length);
        cursor = leaf_end;
        if leaf_start < end && leaf_end > start {
            covered += 1;
            // Canonical, not textual: the registry's rPr and a run's raw rPr are
            // the same formatting spelled differently (attribute order,
            // self-closing tags), and comparing the strings reported every
            // already-styled run as needing the edit.
            let same = match (
                prose::canonical_rpr_json(&leaf.rpr),
                prose::canonical_rpr_json(style_rpr),
            ) {
                (Ok(left), Ok(right)) => left == right,
                _ => false,
            };
            if !same {
                return Ok(false);
            }
        }
    }
    Ok(covered > 0)
}

/// The style a set of requested properties resolves to for a span, against the
/// styles the document already carries.
///
/// One style for the span, derived from the FIRST covered run's properties — the
/// same rule the wider engine's `format_span` tool has always used. A property
/// the document has no variant for is `format-style-unavailable`, listing what it
/// does have: the engine never invents a style.
pub fn resolve_style(
    workdir: &Path,
    paragraph_id: &str,
    start: usize,
    end: usize,
    attributes: &serde_json::Map<String, serde_json::Value>,
) -> Result<String, CollaborationError> {
    let package = draft_package(workdir)?;
    let inventory = prose::enumerate_package_bytes(&package)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    let leaves: Vec<&ProseLeaf> = inventory
        .leaves
        .iter()
        .filter(|leaf| leaf.paragraph_id == paragraph_id)
        .collect();
    let mut cursor = 0usize;
    let mut covered: Vec<&ProseLeaf> = Vec::new();
    for leaf in leaves {
        if leaf.text_start == leaf.text_end {
            continue;
        }
        let length = leaf.text.chars().count();
        let (leaf_start, leaf_end) = (cursor, cursor + length);
        cursor = leaf_end;
        if leaf_start < end && leaf_end > start {
            covered.push(leaf);
        }
    }
    let first_covered = covered.first().copied();
    let Some(base) = first_covered else {
        return Err(CollaborationError::new(
            "format-range-invalid",
            format!("[{start}, {end}) of {paragraph_id} covers no editable text"),
        ));
    };
    // A property request derives ONE style from ONE run's properties. If the span
    // covers runs whose properties differ, that derivation would quietly unify
    // them — "make these bold" is not "make these all look like the first one".
    // Refused, with the way out named: a `style_id` replaces the span's formatting
    // deliberately, and `document_find` reports the properties the span covers.
    let covered_properties: Vec<&str> = covered.iter().map(|leaf| leaf.rpr.as_str()).collect();
    let mut distinct: Vec<&str> = Vec::new();
    for properties in &covered_properties {
        if !distinct.contains(properties) {
            distinct.push(properties);
        }
    }
    if distinct.len() > 1 {
        return Err(CollaborationError::new(
            "format-baseline-mixed",
            format!(
                "[{start}, {end}) of {paragraph_id} covers {} runs with {} different sets of \
                 character properties; a property request is derived from one of them and would \
                 unify the rest. Name a `style_id` to replace the span's formatting deliberately, \
                 or make one request per run",
                covered_properties.len(),
                distinct.len()
            ),
        ));
    }
    let available = prose::load_style_rprs_for(workdir)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    let ids: std::collections::BTreeSet<String> = available.keys().cloned().collect();
    let derived = prose::restyle_run_properties(&base.rpr, attributes, &ids).map_err(|error| {
        let message = error.to_string();
        match message.split_once(':') {
            Some((code, detail)) if !code.contains(' ') => {
                CollaborationError::new(code, detail.trim())
            }
            _ => CollaborationError::new("format-style-unavailable", message),
        }
    });
    match derived {
        Ok(restyle) => Ok(restyle.style_id),
        // "Nothing to change" is not a failure to resolve: the span already holds
        // the requested properties, so the style it holds IS the answer. A
        // whole-set request then skips it as satisfied instead of refusing the
        // batch — measured, bolding every 四氧化三铁 failed because the first one
        // was bold in the source.
        Err(error) if error.to_string().starts_with("format-noop") => {
            Ok(docx2typed_core::document_projection::content_style_id(
                prose::canonical_rpr_json(&base.rpr)
                    .unwrap_or_default()
                    .as_bytes(),
            ))
        }
        Err(error) => Err(error),
    }
}

/// The requests a workdir has accumulated, in the order they were made.
pub fn load_format_requests(workdir: &Path) -> Result<Vec<FormatRequest>, CollaborationError> {
    let path = workdir.join(FORMAT_INTENTS_FILE);
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let bytes = std::fs::read(&path)
        .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    let requests = value
        .get("requests")
        .and_then(|requests| requests.as_array())
        .ok_or_else(|| {
            CollaborationError::new("workdir-invalid", "format_intents.json has no requests")
        })?;
    let mut out = Vec::with_capacity(requests.len());
    for request in requests {
        out.push(serde_json::from_value(request.clone()).map_err(|error| {
            CollaborationError::new(
                "workdir-invalid",
                format!("invalid format request: {error}"),
            )
        })?);
    }
    Ok(out)
}

/// Add a request (idempotent: the same span and style twice is one request) and
/// re-derive the plan from the whole set.
///
/// The plan is a pure function of the requests and the draft, so re-deriving is
/// not a second opinion: it is the same answer, computed once more from inputs
/// that may have grown.
pub fn plan_format(
    workdir: &Path,
    requests: &[FormatRequest],
) -> Result<(FormatPlan, Snapshot), CollaborationError> {
    // A plan's coordinates are the DRAFT's, so the draft has to exist before one
    // can be derived — a workdir straight out of `extract` has none, and asking
    // for a revision token there used to be a refusal rather than a projection.
    crate::draft::ensure_projection(workdir)?;
    let snapshot = snapshot_for(workdir, requests)?;
    let intents: Vec<FormatIntent> = requests
        .iter()
        .map(|request| FormatIntent::SetStyle {
            paragraph_id: request.paragraph_id.clone(),
            range: docx2typed_format::DraftRange::new(request.start, request.end),
            style_id: request.style_id.clone(),
        })
        .collect();
    let plan = plan(&snapshot, &intents)
        .map_err(|error| CollaborationError::new(error.code, error.detail))?;
    Ok((plan, snapshot))
}

/// Record one request: dedupe it against the workdir's set, re-derive the plan
/// from the whole set, and persist both.
///
/// The surface calls this and reads the plan's structure back for its reply. It
/// makes no format decision of its own — no style check, no overlap rule, no run
/// arithmetic: those live in the engine, and a caller that re-implemented one of
/// them would be the second authority the split exists to remove.
pub fn record_format_request(
    workdir: &Path,
    request: FormatRequest,
) -> Result<(FormatPlan, Snapshot, Vec<FormatRequest>), CollaborationError> {
    let mut requests = load_format_requests(workdir)?;
    // A request for the same characters SUPERSEDES the earlier one: the caller
    // changed their mind, which is not a conflict. Keeping both made the plan
    // report a span as overlapping itself — `[126, 131) and [126, 131) overlap in
    // P13 and name different styles` — with no way to withdraw either.
    let same_span = |existing: &FormatRequest| {
        existing.paragraph_id == request.paragraph_id
            && existing.start == request.start
            && existing.end == request.end
    };
    if let Some(index) = requests.iter().position(same_span) {
        requests[index] = request;
    } else {
        requests.push(request);
    }
    let (plan, snapshot) = plan_format(workdir, &requests)?;
    save_format_requests(workdir, &requests)?;
    save_format_plan(workdir, &plan)?;
    // The projection is what the read side publishes, so it has to carry the
    // plan's effect: otherwise a caller records a format edit and every read
    // afterwards still reports the old properties, which is how a blind run
    // concluded five times that its restyle "did nothing".
    crate::draft::apply_projection(workdir)?;
    Ok((plan, snapshot, requests))
}

/// The snapshot a plan was derived from, rebuilt from the workdir and checked
/// against the plan's digest. A plan made against another document is refusable
/// rather than silently applied.
pub fn snapshot_for_plan(
    workdir: &Path,
    plan: &FormatPlan,
) -> Result<Snapshot, CollaborationError> {
    let paragraph_ids: Vec<String> = plan
        .paragraphs
        .iter()
        .map(|paragraph| paragraph.paragraph_id.clone())
        .collect();
    let requests: Vec<FormatRequest> = paragraph_ids
        .iter()
        .map(|paragraph_id| FormatRequest {
            paragraph_id: paragraph_id.clone(),
            start: 0,
            end: 1,
            attributes: None,
            style_id: plan
                .paragraphs
                .iter()
                .find(|paragraph| &paragraph.paragraph_id == paragraph_id)
                .and_then(|paragraph| paragraph.segments.first())
                .map(|segment| segment.style_id.clone())
                .unwrap_or_default(),
        })
        .collect();
    let snapshot = snapshot_for(workdir, &requests)?;
    if snapshot.base_digest != plan.base_digest {
        return Err(CollaborationError::new(
            "format-plan-stale",
            format!(
                "this plan was derived from {} and the workdir now holds {}; re-plan before \
                 building",
                plan.base_digest, snapshot.base_digest
            ),
        ));
    }
    Ok(snapshot)
}

/// Persist the requests and the plan they derive.
pub fn save_format_requests(
    workdir: &Path,
    requests: &[FormatRequest],
) -> Result<(), CollaborationError> {
    let path = workdir.join(FORMAT_INTENTS_FILE);
    if requests.is_empty() {
        if path.is_file() {
            std::fs::remove_file(&path).map_err(|error| {
                CollaborationError::new("workdir-unreadable", error.to_string())
            })?;
        }
        return Ok(());
    }
    let value = serde_json::json!({
        "schema": FORMAT_INTENTS_SCHEMA,
        "unit": "unicode-scalars",
        "requests": requests,
    });
    let mut json = serde_json::to_string_pretty(&value)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    json.push('\n');
    std::fs::write(&path, json)
        .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))
}

pub const FORMAT_INTENTS_SCHEMA: &str = "docx2typed-format-intents-1";

/// Persist the derived plan, which is what the build executes and the verifier
/// checks. Regenerable from the requests; stored so neither of them has to
/// re-derive the rules.
pub fn save_format_plan(workdir: &Path, plan: &FormatPlan) -> Result<(), CollaborationError> {
    let path = workdir.join(FORMAT_PLAN_FILE);
    std::fs::write(&path, docx2typed_format::materialize::to_json(plan))
        .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))
}

/// The plan a workdir carries, if any.
pub fn load_format_plan(workdir: &Path) -> Result<Option<FormatPlan>, CollaborationError> {
    let path = workdir.join(FORMAT_PLAN_FILE);
    if !path.is_file() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path)
        .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))?;
    let plan: FormatPlan = serde_json::from_slice(&bytes)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    Ok(Some(plan))
}

/// The package a plan's coordinates are measured in: the template with the text
/// islands and the structural edits applied, and no formatting yet.
pub fn draft_package(workdir: &Path) -> Result<Vec<u8>, CollaborationError> {
    let islands = prose::load_islands(workdir)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    let structure = prose::load_structure(workdir)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    let built = prose::apply_build(&workdir.join("_template.docx"), &islands, &structure)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    Ok(built.package)
}

/// The package the workdir is at RIGHT NOW: the template with the text islands,
/// the structural edits and the format plan applied.
///
/// Not the plan's input (`draft_package`) — that one has no formatting, which is
/// what makes planning a pure function. This one is what a build would write, so
/// anything that reports "what the document holds" reads it.
pub fn current_package(workdir: &Path) -> Result<Vec<u8>, CollaborationError> {
    let package = draft_package(workdir)?;
    let Some(plan) = load_format_plan(workdir)? else {
        return Ok(package);
    };
    let snapshot = snapshot_for_plan(workdir, &plan)?;
    apply_format_plan(&package, &plan, &snapshot)
}

/// Build the snapshot the engine plans against.
fn snapshot_for(
    workdir: &Path,
    requests: &[FormatRequest],
) -> Result<Snapshot, CollaborationError> {
    let package = draft_package(workdir)?;
    let inventory = prose::enumerate_package_bytes(&package)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    let styles = prose::load_style_rprs_for(workdir)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;

    let mut paragraph_ids: Vec<String> = Vec::new();
    for request in requests {
        if !paragraph_ids.contains(&request.paragraph_id) {
            paragraph_ids.push(request.paragraph_id.clone());
        }
    }
    let mut paragraphs: Vec<ParagraphSnapshot> = Vec::new();
    for paragraph_id in paragraph_ids {
        let leaves: Vec<&ProseLeaf> = inventory
            .leaves
            .iter()
            .filter(|leaf| leaf.paragraph_id == paragraph_id)
            .collect();
        if leaves.is_empty() {
            return Err(CollaborationError::new(
                "format-paragraph-unknown",
                format!("{paragraph_id} is not in this document"),
            ));
        }
        // v1 scope, refused rather than guessed: a paragraph holding a revision
        // gap has a coordinate space of its own, and a paragraph that is not a
        // plain `P{n}` is one this adapter cannot map to a byte range.
        if leaves.iter().any(|leaf| !leaf.projected) {
            return Err(CollaborationError::new(
                "format-paragraph-unsupported",
                format!(
                    "{paragraph_id} holds a revision gap (a tracked deletion); character-level \
                     formatting there is not supported yet"
                ),
            ));
        }
        let draft_text: String = leaves.iter().map(|leaf| leaf.text.as_str()).collect();
        let runs: Vec<OriginalRun> = leaves
            .iter()
            .enumerate()
            .map(|(index, leaf)| OriginalRun {
                index,
                text: leaf.text.clone(),
                rpr: leaf.rpr.clone(),
            })
            .collect();
        paragraphs.push(ParagraphSnapshot {
            paragraph_id,
            draft_text,
            runs,
        });
    }

    Ok(Snapshot {
        generation_id: current_revision(workdir)?,
        base_digest: base_digest(workdir)?,
        styles,
        paragraphs,
    })
}

/// The draft revision this plan is derived from — the same token a caller's
/// handles are bound to.
fn current_revision(workdir: &Path) -> Result<String, CollaborationError> {
    Ok(docx2typed_core::edit_state::classify_edit_state(workdir)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?
        .edit_body_sha256)
}

/// What the plan was derived from: the template, the two structural sidecars and
/// the registry. A build against another one is refusable rather than silently
/// applied.
fn base_digest(workdir: &Path) -> Result<String, CollaborationError> {
    let mut parts: Vec<(String, Vec<u8>)> = Vec::new();
    for name in [
        "_template.docx",
        "islands.json",
        "structure.json",
        "styles.json",
    ] {
        let path = workdir.join(name);
        let bytes = if path.is_file() {
            std::fs::read(&path)
                .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))?
        } else {
            Vec::new()
        };
        parts.push((name.to_string(), bytes));
    }
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    for (name, bytes) in &parts {
        hasher.update(name.as_bytes());
        hasher.update(b"\0");
        hasher.update(bytes);
        hasher.update(b"\0");
    }
    Ok(hex::encode(hasher.finalize()))
}

/// The byte range of each planned paragraph, inside `package`.
///
/// The mapping is computed from the package the plan was made against, so an id
/// that moved — a paragraph inserted before it — resolves to where the plan
/// meant, not to whatever now sits at that ordinal.
pub fn paragraph_ranges(
    package: &[u8],
    plan: &FormatPlan,
) -> Result<ParagraphRanges, CollaborationError> {
    let inventory = prose::enumerate_package_bytes(package)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    let part = member_bytes(package, &prose::part_path("document"))?;
    let tags = docx2typed_format::xml::scan_tags(&part);
    let mut out = Vec::new();
    for planned in &plan.paragraphs {
        // The range comes from the ENGINE's own inventory — the leaves it says
        // belong to this paragraph id — never from a second enumeration of `w:p`
        // elements. A naive tag scan disagrees with the inventory as soon as a
        // paragraph is nested (a table cell, a box, a content control), and the
        // mismatch lands the plan on a different paragraph: measured, a restyle
        // addressed to P18 was applied to whatever a tag scan counts as the
        // eighteenth, and the build refused `format-snapshot-stale` naming two
        // different texts.
        let leaves: Vec<&ProseLeaf> = inventory
            .leaves
            .iter()
            .filter(|leaf| leaf.paragraph_id == planned.paragraph_id && leaf.part_key == "document")
            .collect();
        if leaves.is_empty() {
            return Err(CollaborationError::new(
                "format-paragraph-unknown",
                format!("{} is not in the package", planned.paragraph_id),
            ));
        }
        let first = leaves
            .iter()
            .map(|leaf| leaf.text_start)
            .min()
            .unwrap_or_default();
        let last = leaves
            .iter()
            .map(|leaf| leaf.text_end)
            .max()
            .unwrap_or_default();
        let start = tags
            .iter()
            .filter(|tag| tag.name == "r" && !tag.closing && tag.start <= first)
            .map(|tag| tag.start)
            .next_back()
            .ok_or_else(|| {
                CollaborationError::new(
                    "format-paragraph-unsupported",
                    format!("{}: no run holds its first text", planned.paragraph_id),
                )
            })?;
        let end = tags
            .iter()
            .filter(|tag| tag.name == "r" && tag.closing && tag.end >= last)
            .map(|tag| tag.end)
            .next()
            .ok_or_else(|| {
                CollaborationError::new(
                    "format-paragraph-unsupported",
                    format!("{}: no run holds its last text", planned.paragraph_id),
                )
            })?;
        out.push((planned.paragraph_id.clone(), (start, end)));
    }
    Ok(out)
}

/// One member's bytes, read through the crate's own zip reader.
pub fn member_bytes(package: &[u8], member: &str) -> Result<Vec<u8>, CollaborationError> {
    use std::io::Read;
    let reader = std::io::Cursor::new(package.to_vec());
    let mut archive = zip::ZipArchive::new(reader)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    let mut file = archive
        .by_name(member)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))?;
    Ok(bytes)
}

/// Apply a plan to the draft package: the single run-splitting and property-
/// writing path in the tree.
pub fn apply_format_plan(
    package: &[u8],
    plan: &FormatPlan,
    snapshot: &Snapshot,
) -> Result<Vec<u8>, CollaborationError> {
    let mut ranges = paragraph_ranges(package, plan)?;
    // Back to front: every paragraph is materialised into the CURRENT part, and a
    // splice moves everything after it. Ascending order made the second
    // paragraph's range address the wrong bytes — measured as
    // `format-paragraph-has-no-runs` on a plan that covered twelve of them.
    ranges.sort_by_key(|(_, range)| std::cmp::Reverse(range.0));
    let mut current = package.to_vec();
    for (paragraph_id, range) in ranges {
        // Every paragraph is materialised against the SAME package: the ranges
        // were computed from it, and one paragraph's rewrite must not move
        // another's offsets before it is used.
        let part = prose::part_path("document");
        let part_bytes = member_bytes(&current, &part)?;
        let rewritten = materialize(&part_bytes, range, plan, snapshot, &paragraph_id)
            .map_err(|error| CollaborationError::new(error.code, error.detail))?;
        current = prose::patch_zip_member(&current, &part, &rewritten)
            .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    }
    Ok(current)
}

/// The registry, as the engine's snapshot type wants it.
pub fn style_registry(workdir: &Path) -> Result<BTreeMap<String, String>, CollaborationError> {
    prose::load_style_rprs_for(workdir)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))
}

/// The declared unit, exposed so a caller never has to infer it.
pub const RANGE_UNIT: RangeUnit = RangeUnit::UnicodeScalars;
