//! Typed read-only document projection: a pure-Core render model over one
//! immutable DOCX package.
//!
//! `DocumentProjection` is computed from a DOCX/package `Path` or an
//! explicit `DocumentSource` (never from Store, Review, HTTP, or UI state)
//! and exposes:
//! - an ordered block tree per part (`parts` -> [`Block`]), referencing
//!   canonical typed paragraph ids — blocks never duplicate body text,
//! - five registries: paragraphs (canonical text + [`Segment`]s), tables
//!   (structure only), styles (semantic allowlist, no raw `rPr`),
//!   revisions, and comments (Core `govern` inventory),
//! - opaque blocks and source fingerprints (package + per-part SHA-256),
//! - pure Core fingerprint entries ([`paragraph_fingerprint`] /
//!   [`region_fingerprint`]) over canonical paragraph/segment data, so a
//!   patch backend can re-prove a target without DOM or Store state.
//!
//! Deep-module contract (issue #36): core owns the byte walker and the
//! canonical identities; the projection mirrors the recursive-prose
//! inventory (`prose::enumerate_package` + `govern` locators) so every id
//! comes from Core's canonical source identities, never from a
//! presentation adapter.
//!
//! ## Position contract (frozen)
//!
//! [`POSITION_CONTRACT`] = `"unicode-scalar-1"`: every [`Segment::start`] /
//! [`Segment::end`] is a count of Unicode scalar values (Rust `char`s)
//! from the start of the owning paragraph's canonical text. Never byte
//! offsets, never UTF-16 code units.
//!
//! ## Segmentation contract
//!
//! Within a paragraph, segments are the maximal spans that do not cross
//! ANY semantic boundary of the frozen union: run/style, revision,
//! comment, editable/opaque, and canonical range (one `w:t`/`w:delText`
//! element = one canonical range). OOXML places every boundary source at
//! element granularity, so the emitted segments are exactly the canonical
//! text spans; each carries `start`/`end`/`text`/`style_id`/revision
//! key/comment ids/`editable`/[`Visibility`].
//!
//! ## Visibility (computed by Core)
//!
//! [`Visibility`] is the three frozen views, resolved once here so callers
//! never re-interpret OOXML revision markup:
//! - `original` — visible in the pre-edit document (hidden inside
//!   `w:ins`/`w:moveTo`),
//! - `tracked` — visible with markup shown (always true for prose text),
//! - `final_view` — visible after accepting all changes (hidden inside
//!   `w:del`/`w:moveFrom`).
//!
//! ## Style allowlist
//!
//! Each run's `rPr` maps to a content-addressed [`ProjectionStyle::style_id`]
//! (`s_` + first 16 hex of the SHA-256 over the canonical rPr bytes —
//! `w:rPrChange` children excluded). The registry carries only the
//! semantic allowlist (`features` + `label`); raw `rPr` XML is never
//! exposed to the projection. The default (no-`rPr`) style is
//! [`default_style_id`].

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Serialize;

use docx2typed_protocol::{bytes_sha256, semantic_sha256};

use crate::govern::{self, CommentEntry, LocatedRevision};
use crate::prose::{self, OpaqueBlock, ProjectionImage};
use crate::xml_walker::{build_tree, scan_tags, ElementNode};
use crate::CoreError;

/// Schema of the serialized projection (self-describing JSON).
pub const PROJECTION_SCHEMA: &str = "docx2typed-document-projection-1";
/// Frozen position contract: segment offsets are Unicode scalar values.
pub const POSITION_CONTRACT: &str = "unicode-scalar-1";
/// Prefix of content-addressed style ids (`s_<sha256[:16]>`).
pub const STYLE_ID_PREFIX: &str = "s_";

/// Boolean run properties of the semantic allowlist (mirror of Python
/// `_BOOLEAN_RPR`, plus `dstrike` which Python's label table already maps).
const BOOLEAN_RPR: [&str; 10] = [
    "b",
    "i",
    "strike",
    "dstrike",
    "outline",
    "shadow",
    "smallCaps",
    "caps",
    "vanish",
    "webHidden",
];
/// `w:val` values that disable a boolean property (mirror of
/// `_FALSE_VALUES`).
const FALSE_VALUES: [&str; 4] = ["false", "0", "off", "none"];
/// Valued run properties of the semantic allowlist (mirror of
/// `rpr_features`).
const VALUE_RPR: [&str; 16] = [
    "vertAlign",
    "position",
    "color",
    "sz",
    "szCs",
    "highlight",
    "lang",
    "u",
    "kern",
    "spacing",
    "w",
    "rStyle",
    "em",
    "rtl",
    "cs",
    "textEffect",
];

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Explicit input of one projection: a DOCX/package path or the package
/// bytes in memory. Store, Review, and HTTP types never participate.
#[derive(Clone, Debug)]
pub enum DocumentSource {
    /// Path to a DOCX package (e.g. a pinned generation's `_template.docx`).
    Path(PathBuf),
    /// In-memory DOCX package bytes.
    Bytes(Vec<u8>),
}

/// Project one DOCX package from an explicit source.
pub fn project_document(source: &DocumentSource) -> Result<DocumentProjection, CoreError> {
    let bytes = match source {
        DocumentSource::Path(path) => std::fs::read(path).map_err(CoreError::io)?,
        DocumentSource::Bytes(bytes) => bytes.clone(),
    };
    project_document_bytes(&bytes)
}

/// Project one DOCX package at a path (a pinned generation's
/// `_template.docx`, a workdir template, or any package file).
pub fn project_document_path(path: &Path) -> Result<DocumentProjection, CoreError> {
    let bytes = std::fs::read(path).map_err(CoreError::io)?;
    project_document_bytes(&bytes)
}

/// Convenience for the review layer: project the DOCX package of a workdir
/// or pinned-generation directory (`<dir>/_template.docx`). The Core
/// contract stays the DOCX/package path or [`DocumentSource`]; this entry
/// only locates the template and delegates.
pub fn project_workdir(workdir: &Path) -> Result<DocumentProjection, CoreError> {
    let template = workdir.join("_template.docx");
    if !template.is_file() {
        return Err(CoreError::Domain(format!(
            "projection: _template.docx not found in {}",
            workdir.to_string_lossy()
        )));
    }
    project_document_path(&template)
}

/// Project one DOCX package held in memory.
pub fn project_document_bytes(package: &[u8]) -> Result<DocumentProjection, CoreError> {
    let inventory = prose::enumerate_package_bytes(package)?;
    let manifest = crate::package_manifest_bytes(package)?;
    let source_sha256 = bytes_sha256(package);
    let document_xml_sha256 = manifest
        .get("word/document.xml")
        .cloned()
        .unwrap_or_default();
    let comments = govern::scan_comments_bytes(package)?;
    let part_xml = part_xml_members(package)?;

    let default_style_id = default_style_id();
    let mut styles = StyleRegistry {
        entries: Vec::new(),
        by_id: HashMap::new(),
    };
    styles.ensure(&default_style_id, "Normal".to_string(), BTreeMap::new());

    let mut paragraphs: Vec<ProjectionParagraph> = Vec::new();
    let mut tables: Vec<ProjectionTable> = Vec::new();
    let mut revisions: Vec<ProjectionRevision> = Vec::new();
    let mut parts: Vec<PartProjection> = Vec::new();

    // The prose inventory is the single source of part order and paragraph
    // order; per part we re-locate to obtain byte ranges, then zip.
    let mut cursor = 0usize;
    for part_key in &inventory.part_keys {
        // Comments are a separate registry/anchor surface, not document
        // paragraphs or blocks.
        if part_key == "comments" {
            continue;
        }
        let part_key = part_key.as_str();
        let xml = part_xml.get(part_key).ok_or_else(|| {
            CoreError::Domain(format!("projection: part bytes missing: {part_key}"))
        })?;
        let is_document = part_key == "document";
        let part_path = prose::part_path(part_key);
        let tags = scan_tags(xml);
        let nodes = build_tree(&tags)?;
        let located = if is_document {
            prose::scan_document(xml)?
        } else {
            prose::scan_part(xml, part_key)?
        };
        let located_map: HashMap<(usize, usize), String> = located
            .iter()
            .map(|(id, start, end)| ((*start, *end), id.clone()))
            .collect();

        let end = cursor + located.len();
        if end > inventory.paragraphs.len() {
            return Err(CoreError::Domain(format!(
                "projection: inventory/locator mismatch in {part_key}"
            )));
        }
        let part_entries = &inventory.paragraphs[cursor..end];
        cursor = end;

        // Revision registry for this part (Core govern locator + canonical
        // keys); segments reference revisions by these keys.
        let located_revs = govern::locate_revisions(part_key, xml, &part_path, is_document)?;
        let rev_keys: HashMap<(usize, usize), String> = located_revs
            .iter()
            .map(|r| {
                (
                    (r.start, r.end),
                    govern::revision_key(&part_path, &r.kind, &r.w_id, &r.text),
                )
            })
            .collect();
        for r in &located_revs {
            revisions.push(projection_revision(&part_path, r, is_document));
        }

        // Paragraph registry + segments. Paragraphs are processed in byte
        // order so comment ranges spanning paragraphs resolve correctly;
        // registry order stays the canonical inventory order.
        let mut order: Vec<usize> = (0..located.len()).collect();
        order.sort_by_key(|&i| located[i].1);
        let mut open_comments: Vec<String> = Vec::new();
        let mut by_index: BTreeMap<usize, ProjectionParagraph> = BTreeMap::new();
        for &i in &order {
            let (id, start, end) = &located[i];
            let entry = &part_entries[i];
            let Some(node_idx) = nodes.iter().position(|n| {
                n.open_start == *start
                    && (n.end == *end || (n.self_closing && n.open_start == *end))
            }) else {
                return Err(CoreError::Domain(format!(
                    "projection: paragraph locator disagrees with parsed XML: {id}"
                )));
            };
            let mut walker = SegmentWalker {
                xml,
                nodes: &nodes,
                paragraph_editable: entry.editable,
                default_style_id: default_style_id.clone(),
                styles: &mut styles,
                rev_keys: &rev_keys,
                open_comments: &mut open_comments,
                rev_stack: Vec::new(),
                offset: 0,
                segments: Vec::new(),
            };
            walker.visit_container(node_idx);
            by_index.insert(
                i,
                ProjectionParagraph {
                    part: part_key.to_string(),
                    paragraph_id: id.clone(),
                    editable: entry.editable,
                    leaf_count: entry.leaf_count,
                    opaque_count: entry.opaque_count,
                    start: *start,
                    end: *end,
                    segments: walker.segments,
                },
            );
        }
        for (_, paragraph) in by_index {
            paragraphs.push(paragraph);
        }

        // Ordered block tree of this part.
        let Some(root_idx) = nodes.iter().position(|n| n.parent.is_none()) else {
            return Err(CoreError::Domain(format!(
                "projection: no root element in {part_key}"
            )));
        };
        let kind = if is_document {
            PartKind::Document
        } else if part_key.starts_with("header") || part_key.starts_with("footer") {
            PartKind::HeaderFooter
        } else {
            PartKind::Notes
        };
        let mut cell_groups: HashMap<u32, BTreeMap<(u32, u32), CellGroup>> = HashMap::new();
        if matches!(kind, PartKind::HeaderFooter) {
            for (id, start, end) in &located {
                let Some((table, row, col)) = parse_cell_id(id) else {
                    continue;
                };
                let group = cell_groups
                    .entry(table)
                    .or_default()
                    .entry((row, col))
                    .or_insert_with(|| CellGroup {
                        paragraph_ids: Vec::new(),
                        start: *start,
                        end: *end,
                    });
                group.paragraph_ids.push(id.clone());
                group.start = group.start.min(*start);
                group.end = group.end.max(*end);
            }
        }
        let mut builder = TreeBuilder {
            nodes: &nodes,
            located: &located_map,
            part: part_key,
            tables: Vec::new(),
            table_ordinal: 0,
            box_ordinal: -1,
            sdt_ordinal: -1,
        };
        let blocks = builder.part_blocks(root_idx, kind, &cell_groups);
        tables.extend(builder.tables);
        parts.push(PartProjection {
            part: part_key.to_string(),
            blocks,
        });
    }

    let opaques = inventory.opaques;
    let images = extract_images(package, &opaques, &part_xml)?;

    Ok(DocumentProjection {
        schema: PROJECTION_SCHEMA.to_string(),
        position_contract: POSITION_CONTRACT.to_string(),
        source: SourceIdentity {
            source_sha256,
            package_manifest: manifest,
            document_xml_sha256,
        },
        parts,
        paragraphs,
        tables,
        styles: styles.entries,
        revisions,
        comments,
        opaques,
        images,
    })
}

// ---------------------------------------------------------------------------
// Projection model
// ---------------------------------------------------------------------------

/// The typed render projection of one immutable DOCX package.
#[derive(Clone, Debug, Serialize)]
pub struct DocumentProjection {
    pub schema: String,
    pub position_contract: String,
    pub source: SourceIdentity,
    /// Part projections in canonical extract order (headers, document,
    /// footers, endnotes, and footnotes — comments are the separate
    /// comment registry).
    pub parts: Vec<PartProjection>,
    /// Paragraph registry: canonical text + segments in extract order.
    pub paragraphs: Vec<ProjectionParagraph>,
    /// Table registry: structure only (rows/cells reference paragraph ids).
    pub tables: Vec<ProjectionTable>,
    /// Semantic allowlisted style registry (no raw rPr).
    pub styles: Vec<ProjectionStyle>,
    /// Revision registry (Core govern inventory + byte ranges).
    pub revisions: Vec<ProjectionRevision>,
    /// Comment registry (Core `govern::scan_comments_bytes` inventory).
    pub comments: Vec<CommentEntry>,
    /// Opaque blocks (locked interiors) with byte ranges.
    pub opaques: Vec<OpaqueBlock>,
    /// Raster images inside drawing runs (locked interiors, rendered for
    /// the review console; never part of the editable surface).
    pub images: Vec<ProjectionImage>,
}

impl DocumentProjection {
    /// Look up one paragraph registry entry by part + canonical id.
    pub fn paragraph(&self, part: &str, paragraph_id: &str) -> Option<&ProjectionParagraph> {
        self.paragraphs
            .iter()
            .find(|p| p.part == part && p.paragraph_id == paragraph_id)
    }
}

/// Package identity of the projected source (mirror of `format.json`'s
/// fingerprint records).
#[derive(Clone, Debug, Serialize)]
pub struct SourceIdentity {
    /// SHA-256 of the whole package bytes.
    pub source_sha256: String,
    /// Per-member SHA-256 of the package (zip path -> hash).
    pub package_manifest: BTreeMap<String, String>,
    /// SHA-256 of `word/document.xml`.
    pub document_xml_sha256: String,
}

/// One part of the projection with its ordered block tree.
#[derive(Clone, Debug, Serialize)]
pub struct PartProjection {
    /// Part key: "document", "header1", "footer1", "footnotes", ...
    pub part: String,
    pub blocks: Vec<Block>,
}

/// One node of the ordered block tree.
///
/// Blocks reference the registries by canonical id/index; they never carry
/// body text. Box/Sdt blocks are the content of a `w:txbxContent` /
/// `w:sdtContent`; box blocks nest under their containing paragraph.
#[derive(Clone, Debug, Serialize)]
pub enum Block {
    /// A paragraph element; `nested` holds the box blocks inside it.
    Paragraph {
        paragraph_id: String,
        nested: Vec<Block>,
    },
    /// A table, referenced by its per-part ordinal into the table registry.
    Table { table_index: u32 },
    /// Textbox content (`w:txbxContent`); `blocks` are its paragraphs.
    Box { box_index: u32, blocks: Vec<Block> },
    /// Structured document tag content (`w:sdtContent`).
    Sdt { sdt_index: u32, blocks: Vec<Block> },
}

/// One paragraph of the registry: canonical text + segments.
///
/// The canonical text is the concatenation of `segments[].text` (the
/// single source of body text — blocks and tables only reference ids).
#[derive(Clone, Debug, Serialize)]
pub struct ProjectionParagraph {
    pub part: String,
    pub paragraph_id: String,
    /// true when the paragraph contains no opaque node.
    pub editable: bool,
    pub leaf_count: usize,
    pub opaque_count: usize,
    /// Byte range of the paragraph element inside its part XML.
    pub start: usize,
    pub end: usize,
    pub segments: Vec<Segment>,
}

/// One canonical text span of a paragraph (maximal span that crosses no
/// run/style, revision, comment, editable/opaque, or canonical-range
/// boundary).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Segment {
    /// Unicode-scalar offset, paragraph-relative, inclusive
    /// (`POSITION_CONTRACT = "unicode-scalar-1"`).
    pub start: usize,
    /// Unicode-scalar offset, paragraph-relative, exclusive.
    pub end: usize,
    /// Canonical decoded text of the span.
    pub text: String,
    /// Style registry key of the owning run (`s_<sha256[:16]>`).
    pub style_id: String,
    /// Revision registry key when the span sits inside a tracked revision
    /// (`w:ins`/`w:del`/`w:moveFrom`/`w:moveTo`); None otherwise.
    pub revision: Option<String>,
    /// Comment ids whose ranges cover the span (open `w:commentRangeStart`
    /// at this position), in document order.
    pub comment_ids: Vec<String>,
    /// true when the span is on the editable surface (paragraph has no
    /// opaque node and the element is `w:t`, not `w:delText`).
    pub editable: bool,
    /// The three frozen views, resolved by Core.
    pub visibility: Visibility,
}

/// The three frozen visibility views of one segment, computed by Core so
/// callers never re-interpret OOXML revision markup.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Visibility {
    /// Visible in the pre-edit document (false inside `w:ins`/`w:moveTo`).
    pub original: bool,
    /// Visible with markup shown (always true for prose text).
    pub tracked: bool,
    /// Visible after accepting all changes (false inside
    /// `w:del`/`w:moveFrom`).
    pub final_view: bool,
}

/// One table of the registry: structure only.
#[derive(Clone, Debug, Serialize)]
pub struct ProjectionTable {
    pub part: String,
    /// Per-part canonical table ordinal (`T{i}`).
    pub table_index: u32,
    pub rows: Vec<TableRow>,
    /// Byte range of the `w:tbl` element in the part XML.
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct TableRow {
    /// Canonical row ordinal (`R{r}`).
    pub row_index: u32,
    pub cells: Vec<TableCell>,
}

#[derive(Clone, Debug, Serialize)]
pub struct TableCell {
    /// Canonical cell ordinal (`C{c}`).
    pub col_index: u32,
    /// Canonical paragraph ids of the cell (`T{i}.R{r}.C{c}.P{p}`); text
    /// lives only in the paragraph registry.
    pub paragraph_ids: Vec<String>,
    /// Nested table ordinals inside this cell, in document order.
    pub tables: Vec<u32>,
    /// Byte range of the `w:tc` element (document part) or of the cell's
    /// paragraphs (header/footer parts).
    pub start: usize,
    pub end: usize,
}

/// One semantic allowlisted style (content-addressed; raw `rPr` never
/// exposed).
#[derive(Clone, Debug, Serialize)]
pub struct ProjectionStyle {
    /// `s_` + first 16 hex of the SHA-256 over the canonical rPr bytes.
    pub style_id: String,
    /// Human-readable label ("bold", "normal", "sz=24", ...).
    pub label: String,
    /// Semantic allowlist of run properties; boolean props use
    /// "true"/"false", fonts use `font:<slot>`, others use their `w:val`.
    pub features: BTreeMap<String, String>,
}

/// One tracked revision of the registry (govern inventory + byte range).
///
/// `part` mirrors `govern::RevisionEntry.part` (the zip member path), so
/// `key` equals the canonical key `scan_revisions_bytes` produces.
#[derive(Clone, Debug, Serialize)]
pub struct ProjectionRevision {
    pub key: String,
    pub part: String,
    pub kind: String,
    pub w_id: String,
    pub author: String,
    pub date: String,
    pub text: String,
    pub paragraph_id: Option<String>,
    pub editable: bool,
    pub reason: Option<String>,
    pub scope: Option<String>,
    /// Byte range of the revision element in the part XML.
    pub start: usize,
    pub end: usize,
}

/// Canonical paragraph data — the pure input to the fingerprint entries.
/// Carries exactly the canonical identity and segment data; never DOM or
/// Store state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalParagraph {
    pub part: String,
    pub paragraph_id: String,
    pub segments: Vec<Segment>,
}

impl From<&ProjectionParagraph> for CanonicalParagraph {
    fn from(paragraph: &ProjectionParagraph) -> Self {
        CanonicalParagraph {
            part: paragraph.part.clone(),
            paragraph_id: paragraph.paragraph_id.clone(),
            segments: paragraph.segments.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Fingerprints (pure Core entries)
// ---------------------------------------------------------------------------

/// SHA-256 (hex) over the canonical paragraph data: part, id, and every
/// segment's canonical fields under the frozen position contract. Equal
/// canonical data always hashes equal; a patch backend can re-prove a
/// target by recomputing this from its own canonical paragraph/segment
/// data.
pub fn paragraph_fingerprint(paragraph: &CanonicalParagraph) -> String {
    let segments: Vec<serde_json::Value> = paragraph
        .segments
        .iter()
        .map(segment_fingerprint_value)
        .collect();
    semantic_sha256(&serde_json::json!({
        "position_contract": POSITION_CONTRACT,
        "part": paragraph.part,
        "paragraph_id": paragraph.paragraph_id,
        "text": canonical_text(paragraph),
        "segments": segments,
    }))
}

/// SHA-256 (hex) over the canonical text region `[start, end)` of one
/// paragraph plus the covering segment data: the canonical slice, the
/// paragraph fingerprint, and every segment intersecting the range. A
/// patch backend re-proves a target region with its canonical paragraph
/// data and the same range.
pub fn region_fingerprint(paragraph: &CanonicalParagraph, start: usize, end: usize) -> String {
    let text = canonical_text(paragraph);
    let scalar_count = text.chars().count();
    let clamped_start = start.min(scalar_count);
    let clamped_end = end.min(scalar_count).max(clamped_start);
    let slice: String = text
        .chars()
        .skip(clamped_start)
        .take(clamped_end - clamped_start)
        .collect();
    let covering: Vec<serde_json::Value> = paragraph
        .segments
        .iter()
        .filter(|s| s.end > clamped_start && s.start < clamped_end)
        .map(segment_fingerprint_value)
        .collect();
    semantic_sha256(&serde_json::json!({
        "position_contract": POSITION_CONTRACT,
        "part": paragraph.part,
        "paragraph_id": paragraph.paragraph_id,
        "start": clamped_start,
        "end": clamped_end,
        "text": slice,
        "paragraph_fingerprint": paragraph_fingerprint(paragraph),
        "segments": covering,
    }))
}

/// The canonical text of a paragraph (join of its segment texts).
pub fn canonical_text(paragraph: &CanonicalParagraph) -> String {
    let mut text = String::new();
    for segment in &paragraph.segments {
        text.push_str(&segment.text);
    }
    text
}

fn segment_fingerprint_value(segment: &Segment) -> serde_json::Value {
    serde_json::json!({
        "start": segment.start,
        "end": segment.end,
        "text": segment.text,
        "style_id": segment.style_id,
        "revision": segment.revision,
        "comment_ids": segment.comment_ids,
        "editable": segment.editable,
        "visibility": {
            "original": segment.visibility.original,
            "tracked": segment.visibility.tracked,
            "final": segment.visibility.final_view,
        },
    })
}

// ---------------------------------------------------------------------------
// Style identity + semantic allowlist
// ---------------------------------------------------------------------------

/// The content-addressed style id of one canonical rPr byte sequence
/// (mirror of the typed `s_<sha256[:16]>` convention).
pub fn content_style_id(canonical_rpr_bytes: &[u8]) -> String {
    format!(
        "{STYLE_ID_PREFIX}{}",
        &bytes_sha256(canonical_rpr_bytes)[..16]
    )
}

/// The style id of a run with no `rPr` (the "Normal" default).
pub fn default_style_id() -> String {
    content_style_id(b"")
}

struct StyleRegistry {
    entries: Vec<ProjectionStyle>,
    by_id: HashMap<String, usize>,
}

impl StyleRegistry {
    fn ensure(
        &mut self,
        style_id: &str,
        label: String,
        features: BTreeMap<String, String>,
    ) -> String {
        if let Some(&index) = self.by_id.get(style_id) {
            return self.entries[index].style_id.clone();
        }
        self.by_id.insert(style_id.to_string(), self.entries.len());
        self.entries.push(ProjectionStyle {
            style_id: style_id.to_string(),
            label,
            features,
        });
        style_id.to_string()
    }
}

/// Extract the semantic allowlist of one `w:rPr` element (mirror of
/// Python `rpr_features`; `dstrike` added to the boolean set).
/// Label + features for one rPr FRAGMENT (the text form the registry and the
/// format lane speak). Wraps [`rpr_semantics`] so there is one implementation
/// of the semantics rather than two that can drift.
pub(crate) fn rpr_semantics_for(
    rpr_xml: &str,
) -> Result<(String, BTreeMap<String, String>), CoreError> {
    let text = if rpr_xml.trim().is_empty() {
        format!(r#"<w:rPr xmlns:w="{}"/>"#, crate::prose::WORD_NAMESPACE)
    } else {
        rpr_xml.to_string()
    };
    let bytes = text.into_bytes();
    let nodes = crate::xml_walker::build_tree(&crate::xml_walker::scan_tags(&bytes))?;
    let root = nodes
        .iter()
        .find(|node| node.parent.is_none())
        .cloned()
        .ok_or_else(|| CoreError::Domain("invalid-rpr: no root element".to_string()))?;
    Ok(rpr_semantics(&root, &nodes, &bytes))
}

pub(crate) fn rpr_semantics(
    node: &ElementNode,
    nodes: &[ElementNode],
    xml: &[u8],
) -> (String, BTreeMap<String, String>) {
    let mut features: BTreeMap<String, String> = BTreeMap::new();
    for &child in &node.children {
        let child_node = &nodes[child];
        let name = child_node.name.as_str();
        if name == "rPrChange" {
            continue;
        }
        let attrs = open_tag_attrs(xml, child_node.open_start, child_node.open_end);
        if BOOLEAN_RPR.contains(&name) {
            // Python's `rpr_features` records a boolean only when it is ON:
            // an explicitly false property is absent from the feature map, and
            // the map goes into styles.json, so recording "false" here would
            // move the file's bytes and with them the tree digest.
            let value = attr_val(&attrs);
            if !FALSE_VALUES.contains(&value.to_lowercase().as_str()) {
                features.insert(name.to_string(), "true".to_string());
            }
        } else if name == "rFonts" {
            for (key, value) in attrs {
                features.insert(format!("font:{key}"), value);
            }
        } else if VALUE_RPR.contains(&name) {
            features.insert(name.to_string(), attr_val(&attrs));
        }
    }
    (style_label(&features), features)
}

/// Human-readable style label from the allowlist (mirror of Python
/// `style_label`).
pub(crate) fn style_label(features: &BTreeMap<String, String>) -> String {
    let mut labels: Vec<String> = Vec::new();
    for (name, word) in [
        ("b", "bold"),
        ("i", "italic"),
        ("strike", "strike"),
        ("dstrike", "double-strike"),
        ("smallCaps", "small-caps"),
        ("caps", "caps"),
        ("outline", "outline"),
        ("imprint", "imprint"),
    ] {
        if features.get(name).map(String::as_str) == Some("true") {
            labels.push(word.to_string());
        }
    }
    let mut fonts: Vec<String> = Vec::new();
    for (key, value) in features {
        if let Some(slot) = key.strip_prefix("font:") {
            if matches!(slot, "ascii" | "eastAsia" | "hAnsi" | "cs") && !fonts.contains(value) {
                fonts.push(value.clone());
            }
        }
    }
    if !fonts.is_empty() {
        labels.push(fonts.join("/"));
    }
    for key in VALUE_RPR {
        if let Some(value) = features.get(key) {
            labels.push(format!("{key}={value}"));
        }
    }
    if labels.is_empty() {
        "normal".to_string()
    } else {
        labels.join(", ")
    }
}

// ---------------------------------------------------------------------------
// Segment walker
// ---------------------------------------------------------------------------

/// Walks one paragraph's element subtree and emits canonical segments.
struct SegmentWalker<'a> {
    xml: &'a [u8],
    nodes: &'a [ElementNode],
    paragraph_editable: bool,
    default_style_id: String,
    styles: &'a mut StyleRegistry,
    /// Revision element byte range -> canonical registry key.
    rev_keys: &'a HashMap<(usize, usize), String>,
    /// Comment ranges open at the current position (part-global state,
    /// spans paragraphs).
    open_comments: &'a mut Vec<String>,
    /// (revision key, kind) stack of open revision elements.
    rev_stack: Vec<(String, String)>,
    /// Current unicode-scalar offset within the paragraph.
    offset: usize,
    segments: Vec<Segment>,
}

impl<'a> SegmentWalker<'a> {
    fn visit_container(&mut self, node_idx: usize) {
        let children = self.nodes[node_idx].children.clone();
        for child in children {
            let name = self.nodes[child].name.clone();
            match name.as_str() {
                "commentRangeStart" => {
                    let node = &self.nodes[child];
                    if let Some(id) = self.comment_id(node) {
                        self.open_comments.push(id);
                    }
                }
                "commentRangeEnd" => {
                    let node = &self.nodes[child];
                    if let Some(id) = self.comment_id(node) {
                        if let Some(pos) = self.open_comments.iter().rposition(|open| open == &id) {
                            self.open_comments.remove(pos);
                        }
                    }
                }
                "r" => self.visit_run(child),
                "ins" | "del" | "moveFrom" | "moveTo" => {
                    let node = &self.nodes[child];
                    let key = self.rev_keys.get(&(node.open_start, node.end)).cloned();
                    let kind = govern::revision_kind(&name);
                    if let (Some(kind), Some(key)) = (kind, key) {
                        self.rev_stack.push((key, kind.to_string()));
                        self.visit_container(child);
                        self.rev_stack.pop();
                    } else {
                        self.visit_container(child);
                    }
                }
                "hyperlink" => self.visit_container(child),
                _ => {
                    // pPr/proofErr/bookmarks/anchors/opaque: no canonical
                    // text; opaque blocks live in the projection registry.
                }
            }
        }
    }

    fn visit_run(&mut self, run_idx: usize) {
        let children = self.nodes[run_idx].children.clone();
        let mut style_id = self.default_style_id.clone();
        for &child in &children {
            if self.nodes[child].name == "rPr" {
                style_id = self.style_id_for_rpr(child);
                break;
            }
        }
        for &child in &children {
            let name = self.nodes[child].name.clone();
            if name == "t" || name == "delText" {
                self.emit_text(child, name == "delText", &style_id);
            }
        }
    }

    fn emit_text(&mut self, node_idx: usize, is_del_text: bool, style_id: &str) {
        let node = &self.nodes[node_idx];
        if !node.children.is_empty() {
            // A text element with element children cannot be mapped
            // canonically (mirror of the prose run-lock rule).
            return;
        }
        let (decoded, _) = prose::decode_text_segments(&self.xml[node.open_end..node.close_start])
            .unwrap_or_default();
        if decoded.is_empty() {
            return;
        }
        let start = self.offset;
        let count = decoded.chars().count();
        self.offset += count;
        let original = !self
            .rev_stack
            .iter()
            .any(|(_, kind)| kind == "insert" || kind == "move_to");
        let final_view = !self
            .rev_stack
            .iter()
            .any(|(_, kind)| kind == "delete" || kind == "move_from");
        self.segments.push(Segment {
            start,
            end: start + count,
            text: decoded,
            style_id: style_id.to_string(),
            revision: self.rev_stack.last().map(|(key, _)| key.clone()),
            comment_ids: self.open_comments.clone(),
            editable: self.paragraph_editable && !is_del_text,
            visibility: Visibility {
                original,
                tracked: true,
                final_view,
            },
        });
    }

    fn style_id_for_rpr(&mut self, rpr_idx: usize) -> String {
        let node = &self.nodes[rpr_idx];
        // Same canonical form as `prose::style_id_for_rpr`, so the read
        // surface and the format lane agree on what a style IS. Falls back to
        // the byte form only if the fragment cannot be canonicalised at all.
        let fragment = String::from_utf8_lossy(&self.xml[node.open_start..node.end]).into_owned();
        let style_id = crate::prose::style_id_for_rpr(&fragment)
            .unwrap_or_else(|_| content_style_id(&self.canonical_rpr_bytes(node)));
        let (label, features) = rpr_semantics(node, self.nodes, self.xml);
        self.styles.ensure(&style_id, label, features)
    }

    /// Canonical rPr bytes: open tag + non-`rPrChange` children + close
    /// tag (`w:rPrChange` is format history, never a format property).
    fn canonical_rpr_bytes(&self, node: &ElementNode) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.xml[node.open_start..node.open_end]);
        for &child in &node.children {
            let child_node = &self.nodes[child];
            if child_node.name != "rPrChange" {
                out.extend_from_slice(&self.xml[child_node.open_start..child_node.end]);
            }
        }
        if !node.self_closing {
            out.extend_from_slice(&self.xml[node.close_start..node.end]);
        }
        out
    }

    fn comment_id(&self, node: &ElementNode) -> Option<String> {
        open_tag_attrs(self.xml, node.open_start, node.open_end)
            .into_iter()
            .find(|(name, _)| name == "id")
            .map(|(_, value)| value)
    }
}

// ---------------------------------------------------------------------------
// Block tree builder
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum PartKind {
    /// word/document.xml: full tree (paragraphs, tables, boxes, sdts).
    Document,
    /// word/header*.xml / word/footer*.xml: paragraphs + root tables
    /// (cells grouped from the locator's canonical ids).
    HeaderFooter,
    /// word/footnotes|endnotes|comments.xml: container > paragraph only.
    Notes,
}

/// Cell content group of a header/footer table (locator authority).
struct CellGroup {
    paragraph_ids: Vec<String>,
    start: usize,
    end: usize,
}

/// Builds one part's ordered block tree and its table registry entries.
struct TreeBuilder<'a> {
    nodes: &'a [ElementNode],
    located: &'a HashMap<(usize, usize), String>,
    part: &'a str,
    tables: Vec<ProjectionTable>,
    table_ordinal: u32,
    box_ordinal: i64,
    sdt_ordinal: i64,
}

impl<'a> TreeBuilder<'a> {
    fn part_blocks(
        &mut self,
        root_idx: usize,
        kind: PartKind,
        cell_groups: &HashMap<u32, BTreeMap<(u32, u32), CellGroup>>,
    ) -> Vec<Block> {
        match kind {
            PartKind::Document => {
                let body = self.nodes[root_idx]
                    .children
                    .iter()
                    .copied()
                    .find(|&child| self.nodes[child].name == "body");
                match body {
                    Some(body) => self.body_blocks(body, true),
                    None => Vec::new(),
                }
            }
            PartKind::HeaderFooter => self.header_blocks(root_idx, cell_groups),
            PartKind::Notes => self.notes_blocks(root_idx),
        }
    }

    fn body_blocks(&mut self, container_idx: usize, document: bool) -> Vec<Block> {
        let children = self.nodes[container_idx].children.clone();
        let mut out = Vec::new();
        for child in children {
            let name = self.nodes[child].name.clone();
            match name.as_str() {
                "p" => {
                    if let Some(block) = self.paragraph_block(child, document) {
                        out.push(block);
                    }
                }
                "tbl" => {
                    let table_index = self.table_block(child);
                    out.push(Block::Table { table_index });
                }
                "sdt" if document => {
                    let content = self.nodes[child]
                        .children
                        .iter()
                        .copied()
                        .find(|&c| self.nodes[c].name == "sdtContent");
                    if let Some(content) = content {
                        self.sdt_ordinal += 1;
                        let blocks = self.sdt_content_blocks(content);
                        out.push(Block::Sdt {
                            sdt_index: self.sdt_ordinal as u32,
                            blocks,
                        });
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// Content of a body-level `w:sdtContent`: paragraphs only (tables and
    /// boxes inside sdts are not counted by the paragraph locator).
    fn sdt_content_blocks(&mut self, container_idx: usize) -> Vec<Block> {
        let children = self.nodes[container_idx].children.clone();
        let mut out = Vec::new();
        for child in children {
            if self.nodes[child].name == "p" {
                if let Some(block) = self.paragraph_block(child, false) {
                    out.push(block);
                }
            }
        }
        out
    }

    /// Header/footer part: paragraphs + root-level tables. Table cells are
    /// grouped from the located cell ids (the part locator numbers nested
    /// tables' paragraphs into the root table's row/cell space, so ids stay
    /// authoritative).
    fn header_blocks(
        &mut self,
        root_idx: usize,
        cell_groups: &HashMap<u32, BTreeMap<(u32, u32), CellGroup>>,
    ) -> Vec<Block> {
        let children = self.nodes[root_idx].children.clone();
        let mut out = Vec::new();
        for child in children {
            let name = self.nodes[child].name.clone();
            match name.as_str() {
                "p" => {
                    if let Some(block) = self.paragraph_block(child, false) {
                        out.push(block);
                    }
                }
                "tbl" => {
                    let index = self.table_ordinal;
                    self.table_ordinal += 1;
                    let mut rows: Vec<TableRow> = Vec::new();
                    if let Some(groups) = cell_groups.get(&index) {
                        let mut row_map: BTreeMap<u32, BTreeMap<u32, &CellGroup>> = BTreeMap::new();
                        for ((row, col), group) in groups {
                            row_map.entry(*row).or_default().insert(*col, group);
                        }
                        for (row, cells) in row_map {
                            let mut row_cells = Vec::new();
                            for (col, group) in cells {
                                row_cells.push(TableCell {
                                    col_index: col,
                                    paragraph_ids: group.paragraph_ids.clone(),
                                    tables: Vec::new(),
                                    start: group.start,
                                    end: group.end,
                                });
                            }
                            rows.push(TableRow {
                                row_index: row,
                                cells: row_cells,
                            });
                        }
                    }
                    self.tables.push(ProjectionTable {
                        part: self.part.to_string(),
                        table_index: index,
                        rows,
                        start: self.nodes[child].open_start,
                        end: self.nodes[child].end,
                    });
                    out.push(Block::Table { table_index: index });
                }
                _ => {}
            }
        }
        out
    }

    /// Footnotes/endnotes/comments parts: `w:footnote`/`w:comment`
    /// containers with direct paragraphs only.
    fn notes_blocks(&mut self, root_idx: usize) -> Vec<Block> {
        let children = self.nodes[root_idx].children.clone();
        let mut out = Vec::new();
        for child in children {
            let container_children = self.nodes[child].children.clone();
            for inner in container_children {
                if self.nodes[inner].name == "p" {
                    if let Some(block) = self.paragraph_block(inner, false) {
                        out.push(block);
                    }
                }
            }
        }
        out
    }

    fn paragraph_block(&mut self, node_idx: usize, with_boxes: bool) -> Option<Block> {
        let node = &self.nodes[node_idx];
        let id = self.located.get(&(node.open_start, node.end))?;
        let nested = if with_boxes {
            self.box_blocks(node_idx)
        } else {
            Vec::new()
        };
        Some(Block::Paragraph {
            paragraph_id: id.clone(),
            nested,
        })
    }

    /// `w:txbxContent` blocks inside one paragraph's subtree, in document
    /// order (ordinals mirror the document locator's box counter).
    fn box_blocks(&mut self, paragraph_idx: usize) -> Vec<Block> {
        let mut out = Vec::new();
        self.collect_boxes(paragraph_idx, &mut out);
        out
    }

    fn collect_boxes(&mut self, node_idx: usize, out: &mut Vec<Block>) {
        let children = self.nodes[node_idx].children.clone();
        for child in children {
            let name = self.nodes[child].name.clone();
            if name == "txbxContent" {
                self.box_ordinal += 1;
                let box_index = self.box_ordinal as u32;
                let blocks = self.txbx_content_blocks(child);
                out.push(Block::Box { box_index, blocks });
            }
            self.collect_boxes(child, out);
        }
    }

    fn txbx_content_blocks(&mut self, container_idx: usize) -> Vec<Block> {
        let children = self.nodes[container_idx].children.clone();
        let mut out = Vec::new();
        for child in children {
            if self.nodes[child].name == "p" {
                if let Some(block) = self.paragraph_block(child, false) {
                    out.push(block);
                }
            }
        }
        out
    }

    /// Structural walk of one `w:tbl` (document part): rows/cells from the
    /// element tree, nested tables recursed; ordinals mirror the locator's
    /// table counter (pre-order).
    fn table_block(&mut self, tbl_idx: usize) -> u32 {
        let index = self.table_ordinal;
        self.table_ordinal += 1;
        let mut rows: Vec<TableRow> = Vec::new();
        for tr in self.nodes[tbl_idx].children.clone() {
            if self.nodes[tr].name != "tr" {
                continue;
            }
            let mut cells: Vec<TableCell> = Vec::new();
            for tc in self.nodes[tr].children.clone() {
                if self.nodes[tc].name != "tc" {
                    continue;
                }
                let mut paragraph_ids: Vec<String> = Vec::new();
                let mut tables: Vec<u32> = Vec::new();
                for child in self.nodes[tc].children.clone() {
                    let name = self.nodes[child].name.clone();
                    if name == "p" {
                        let node = &self.nodes[child];
                        if let Some(id) = self.located.get(&(node.open_start, node.end)) {
                            paragraph_ids.push(id.clone());
                        }
                    } else if name == "tbl" {
                        tables.push(self.table_block(child));
                    }
                }
                cells.push(TableCell {
                    col_index: cells.len() as u32,
                    paragraph_ids,
                    tables,
                    start: self.nodes[tc].open_start,
                    end: self.nodes[tc].end,
                });
            }
            rows.push(TableRow {
                row_index: rows.len() as u32,
                cells,
            });
        }
        self.tables.push(ProjectionTable {
            part: self.part.to_string(),
            table_index: index,
            rows,
            start: self.nodes[tbl_idx].open_start,
            end: self.nodes[tbl_idx].end,
        });
        index
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Read the editable prose parts of a package (part key -> bytes), mirror
/// of `prose::enumerate_package`'s read loop.
pub(crate) fn part_xml_members(package: &[u8]) -> Result<BTreeMap<String, Vec<u8>>, CoreError> {
    let file = std::io::Cursor::new(package.to_vec());
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| CoreError::Message(format!("not a valid DOCX: {error}")))?;
    let mut members = BTreeMap::new();
    for index in 0..archive.len() {
        let mut member = archive
            .by_index(index)
            .map_err(|error| CoreError::Message(format!("not a valid DOCX: {error}")))?;
        let name = member.name().to_string();
        if let Some(part_key) = prose::part_key_from_path(&name) {
            let mut bytes = Vec::new();
            member.read_to_end(&mut bytes).map_err(CoreError::io)?;
            members.insert(part_key, bytes);
        }
    }
    Ok(members)
}

/// Extract every raster image embedded in drawing runs of the projection.
/// A drawing run is opaque (locked) in the prose surface; the image bytes
/// are surfaced as a data URI so the review console can render the
/// document approximately. Only `r:embed` blips are supported (`r:link`
/// external references are skipped); the first `wp:extent` of the drawing
/// gives the rendered size in EMU (1 CSS px = 9525 EMU).
fn extract_images(
    package: &[u8],
    opaques: &[OpaqueBlock],
    part_xml: &BTreeMap<String, Vec<u8>>,
) -> Result<Vec<ProjectionImage>, CoreError> {
    let file = std::io::Cursor::new(package.to_vec());
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| CoreError::Message(format!("not a valid DOCX: {error}")))?;
    let mut images = Vec::new();
    for opaque in opaques {
        if opaque.tag != "w:r" {
            continue;
        }
        let xml = match part_xml.get(&opaque.part_key) {
            Some(xml) => xml,
            None => continue,
        };
        if opaque.end > xml.len() || opaque.start >= opaque.end {
            continue;
        }
        let slice = &xml[opaque.start..opaque.end];
        if !slice.windows(8).any(|w| w == b"<w:drawi") {
            // Not a drawing run (fldChar/object/math etc.): stay opaque.
            continue;
        }
        let Some(r_id) = embedded_relationship_id(slice) else {
            continue;
        };
        let (cx, cy) = extent_emu(slice).unwrap_or((0, 0));
        let Some(target) = relationship_target(&mut archive, &opaque.part_key, &r_id)? else {
            continue;
        };
        let Some((mime, bytes)) = read_media(&mut archive, &opaque.part_key, &target)? else {
            continue;
        };
        images.push(ProjectionImage {
            part_key: opaque.part_key.clone(),
            paragraph_id: opaque.paragraph_id.clone(),
            data_uri: format!("data:{mime};base64,{}", base64_encode(&bytes)),
            width_px: (cx / 9525).max(1),
            height_px: (cy / 9525).max(1),
        });
    }
    Ok(images)
}

/// The relationship id of the first embedded blip in a drawing run:
/// `r:embed="rIdN"` (external `r:link` references are ignored).
fn embedded_relationship_id(slice: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(slice).ok()?;
    let mut search = text;
    while let Some(embed_at) = search.find("embed=\"") {
        let start = embed_at + "embed=\"".len();
        let rest = &search[start..];
        let end = rest.find('"')?;
        let value = &rest[..end];
        if value.starts_with("rId") && !value.is_empty() {
            return Some(value.to_string());
        }
        search = rest;
    }
    None
}

/// The first `wp:extent` of a drawing run as (cx, cy) in EMU.
fn extent_emu(slice: &[u8]) -> Option<(u32, u32)> {
    let text = std::str::from_utf8(slice).ok()?;
    let extent_at = text.find("<wp:extent")?;
    let tail = &text[extent_at..];
    let cx = attribute_u32(tail, "cx")?;
    let cy = attribute_u32(tail, "cy")?;
    Some((cx, cy))
}

/// Parse a quoted numeric attribute (`cx="6096000"`) from the head of a
/// slice; the attribute may use single or double quotes.
fn attribute_u32(text: &str, name: &str) -> Option<u32> {
    let mut search = text;
    while let Some(at) = search.find(name) {
        let rest = &search[at + name.len()..];
        let rest = rest.strip_prefix('=')?;
        let (open, close) = if let Some(v) = rest.strip_prefix('"') {
            (v, '"')
        } else if let Some(v) = rest.strip_prefix('\'') {
            (v, '\'')
        } else {
            search = rest;
            continue;
        };
        let end = open.find(close)?;
        return open[..end].parse().ok();
    }
    None
}

/// Resolve a relationship id to its package-relative target for one part
/// (e.g. `word/document.xml` -> `word/_rels/document.xml.rels`).
fn relationship_target(
    archive: &mut zip::ZipArchive<std::io::Cursor<Vec<u8>>>,
    part_key: &str,
    r_id: &str,
) -> Result<Option<String>, CoreError> {
    let part_path = prose::part_path(part_key);
    let (dir, name) = match part_path.rfind('/') {
        Some(index) => (&part_path[..index], &part_path[index + 1..]),
        None => ("", part_path.as_str()),
    };
    let rels_path = if dir.is_empty() {
        format!("_rels/{name}.rels")
    } else {
        format!("{dir}/_rels/{name}.rels")
    };
    let rels_bytes = match archive.by_name(&rels_path) {
        Ok(mut member) => {
            let mut bytes = Vec::new();
            member.read_to_end(&mut bytes).map_err(CoreError::io)?;
            bytes
        }
        Err(_error) => {
            return Ok(None);
        }
    };
    let rels = std::str::from_utf8(&rels_bytes).unwrap_or("");
    let mut search = rels;
    while let Some(at) = search.find("Relationship ") {
        let head = &search[at..];
        let close = head.find("/>").unwrap_or(0);
        let tag = &head[..close];
        let id_matches =
            tag.contains(&format!("Id=\"{r_id}\"")) || tag.contains(&format!("Id='{r_id}'"));
        if id_matches {
            let target = match quoted_value(tag, "Target") {
                Some(target) => target,
                None => {
                    search = &head[1..];
                    continue;
                }
            };
            // Targets are relative to the part's directory.
            if target.starts_with('/') {
                return Ok(Some(target.trim_start_matches('/').to_string()));
            }
            if dir.is_empty() {
                return Ok(Some(target.to_string()));
            }
            return Ok(Some(format!("{dir}/{target}")));
        }
        search = &head[1..];
    }
    Ok(None)
}

/// Extract the quoted value of one attribute (`Target="media/image1.png"`).
fn quoted_value(text: &str, name: &str) -> Option<String> {
    let mut search = text;
    while let Some(at) = search.find(name) {
        let rest = &search[at + name.len()..];
        let rest = rest.strip_prefix('=')?;
        let (open, close) = if let Some(v) = rest.strip_prefix('"') {
            (v, '"')
        } else if let Some(v) = rest.strip_prefix('\'') {
            (v, '\'')
        } else {
            search = rest;
            continue;
        };
        let end = open.find(close)?;
        return Some(open[..end].to_string());
    }
    None
}

/// Read one media part from the package; the MIME type is derived from the
/// file extension. Returns None when the part is missing or unsupported.
fn read_media(
    archive: &mut zip::ZipArchive<std::io::Cursor<Vec<u8>>>,
    part_key: &str,
    target: &str,
) -> Result<Option<(String, Vec<u8>)>, CoreError> {
    let mime = match target
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "webp" => "image/webp",
        "tif" | "tiff" => "image/tiff",
        "svg" => "image/svg+xml",
        _ => return Ok(None),
    };
    let path = if target.starts_with("word/") {
        target.to_string()
    } else if part_key == "document" {
        format!("word/{target}")
    } else {
        target.to_string()
    };
    let mut member = match archive.by_name(&path) {
        Ok(member) => member,
        Err(_) => return Ok(None),
    };
    let mut bytes = Vec::new();
    member.read_to_end(&mut bytes).map_err(CoreError::io)?;
    Ok(Some((mime.to_string(), bytes)))
}

/// Standard base64 (RFC 4648 with padding) — no external crate needed.
fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(triple >> 18) as usize & 63] as char);
        out.push(TABLE[(triple >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(triple >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(TABLE[triple as usize & 63] as char);
        }
    }
    out
}

/// One projection revision record from a located revision (mirror of the
/// `scan_revisions_bytes` reason/scope classification; `key` equals the
/// govern canonical key).
fn projection_revision(part_path: &str, r: &LocatedRevision, document: bool) -> ProjectionRevision {
    let key = govern::revision_key(part_path, &r.kind, &r.w_id, &r.text);
    let (editable, reason, scope) = if !document {
        (
            false,
            Some("nested-container-or-non-editable-part".to_string()),
            None,
        )
    } else if r.mark {
        (
            false,
            Some("paragraph-mark-revision".to_string()),
            Some("paragraph-mark".to_string()),
        )
    } else if r.inside_opaque {
        (
            false,
            Some("nested-container-or-non-editable-part".to_string()),
            None,
        )
    } else if r.paragraph_opaque {
        (
            false,
            Some("paragraph-contains-unsupported-node".to_string()),
            None,
        )
    } else {
        (true, None, None)
    };
    ProjectionRevision {
        key,
        part: part_path.to_string(),
        kind: r.kind.clone(),
        w_id: r.w_id.clone(),
        author: r.author.clone(),
        date: r.date.clone(),
        text: r.text.clone(),
        paragraph_id: r.paragraph_id.clone(),
        editable,
        reason,
        scope,
        start: r.start,
        end: r.end,
    }
}

/// Parse the table/row/cell ordinals of a cell paragraph id
/// (`T{i}.R{r}.C{c}.P{p}`, with optional part-key prefix).
fn parse_cell_id(id: &str) -> Option<(u32, u32, u32)> {
    let mut table = None;
    let mut row = None;
    let mut col = None;
    for segment in id.split('.') {
        if let Some(rest) = segment.strip_prefix('T') {
            table = rest.parse().ok();
        } else if let Some(rest) = segment.strip_prefix('R') {
            row = rest.parse().ok();
        } else if let Some(rest) = segment.strip_prefix('C') {
            col = rest.parse().ok();
        }
    }
    Some((table?, row?, col?))
}

/// The `w:val` (or bare `val`) attribute of an element's attributes,
/// defaulting to "true" (mirror of Python's `rpr_features` lookup).
fn attr_val(attrs: &[(String, String)]) -> String {
    attrs
        .iter()
        .find(|(key, _)| key == "val")
        .map(|(_, value)| value.clone())
        .unwrap_or_else(|| "true".to_string())
}

/// Attribute pairs of one element open tag (local names, prefix-agnostic).
fn open_tag_attrs(xml: &[u8], start: usize, open_end: usize) -> Vec<(String, String)> {
    let bytes = &xml[start..open_end];
    let mut attrs = Vec::new();
    // Skip '<' and the tag name.
    let mut index = 1usize;
    while index < bytes.len()
        && (bytes[index].is_ascii_alphanumeric()
            || matches!(bytes[index], b'_' | b'.' | b':' | b'-'))
    {
        index += 1;
    }
    loop {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= bytes.len() || bytes[index] == b'/' || bytes[index] == b'>' {
            break;
        }
        let name_start = index;
        while index < bytes.len()
            && (bytes[index].is_ascii_alphanumeric()
                || matches!(bytes[index], b'_' | b'.' | b':' | b'-'))
        {
            index += 1;
        }
        let name = String::from_utf8_lossy(&bytes[name_start..index]).into_owned();
        while index < bytes.len() && (bytes[index].is_ascii_whitespace() || bytes[index] == b'=') {
            index += 1;
        }
        let value = if index < bytes.len() && (bytes[index] == b'"' || bytes[index] == b'\'') {
            let quote = bytes[index];
            index += 1;
            let value_start = index;
            while index < bytes.len() && bytes[index] != quote {
                index += 1;
            }
            let value = String::from_utf8_lossy(&bytes[value_start..index]).into_owned();
            if index < bytes.len() {
                index += 1;
            }
            value
        } else {
            String::new()
        };
        attrs.push((crate::xml_walker::local_name(&name), value));
    }
    attrs
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../corpus/release")
            .join(name)
    }

    /// Minimal package with two styled runs, an insertion + deletion
    /// revision, a commented paragraph, and one table.
    fn synthetic_package() -> Vec<u8> {
        let document_xml = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>
<w:p><w:r><w:t>Hello </w:t></w:r><w:r><w:rPr><w:i/></w:rPr><w:t>world</w:t></w:r></w:p>
<w:p><w:ins w:id="1" w:author="alice" w:date="2026-01-01T00:00:00Z"><w:r><w:t>added</w:t></w:r></w:ins><w:del w:id="2" w:author="bob" w:date="2026-01-02T00:00:00Z"><w:r><w:delText>gone</w:delText></w:r></w:del><w:r><w:t>keep</w:t></w:r></w:p>
<w:p><w:commentRangeStart w:id="0"/><w:r><w:t>commented</w:t></w:r><w:commentRangeEnd w:id="0"/></w:p>
<w:tbl><w:tr><w:tc><w:p><w:r><w:t>cell-a</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>cell-b</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
</w:body>
</w:document>"#;
        let comments_xml = br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:comments xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:comment w:id="0" w:author="carol" w:date="2026-01-03T00:00:00Z" w:initials="C"><w:p><w:r><w:t>note</w:t></w:r></w:p></w:comment>
</w:comments>"#;
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        writer
            .start_file(
                "word/document.xml",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        writer.write_all(document_xml).unwrap();
        writer
            .start_file(
                "word/comments.xml",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        writer.write_all(comments_xml).unwrap();
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn synthetic_projection_covers_segments_visibility_comments_and_tables() {
        let package = synthetic_package();
        let projection = project_document_bytes(&package).expect("project synthetic package");
        assert_eq!(projection.schema, PROJECTION_SCHEMA);
        assert_eq!(projection.position_contract, POSITION_CONTRACT);
        assert_eq!(projection.source.document_xml_sha256.len(), 64);
        assert_eq!(projection.source.package_manifest.len(), 2);
        assert_eq!(projection.parts.len(), 1);
        assert_eq!(projection.parts[0].part, "document");
        assert_eq!(projection.paragraphs.len(), 5);

        // P0: two runs -> two segments; positions are unicode-scalar.
        let p0 = projection.paragraph("document", "P0").expect("P0");
        assert_eq!(p0.segments.len(), 2);
        assert_eq!(p0.segments[0].text, "Hello ");
        assert_eq!((p0.segments[0].start, p0.segments[0].end), (0, 6));
        assert_eq!(p0.segments[1].text, "world");
        assert_eq!((p0.segments[1].start, p0.segments[1].end), (6, 11));
        assert!(p0.segments.iter().all(|s| s.editable));
        assert!(p0.segments.iter().all(|s| {
            s.visibility
                == Visibility {
                    original: true,
                    tracked: true,
                    final_view: true,
                }
        }));
        assert_eq!(p0.segments[0].style_id, default_style_id());
        let italic_id = p0.segments[1].style_id.clone();
        let italic = projection
            .styles
            .iter()
            .find(|s| s.style_id == italic_id)
            .expect("italic style in registry");
        assert_eq!(italic.label, "italic");
        assert_eq!(italic.features.get("i").map(String::as_str), Some("true"));

        // P1: insertion + deletion + plain; visibility resolved by Core.
        let p1 = projection.paragraph("document", "P1").expect("P1");
        let texts: Vec<&str> = p1.segments.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, ["added", "gone", "keep"]);
        assert_eq!(
            p1.segments[0].visibility,
            Visibility {
                original: false,
                tracked: true,
                final_view: true,
            }
        );
        assert_eq!(
            p1.segments[1].visibility,
            Visibility {
                original: true,
                tracked: true,
                final_view: false,
            }
        );
        assert!(!p1.segments[1].editable, "delText is locked");
        assert_eq!(
            p1.segments[2].visibility,
            Visibility {
                original: true,
                tracked: true,
                final_view: true,
            }
        );
        let ins_key = p1.segments[0]
            .revision
            .as_ref()
            .expect("insert revision key");
        let del_key = p1.segments[1]
            .revision
            .as_ref()
            .expect("delete revision key");
        let ins_entry = projection
            .revisions
            .iter()
            .find(|r| &r.key == ins_key)
            .expect("insert in registry");
        assert_eq!(
            (ins_entry.kind.as_str(), ins_entry.w_id.as_str()),
            ("insert", "1")
        );
        let del_entry = projection
            .revisions
            .iter()
            .find(|r| &r.key == del_key)
            .expect("delete in registry");
        assert_eq!(
            (del_entry.kind.as_str(), del_entry.w_id.as_str()),
            ("delete", "2")
        );
        assert_eq!(projection.revisions.len(), 2);

        // P2: comment coverage.
        let p2 = projection.paragraph("document", "P2").expect("P2");
        assert_eq!(p2.segments.len(), 1);
        assert_eq!(p2.segments[0].text, "commented");
        assert_eq!(p2.segments[0].comment_ids, ["0"]);
        let comment = projection
            .comments
            .iter()
            .find(|c| c.id == "0")
            .expect("comment registry");
        assert_eq!(comment.text, "note");
        assert!(comment.anchors.iter().any(|a| a.kind == "comment-start"));

        // Block tree: P0, P1, P2, Table 0; tables carry ids only.
        let blocks = &projection.parts[0].blocks;
        assert_eq!(blocks.len(), 4);
        match &blocks[3] {
            Block::Table { table_index } => assert_eq!(*table_index, 0),
            other => panic!("expected table block, got {other:?}"),
        }
        let table = &projection.tables[0];
        assert_eq!(table.table_index, 0);
        assert_eq!(table.rows.len(), 1);
        assert_eq!(table.rows[0].cells.len(), 2);
        assert_eq!(table.rows[0].cells[0].paragraph_ids, ["T0.R0.C0.P0"]);
        assert_eq!(table.rows[0].cells[1].paragraph_ids, ["T0.R0.C1.P0"]);
        assert!(table
            .rows
            .iter()
            .flat_map(|r| &r.cells)
            .all(|c| c.paragraph_ids.iter().all(|id| id.starts_with('T'))));
    }

    #[test]
    fn fingerprints_are_deterministic_and_content_sensitive() {
        let package = synthetic_package();
        let projection = project_document_bytes(&package).unwrap();
        let canonical: Vec<CanonicalParagraph> = projection
            .paragraphs
            .iter()
            .map(CanonicalParagraph::from)
            .collect();

        let first = paragraph_fingerprint(&canonical[0]);
        assert_eq!(first, paragraph_fingerprint(&canonical[0]));

        let mut changed = canonical[0].clone();
        changed.segments[0].text.push('!');
        assert_ne!(first, paragraph_fingerprint(&changed));

        let region = region_fingerprint(&canonical[0], 0, 6);
        assert_eq!(region, region_fingerprint(&canonical[0], 0, 6));
        assert_ne!(region, region_fingerprint(&canonical[0], 0, 5));
        assert_eq!(
            region_fingerprint(&canonical[0], 0, 100),
            region_fingerprint(&canonical[0], 0, 11),
            "range is clamped to the paragraph scalar count"
        );
    }

    #[test]
    fn boxes_nest_under_their_paragraph() {
        let projection = project_document_path(&fixture("boxes.docx")).expect("project boxes.docx");
        let document = projection
            .parts
            .iter()
            .find(|p| p.part == "document")
            .expect("document part");
        let (parent_id, nested) = document
            .blocks
            .iter()
            .find_map(|block| match block {
                Block::Paragraph {
                    paragraph_id,
                    nested,
                } if !nested.is_empty() => Some((paragraph_id, nested)),
                _ => None,
            })
            .expect("a paragraph with a nested box");
        // The box's located id closes after P0 and before P1, so the
        // drawing paragraph P1 carries the box content.
        assert_eq!(parent_id, "P1");
        assert_eq!(nested.len(), 1);
        match &nested[0] {
            Block::Box { box_index, blocks } => {
                assert_eq!(*box_index, 0);
                assert_eq!(blocks.len(), 1);
                match &blocks[0] {
                    Block::Paragraph { paragraph_id, .. } => {
                        assert_eq!(paragraph_id, "B0.P0")
                    }
                    other => panic!("expected paragraph, got {other:?}"),
                }
            }
            other => panic!("expected box, got {other:?}"),
        }
    }

    fn collect_block_ids(blocks: &[Block], out: &mut Vec<String>) {
        for block in blocks {
            match block {
                Block::Paragraph {
                    paragraph_id,
                    nested,
                } => {
                    out.push(paragraph_id.clone());
                    collect_block_ids(nested, out);
                }
                Block::Box { blocks, .. } | Block::Sdt { blocks, .. } => {
                    collect_block_ids(blocks, out)
                }
                Block::Table { .. } => {}
            }
        }
    }

    #[test]
    fn fixture_invariants_hold_across_corpus() {
        for name in [
            "plain.docx",
            "table.docx",
            "boxes.docx",
            "parts.docx",
            "complex.docx",
            "revisions.docx",
            "comments.docx",
            "styled.docx",
            "anchors.docx",
            "move-conflict.docx",
        ] {
            let package = std::fs::read(fixture(name)).expect("fixture readable");
            let projection =
                project_document_bytes(&package).unwrap_or_else(|e| panic!("{name}: {e}"));
            let inventory = prose::enumerate_package_bytes(&package).expect("inventory");

            // 1. Segments concatenate to the inventory visible text. Comments
            // are a separate registry, not document projection paragraphs.
            let expected = inventory
                .paragraphs
                .iter()
                .filter(|entry| entry.part_key != "comments")
                .collect::<Vec<_>>();
            assert_eq!(projection.paragraphs.len(), expected.len(), "{name}");
            for (proj, inv) in projection.paragraphs.iter().zip(expected) {
                assert_eq!(proj.part, inv.part_key, "{name}");
                assert_eq!(proj.paragraph_id, inv.paragraph_id, "{name}");
                let joined: String = proj.segments.iter().map(|s| s.text.clone()).collect();
                assert_eq!(joined, inv.visible_text, "{name} {}", inv.paragraph_id);
            }

            // 2. Positions are contiguous unicode-scalar ranges.
            for p in &projection.paragraphs {
                let mut offset = 0usize;
                for s in &p.segments {
                    assert_eq!(s.start, offset, "{name} {}", p.paragraph_id);
                    assert_eq!(
                        s.end - s.start,
                        s.text.chars().count(),
                        "{name} {}",
                        p.paragraph_id
                    );
                    offset = s.end;
                }
            }

            // 3. Registry references resolve.
            for p in &projection.paragraphs {
                for s in &p.segments {
                    assert!(
                        projection.styles.iter().any(|st| st.style_id == s.style_id),
                        "{name} {} style",
                        p.paragraph_id
                    );
                    if let Some(key) = &s.revision {
                        assert!(
                            projection.revisions.iter().any(|r| &r.key == key),
                            "{name} {} revision",
                            p.paragraph_id
                        );
                    }
                    for cid in &s.comment_ids {
                        assert!(
                            projection.comments.iter().any(|c| &c.id == cid),
                            "{name} {} comment",
                            p.paragraph_id
                        );
                    }
                }
            }

            // 4. Visibility agrees with the revision kinds.
            for p in &projection.paragraphs {
                for s in &p.segments {
                    assert!(s.visibility.tracked, "{name} {}", p.paragraph_id);
                    if let Some(key) = &s.revision {
                        let entry = projection
                            .revisions
                            .iter()
                            .find(|r| &r.key == key)
                            .expect("revision registry");
                        if entry.kind == "insert" || entry.kind == "move_to" {
                            assert!(!s.visibility.original, "{name} {}", p.paragraph_id);
                        }
                        if entry.kind == "delete" || entry.kind == "move_from" {
                            assert!(!s.visibility.final_view, "{name} {}", p.paragraph_id);
                        }
                    }
                }
            }

            // 5. The block tree references every registry paragraph exactly
            // once (blocks + table cells).
            let mut tree_ids: Vec<String> = Vec::new();
            for part in &projection.parts {
                collect_block_ids(&part.blocks, &mut tree_ids);
            }
            for table in &projection.tables {
                for row in &table.rows {
                    for cell in &row.cells {
                        tree_ids.extend(cell.paragraph_ids.iter().cloned());
                    }
                }
            }
            tree_ids.sort();
            let mut registry_ids: Vec<String> = projection
                .paragraphs
                .iter()
                .map(|p| p.paragraph_id.clone())
                .collect();
            registry_ids.sort();
            assert_eq!(tree_ids, registry_ids, "{name}");

            // 6. Table blocks resolve into the registry.
            for part in &projection.parts {
                for block in &part.blocks {
                    if let Block::Table { table_index } = block {
                        assert!(
                            projection
                                .tables
                                .iter()
                                .any(|t| t.part == part.part && t.table_index == *table_index),
                            "{name}"
                        );
                    }
                }
            }

            // 7. Opaque blocks are preserved from the inventory.
            assert_eq!(projection.opaques.len(), inventory.opaques.len(), "{name}");
        }
    }

    #[test]
    fn drawing_images_extract_from_complex_fixture() {
        let bytes = std::fs::read(fixture("complex.docx")).expect("fixture exists");
        let projection = project_document_bytes(&bytes).expect("project");
        assert!(
            !projection.images.is_empty(),
            "expected images in complex fixture"
        );
        for image in &projection.images {
            assert!(image.data_uri.starts_with("data:image/"), "data uri");
            assert!(image.width_px >= 1 && image.height_px >= 1, "sized");
        }
    }
}
