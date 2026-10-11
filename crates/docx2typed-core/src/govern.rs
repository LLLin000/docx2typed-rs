//! Governed document workflows (issue #59): tracked-revision inventory and
//! accept/reject views, single-revision settlement and reinsertion,
//! comment inventory and byte-surgery deletion, body-table structure
//! surgery, and the read-only Unicode normalization audit.
//!
//! Mirrors the Python Reference at the byte level:
//! - `scripts/edit_sync.py` `render_revisions_json` /
//!   `collect_document_revisions` (revision inventory, `revision_key`,
//!   fingerprint = `sha256(text)[:12]`),
//! - `scripts/typed_core.py` `apply_revision_decision` /
//!   `reinsert_deleted_text` (unwrap/remove settlement, new insert after a
//!   deletion) and `scripts/typed_docx.py` `settle_xml_revisions`
//!   (byte-level unwrap/remove with `w:delText` -> `w:t`),
//! - `scripts/decisions.py` `_delete_comment` (comment entry + anchors +
//!   references removed; everything else replays verbatim),
//! - `scripts/typed_docx.py` `apply_table_operation` (structure-only row/
//!   column/merge/split bytes synthesized from the template; cell text is
//!   never rewritten),
//! - `scripts/typed_normalize.py` `find_candidates` (read-only Unicode
//!   vertical-catalog scan; no mutation, no standalone normalize surface).
//!
//! Deep-module contract (issue #36): core owns the byte machinery and
//! returns declarative results; the App orchestrates store generations,
//! verification, and evidence.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::prose::{self, part_key_from_path};
use crate::xml_walker::{scan_tags, Tag};
use crate::CoreError;

pub const REVISIONS_SCHEMA: &str = "typed-revisions-1";
pub const COMMENT_SCHEMA: &str = "docx2typed-comments-inventory-1";
pub const VIEW_SCHEMA: &str = "docx2typed-revision-view-1";
pub const AUDIT_SCHEMA: &str = "docx2typed-unicode-audit-1";

/// Comment parts beside `word/comments.xml` that reference comment ids.
const EXTENDED_COMMENT_PARTS: [&str; 3] = [
    "word/commentsExtended.xml",
    "word/commentsIds.xml",
    "word/commentsExtensible.xml",
];

/// Revision element local names -> Python inventory kinds.
pub fn revision_kind(local: &str) -> Option<&'static str> {
    match local {
        "ins" => Some("insert"),
        "del" => Some("delete"),
        "moveFrom" => Some("move_from"),
        "moveTo" => Some("move_to"),
        _ => None,
    }
}

/// Content fingerprint of a revision (mirror of `revision_fingerprint`:
/// SHA-256 of the joined text, first 12 hex chars).
pub fn revision_fingerprint(text: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(text.as_bytes()))[..12].to_string()
}

/// The stable revision key `<part>|<kind>|<w_id>|<fingerprint>` (mirror of
/// `edit_sync._revision_key`).
pub fn revision_key(part: &str, kind: &str, w_id: &str, text: &str) -> String {
    format!("{part}|{kind}|{w_id}|{}", revision_fingerprint(text))
}

/// One tracked revision of the inventory (mirror of the entries
/// `render_revisions_json` emits).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RevisionEntry {
    pub part: String,
    pub kind: String,
    pub w_id: String,
    pub author: String,
    pub date: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paragraph_id: Option<String>,
    pub editable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

impl RevisionEntry {
    pub fn revision_key(&self) -> String {
        revision_key(&self.part, &self.kind, &self.w_id, &self.text)
    }
}

/// The `revisions.md` sidecar: the same inventory, for a reader.
///
/// Pure derivation of `revisions_sidecar_value`'s input — no second walk, no
/// second classifier. The reference regenerates this together with
/// `revisions.json`, so a consumer may rely on the pair moving as one.
pub fn revisions_md(entries: &[RevisionEntry]) -> String {
    let mut lines: Vec<String> = vec![
        "# Revisions".to_string(),
        String::new(),
        "Tracked changes in this document. editable=false entries live outside".to_string(),
        "the editable surface (nested containers, other parts) and can only be".to_string(),
        "viewed; direct-body revisions can be accepted/rejected in R3.".to_string(),
        String::new(),
    ];
    for entry in entries {
        let flag = if entry.editable {
            "ok".to_string()
        } else {
            format!("locked ({})", entry.reason.clone().unwrap_or_default())
        };
        let place = entry
            .paragraph_id
            .clone()
            .unwrap_or_else(|| entry.part.clone());
        lines.push(format!(
            "- {} w:id={} [{}] @{} {} {} \u{2014} {}",
            entry.kind,
            entry.w_id,
            place,
            entry.author,
            entry.date,
            python_repr_prefix(&entry.text, 40),
            flag
        ));
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// `repr(text[:limit])` as the reference writes it: single quotes, backslashes
/// and quotes escaped, everything else literal. Only the shapes the inventory
/// actually carries are reproduced — this is a display line, not a parser.
fn python_repr_prefix(text: &str, limit: usize) -> String {
    let head: String = text.chars().take(limit).collect();
    let mut out = String::from("'");
    for ch in head.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('\'');
    out
}

/// The `revisions.json` sidecar: the inventory as the reference writes it.
///
/// A DERIVED view, never a source. It exists so a consumer can address a
/// revision (`revision_key`) and so a projection change can be judged against a
/// record taken before it — not so anything can be read back out of it and
/// treated as the document's state. Everything here comes from
/// `scan_revisions_bytes`; nothing is parsed back out of Markdown.
pub fn revisions_sidecar_value(entries: &[RevisionEntry]) -> serde_json::Value {
    let revisions: Vec<serde_json::Value> = entries
        .iter()
        .map(|entry| {
            serde_json::json!({
                "paragraph_id": entry.paragraph_id,
                "kind": entry.kind,
                "w_id": entry.w_id,
                "author": entry.author,
                "date": entry.date,
                "text": entry.text,
                "editable": entry.editable,
                "reason": entry.reason,
                "part": entry.part,
                "scope": entry.scope,
                "fingerprint": revision_fingerprint(&entry.text),
                "revision_key": entry.revision_key(),
            })
        })
        .collect();
    serde_json::json!({ "schema": REVISIONS_SCHEMA, "revisions": revisions })
}

/// One per-paragraph view line (visible text under a settlement action).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParagraphView {
    pub part: String,
    pub id: String,
    pub text: String,
}

/// One comment with its document anchors.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommentAnchor {
    pub part: String,
    pub paragraph_id: String,
    pub kind: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommentEntry {
    pub id: String,
    pub author: String,
    pub date: String,
    pub text: String,
    pub anchors: Vec<CommentAnchor>,
}

/// One Unicode normalization candidate (mirror of `find_candidates`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct NormalizationCandidate {
    pub candidate_id: String,
    pub occurrence_id: String,
    pub paragraph_id: String,
    pub codepoint: String,
    pub source: String,
    pub name: String,
    pub category: String,
    pub vertical: String,
    pub proposed_target: String,
    pub reversible: bool,
    pub context: String,
}

/// A located revision element inside one part.
///
/// `pub(crate)` for the `document_projection` read model, which reuses the
/// locator (ranges + canonical keys) so segments agree with the govern
/// revision inventory.
#[derive(Clone, Debug)]
pub(crate) struct LocatedRevision {
    /// Element range in the part bytes (open tag start .. past close tag).
    pub start: usize,
    pub end: usize,
    pub open_end: usize,
    pub close_start: usize,
    pub kind: String,
    pub w_id: String,
    pub author: String,
    pub date: String,
    /// true when the element is self-closing (paragraph mark).
    pub mark: bool,
    /// true when the element sits in an opaque interior (not AST-visible).
    pub inside_opaque: bool,
    /// true when the owning paragraph contains any opaque node.
    pub paragraph_opaque: bool,
    pub paragraph_id: Option<String>,
    /// Joined visible text of the element (w:t + w:delText descendants).
    pub text: String,
}

// ---------------------------------------------------------------------------
// Package reading
// ---------------------------------------------------------------------------

/// The `word/document.xml` bytes of a package (used by the decision and
/// table closures).
pub fn document_xml_bytes(package: &[u8]) -> Result<Vec<u8>, CoreError> {
    let file = std::io::Cursor::new(package.to_vec());
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| CoreError::Message(format!("not a valid DOCX: {error}")))?;
    for index in 0..archive.len() {
        let mut member = archive
            .by_index(index)
            .map_err(|error| CoreError::Message(format!("not a valid DOCX: {error}")))?;
        if member.name() == "word/document.xml" {
            let mut bytes = Vec::new();
            member.read_to_end(&mut bytes).map_err(CoreError::io)?;
            return Ok(bytes);
        }
    }
    Err(CoreError::Message(
        "not a valid DOCX: missing word/document.xml".to_string(),
    ))
}

/// Replace `word/document.xml` in a package; every other byte replays
/// verbatim (zip-level byte surgery).
pub fn patch_document_xml(package: &[u8], new_document_xml: &[u8]) -> Result<Vec<u8>, CoreError> {
    prose::patch_zip_member(package, "word/document.xml", new_document_xml)
}

/// Read every XML member of a package (name -> bytes), sorted by name.
fn package_xml_members(package: &[u8]) -> Result<Vec<(String, Vec<u8>)>, CoreError> {
    let file = std::io::Cursor::new(package.to_vec());
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| CoreError::Message(format!("not a valid DOCX: {error}")))?;
    let mut members = Vec::new();
    for index in 0..archive.len() {
        let mut member = archive
            .by_index(index)
            .map_err(|error| CoreError::Message(format!("not a valid DOCX: {error}")))?;
        let name = member.name().to_string();
        if !name.ends_with(".xml") {
            continue;
        }
        let mut bytes = Vec::new();
        member.read_to_end(&mut bytes).map_err(CoreError::io)?;
        members.push((name, bytes));
    }
    members.sort();
    Ok(members)
}

/// Is `part` an editable prose part (document or header/footer/footnotes/
/// endnotes/comments)?
fn is_prose_part(name: &str) -> bool {
    name == "word/document.xml" || part_key_from_path(name).is_some()
}

/// Decode the text of one `w:t`/`w:delText` element body (entity decoding,
/// mirroring Python's `itertext` join).
fn element_text(xml: &[u8], open_end: usize, close_start: usize) -> String {
    let (decoded, _) =
        prose::decode_text_segments(&xml[open_end..close_start]).unwrap_or_else(|_| {
            (
                String::from_utf8_lossy(&xml[open_end..close_start]).into_owned(),
                Vec::new(),
            )
        });
    decoded
}

/// Collect the text of every `w:t`/`w:delText` descendant of one revision
/// element (mirror of Python's `element.iter()` text join).
fn revision_text(xml: &[u8], tags: &[Tag], start: usize, end: usize) -> String {
    let mut out = String::new();
    // A revision's text is its OWN runs, not the runs of a revision nested in
    // it. `w:ins` containing only a `w:del` is legitimate Word structure and its
    // final-view text is empty — the insertion contributes nothing once the
    // deletion inside it is rejected. Descending collected the nested
    // `delText` as if the outer insertion had written it, so 125 revisions on a
    // real patent carried a fingerprint the reference did not have (its own was
    // the empty-string hash) and the two inventories could not be compared key
    // for key at all.
    let mut nested: Vec<String> = Vec::new();
    for tag in tags {
        if tag.start < start || tag.start >= end {
            continue;
        }
        if tag.closing {
            if nested.last().map(String::as_str) == Some(tag.name.as_str()) {
                nested.pop();
            }
            continue;
        }
        // `tag.start == start` is the revision element ITSELF; only a
        // strictly-inner one is nested.
        if tag.start > start && revision_kind(&tag.name).is_some() && !tag.self_closing {
            nested.push(tag.name.clone());
            continue;
        }
        if nested.is_empty() && (tag.name == "t" || tag.name == "delText") {
            if !tag.self_closing {
                let open_end = tag.end;
                // find the matching close tag for this text element
                let close_start = tags
                    .iter()
                    .find(|candidate| {
                        candidate.closing
                            && candidate.name == tag.name
                            && candidate.start >= tag.end
                            && candidate.start < end
                    })
                    .map(|candidate| candidate.start)
                    .unwrap_or(tag.end);
                out.push_str(&element_text(xml, open_end, close_start));
            }
            continue;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Revision inventory + views
// ---------------------------------------------------------------------------

/// Locate every revision element of one part with its byte range, using the
/// paragraph locator for ids and the opaque rules for edibility.
pub(crate) fn locate_revisions(
    part_key: &str,
    xml: &[u8],
    part: &str,
    is_document: bool,
) -> Result<Vec<LocatedRevision>, CoreError> {
    let tags = scan_tags(xml);
    // Element ranges via a stack of open elements (name, start, open_end).
    let mut stack: Vec<(String, usize, usize)> = Vec::new();
    // Range of the innermost "interior" container the current element sits
    // inside: opaque elements (fldSimple/oMath/...) and runs/pPr drop the
    // AST-visibility of revision descendants.
    let mut interior_depth = 0usize;
    let mut found: Vec<LocatedRevision> = Vec::new();
    // close tracking: on a closing tag, pop the matching open.
    for tag in &tags {
        if tag.closing {
            if let Some((name, _, _)) = stack.last().cloned() {
                if name == tag.name {
                    let (name, start, open_end) = stack.pop().expect("checked");
                    if interior_depth > 0 && is_interior_container(&name) {
                        interior_depth -= 1;
                    }
                    if let Some(kind) = revision_kind(&name) {
                        // Revisions that opened at the CURRENT stack depth
                        // closed here; the element range is
                        // [start, tag.end). `interior_depth` is the
                        // interior state of the PARENT now.
                        let w_id = attr_value(xml, start, open_end, "w:id");
                        let inside = interior_depth > 0;
                        let paragraph_opaque = false; // filled below
                        found.push(LocatedRevision {
                            start,
                            end: tag.end,
                            open_end,
                            close_start: tag.start,
                            kind: kind.to_string(),
                            w_id: w_id.unwrap_or_default(),
                            author: attr_value(xml, start, open_end, "w:author")
                                .unwrap_or_default(),
                            date: attr_value(xml, start, open_end, "w:date").unwrap_or_default(),
                            mark: false,
                            inside_opaque: inside,
                            paragraph_opaque,
                            paragraph_id: None,
                            text: String::new(),
                        });
                    }
                }
            }
            continue;
        }
        if tag.self_closing {
            if let Some(kind) = revision_kind(&tag.name) {
                let inside = interior_depth > 0;
                found.push(LocatedRevision {
                    start: tag.start,
                    end: tag.end,
                    open_end: tag.end,
                    close_start: tag.end,
                    kind: kind.to_string(),
                    w_id: attr_value(xml, tag.start, tag.end, "w:id").unwrap_or_default(),
                    author: attr_value(xml, tag.start, tag.end, "w:author").unwrap_or_default(),
                    date: attr_value(xml, tag.start, tag.end, "w:date").unwrap_or_default(),
                    mark: true,
                    inside_opaque: inside,
                    paragraph_opaque: false,
                    paragraph_id: None,
                    text: String::new(),
                });
            }
            continue;
        }
        if is_interior_container(&tag.name) {
            interior_depth += 1;
        }
        stack.push((tag.name.clone(), tag.start, tag.end));
    }
    if !stack.is_empty() {
        return Err(CoreError::Domain(format!(
            "{part} XML has unclosed elements"
        )));
    }
    // Paragraph mapping + opaque resolution.
    let paragraphs: Vec<(String, usize, usize)> = if is_document {
        prose::scan_document(xml)?
    } else {
        prose::scan_part(xml, part_key)?
    };
    let mut opaque_ranges: Vec<(usize, usize)> = Vec::new();
    for (_, start, end) in &paragraphs {
        for block in opaque_blocks_in_range(xml, *start, *end) {
            opaque_ranges.push(block);
        }
    }
    for revision in found.iter_mut() {
        let (paragraph_id, paragraph_opaque, in_opaque_paragraph) =
            paragraph_for_revision(&paragraphs, &opaque_ranges, revision.start);
        revision.paragraph_id = paragraph_id;
        revision.paragraph_opaque = paragraph_opaque;
        revision.inside_opaque = revision.inside_opaque || in_opaque_paragraph;
        revision.text = revision_text(xml, &tags, revision.start, revision.end);
    }
    // The reference's order, which is a walk of the tree and therefore a fact
    // about the inventory, not a presentation choice:
    //
    //     for each paragraph, in document order:
    //         its revisions, PARENT BEFORE CHILD
    //         then its paragraph-mark revision
    //
    // Ours pushed on the CLOSING tag, so a nested pair came out inner-first,
    // and a mark revision — which lives in the paragraph's `pPr`, early in its
    // byte range — came out before the paragraph's own content. Same entries,
    // a 436-line diff.
    let ordinal: std::collections::BTreeMap<&str, usize> = paragraphs
        .iter()
        .enumerate()
        .map(|(index, (id, _, _))| (id.as_str(), index))
        .collect();
    found.sort_by_key(|revision| {
        let paragraph = revision
            .paragraph_id
            .as_deref()
            .and_then(|id| ordinal.get(id).copied())
            .unwrap_or(usize::MAX);
        (paragraph, u8::from(revision.mark), revision.start)
    });
    Ok(found)
}

/// Elements whose subtree is not AST-visible to the revision model: opaque
/// containers, runs, and paragraph properties are all "interiors".
fn is_interior_container(name: &str) -> bool {
    // "Interior" means: a revision inside this element is not AST-visible.
    // A run and `pPr` qualify — a revision cannot start inside one and still be
    // a node of the paragraph. So do the genuinely opaque containers.
    //
    // A revision container does NOT qualify, and neither does a hyperlink:
    // their children are ordinary AST nodes. `w:del` nested inside `w:ins` is
    // legitimate Word structure, and the reference renders it as a nested
    // revision and calls it editable. Listing them here made 125 revisions on a
    // real patent read `nested-container-or-non-editable-part` when they were
    // plain nested deletions.
    matches!(
        name,
        "r" | "pPr"
            | "fldSimple"
            | "oMath"
            | "oMathPara"
            | "drawing"
            | "pict"
            | "object"
            | "sdt"
            | "smartTag"
            | "proofErr"
    )
}

/// Raw attribute value lookup inside one element's open tag bytes.
fn attr_value(xml: &[u8], start: usize, open_end: usize, name: &str) -> Option<String> {
    let raw = &xml[start..open_end];
    let needle = format!("{name}=\"");
    let needle_bytes = needle.as_bytes();
    let pos = raw
        .windows(needle_bytes.len())
        .position(|w| w == needle_bytes)?;
    let rest = &raw[pos + needle_bytes.len()..];
    let end = rest.iter().position(|&b| b == b'"')?;
    Some(String::from_utf8_lossy(&rest[..end]).into_owned())
}

/// Opaque element byte ranges inside one paragraph (the container-level and
/// run-level unknown-node rules of `extract_paragraph`).
fn opaque_blocks_in_range(xml: &[u8], start: usize, end: usize) -> Vec<(usize, usize)> {
    use crate::prose::{KNOWN_CONTAINER_CHILDREN, KNOWN_RUN_CHILDREN};
    let tags = scan_tags(xml);
    let mut stack: Vec<(String, usize, usize)> = Vec::new();
    let mut opaques = Vec::new();
    // Track the run context of the current element.
    let mut run_depth = 0usize;
    // And the paragraph-properties context. `pPr` has its own vocabulary
    // (`spacing`, `jc`, `ind`, `rPr`, ...) which shares nothing with the
    // paragraph CONTENT list below, so checking its children against that list
    // marked every paragraph whose mark carried formatting as opaque — 79 of
    // 79 on a real patent, because Word writes `<w:pPr><w:rPr>` on all of
    // them. The reference parses `pPr` wholesale and never makes an opaque of
    // any of it.
    let mut ppr_depth = 0usize;
    // And the run-properties context. `rPr`'s children are PROPERTIES
    // (`rFonts`, `b`, `sz`, `vertAlign`, `rPrChange`, ...); the list below
    // names run CONTENT (`t`, `tab`, `br`, ...). Sharing one list between the
    // two marked every property opaque, which in turn marked the paragraph
    // opaque. Measured: the detector's only output on a real patent was
    // `<w:rFonts w:hint="eastAsia"/>`, repeated.
    let mut rpr_depth = 0usize;
    // The first tag in the range is the paragraph element itself (its open
    // tag sits at `start`); it is the scan root, never classified.
    let mut first = true;
    for tag in &tags {
        if tag.start < start || tag.start >= end {
            continue;
        }
        if tag.closing {
            if let Some((name, _, _)) = stack.last().cloned() {
                if name == tag.name {
                    let (name, _, _) = stack.pop().expect("checked");
                    if name == "r" && run_depth > 0 {
                        run_depth -= 1;
                    }
                    if name == "pPr" && ppr_depth > 0 {
                        ppr_depth -= 1;
                    }
                    if name == "rPr" && rpr_depth > 0 {
                        rpr_depth -= 1;
                    }
                }
            }
            continue;
        }
        if first {
            first = false;
            if !tag.self_closing {
                stack.push((tag.name.clone(), tag.start, tag.end));
            }
            continue;
        }
        let name = tag.name.as_str();
        // Inside a run, "known" means what the run PARSER handles: `rPr`, and
        // the children it turns into inline nodes. `rPrChange` lives inside
        // `rPr`, so `run_depth > 0` when the walk reaches it, and it was in
        // neither list — every run carrying format history therefore marked its
        // whole paragraph opaque. On a real patent, where an advisor's edits
        // left `rPrChange` throughout, that made 857 of 860 revisions
        // `editable: false, reason: "paragraph-contains-unsupported-node"` while
        // the reference marked them editable: it parses the same element as an
        // inline (`rpr-change`), and only an OpaqueNode disqualifies a
        // paragraph. Format history is not an unsupported node.
        let known_run_child =
            name == "rPr" || name == "rPrChange" || KNOWN_RUN_CHILDREN.contains(&name);
        let opaque = if ppr_depth > 0 || rpr_depth > 0 {
            false
        } else if run_depth > 0 {
            !known_run_child
        } else {
            name == "proofErr" || !KNOWN_CONTAINER_CHILDREN.contains(&name)
        };
        if opaque {
            opaques.push((tag.start, tag.end));
        }
        if name == "r" {
            run_depth += 1;
        }
        if name == "pPr" {
            ppr_depth += 1;
        }
        if name == "rPr" {
            rpr_depth += 1;
        }
        if !tag.self_closing {
            stack.push((tag.name.clone(), tag.start, tag.end));
        }
    }
    opaques
}

/// Find the paragraph containing `position` and decide edibility.
fn paragraph_for_revision(
    paragraphs: &[(String, usize, usize)],
    opaque_ranges: &[(usize, usize)],
    position: usize,
) -> (Option<String>, bool, bool) {
    let mut owner: Option<&(String, usize, usize)> = None;
    for paragraph in paragraphs {
        if paragraph.1 <= position
            && position < paragraph.2
            && (owner.is_none() || paragraph.1 >= owner.expect("checked").1)
        {
            owner = Some(paragraph);
        }
    }
    let Some((id, p_start, p_end)) = owner else {
        return (None, false, false);
    };
    let in_opaque_paragraph = opaque_ranges
        .iter()
        .any(|(o_start, o_end)| *o_start >= *p_start && *o_end <= *p_end);
    // A revision inside an opaque element inside the paragraph.
    let inside_opaque_block = opaque_ranges
        .iter()
        .any(|(o_start, o_end)| *o_start <= position && position < *o_end);
    (Some(id.clone()), in_opaque_paragraph, inside_opaque_block)
}

/// Full revision inventory of one package (mirror of
/// `render_revisions_json(document, package_revisions)`).
pub fn scan_revisions(package: &Path) -> Result<Vec<RevisionEntry>, CoreError> {
    let bytes = std::fs::read(package).map_err(CoreError::io)?;
    scan_revisions_bytes(&bytes)
}

/// Revision inventory over raw package bytes.
pub fn scan_revisions_bytes(package: &[u8]) -> Result<Vec<RevisionEntry>, CoreError> {
    let members = package_xml_members(package)?;
    let mut entries = Vec::new();
    for (name, xml) in &members {
        if !is_prose_part(name) {
            continue;
        }
        let is_document = name == "word/document.xml";
        let part_key = if is_document {
            "document".to_string()
        } else {
            part_key_from_path(name).expect("prose part")
        };
        let located = locate_revisions(&part_key, xml, name, is_document)?;
        for revision in located {
            if is_document {
                // Document entries mirror `collect_document_revisions`:
                // paragraph marks are viewable but locked; opaque
                // paragraphs lock their (AST-visible) revisions.
                let (reason, scope) = if revision.mark {
                    (
                        Some("paragraph-mark-revision".to_string()),
                        Some("paragraph-mark".to_string()),
                    )
                } else if revision.inside_opaque {
                    // Not AST-visible: Python reports revision-not-found for
                    // decisions and does not inventory them as editable.
                    (
                        Some("nested-container-or-non-editable-part".to_string()),
                        None,
                    )
                } else if revision.paragraph_opaque {
                    (
                        Some("paragraph-contains-unsupported-node".to_string()),
                        None,
                    )
                } else {
                    (None, None)
                };
                entries.push(RevisionEntry {
                    part: name.clone(),
                    kind: revision.kind,
                    w_id: revision.w_id,
                    author: revision.author,
                    date: revision.date,
                    text: revision.text,
                    paragraph_id: revision.paragraph_id,
                    editable: reason.is_none(),
                    reason,
                    scope,
                });
            } else {
                entries.push(RevisionEntry {
                    part: name.clone(),
                    kind: revision.kind,
                    w_id: revision.w_id,
                    author: revision.author,
                    date: revision.date,
                    text: revision.text,
                    paragraph_id: None,
                    editable: false,
                    reason: Some("nested-container-or-non-editable-part".to_string()),
                    scope: None,
                });
            }
        }
    }
    Ok(entries)
}

/// Per-paragraph visible text under a settlement action (accept/reject),
/// mirroring what `settle_xml_revisions` then a w:t join would produce:
/// under accept, `w:del`/`w:moveFrom` subtrees are hidden; under reject,
/// `w:ins`/`w:moveTo` subtrees are hidden.
pub fn revision_views(package: &Path, action: &str) -> Result<Vec<ParagraphView>, CoreError> {
    let bytes = std::fs::read(package).map_err(CoreError::io)?;
    revision_views_bytes(&bytes, action)
}

/// Views over raw package bytes.
pub fn revision_views_bytes(package: &[u8], action: &str) -> Result<Vec<ParagraphView>, CoreError> {
    if action != "accept" && action != "reject" {
        return Err(CoreError::Domain(format!(
            "invalid-action: view action must be accept or reject, got {action}"
        )));
    }
    let members = package_xml_members(package)?;
    let mut views = Vec::new();
    for (name, xml) in &members {
        if !is_prose_part(name) {
            continue;
        }
        let is_document = name == "word/document.xml";
        let part_key = if is_document {
            "document".to_string()
        } else {
            part_key_from_path(name).expect("prose part")
        };
        let paragraphs: Vec<(String, usize, usize)> = if is_document {
            prose::scan_document(xml)?
        } else {
            prose::scan_part(xml, &part_key)?
        };
        for (id, start, end) in paragraphs {
            let text = paragraph_visible_text(xml, start, end, action);
            views.push(ParagraphView {
                part: name.clone(),
                id,
                text,
            });
        }
    }
    Ok(views)
}

/// Visible text of one paragraph under a settlement action.
///
/// A text element is visible unless a hidden revision (`w:del`/`w:moveFrom`
/// under accept, `w:ins`/`w:moveTo` under reject) is an open ancestor.
pub fn paragraph_visible_text(xml: &[u8], start: usize, end: usize, action: &str) -> String {
    let tags = scan_tags(xml);
    let hidden: &[&str] = if action == "accept" {
        &["del", "moveFrom"]
    } else {
        &["ins", "moveTo"]
    };
    let mut out = String::new();
    let mut skip = 0usize;
    for tag in &tags {
        if tag.start < start || tag.start >= end {
            continue;
        }
        if skip > 0 {
            if tag.closing {
                skip -= 1;
            } else if !tag.self_closing {
                skip += 1;
            }
            continue;
        }
        if (tag.name == "t" || tag.name == "delText") && !tag.self_closing && !tag.closing {
            let open_end = tag.end;
            let close_start = tags
                .iter()
                .find(|candidate| {
                    candidate.closing
                        && candidate.name == tag.name
                        && candidate.start >= tag.end
                        && candidate.start < end
                })
                .map(|candidate| candidate.start)
                .unwrap_or(tag.end);
            out.push_str(&element_text(xml, open_end, close_start));
            continue;
        }
        if tag.self_closing || tag.closing {
            continue;
        }
        if hidden.contains(&tag.name.as_str()) {
            skip = 1;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Single-revision settlement (byte surgery)
// ---------------------------------------------------------------------------

/// The w:id values used by tracked revisions across the whole package
/// (mirror of `used_revision_ids`).
pub fn used_revision_ids(package: &[u8]) -> Result<BTreeMap<String, ()>, CoreError> {
    let members = package_xml_members(package)?;
    let mut ids = BTreeMap::new();
    for (name, xml) in &members {
        if !name.ends_with(".xml") {
            continue;
        }
        for tag in scan_tags(xml) {
            if revision_kind(&tag.name).is_none() {
                continue;
            }
            if let Some(id) = attr_value(xml, tag.start, tag.end, "w:id") {
                if id.parse::<i64>().is_ok() {
                    ids.insert(id, ());
                }
            }
        }
    }
    Ok(ids)
}

/// Lowest available non-negative w:id over the package (mirror of
/// `next_revision_id`).
pub fn next_revision_id(package: &[u8]) -> Result<i64, CoreError> {
    let used = used_revision_ids(package)?;
    let mut candidate = 0i64;
    while used.contains_key(&candidate.to_string()) {
        candidate += 1;
    }
    Ok(candidate)
}

/// Locate ONE settleable revision of `word/document.xml` by w:id.
///
/// Mirror of Python's decision gating: the element must be AST-visible
/// (not inside pPr/r/opaque interiors) and the paragraph must contain no
/// opaque node; paragraph marks (self-closing) are view-only. Returns
/// `(revision, None)` when found; `(None, code)` when not settleable.
fn find_settleable_revision(
    xml: &[u8],
    w_id: &str,
) -> Result<Option<LocatedRevision>, (String, String)> {
    let tags = scan_tags(xml);
    let mut stack: Vec<(String, usize, usize)> = Vec::new();
    let mut interior_depth = 0usize;
    let mut found: Option<LocatedRevision> = None;
    for tag in &tags {
        if tag.closing {
            if let Some((name, _, _)) = stack.last().cloned() {
                if name == tag.name {
                    let (name, start, open_end) = stack.pop().expect("checked");
                    if interior_depth > 0 && is_interior_container(&name) {
                        interior_depth -= 1;
                    }
                    if let Some(kind) = revision_kind(&name) {
                        let current_id =
                            attr_value(xml, start, open_end, "w:id").unwrap_or_default();
                        if current_id == w_id && found.is_none() {
                            found = Some(LocatedRevision {
                                start,
                                end: tag.end,
                                open_end,
                                close_start: tag.start,
                                kind: kind.to_string(),
                                w_id: current_id,
                                author: attr_value(xml, start, open_end, "w:author")
                                    .unwrap_or_default(),
                                date: attr_value(xml, start, open_end, "w:date")
                                    .unwrap_or_default(),
                                mark: false,
                                inside_opaque: interior_depth > 0,
                                paragraph_opaque: false,
                                paragraph_id: None,
                                text: revision_text(xml, &tags, start, tag.end),
                            });
                        }
                    }
                }
            }
            continue;
        }
        if tag.self_closing {
            if let Some(kind) = revision_kind(&tag.name) {
                let current_id = attr_value(xml, tag.start, tag.end, "w:id").unwrap_or_default();
                if current_id == w_id && found.is_none() {
                    found = Some(LocatedRevision {
                        start: tag.start,
                        end: tag.end,
                        open_end: tag.end,
                        close_start: tag.end,
                        kind: kind.to_string(),
                        w_id: current_id,
                        author: attr_value(xml, tag.start, tag.end, "w:author").unwrap_or_default(),
                        date: attr_value(xml, tag.start, tag.end, "w:date").unwrap_or_default(),
                        mark: true,
                        inside_opaque: interior_depth > 0,
                        paragraph_opaque: false,
                        paragraph_id: None,
                        text: String::new(),
                    });
                }
            }
            continue;
        }
        if is_interior_container(&tag.name) {
            interior_depth += 1;
        }
        stack.push((tag.name.clone(), tag.start, tag.end));
    }
    let Some(mut revision) = found else {
        return Err((
            "revision-not-found".to_string(),
            format!("revision not found: w:id {w_id}"),
        ));
    };
    // Paragraph opacity resolution (mirror of `_find_paragraph_with_revision`
    // + `contains_opaque` gating).
    let paragraphs = prose::scan_document(xml)
        .map_err(|error| ("workdir-invalid".to_string(), error.to_string()))?;
    let mut opaque_ranges: Vec<(usize, usize)> = Vec::new();
    for (_, start, end) in &paragraphs {
        opaque_ranges.extend(opaque_blocks_in_range(xml, *start, *end));
    }
    let (paragraph_id, paragraph_opaque, inside_opaque_block) =
        paragraph_for_revision(&paragraphs, &opaque_ranges, revision.start);
    revision.paragraph_id = paragraph_id;
    revision.paragraph_opaque = paragraph_opaque;
    revision.inside_opaque = revision.inside_opaque || inside_opaque_block;
    if revision.mark {
        return Err((
            "revision-not-found".to_string(),
            format!("revision not found: w:id {w_id}"),
        ));
    }
    if revision.inside_opaque {
        return Err((
            "revision-not-found".to_string(),
            format!("revision not found: w:id {w_id}"),
        ));
    }
    if revision.paragraph_opaque {
        return Err((
            "revision-outside-editable-surface".to_string(),
            format!(
                "paragraph {} contains unsupported structure; its revisions can only be viewed",
                revision.paragraph_id.as_deref().unwrap_or("?")
            ),
        ));
    }
    Ok(Some(revision))
}

/// The result of one settlement.
#[derive(Clone, Debug)]
pub struct Settlement {
    /// The settled part bytes (document.xml).
    pub part_xml: Vec<u8>,
    /// The revision record the caller may report (kind/w_id/text).
    pub revision: LocatedRevisionPublic,
    /// Byte offset the revision occupied (for verification re-scans).
    pub revision_w_id: String,
}

/// Public view of the settled revision (serde-able).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LocatedRevisionPublic {
    pub kind: String,
    pub w_id: String,
    pub author: String,
    pub date: String,
    pub text: String,
    pub paragraph_id: Option<String>,
}

impl LocatedRevisionPublic {
    pub fn fingerprint(&self) -> String {
        revision_fingerprint(&self.text)
    }
}

/// Settle ONE revision of `document.xml` (mirror of
/// `apply_revision_decision` at the byte level, plus the `w:delText` ->
/// `w:t` conversion of `settle_xml_revisions` for reject-of-delete).
///
/// Accept: insert/move_to unwrap (children kept), delete/move_from removed
/// wholesale. Reject: insert/move_to removed, delete/move_from unwrapped
/// with delText -> t.
pub fn settle_one_revision(
    document_xml: &[u8],
    w_id: &str,
    action: &str,
) -> Result<Settlement, (String, String)> {
    if action != "accept" && action != "reject" {
        return Err((
            "invalid-action".to_string(),
            format!("unknown decision action: {action}"),
        ));
    }
    let revision =
        find_settleable_revision(document_xml, w_id)?.expect("settleable revision exists");
    let remove_kinds: &[&str] = if action == "accept" {
        &["delete", "move_from"]
    } else {
        &["insert", "move_to"]
    };
    let remove = remove_kinds.contains(&revision.kind.as_str());
    let mut out: Vec<u8> = Vec::with_capacity(document_xml.len());
    if remove {
        out.extend_from_slice(&document_xml[..revision.start]);
        out.extend_from_slice(&document_xml[revision.end..]);
    } else {
        // Unwrap: drop the open tag and the close tag; convert w:delText ->
        // w:t inside (reject of a delete/move_from).
        out.extend_from_slice(&document_xml[..revision.start]);
        if action == "reject" {
            let interior = &document_xml[revision.open_end..revision.close_start];
            let mut converted = interior.to_vec();
            convert_deltext(&mut converted);
            out.extend_from_slice(&converted);
        } else {
            out.extend_from_slice(&document_xml[revision.open_end..revision.close_start]);
        }
        out.extend_from_slice(&document_xml[revision.end..]);
    }
    let public = LocatedRevisionPublic {
        kind: revision.kind.clone(),
        w_id: revision.w_id.clone(),
        author: revision.author.clone(),
        date: revision.date.clone(),
        text: revision.text.clone(),
        paragraph_id: revision.paragraph_id.clone(),
    };
    Ok(Settlement {
        part_xml: out,
        revision: public,
        revision_w_id: w_id.to_string(),
    })
}

/// Byte-level `w:delText` -> `w:t` conversion (open and close tags).
fn convert_deltext(xml: &mut Vec<u8>) {
    // Convert via byte search: any `<w:delText` open and `</w:delText>`
    // close inside the interior.
    let mut index = 0usize;
    while index + 10 < xml.len() {
        if xml[index..].starts_with(b"<w:delText") {
            let end = xml[index..]
                .iter()
                .position(|&b| b == b'>')
                .map(|pos| index + pos + 1)
                .unwrap_or(xml.len());
            let token = &xml[index..end];
            let replacement = token
                .strip_prefix(b"<w:delText")
                .map(|rest| {
                    let mut out = b"<w:t".to_vec();
                    out.extend_from_slice(rest);
                    out
                })
                .unwrap_or_default();
            xml.splice(index..end, replacement);
            continue;
        }
        if xml[index..].starts_with(b"</w:delText>") {
            xml.splice(index..index + 12, b"</w:t>".to_vec());
            continue;
        }
        index += 1;
    }
}

/// Reinsert the deleted text of ONE deletion revision as a NEW insertion
/// revision placed directly after it (mirror of `reinsert_deleted_text`).
#[derive(Clone, Debug)]
pub struct Reinsertion {
    pub part_xml: Vec<u8>,
    pub new_w_id: i64,
    pub text: String,
    pub paragraph_id: Option<String>,
    pub kind: String,
    pub fingerprint: String,
}

/// The record of a reinsertion (serde-able, mirrors Python's decision
/// record `new-insert-after-deletion`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReinsertRecord {
    pub w_id: String,
    pub kind: String,
    pub action: String,
    pub fingerprint: String,
    pub paragraph_id: Option<String>,
    pub operation: String,
    pub new_w_id: String,
}

pub fn reinsert_deleted_text(
    document_xml: &[u8],
    package: &[u8],
    w_id: &str,
    author: &str,
    date: &str,
    text: Option<&str>,
) -> Result<(Vec<u8>, ReinsertRecord), (String, String)> {
    let revision =
        find_settleable_revision(document_xml, w_id)?.expect("settleable revision exists");
    if !matches!(revision.kind.as_str(), "delete" | "move_from") {
        return Err((
            "workdir-invalid".to_string(),
            format!("reinsert target is not a deletion: {}", revision.kind),
        ));
    }
    let deleted = if let Some(text) = text {
        text.to_string()
    } else {
        revision.text.clone()
    };
    let new_w_id = next_revision_id(package)
        .map_err(|error| ("workdir-invalid".to_string(), error.to_string()))?;
    // Clone the deletion's runs, converting delText -> t (style spans
    // preserved byte-for-byte).
    let tags = scan_tags(document_xml);
    let mut runs: Vec<u8> = Vec::new();
    let mut depth = 0usize;
    let mut pending: Vec<(String, usize, usize)> = Vec::new(); // open tags needing close
    for tag in &tags {
        if tag.start < revision.start || tag.start >= revision.end {
            continue;
        }
        if tag.closing {
            if let Some((name, _, _)) = pending.last().cloned() {
                if name == tag.name {
                    let (name, start, open_end) = pending.pop().expect("checked");
                    if name == "r" {
                        let run_bytes = &document_xml[start..tag.end];
                        let mut converted = run_bytes.to_vec();
                        convert_deltext(&mut converted);
                        runs.extend_from_slice(&converted);
                    } else {
                        runs.extend_from_slice(&document_xml[start..open_end]);
                        runs.extend_from_slice(&document_xml[open_end..tag.end]);
                    }
                }
            }
            depth = depth.saturating_sub(1);
            continue;
        }
        if depth == 0 {
            if tag.name == "r" && !tag.self_closing {
                pending.push((tag.name.clone(), tag.start, tag.end));
            }
            depth += 1;
            continue;
        }
        if !tag.self_closing {
            pending.push((tag.name.clone(), tag.start, tag.end));
        }
        depth += 1;
    }
    if runs.is_empty() {
        // The deletion had no runs (or only self-closing children): emit a
        // plain run with the deleted text.
        runs = format!("<w:r><w:t>{}</w:t></w:r>", prose::xml_escape(&deleted)).into_bytes();
    }
    let escaped_author = prose::xml_escape(author);
    let mut insertion =
        format!("<w:ins w:id=\"{new_w_id}\" w:author=\"{escaped_author}\" w:date=\"{date}\">")
            .into_bytes();
    insertion.extend_from_slice(&runs);
    insertion.extend_from_slice(b"</w:ins>");
    let mut out: Vec<u8> = Vec::with_capacity(document_xml.len() + insertion.len());
    out.extend_from_slice(&document_xml[..revision.end]);
    out.extend_from_slice(&insertion);
    out.extend_from_slice(&document_xml[revision.end..]);
    let record = ReinsertRecord {
        w_id: revision.w_id.clone(),
        kind: revision.kind.clone(),
        action: "reinsert".to_string(),
        fingerprint: revision_fingerprint(&revision.text),
        paragraph_id: revision.paragraph_id.clone(),
        operation: "new-insert-after-deletion".to_string(),
        new_w_id: new_w_id.to_string(),
    };
    Ok((out, record))
}

// ---------------------------------------------------------------------------
// Comments
// ---------------------------------------------------------------------------

/// Parse `word/comments.xml` entries (id, author, date, text).
fn parse_comment_entries(xml: &[u8]) -> Vec<(String, String, String, String)> {
    let tags = scan_tags(xml);
    let mut entries = Vec::new();
    let mut stack: Vec<(String, usize, usize)> = Vec::new();
    for tag in &tags {
        if tag.closing {
            if let Some((name, _, _)) = stack.last().cloned() {
                if name == tag.name {
                    let (name, start, open_end) = stack.pop().expect("checked");
                    if name == "comment" {
                        let id = attr_value(xml, start, open_end, "w:id").unwrap_or_default();
                        let author =
                            attr_value(xml, start, open_end, "w:author").unwrap_or_default();
                        let date = attr_value(xml, start, open_end, "w:date").unwrap_or_default();
                        let text = paragraph_visible_text(xml, start, tag.end, "accept");
                        entries.push((id, author, date, text));
                    }
                }
            }
            continue;
        }
        if !tag.self_closing {
            stack.push((tag.name.clone(), tag.start, tag.end));
        }
    }
    entries
}

/// Full comment inventory of one package: comment definitions plus the
/// anchors/references in the editable parts.
pub fn scan_comments(package: &Path) -> Result<Vec<CommentEntry>, CoreError> {
    let bytes = std::fs::read(package).map_err(CoreError::io)?;
    scan_comments_bytes(&bytes)
}

/// Comment inventory over raw package bytes.
pub fn scan_comments_bytes(package: &[u8]) -> Result<Vec<CommentEntry>, CoreError> {
    let members = package_xml_members(package)?;
    let mut defs: BTreeMap<String, (String, String, String)> = BTreeMap::new();
    for (name, xml) in &members {
        if name == "word/comments.xml" {
            for (id, author, date, text) in parse_comment_entries(xml) {
                defs.insert(id, (author, date, text));
            }
        }
    }
    let mut anchors: BTreeMap<String, Vec<CommentAnchor>> = BTreeMap::new();
    for (name, xml) in &members {
        if !is_prose_part(name) {
            continue;
        }
        let is_document = name == "word/document.xml";
        let part_key = if is_document {
            "document".to_string()
        } else {
            part_key_from_path(name).expect("prose part")
        };
        let paragraphs: Vec<(String, usize, usize)> = if is_document {
            prose::scan_document(xml)?
        } else {
            prose::scan_part(xml, &part_key)?
        };
        for tag in scan_tags(xml) {
            let kind = match tag.name.as_str() {
                "commentRangeStart" => "comment-start",
                "commentRangeEnd" => "comment-end",
                "commentReference" => "comment-reference",
                _ => continue,
            };
            let Some(id) = attr_value(xml, tag.start, tag.end, "w:id") else {
                continue;
            };
            let paragraph_id = paragraphs
                .iter()
                .find(|(_, start, end)| *start <= tag.start && tag.start < *end)
                .map(|(id, _, _)| id.clone())
                .unwrap_or_default();
            anchors.entry(id.clone()).or_default().push(CommentAnchor {
                part: name.clone(),
                paragraph_id,
                kind: kind.to_string(),
            });
        }
    }
    let mut comments: Vec<CommentEntry> = Vec::new();
    for (id, (author, date, text)) in defs {
        let anchor_list = anchors.remove(&id).unwrap_or_default();
        comments.push(CommentEntry {
            id,
            author,
            date,
            text,
            anchors: anchor_list,
        });
    }
    // Anchors without a definition (orphans) still surface as entries.
    for (id, anchor_list) in anchors {
        comments.push(CommentEntry {
            id,
            author: String::new(),
            date: String::new(),
            text: String::new(),
            anchors: anchor_list,
        });
    }
    comments.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(comments)
}

/// Byte range of one `<w:comment w:id="X">...</w:comment>` element.
fn find_comment_element(xml: &[u8], comment_id: &str) -> Option<(usize, usize)> {
    let tags = scan_tags(xml);
    let mut stack: Vec<(String, usize, usize)> = Vec::new();
    for tag in &tags {
        if tag.closing {
            if let Some((name, _, _)) = stack.last().cloned() {
                if name == tag.name {
                    let (name, start, open_end) = stack.pop().expect("checked");
                    if name == "comment"
                        && attr_value(xml, start, open_end, "w:id").as_deref() == Some(comment_id)
                    {
                        return Some((start, tag.end));
                    }
                }
            }
            continue;
        }
        if !tag.self_closing {
            stack.push((tag.name.clone(), tag.start, tag.end));
        }
    }
    None
}

/// Remove every anchor/reference element carrying `w:id == comment_id`
/// from one part (scoped `clear_comments_from_document`).
fn strip_comment_anchors(xml: &[u8], comment_id: &str) -> Vec<u8> {
    let tags = scan_tags(xml);
    let mut out: Vec<u8> = Vec::with_capacity(xml.len());
    let mut cursor = 0usize;
    for tag in &tags {
        let is_anchor = matches!(
            tag.name.as_str(),
            "commentRangeStart" | "commentRangeEnd" | "commentReference"
        ) && tag.self_closing;
        if is_anchor && attr_value(xml, tag.start, tag.end, "w:id").as_deref() == Some(comment_id) {
            out.extend_from_slice(&xml[cursor..tag.start]);
            cursor = tag.end;
        }
    }
    out.extend_from_slice(&xml[cursor..]);
    out
}

/// Remove the definition entry for one comment id from an extended comment
/// part (commentsExtended/commentsIds/commentsExtensible): any element that
/// carries an id-valued attribute equal to `comment_id` is dropped.
fn strip_extended_entry(xml: &[u8], comment_id: &str) -> Vec<u8> {
    let tags = scan_tags(xml);
    let mut stack: Vec<(String, usize, usize)> = Vec::new();
    let mut drop_ranges: Vec<(usize, usize)> = Vec::new();
    for tag in &tags {
        if tag.closing {
            if let Some((name, _, _)) = stack.last().cloned() {
                if name == tag.name {
                    let (_name, start, open_end) = stack.pop().expect("checked");
                    if attr_value(xml, start, open_end, "w:id").as_deref() == Some(comment_id)
                        || attr_value(xml, start, open_end, "w15:commentId").as_deref()
                            == Some(comment_id)
                    {
                        drop_ranges.push((start, tag.end));
                    }
                }
            }
            continue;
        }
        if !tag.self_closing {
            stack.push((tag.name.clone(), tag.start, tag.end));
        }
    }
    if drop_ranges.is_empty() {
        return xml.to_vec();
    }
    let mut out: Vec<u8> = Vec::with_capacity(xml.len());
    let mut cursor = 0usize;
    for (start, end) in drop_ranges {
        out.extend_from_slice(&xml[cursor..start]);
        cursor = end;
    }
    out.extend_from_slice(&xml[cursor..]);
    out
}

/// Delete one comment from a package (entry + anchors + references), leaving
/// every other byte of every part verbatim. Mirrors Python's
/// `_delete_comment` outcome at the package level.
pub fn delete_comment_bytes(package: &[u8], comment_id: &str) -> Result<Vec<u8>, CoreError> {
    let members = package_xml_members(package)?;
    let mut document_xml: Option<Vec<u8>> = None;
    let mut comments_xml: Option<Vec<u8>> = None;
    let mut extended: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for (name, xml) in &members {
        if name == "word/comments.xml" {
            comments_xml = Some(xml.clone());
        } else if EXTENDED_COMMENT_PARTS.contains(&name.as_str()) {
            extended.insert(name.clone(), xml.clone());
        } else if name == "word/document.xml" {
            document_xml = Some(xml.clone());
        }
    }
    let Some(comments_ref) = comments_xml.as_ref() else {
        return Err(CoreError::Domain(
            "comment-not-found: no comments.xml in the package".to_string(),
        ));
    };
    if find_comment_element(comments_ref, comment_id).is_none() {
        return Err(CoreError::Domain(format!(
            "comment-not-found: {comment_id}"
        )));
    }
    let mut current = package.to_vec();
    let mut new_comments = Vec::with_capacity(comments_ref.len());
    if let Some((start, end)) = find_comment_element(comments_ref, comment_id) {
        new_comments.extend_from_slice(&comments_ref[..start]);
        new_comments.extend_from_slice(&comments_ref[end..]);
    } else {
        new_comments = comments_ref.clone();
    }
    if new_comments != *comments_ref {
        current = prose::patch_zip_member(&current, "word/comments.xml", &new_comments)?;
    }
    for (name, xml) in &extended {
        let stripped = strip_extended_entry(xml, comment_id);
        if stripped != *xml {
            current = prose::patch_zip_member(&current, name, &stripped)?;
        }
    }
    if let Some(document_xml) = document_xml {
        let stripped = strip_comment_anchors(&document_xml, comment_id);
        if stripped != document_xml {
            current = prose::patch_zip_member(&current, "word/document.xml", &stripped)?;
        }
    }
    Ok(current)
}

// ---------------------------------------------------------------------------
// Tables (structure-only byte surgery)
// ---------------------------------------------------------------------------

/// Byte ranges of body-level tables (w:tbl whose parent is w:body), in
/// document order.
fn body_table_ranges(xml: &[u8]) -> Vec<(usize, usize)> {
    let tags = scan_tags(xml);
    let mut tables: Vec<(usize, usize)> = Vec::new();
    let mut open_tables: Vec<usize> = Vec::new();
    let mut body_depth: Option<usize> = None;
    let mut depth = 0usize;
    let mut stack: Vec<(String, usize)> = Vec::new();
    for tag in &tags {
        if tag.closing {
            if let Some((name, _)) = stack.last().cloned() {
                if name == tag.name {
                    let (name, open_start) = stack.pop().expect("checked");
                    if name == "body" && depth == 1 {
                        body_depth = None;
                    }
                    if name == "tbl" && open_tables.last().is_some_and(|open| *open == open_start) {
                        let open = open_tables.pop().expect("checked");
                        tables.push((open, tag.end));
                    }
                    depth = depth.saturating_sub(1);
                }
            }
            continue;
        }
        if tag.name == "body" && body_depth.is_none() {
            body_depth = Some(depth);
        }
        if tag.name == "tbl"
            && body_depth.is_some_and(|body| depth == body + 1)
            && !tag.self_closing
        {
            open_tables.push(tag.start);
        }
        if !tag.self_closing {
            stack.push((tag.name.clone(), tag.start));
            depth += 1;
        }
    }
    tables
}

/// Row and cell byte ranges of ONE table at its own depth (nested tables
/// inside cells excluded — mirror of `_locate_table_elements`).
type ByteRange = (usize, usize);

fn table_rows_and_cells(
    xml: &[u8],
    table_start: usize,
    table_end: usize,
) -> Result<(Vec<ByteRange>, Vec<ByteRange>), CoreError> {
    let tags = scan_tags(xml);
    let mut rows: Vec<(usize, usize)> = Vec::new();
    let mut cells: Vec<(usize, usize)> = Vec::new();
    let mut stack: Vec<(String, usize)> = Vec::new();
    let mut nested_tbls = 0usize;
    for tag in &tags {
        if tag.start < table_start || tag.start >= table_end {
            continue;
        }
        if tag.closing {
            let Some((name, _open_start)) = stack.last().cloned() else {
                return Err(CoreError::Domain(format!(
                    "malformed table XML nesting near {}",
                    tag.raw_name
                )));
            };
            if name != tag.name {
                return Err(CoreError::Domain(format!(
                    "malformed table XML nesting near {}",
                    tag.raw_name
                )));
            }
            let (name, open_start) = stack.pop().expect("checked");
            match name.as_str() {
                "tbl" if nested_tbls > 0 => nested_tbls -= 1,
                "tr" if nested_tbls == 0 => rows.push((open_start, tag.end)),
                "tc" if nested_tbls == 0 => cells.push((open_start, tag.end)),
                _ => {}
            }
            continue;
        }
        if tag.name == "tbl" && tag.start != table_start {
            nested_tbls += 1;
        }
        if !tag.self_closing {
            stack.push((tag.name.clone(), tag.start));
        }
    }
    Ok((rows, cells))
}

/// Text of one cell fragment (w:t and delText content, stripped).
fn cell_visible_text(cell_xml: &[u8]) -> String {
    let text = paragraph_visible_text(cell_xml, 0, cell_xml.len(), "accept");
    text.trim().to_string()
}

/// `_clone_row_with_empty_cells`: clone a row, preserving cell properties,
/// clearing cell text (single empty paragraph per cell; nested tables
/// dropped).
fn clone_row_with_empty_cells(row_xml: &[u8]) -> Vec<u8> {
    let tags = scan_tags(row_xml);
    let mut out: Vec<u8> = Vec::new();
    let mut cursor = 0usize;
    let mut skip_depth = 0usize;
    let mut skip_name: Option<String> = None;
    for tag in &tags {
        let token = tag.bytes(row_xml);
        let closing = tag.closing;
        let self_closing = tag.self_closing;
        let name = tag.name.as_str();
        if let Some(skip) = &skip_name {
            cursor = tag.end;
            if closing && name == skip {
                skip_name = None;
            }
            continue;
        }
        if skip_depth == 0 {
            out.extend_from_slice(&row_xml[cursor..tag.start]);
        }
        cursor = tag.end;
        if name == "p" && !closing && !self_closing {
            out.extend_from_slice(b"<w:p/>");
            skip_depth = 1;
            continue;
        }
        if name == "p" && closing && skip_depth > 0 {
            skip_depth -= 1;
            continue;
        }
        if name == "tbl" && !closing && !self_closing {
            skip_name = Some(tag.name.clone());
            continue;
        }
        if skip_depth > 0 {
            continue;
        }
        out.extend_from_slice(token);
    }
    if skip_depth == 0 && skip_name.is_none() {
        out.extend_from_slice(&row_xml[cursor..]);
    }
    out
}

/// `_clone_cell_with_empty_paragraph`: clone a cell keeping tcPr but with a
/// single empty paragraph.
fn clone_cell_with_empty_paragraph(cell_xml: &[u8]) -> Vec<u8> {
    let tags = scan_tags(cell_xml);
    let mut open_end = None;
    for tag in &tags {
        if tag.name == "tc" && !tag.closing && !tag.self_closing {
            open_end = Some(tag.end);
            break;
        }
    }
    let Some(open_end) = open_end else {
        return cell_xml.to_vec();
    };
    let tc_pr = find_element_bytes(cell_xml, "tcPr");
    let mut out: Vec<u8> = Vec::with_capacity(cell_xml.len());
    out.extend_from_slice(&cell_xml[..open_end]);
    out.extend_from_slice(&tc_pr);
    out.extend_from_slice(b"<w:p/>");
    out.extend_from_slice(b"</w:tc>");
    out
}

/// Byte range of one element by local name (first occurrence).
fn find_element_bytes(xml: &[u8], name: &str) -> Vec<u8> {
    let tags = scan_tags(xml);
    let mut stack: Vec<(String, usize, usize)> = Vec::new();
    for tag in &tags {
        if tag.closing {
            if let Some((top, _, _)) = stack.last().cloned() {
                if top == tag.name {
                    let (top, start, _) = stack.pop().expect("checked");
                    if top == name {
                        return xml[start..tag.end].to_vec();
                    }
                }
            }
            continue;
        }
        if !tag.self_closing {
            stack.push((tag.name.clone(), tag.start, tag.end));
        }
    }
    Vec::new()
}

/// `_merge_cell_bytes`: set `gridSpan=span` on a cell (or add it to tcPr).
fn merge_cell_bytes(cell_xml: &[u8], span: usize) -> Vec<u8> {
    if span <= 1 {
        return cell_xml.to_vec();
    }
    let tc_pr = find_element_bytes(cell_xml, "tcPr");
    if !tc_pr.is_empty() {
        let mut merged: Vec<u8>;
        if tc_pr.windows(10).any(|w| w == b"gridSpan") {
            merged = tc_pr.clone();
            replace_grid_span(&mut merged, span);
        } else {
            merged = tc_pr[..tc_pr.len() - b"</w:tcPr>".len()].to_vec();
            merged.extend_from_slice(format!("<w:gridSpan w:val=\"{span}\"/>").as_bytes());
            merged.extend_from_slice(b"</w:tcPr>");
        }
        return splice_replace(cell_xml, &tc_pr, &merged);
    }
    // no tcPr: inject one before the first child
    let tags = scan_tags(cell_xml);
    let open_end = tags
        .iter()
        .find(|tag| tag.name == "tc" && !tag.closing && !tag.self_closing)
        .map(|tag| tag.end)
        .unwrap_or(0);
    let mut out: Vec<u8> = Vec::with_capacity(cell_xml.len() + 40);
    out.extend_from_slice(&cell_xml[..open_end]);
    out.extend_from_slice(format!("<w:tcPr><w:gridSpan w:val=\"{span}\"/></w:tcPr>").as_bytes());
    out.extend_from_slice(&cell_xml[open_end..]);
    out
}

/// Replace the first `<w:gridSpan w:val="N"/>` with `span`.
fn replace_grid_span(tc_pr: &mut Vec<u8>, span: usize) {
    let needle = b"<w:gridSpan";
    let Some(pos) = tc_pr.windows(needle.len()).position(|w| w == needle) else {
        return;
    };
    let rest = &tc_pr[pos..];
    let Some(end) = rest.iter().position(|&b| b == b'>') else {
        return;
    };
    tc_pr.splice(
        pos..pos + end + 1,
        format!("<w:gridSpan w:val=\"{span}\"/>").into_bytes(),
    );
}

/// Replace the first occurrence of `old` in `xml` with `new`.
fn splice_replace(xml: &[u8], old: &[u8], new: &[u8]) -> Vec<u8> {
    if old.is_empty() {
        return xml.to_vec();
    }
    let Some(pos) = xml.windows(old.len()).position(|w| w == old) else {
        return xml.to_vec();
    };
    let mut out = Vec::with_capacity(xml.len() + new.len());
    out.extend_from_slice(&xml[..pos]);
    out.extend_from_slice(new);
    out.extend_from_slice(&xml[pos + old.len()..]);
    out
}

/// `_split_cell_bytes`: reduce gridSpan to 1 and return `span` copies (the
/// first keeps content; the extras are empty cells with a single paragraph).
fn split_cell_bytes(cell_xml: &[u8], span: usize) -> Vec<Vec<u8>> {
    let tc_pr = find_element_bytes(cell_xml, "tcPr");
    let mut parts: Vec<Vec<u8>> = Vec::with_capacity(span);
    for index in 0..span {
        if index == 0 {
            if !tc_pr.is_empty() && tc_pr.windows(10).any(|w| w == b"gridSpan") {
                let mut single = tc_pr.clone();
                replace_grid_span(&mut single, 1);
                parts.push(splice_replace(cell_xml, &tc_pr, &single));
            } else {
                parts.push(cell_xml.to_vec());
            }
        } else {
            let mut clone = clone_cell_with_empty_paragraph(cell_xml);
            if !tc_pr.is_empty() && tc_pr.windows(10).any(|w| w == b"gridSpan") {
                let clone_tc_pr = find_element_bytes(&clone, "tcPr");
                if !clone_tc_pr.is_empty() {
                    let stripped = strip_grid_span(&clone_tc_pr);
                    clone = splice_replace(&clone, &clone_tc_pr, &stripped);
                }
            }
            parts.push(clone);
        }
    }
    parts
}

/// Remove every `<w:gridSpan .../>` from a tcPr fragment.
fn strip_grid_span(tc_pr: &[u8]) -> Vec<u8> {
    let needle = b"<w:gridSpan";
    let mut out: Vec<u8> = Vec::with_capacity(tc_pr.len());
    let mut cursor = 0usize;
    while let Some(pos) = tc_pr[cursor..]
        .windows(needle.len())
        .position(|w| w == needle)
    {
        let start = cursor + pos;
        let rest = &tc_pr[start..];
        let Some(end) = rest.iter().position(|&b| b == b'>') else {
            break;
        };
        out.extend_from_slice(&tc_pr[cursor..start]);
        cursor = start + end + 1;
    }
    out.extend_from_slice(&tc_pr[cursor..]);
    out
}

/// Byte-level table structure operation on a body-level table (mirror of
/// `apply_table_operation`). Cell text is never rewritten; new structure
/// bytes are synthesized from the template.
#[allow(clippy::too_many_arguments)]
pub fn apply_table_operation(
    xml: &[u8],
    table_index: usize,
    operation: &str,
    args: &[usize],
    discard_content: bool,
) -> Result<Vec<u8>, CoreError> {
    let tables = body_table_ranges(xml);
    let Some(table) = tables.get(table_index).copied() else {
        return Err(CoreError::Domain(format!(
            "invalid-table-reference: T{table_index}"
        )));
    };
    let (rows, cells) = table_rows_and_cells(xml, table.0, table.1)?;
    if rows.is_empty() {
        return Err(CoreError::Domain(format!(
            "table has no rows: T{table_index}"
        )));
    }
    match operation {
        "insert-row" => {
            let after = args.first().copied().ok_or_else(|| {
                CoreError::Domain("row index out of range: insert-row needs an index".to_string())
            })?;
            if after >= rows.len() {
                return Err(CoreError::Domain(format!(
                    "row index out of range: {after}"
                )));
            }
            let template_start = rows[after].0;
            let template_end = rows[after].1;
            let new_row = clone_row_with_empty_cells(&xml[template_start..template_end]);
            let mut out: Vec<u8> = Vec::with_capacity(xml.len() + new_row.len());
            out.extend_from_slice(&xml[..table.0]);
            let mut cursor = table.0;
            for (row_start, row_end) in &rows {
                out.extend_from_slice(&xml[cursor..*row_start]);
                out.extend_from_slice(&xml[*row_start..*row_end]);
                cursor = *row_end;
                if *row_start == template_start {
                    out.extend_from_slice(&new_row);
                }
            }
            out.extend_from_slice(&xml[cursor..]);
            Ok(out)
        }
        "delete-row" => {
            let row = args.first().copied().ok_or_else(|| {
                CoreError::Domain("row index out of range: delete-row needs an index".to_string())
            })?;
            if row >= rows.len() {
                return Err(CoreError::Domain(format!("row index out of range: {row}")));
            }
            let mut out: Vec<u8> = Vec::with_capacity(xml.len());
            out.extend_from_slice(&xml[..table.0]);
            let mut cursor = table.0;
            out.extend_from_slice(&xml[cursor..rows[0].0]);
            cursor = rows[0].0;
            for (idx, (row_start, row_end)) in rows.iter().enumerate() {
                if idx == row {
                    cursor = *row_end;
                    continue;
                }
                out.extend_from_slice(&xml[cursor..*row_start]);
                out.extend_from_slice(&xml[*row_start..*row_end]);
                cursor = *row_end;
            }
            out.extend_from_slice(&xml[cursor..]);
            Ok(out)
        }
        "insert-col" => {
            let after = args.first().copied().ok_or_else(|| {
                CoreError::Domain(
                    "column index out of range: insert-col needs an index".to_string(),
                )
            })?;
            let mut out: Vec<u8> = Vec::with_capacity(xml.len());
            out.extend_from_slice(&xml[..table.0]);
            let mut cursor = table.0;
            for (row_start, row_end) in &rows {
                let row_cells: Vec<(usize, usize)> = cells
                    .iter()
                    .filter(|(cell_start, cell_end)| {
                        *cell_start >= *row_start && *cell_end <= *row_end
                    })
                    .copied()
                    .collect();
                if after >= row_cells.len() {
                    return Err(CoreError::Domain(format!(
                        "column index out of range: {after}"
                    )));
                }
                let template_start = row_cells[after].0;
                let template_end = row_cells[after].1;
                let new_cell = clone_cell_with_empty_paragraph(&xml[template_start..template_end]);
                out.extend_from_slice(&xml[cursor..*row_start]);
                cursor = *row_start;
                for (cell_start, cell_end) in &row_cells {
                    out.extend_from_slice(&xml[cursor..*cell_start]);
                    out.extend_from_slice(&xml[*cell_start..*cell_end]);
                    cursor = *cell_end;
                    if *cell_start == template_start {
                        out.extend_from_slice(&new_cell);
                    }
                }
                out.extend_from_slice(&xml[cursor..*row_end]);
                cursor = *row_end;
            }
            out.extend_from_slice(&xml[cursor..]);
            Ok(out)
        }
        "delete-col" => {
            let col = args.first().copied().ok_or_else(|| {
                CoreError::Domain(
                    "column index out of range: delete-col needs an index".to_string(),
                )
            })?;
            let mut out: Vec<u8> = Vec::with_capacity(xml.len());
            out.extend_from_slice(&xml[..table.0]);
            let mut cursor = table.0;
            for (row_start, row_end) in &rows {
                let row_cells: Vec<(usize, usize)> = cells
                    .iter()
                    .filter(|(cell_start, cell_end)| {
                        *cell_start >= *row_start && *cell_end <= *row_end
                    })
                    .copied()
                    .collect();
                if col >= row_cells.len() {
                    return Err(CoreError::Domain(format!(
                        "column index out of range: {col}"
                    )));
                }
                out.extend_from_slice(&xml[cursor..*row_start]);
                cursor = *row_start;
                for (idx, (cell_start, cell_end)) in row_cells.iter().enumerate() {
                    if idx == col {
                        out.extend_from_slice(&xml[cursor..*cell_start]);
                        cursor = *cell_end;
                        continue;
                    }
                    out.extend_from_slice(&xml[cursor..*cell_start]);
                    out.extend_from_slice(&xml[*cell_start..*cell_end]);
                    cursor = *cell_end;
                }
                out.extend_from_slice(&xml[cursor..*row_end]);
                cursor = *row_end;
            }
            out.extend_from_slice(&xml[cursor..]);
            Ok(out)
        }
        "merge-cells" => {
            if args.len() < 3 {
                return Err(CoreError::Domain(
                    "merge span out of range: merge-cells needs row col span".to_string(),
                ));
            }
            let (row, col, span) = (args[0], args[1], args[2]);
            let row_cells: Vec<Vec<(usize, usize)>> = rows
                .iter()
                .map(|(row_start, row_end)| {
                    cells
                        .iter()
                        .filter(|(cell_start, cell_end)| {
                            *cell_start >= *row_start && *cell_end <= *row_end
                        })
                        .copied()
                        .collect()
                })
                .collect();
            if !discard_content && row < row_cells.len() {
                let target = &row_cells[row];
                let mut discarded: Vec<(usize, String)> = Vec::new();
                for idx in (col + 1)..(col + span) {
                    if idx >= target.len() {
                        break;
                    }
                    let (cell_start, cell_end) = target[idx];
                    let text = cell_visible_text(&xml[cell_start..cell_end]);
                    if !text.is_empty() {
                        discarded.push((idx, text));
                    }
                }
                if !discarded.is_empty() {
                    let detail: Vec<String> = discarded
                        .iter()
                        .map(|(idx, text)| format!("cell {row},{idx} contains {text:?}"))
                        .collect();
                    return Err(CoreError::Domain(format!(
                        "merge-would-discard-content: {}; pass discard_content=true to drop the spanned cells' text",
                        detail.join(", ")
                    )));
                }
            }
            let mut out: Vec<u8> = Vec::with_capacity(xml.len());
            out.extend_from_slice(&xml[..table.0]);
            let mut cursor = table.0;
            for (row_idx, (row_start, row_end)) in rows.iter().enumerate() {
                let row_cells_now = &row_cells[row_idx];
                out.extend_from_slice(&xml[cursor..*row_start]);
                cursor = *row_start;
                if row_idx == row {
                    if col + span > row_cells_now.len() {
                        return Err(CoreError::Domain(format!(
                            "merge span out of range: {col}+{span}"
                        )));
                    }
                    for (idx, (cell_start, cell_end)) in row_cells_now.iter().enumerate() {
                        if idx == col {
                            out.extend_from_slice(&xml[cursor..*cell_start]);
                            let merged = merge_cell_bytes(&xml[*cell_start..*cell_end], span);
                            out.extend_from_slice(&merged);
                            cursor = row_cells_now[col + span - 1].1;
                            continue;
                        }
                        if col < idx && idx < col + span {
                            continue;
                        }
                        out.extend_from_slice(&xml[cursor..*cell_start]);
                        out.extend_from_slice(&xml[*cell_start..*cell_end]);
                        cursor = *cell_end;
                    }
                    out.extend_from_slice(&xml[cursor..*row_end]);
                    cursor = *row_end;
                    continue;
                }
                for (cell_start, cell_end) in row_cells_now {
                    out.extend_from_slice(&xml[cursor..*cell_start]);
                    out.extend_from_slice(&xml[*cell_start..*cell_end]);
                    cursor = *cell_end;
                }
                out.extend_from_slice(&xml[cursor..*row_end]);
                cursor = *row_end;
            }
            out.extend_from_slice(&xml[cursor..]);
            Ok(out)
        }
        "split-cells" => {
            if args.len() < 3 {
                return Err(CoreError::Domain(
                    "split out of range: split-cells needs row col span".to_string(),
                ));
            }
            let (row, col, span) = (args[0], args[1], args[2]);
            let mut out: Vec<u8> = Vec::with_capacity(xml.len());
            out.extend_from_slice(&xml[..table.0]);
            let mut cursor = table.0;
            for (row_idx, (row_start, row_end)) in rows.iter().enumerate() {
                let row_cells_now: Vec<(usize, usize)> = cells
                    .iter()
                    .filter(|(cell_start, cell_end)| {
                        *cell_start >= *row_start && *cell_end <= *row_end
                    })
                    .copied()
                    .collect();
                out.extend_from_slice(&xml[cursor..*row_start]);
                cursor = *row_start;
                if row_idx == row {
                    if col >= row_cells_now.len() || span < 1 {
                        return Err(CoreError::Domain(format!(
                            "split out of range: {col}/{span}"
                        )));
                    }
                    let (cell_start, cell_end) = row_cells_now[col];
                    let split_parts = split_cell_bytes(&xml[cell_start..cell_end], span);
                    out.extend_from_slice(&xml[cursor..cell_start]);
                    for part in &split_parts {
                        out.extend_from_slice(part);
                    }
                    cursor = cell_end;
                    for (later_start, later_end) in &row_cells_now[col + 1..] {
                        out.extend_from_slice(&xml[cursor..*later_start]);
                        out.extend_from_slice(&xml[*later_start..*later_end]);
                        cursor = *later_end;
                    }
                    out.extend_from_slice(&xml[cursor..*row_end]);
                    cursor = *row_end;
                    continue;
                }
                for (cell_start, cell_end) in &row_cells_now {
                    out.extend_from_slice(&xml[cursor..*cell_start]);
                    out.extend_from_slice(&xml[*cell_start..*cell_end]);
                    cursor = *cell_end;
                }
                out.extend_from_slice(&xml[cursor..*row_end]);
                cursor = *row_end;
            }
            out.extend_from_slice(&xml[cursor..]);
            Ok(out)
        }
        other => Err(CoreError::Domain(format!(
            "unknown table operation: {other}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Unicode normalization audit (read-only)
// ---------------------------------------------------------------------------

/// Scan the package's editable prose for Unicode vertical-catalog
/// candidates (mirror of `typed_normalize.find_candidates` over the raw
/// leaves). `catalog` is the `unicode-vertical-catalog-1` payload.
pub fn scan_normalization_candidates(
    package: &[u8],
    catalog: &serde_json::Value,
) -> Result<Vec<NormalizationCandidate>, CoreError> {
    let entries = catalog
        .get("entries")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| CoreError::Domain("incompatible Unicode vertical catalog".to_string()))?;
    let mut candidates: Vec<NormalizationCandidate> = Vec::new();
    let mut candidate_count = 0usize;
    let members = package_xml_members(package)?;
    for (name, xml) in &members {
        if !is_prose_part(name) {
            continue;
        }
        let is_document = name == "word/document.xml";
        let part_key = if is_document {
            "document".to_string()
        } else {
            part_key_from_path(name).expect("prose part")
        };
        let paragraphs: Vec<(String, usize, usize)> = if is_document {
            prose::scan_document(xml)?
        } else {
            prose::scan_part(xml, &part_key)?
        };
        for (paragraph_id, start, end) in paragraphs {
            let mut occurrence = 0usize;
            let tags = scan_tags(xml);
            let mut depth = 0usize;
            for tag in &tags {
                if tag.start < start || tag.start >= end {
                    continue;
                }
                if tag.closing {
                    depth = depth.saturating_sub(1);
                    continue;
                }
                if tag.name == "t" && !tag.self_closing {
                    let open_end = tag.end;
                    let close_start = tags
                        .iter()
                        .find(|candidate| {
                            candidate.closing
                                && candidate.name == "t"
                                && candidate.start >= tag.end
                                && candidate.start < end
                        })
                        .map(|candidate| candidate.start)
                        .unwrap_or(tag.end);
                    let text = element_text(xml, open_end, close_start);
                    for (char_index, ch) in text.char_indices() {
                        let codepoint = format!("U+{:04X}", ch as u32);
                        if let Some(entry) = entries.get(&codepoint) {
                            occurrence += 1;
                            candidate_count += 1;
                            candidates.push(NormalizationCandidate {
                                candidate_id: format!("C{candidate_count:05}"),
                                occurrence_id: format!("{paragraph_id}-V{occurrence:04}"),
                                paragraph_id: paragraph_id.clone(),
                                codepoint: codepoint.clone(),
                                source: ch.to_string(),
                                name: entry
                                    .get("name")
                                    .and_then(serde_json::Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                                category: entry
                                    .get("class")
                                    .and_then(serde_json::Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                                vertical: entry
                                    .get("vertical")
                                    .and_then(serde_json::Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                                proposed_target: entry
                                    .get("target")
                                    .and_then(serde_json::Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                                reversible: entry
                                    .get("reversible")
                                    .and_then(serde_json::Value::as_bool)
                                    .unwrap_or(false),
                                context: context_text(&text, char_index),
                            });
                        }
                    }
                    continue;
                }
                if !tag.self_closing {
                    depth += 1;
                }
            }
        }
    }
    Ok(candidates)
}

/// 12-char context around `char_index` (mirror of `_context`).
fn context_text(text: &str, char_index: usize) -> String {
    let start = text[..char_index]
        .char_indices()
        .nth_back(11)
        .map(|(index, _)| index)
        .unwrap_or(0);
    let end = text[char_index..]
        .char_indices()
        .nth(12)
        .map(|(index, _)| char_index + index)
        .unwrap_or(text.len());
    text[start..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_matches_python() {
        assert_eq!(revision_fingerprint("已插入内容"), "888c104169b5");
        assert_eq!(revision_fingerprint(""), "e3b0c44298fc");
        assert_eq!(
            revision_key("word/document.xml", "insert", "1", "已插入内容"),
            "word/document.xml|insert|1|888c104169b5"
        );
    }

    #[test]
    fn views_accept_and_reject_differ() {
        let package = std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus/release/revisions.docx"),
        )
        .unwrap();
        let accept = revision_views_bytes(&package, "accept").unwrap();
        let reject = revision_views_bytes(&package, "reject").unwrap();
        let accept_p1 = accept.iter().find(|v| v.id == "P1").unwrap();
        let reject_p1 = reject.iter().find(|v| v.id == "P1").unwrap();
        assert_eq!(accept_p1.text, "保留");
        assert_eq!(reject_p1.text, "保留旧文本");
        let accept_p0 = accept.iter().find(|v| v.id == "P0").unwrap();
        assert_eq!(accept_p0.text, "修订前文已插入内容修订后文");
        // P6 del hidden under accept, visible under reject.
        let accept_p6 = accept.iter().find(|v| v.id == "P6").unwrap();
        assert_eq!(accept_p6.text, "");
        let reject_p6 = reject.iter().find(|v| v.id == "P6").unwrap();
        assert_eq!(reject_p6.text, "修订六");
    }

    #[test]
    fn inventory_matches_python() {
        let package = std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus/release/revisions.docx"),
        )
        .unwrap();
        let entries = scan_revisions_bytes(&package).unwrap();
        assert_eq!(entries.len(), 8);
        let by_id: BTreeMap<&str, &RevisionEntry> = entries
            .iter()
            .map(|entry| (entry.w_id.as_str(), entry))
            .collect();
        let ins1 = by_id["1"];
        assert_eq!(ins1.kind, "insert");
        assert_eq!(ins1.text, "已插入内容");
        assert!(ins1.editable);
        assert_eq!(ins1.paragraph_id.as_deref(), Some("P0"));
        assert_eq!(
            ins1.revision_key(),
            "word/document.xml|insert|1|888c104169b5"
        );
        let del2 = by_id["2"];
        assert_eq!(del2.kind, "delete");
        assert!(del2.editable);
        let mark3 = by_id["3"];
        assert!(!mark3.editable);
        assert_eq!(mark3.reason.as_deref(), Some("paragraph-mark-revision"));
        let opaque4 = by_id["4"];
        assert!(!opaque4.editable);
        assert_eq!(
            opaque4.reason.as_deref(),
            Some("nested-container-or-non-editable-part")
        );
        let opaque5 = by_id["5"];
        assert!(!opaque5.editable);
        let six = by_id["6"];
        assert_eq!(six.text, "修订五");
        let seven = by_id["7"];
        assert_eq!(seven.text, "修订六");
        assert!(seven.editable);
    }

    #[test]
    fn settle_one_accept_unwraps_insert() {
        let package = std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus/release/revisions.docx"),
        )
        .unwrap();
        let member = package_xml_members(&package)
            .unwrap()
            .into_iter()
            .find(|(name, _)| name == "word/document.xml")
            .unwrap()
            .1;
        let settled = settle_one_revision(&member, "1", "accept").unwrap();
        let text = paragraph_visible_text(&settled.part_xml, 0, settled.part_xml.len(), "accept");
        assert!(text.contains("修订前文已插入内容修订后文"));
        // The w:ins wrapper is gone.
        assert!(!String::from_utf8_lossy(&settled.part_xml).contains("w:ins w:id=\"1\""));
    }

    #[test]
    fn settle_one_reject_unwraps_delete_with_deltext() {
        let package = std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus/release/revisions.docx"),
        )
        .unwrap();
        let member = package_xml_members(&package)
            .unwrap()
            .into_iter()
            .find(|(name, _)| name == "word/document.xml")
            .unwrap()
            .1;
        let settled = settle_one_revision(&member, "2", "reject").unwrap();
        let xml = String::from_utf8_lossy(&settled.part_xml);
        // The target deletion unwrapped: its delText became a live w:t.
        assert!(xml.contains("<w:t>旧文本</w:t>"));
        assert!(!xml.contains("w:del w:id=\"2\""));
        // Untouched revisions keep their delText (del w:id 7 remains).
        assert!(xml.contains("<w:delText>修订六</w:delText>"));
        // The P1 region has no delText left.
        let p1_start = xml.find("<w:p><w:r><w:t>保留").expect("P1");
        let p1_end = xml[p1_start..]
            .find("</w:p>")
            .map(|i| p1_start + i)
            .unwrap();
        assert!(!xml[p1_start..p1_end].contains("delText"));
    }

    #[test]
    fn settle_rejects_unknown_and_mark_and_opaque() {
        let package = std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus/release/revisions.docx"),
        )
        .unwrap();
        let member = package_xml_members(&package)
            .unwrap()
            .into_iter()
            .find(|(name, _)| name == "word/document.xml")
            .unwrap()
            .1;
        assert_eq!(
            settle_one_revision(&member, "99", "accept").unwrap_err().0,
            "revision-not-found"
        );
        assert_eq!(
            settle_one_revision(&member, "3", "accept").unwrap_err().0,
            "revision-not-found"
        );
        assert_eq!(
            settle_one_revision(&member, "4", "accept").unwrap_err().0,
            "revision-not-found"
        );
    }

    #[test]
    fn reinsert_creates_new_insert_after_deletion() {
        let package = std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus/release/revisions.docx"),
        )
        .unwrap();
        let member = package_xml_members(&package)
            .unwrap()
            .into_iter()
            .find(|(name, _)| name == "word/document.xml")
            .unwrap()
            .1;
        let (out, record) = reinsert_deleted_text(
            &member,
            &package,
            "2",
            "审稿人",
            "2026-08-14T00:00:00+00:00",
            None,
        )
        .unwrap();
        assert_eq!(record.new_w_id, "0");
        assert_eq!(record.operation, "new-insert-after-deletion");
        let xml = String::from_utf8_lossy(&out);
        assert!(xml.contains("<w:ins w:id=\"0\" w:author=\"审稿人\""));
        assert!(xml.contains("<w:t>旧文本</w:t>"));
    }

    #[test]
    fn table_ops_preserve_content_and_structure() {
        let package = std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus/release/table.docx"),
        )
        .unwrap();
        let member = package_xml_members(&package)
            .unwrap()
            .into_iter()
            .find(|(name, _)| name == "word/document.xml")
            .unwrap()
            .1;
        // insert-row after 1 on T1 (table index 1).
        let patched = apply_table_operation(&member, 1, "insert-row", &[1], false).unwrap();
        let tables = body_table_ranges(&patched);
        assert_eq!(tables.len(), 2);
        let (rows, _) = table_rows_and_cells(&patched, tables[1].0, tables[1].1).unwrap();
        assert_eq!(rows.len(), 4); // 3 -> 4
                                   // inserted row is empty (no text beyond the source rows).
        let before_text = cell_texts(&member, 1);
        let after_text = cell_texts(&patched, 1);
        assert_eq!(after_text.len(), before_text.len() + 3);
        // delete-col guard and content preservation
        assert!(apply_table_operation(&member, 0, "merge-cells", &[0, 0, 2], false).is_err());
        let merged = apply_table_operation(&member, 0, "merge-cells", &[0, 0, 2], true).unwrap();
        let (_, merged_cells) = table_rows_and_cells(&merged, tables[0].0, tables[0].1).unwrap();
        assert_eq!(merged_cells.len(), 8); // 9 - 2 + 1 (gridSpan merge)
    }

    fn cell_texts(xml: &[u8], table_index: usize) -> Vec<String> {
        let tables = body_table_ranges(xml);
        let (_, cells) =
            table_rows_and_cells(xml, tables[table_index].0, tables[table_index].1).unwrap();
        cells
            .iter()
            .map(|(start, end)| cell_visible_text(&xml[*start..*end]))
            .collect()
    }
}
