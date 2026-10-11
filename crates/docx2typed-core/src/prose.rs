//! Recursive prose enumeration and island-local text editing (issue #58).
//!
//! Deep-module contract (issue #36): core owns the byte walker, recursive
//! prose discovery, typed path assignment, and the island-edit byte
//! surgery. Enumeration mirrors the Python Reference (`scripts/typed_docx.py`
//! `locate_document_xml` / `locate_part_xml`) so typed paths match
//! (`P{n}`, `T{r}.R{r}.C{c}.P{p}`, `B{n}.P{p}`, `S{n}.P{p}`,
//! `header{n}.P{p}`, `footnotes.P{p}`, ...).
//!
//! An **island edit** replaces text inside ONE prose leaf (one `w:t`
//! element): the leaf's text bytes change; every other byte of the package
//! replays unchanged (zip-level byte surgery — untouched members are copied
//! verbatim, including their deflate streams). Edits into locked (opaque)
//! interiors, non-editable leaves, or leaves that cannot prove the old text
//! fail closed with frozen diagnostics; a global invariant gate
//! (package prevalidation + per-part XML well-formedness + opaque
//! containment) runs before any edited build and rejects the whole build
//! with no output on failure.
//!
//! A **structural edit** is the other edited-build path (PR 3): a new
//! paragraph has no baseline byte range at all (`base=""`), so it is
//! *synthesised* — the inherit paragraph's opening tag (ephemeral
//! attributes stripped) plus its `pPr` and run properties, spliced before
//! the next surviving baseline paragraph — and a deleted paragraph is
//! simply not rendered. Both live in `structure.json` and are validated by
//! the same fail-closed gate.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;

use serde::{Deserialize, Serialize};

use docx2typed_protocol::{bytes_sha256, canonical_json};

use crate::document_projection::Segment;
use crate::xml_walker::{build_tree, scan_tags, ElementNode};
use crate::CoreError;

pub const ISLANDS_SCHEMA: &str = "docx2typed-islands-1";
pub const ISLANDS_FILE: &str = "islands.json";

/// One recorded island edit (the authoritative workdir sidecar the build
/// applies and the independent verifier re-checks).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct IslandEdit {
    /// Part key: "document", "header1", "footer1", "footnotes", "endnotes",
    /// "comments".
    pub part: String,
    /// Typed paragraph id (P0, T0.R1.C1.P0, B0.P0, S0.P0, header1.P0, ...).
    pub paragraph_id: String,
    /// Ordinal of the leaf (w:t) within the paragraph, 0-based.
    pub leaf_index: usize,
    /// Decoded text to find (must be a substring of the leaf's text).
    pub old: String,
    /// Replacement text (XML-escaped on write).
    pub new: String,
    /// Generate a tracked OOXML delete+insert pair instead of a direct swap.
    #[serde(default)]
    pub tracked: bool,
    /// Revision author for tracked edits.
    #[serde(default)]
    pub author: String,
    /// Revision date in ISO-8601 form for tracked edits.
    #[serde(default)]
    pub date: String,
}

/// A resolved prose leaf with its byte ranges inside one part.
#[derive(Clone, Debug)]
pub struct ProseLeaf {
    pub part_key: String,
    pub paragraph_id: String,
    pub leaf_index: usize,
    /// Decoded visible text of the leaf.
    pub text: String,
    /// true when the leaf sits on the editable surface (no opaque node in
    /// its paragraph, run not opaque, not a deletion revision).
    pub editable: bool,
    /// Byte range of the leaf's text content within the part XML.
    pub text_start: usize,
    pub text_end: usize,
    /// Byte offset just past the `w:t` open tag (start of the text).
    pub open_end: usize,
    /// Content hash of the run's raw rPr bytes (style-span identity).
    pub style_sha256: String,
    /// Raw rPr XML of the owning run ("" when the run has no rPr).
    pub rpr: String,
    /// Byte range of the owning run's `w:rPr` within the part XML. Empty
    /// (`start == end`) when the run has none — a restyle then inserts one at
    /// `run_open_end`, which is what the reference does: it always writes the
    /// derived rPr, even onto a run that carried none.
    pub rpr_start: usize,
    pub rpr_end: usize,
    /// Byte offset just past the owning run's open tag: where a `w:rPr`
    /// belongs when the run has none (rPr precedes every other run child).
    pub run_open_end: usize,
}

/// One opaque block (locked interior) with its byte range.
#[derive(Clone, Debug, Serialize)]
pub struct OpaqueBlock {
    pub part_key: String,
    pub paragraph_id: String,
    /// Raw qname of the offending element (e.g. "w:drawing").
    pub tag: String,
    pub start: usize,
    pub end: usize,
}

/// One raster image embedded in a drawing run of the projection. The
/// drawing run stays opaque (locked); the image is surfaced as a data URI
/// so the review console can render the document approximately without
/// ever exposing or editing raw package bytes.
#[derive(Clone, Debug, Serialize)]
pub struct ProjectionImage {
    pub part_key: String,
    pub paragraph_id: String,
    /// Standard base64 data URI (`data:<mime>;base64,...`).
    pub data_uri: String,
    /// Rendered width in CSS pixels (EMU / 9525).
    pub width_px: u32,
    /// Rendered height in CSS pixels (EMU / 9525).
    pub height_px: u32,
}

/// One enumerated paragraph (extract order).
#[derive(Clone, Debug)]
pub struct ParagraphEntry {
    pub part_key: String,
    pub paragraph_id: String,
    /// Final-view visible text (w:t leaves only, deletion revisions hidden).
    pub visible_text: String,
    /// true when the paragraph contains no opaque nodes.
    pub editable: bool,
    pub leaf_count: usize,
    pub opaque_count: usize,
}

/// The full recursive-prose inventory of one package.
#[derive(Clone, Debug)]
pub struct ProseInventory {
    /// Part keys in extract order (headers, document, footers, endnotes,
    /// footnotes, comments).
    pub part_keys: Vec<String>,
    /// Paragraphs in extract order.
    pub paragraphs: Vec<ParagraphEntry>,
    /// All text leaves, in document order.
    pub leaves: Vec<ProseLeaf>,
    /// All opaque blocks, in document order.
    pub opaques: Vec<OpaqueBlock>,
}

/// Package facts produced by the global invariant gate.
#[derive(Clone, Debug)]
pub struct PackageFacts {
    pub parts: usize,
    pub xml_parts: usize,
    pub has_document_xml: bool,
}

// ---------------------------------------------------------------------------
// Part naming
// ---------------------------------------------------------------------------

/// Full zip path of a part key ("document" -> "word/document.xml").
pub fn part_path(part_key: &str) -> String {
    if part_key == "document" {
        "word/document.xml".to_string()
    } else {
        format!("word/{part_key}.xml")
    }
}

/// The part key of a zip member path, when it is an editable prose part.
pub fn part_key_from_path(path: &str) -> Option<String> {
    let name = path.strip_prefix("word/")?.strip_suffix(".xml")?;
    let editable = name == "document"
        || name.starts_with("header")
        || name.starts_with("footer")
        || matches!(name, "footnotes" | "endnotes" | "comments");
    editable.then(|| name.to_string())
}

/// Infer the part key from a typed paragraph id.
pub fn part_for_paragraph_id(id: &str) -> Option<String> {
    if id.starts_with("header") || id.starts_with("footer") {
        let dot = id.find('.')?;
        return Some(id[..dot].to_string());
    }
    if id.starts_with("footnotes") || id.starts_with("endnotes") || id.starts_with("comments") {
        return Some(id.split('.').next()?.to_string());
    }
    if id.starts_with('P') || id.starts_with('T') || id.starts_with('B') || id.starts_with('S') {
        return Some("document".to_string());
    }
    None
}

/// Split a leaf path `<paragraph-id>.<leaf-index>` (rsplit at the last dot).
pub fn parse_leaf_path(path: &str) -> Option<(String, usize)> {
    let (paragraph_id, leaf) = path.rsplit_once('.')?;
    let leaf_index: usize = leaf.parse().ok()?;
    if paragraph_id.is_empty() {
        return None;
    }
    Some((paragraph_id.to_string(), leaf_index))
}

// ---------------------------------------------------------------------------
// Text decode / escape (byte-preserving leaf surgery)
// ---------------------------------------------------------------------------

/// XML-escape text for a `w:t` body (mirror of Python `xml_escape` for the
/// character data surface: `&` `<` `>` only; quotes stay literal inside
/// element text).
pub fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Decode a `w:t` text body into its characters, mapping each decoded char
/// to the byte range that produced it. Handles the five predefined
/// entities plus decimal/hex character references; CDATA is treated as
/// literal text. Unknown entity syntax fails closed (an unprovable byte
/// mapping must never be edited through).
pub fn decode_text_segments(raw: &[u8]) -> Result<(String, Vec<(usize, usize)>), CoreError> {
    let mut decoded = String::new();
    let mut char_bytes: Vec<(usize, usize)> = Vec::new();
    let mut index = 0usize;
    while index < raw.len() {
        let byte = raw[index];
        if byte == b'&' {
            let Some(semi) = raw[index..].iter().position(|&b| b == b';') else {
                return Err(CoreError::Domain(
                    "prose-xml-invalid: unterminated entity in text".to_string(),
                ));
            };
            let entity = &raw[index..index + semi + 1];
            let entity_text = std::str::from_utf8(entity).map_err(|_| {
                CoreError::Domain("prose-xml-invalid: entity is not ASCII".to_string())
            })?;
            let ch = decode_entity(entity_text).ok_or_else(|| {
                CoreError::Domain(format!(
                    "prose-xml-invalid: unrecognized entity {entity_text}"
                ))
            })?;
            let before = decoded.len();
            decoded.push(ch);
            debug_assert_eq!(decoded.len() - before, ch.len_utf8());
            char_bytes.push((index, index + entity.len()));
            index += entity.len();
            continue;
        }
        if byte == b'<' && raw[index..].starts_with(b"<![CDATA[") {
            let Some(end) = raw[index + 9..]
                .windows(3)
                .position(|window| window == b"]]>")
            else {
                return Err(CoreError::Domain(
                    "prose-xml-invalid: unterminated CDATA in text".to_string(),
                ));
            };
            let cdata = &raw[index + 9..index + 9 + end];
            let text = std::str::from_utf8(cdata).map_err(|_| {
                CoreError::Domain("prose-xml-invalid: CDATA is not UTF-8".to_string())
            })?;
            let mut char_start = index + 9;
            for ch in text.chars() {
                let char_end = char_start + ch.len_utf8();
                char_bytes.push((char_start, char_end));
                decoded.push(ch);
                char_start = char_end;
            }
            index = index + 9 + end + 3;
            continue;
        }
        // One UTF-8 code point.
        let len = utf8_len(byte);
        if index + len > raw.len() {
            return Err(CoreError::Domain(
                "prose-xml-invalid: truncated UTF-8 in text".to_string(),
            ));
        }
        let slice = &raw[index..index + len];
        let ch = std::str::from_utf8(slice)
            .map_err(|_| CoreError::Domain("prose-xml-invalid: bad UTF-8 in text".to_string()))?
            .chars()
            .next()
            .expect("utf8 slice has one char");
        char_bytes.push((index, index + len));
        decoded.push(ch);
        index += len;
    }
    Ok((decoded, char_bytes))
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

fn decode_entity(entity: &str) -> Option<char> {
    match entity {
        "&amp;" => Some('&'),
        "&lt;" => Some('<'),
        "&gt;" => Some('>'),
        "&quot;" => Some('"'),
        "&apos;" => Some('\''),
        _ => {
            let digits = entity
                .strip_prefix("&#x")
                .or_else(|| entity.strip_prefix("&#X"))?;
            let code = digits.strip_suffix(';')?;
            let value = u32::from_str_radix(code, 16).ok()?;
            char::from_u32(value)
        }
    }
    .or_else(|| {
        let digits = entity.strip_prefix("&#")?;
        let code = digits.strip_suffix(';')?;
        let value: u32 = code.parse().ok()?;
        char::from_u32(value)
    })
}

/// Find `old` (decoded) inside a leaf text body and return the byte range
/// of the first occurrence. Fails when the occurrence is not unique within
/// the leaf (an ambiguous old text is unprovable), mirroring the
/// fail-closed island contract.
pub fn find_old_byte_range(
    raw: &[u8],
    text_start: usize,
    text_end: usize,
    old: &str,
) -> Result<Option<(usize, usize)>, CoreError> {
    if old.is_empty() {
        return Ok(None);
    }
    let (decoded, char_bytes) = decode_text_segments(&raw[text_start..text_end])?;
    // `match_indices` yields BYTE indices; translate to decoded char index
    // via the char count of the decoded prefix (CJK text is multi-byte).
    let mut matches = decoded.match_indices(old);
    let Some((byte_index, _)) = matches.next() else {
        return Ok(None);
    };
    if matches.next().is_some() {
        return Err(CoreError::Domain(
            "prose-edit-ambiguous: old text occurs more than once in the leaf".to_string(),
        ));
    }
    let char_index = decoded[..byte_index].chars().count();
    let byte_start = char_bytes[char_index].0 + text_start;
    let byte_end = char_bytes[char_index + old.chars().count() - 1].1 + text_start;
    Ok(Some((byte_start, byte_end)))
}

// ---------------------------------------------------------------------------
// Enumeration
// ---------------------------------------------------------------------------

/// The kind of one located paragraph (mirror of Python's container paths).
#[derive(Clone, Debug)]
enum ParagraphPath {
    Direct,
    Box {
        box_index: i64,
        p_ordinal: u32,
    },
    Sdt {
        sdt_index: i64,
        p_ordinal: u32,
    },
    Cell {
        table_index: i64,
        chain: Vec<(String, u32)>,
    },
}

impl ParagraphPath {
    fn id(&self) -> String {
        match self {
            ParagraphPath::Direct => "".to_string(),
            ParagraphPath::Box {
                box_index,
                p_ordinal,
            } => {
                format!("B{box_index}.P{p_ordinal}")
            }
            ParagraphPath::Sdt {
                sdt_index,
                p_ordinal,
            } => {
                format!("S{sdt_index}.P{p_ordinal}")
            }
            ParagraphPath::Cell { table_index, chain } => {
                let mut values: BTreeMap<&str, u32> = BTreeMap::new();
                for (name, ordinal) in chain {
                    values.insert(name.as_str(), *ordinal);
                }
                format!(
                    "T{table_index}.R{}.C{}.P{}",
                    values.get("tr").copied().unwrap_or(0),
                    values.get("tc").copied().unwrap_or(0),
                    values.get("p").copied().unwrap_or(0)
                )
            }
        }
    }
}

fn is_sdt_content_stack(chain: &[&str]) -> bool {
    if chain.first().copied() != Some("sdt") {
        return false;
    }
    chain[1..]
        .iter()
        .all(|name| *name == "sdtPr" || *name == "sdtEndPr")
}

fn is_sdt_paragraph_stack(chain: &[&str]) -> bool {
    chain.last().copied() == Some("p")
        && chain.len() == 3
        && chain[0] == "sdt"
        && chain[1] == "sdtContent"
}

fn is_box_paragraph_stack(chain: &[&str]) -> bool {
    chain.last().copied() == Some("p")
        && chain.contains(&"txbxContent")
        && !chain
            .iter()
            .any(|name| matches!(*name, "tbl" | "tr" | "tc"))
}

fn is_cell_paragraph_stack(chain: &[&str]) -> bool {
    chain.last().copied() == Some("p") && chain.len() >= 4 && {
        let mut ok = true;
        for (level, name) in chain[..chain.len() - 1].iter().rev().enumerate() {
            let expected = match level % 3 {
                0 => "tc",
                1 => "tr",
                _ => "tbl",
            };
            ok = ok && *name == expected;
        }
        ok
    }
}

fn in_body_paragraph(stack: &[(String, usize, u32)], body_depth: usize) -> bool {
    stack
        .get(body_depth + 1)
        .is_some_and(|(name, _, _)| name == "p")
}

/// Scan `word/document.xml` into located paragraphs (mirror of
/// `locate_document_xml`): (paragraph id, start, end) in scan order.
pub fn scan_document(xml: &[u8]) -> Result<Vec<(String, usize, usize)>, CoreError> {
    let tags = scan_tags(xml);
    let mut stack: Vec<(String, usize, u32)> = Vec::new();
    let mut ordinals: BTreeMap<(usize, usize, String), u32> = BTreeMap::new();
    let mut body_depth: Option<usize> = None;
    let mut paragraph_starts: Vec<(usize, usize, ParagraphPath)> = Vec::new();
    let mut table_ordinal: i64 = -1;
    let mut open_tables: Vec<(i64, usize)> = Vec::new();
    let mut box_ordinal: i64 = -1;
    let mut open_boxes: Vec<(i64, usize)> = Vec::new();
    let mut sdt_ordinal: i64 = -1;
    let mut open_sdts: Vec<(i64, usize)> = Vec::new();
    let mut direct_count: u32 = 0;

    for tag in &tags {
        if tag.closing {
            let depth = stack.len();
            let Some((top_name, top_start, top_ordinal)) = stack.last() else {
                return Err(CoreError::Domain(format!(
                    "malformed document XML nesting near {}",
                    tag.raw_name
                )));
            };
            if top_name != &tag.name {
                return Err(CoreError::Domain(format!(
                    "malformed document XML nesting near {}",
                    tag.raw_name
                )));
            }
            let Some(body_depth) = body_depth else {
                return Err(CoreError::Domain(
                    "document XML has no body element".to_string(),
                ));
            };
            if tag.name == "p" && depth == body_depth + 2 {
                paragraph_starts.push((*top_start, tag.end, ParagraphPath::Direct));
            } else if tag.name == "p"
                && depth > body_depth + 1
                && (!open_tables.is_empty() || !open_boxes.is_empty() || !open_sdts.is_empty())
            {
                let chain: Vec<&str> = stack[body_depth + 1..]
                    .iter()
                    .map(|(name, _, _)| name.as_str())
                    .collect();
                if is_box_paragraph_stack(&chain) {
                    let box_index = open_boxes.last().expect("open box").0;
                    paragraph_starts.push((
                        *top_start,
                        tag.end,
                        ParagraphPath::Box {
                            box_index,
                            p_ordinal: *top_ordinal,
                        },
                    ));
                } else if is_sdt_paragraph_stack(&chain) {
                    let sdt_index = open_sdts.last().expect("open sdt").0;
                    paragraph_starts.push((
                        *top_start,
                        tag.end,
                        ParagraphPath::Sdt {
                            sdt_index,
                            p_ordinal: *top_ordinal,
                        },
                    ));
                } else if is_cell_paragraph_stack(&chain) {
                    let table_index = open_tables.last().expect("open table").0;
                    let path: Vec<(String, u32)> = stack[body_depth + 1..]
                        .iter()
                        .map(|(name, _, ordinal)| (name.clone(), *ordinal))
                        .collect();
                    paragraph_starts.push((
                        *top_start,
                        tag.end,
                        ParagraphPath::Cell {
                            table_index,
                            chain: path,
                        },
                    ));
                }
            }
            if tag.name == "tbl"
                && open_tables
                    .last()
                    .is_some_and(|(_, start)| *start == *top_start)
            {
                open_tables.pop();
            }
            if tag.name == "txbxContent"
                && open_boxes
                    .last()
                    .is_some_and(|(_, start)| *start == *top_start)
            {
                open_boxes.pop();
            }
            if tag.name == "sdtContent"
                && open_sdts
                    .last()
                    .is_some_and(|(_, start)| *start == *top_start)
            {
                open_sdts.pop();
            }
            stack.pop();
            continue;
        }
        let depth = stack.len();
        if tag.name == "body" && body_depth.is_none() {
            body_depth = Some(depth);
        }
        let parent_key = (
            depth.saturating_sub(1),
            stack
                .last()
                .map(|(_, start, _)| *start)
                .unwrap_or(usize::MAX),
            tag.name.clone(),
        );
        let ordinal = ordinals.get(&parent_key).copied().unwrap_or(0);
        ordinals.insert(parent_key, ordinal + 1);
        if let Some(body_depth) = body_depth {
            if tag.name == "p" && depth == body_depth + 1 {
                if tag.self_closing {
                    paragraph_starts.push((tag.start, tag.end, ParagraphPath::Direct));
                } else {
                    stack.push((tag.name.clone(), tag.start, ordinal));
                }
                continue;
            }
            if tag.name == "sdtContent"
                && depth > body_depth + 1
                && is_sdt_content_stack(
                    &stack[body_depth + 1..]
                        .iter()
                        .map(|(name, _, _)| name.as_str())
                        .collect::<Vec<&str>>(),
                )
            {
                sdt_ordinal += 1;
                open_sdts.push((sdt_ordinal, tag.start));
            }
            if tag.name == "txbxContent"
                && depth > body_depth + 1
                && in_body_paragraph(&stack, body_depth)
            {
                box_ordinal += 1;
                open_boxes.push((box_ordinal, tag.start));
            }
            if tag.name == "tbl" && depth == body_depth + 1 {
                table_ordinal += 1;
                open_tables.push((table_ordinal, tag.start));
                stack.push((tag.name.clone(), tag.start, ordinal));
                continue;
            }
            if tag.name == "tbl"
                && depth > body_depth + 1
                && stack.last().is_some_and(|(name, _, _)| name == "tc")
            {
                table_ordinal += 1;
                open_tables.push((table_ordinal, tag.start));
            }
        }
        if !tag.self_closing {
            stack.push((tag.name.clone(), tag.start, ordinal));
        }
    }
    if !stack.is_empty() {
        return Err(CoreError::Domain(
            "document XML has unclosed elements".to_string(),
        ));
    }
    if !open_tables.is_empty() || !open_boxes.is_empty() || !open_sdts.is_empty() {
        return Err(CoreError::Domain(
            "document XML has unclosed containers".to_string(),
        ));
    }
    if body_depth.is_none() {
        return Err(CoreError::Domain(
            "document XML has no body element".to_string(),
        ));
    }
    let mut located: Vec<(String, usize, usize)> = Vec::with_capacity(paragraph_starts.len());
    for (start, end, path) in paragraph_starts {
        let id = match path {
            ParagraphPath::Direct => {
                let id = format!("P{direct_count}");
                direct_count += 1;
                id
            }
            other => other.id(),
        };
        located.push((id, start, end));
    }
    Ok(located)
}

/// Scan one header/footer/footnote/endnote/comments part (mirror of
/// `locate_part_xml`): (paragraph id, start, end) — direct part paragraphs
/// first, then cell paragraphs of part-level tables.
pub fn scan_part(xml: &[u8], part_key: &str) -> Result<Vec<(String, usize, usize)>, CoreError> {
    let base = part_key
        .trim_end_matches(|ch: char| ch.is_ascii_digit())
        .to_string();
    let root_name = match base.as_str() {
        "header" => "hdr",
        "footer" => "ftr",
        "footnotes" | "endnotes" | "comments" => base.as_str(),
        _ => return Err(CoreError::Domain(format!("unknown part key: {part_key}"))),
    };
    let tags = scan_tags(xml);
    let mut stack: Vec<(String, usize)> = Vec::new();
    let mut root_start: Option<usize> = None;
    let mut root_end: Option<usize> = None;
    let mut paragraphs: Vec<(usize, usize)> = Vec::new();
    let mut cell_paragraphs: Vec<(String, usize, usize)> = Vec::new();
    let mut table_ordinal: i64 = -1;
    let mut tr_ordinal: i64 = 0;
    let mut tc_ordinal: i64 = 0;
    let mut cell_p_ordinal: i64 = 0;
    let mut open_tables: Vec<(i64, usize)> = Vec::new();

    for tag in &tags {
        if tag.closing {
            let depth = stack.len();
            let Some((top_name, top_start)) = stack.last() else {
                return Err(CoreError::Domain(format!(
                    "malformed {part_key} XML nesting near {}",
                    tag.raw_name
                )));
            };
            if top_name != &tag.name {
                return Err(CoreError::Domain(format!(
                    "malformed {part_key} XML nesting near {}",
                    tag.raw_name
                )));
            }
            if tag.name == root_name && depth == 1 {
                root_end = Some(tag.start);
            }
            if tag.name == "p" && depth == 2 && matches!(root_name, "hdr" | "ftr") {
                paragraphs.push((*top_start, tag.end));
            }
            if tag.name == "p"
                && depth == 3
                && matches!(root_name, "footnotes" | "endnotes" | "comments")
            {
                paragraphs.push((*top_start, tag.end));
            }
            if tag.name == "p"
                && depth >= 5
                && matches!(root_name, "hdr" | "ftr")
                && !open_tables.is_empty()
                && stack
                    .get(stack.len() - 4..stack.len() - 1)
                    .is_some_and(|slice| {
                        slice
                            .iter()
                            .map(|(name, _)| name.as_str())
                            .collect::<Vec<_>>()
                            == ["tbl", "tr", "tc"]
                    })
            {
                let table_index = open_tables.last().expect("open table").0;
                let id = format!(
                    "{part_key}.T{table_index}.R{}.C{}.P{cell_p_ordinal}",
                    tr_ordinal - 1,
                    tc_ordinal - 1
                );
                cell_paragraphs.push((id, *top_start, tag.end));
                cell_p_ordinal += 1;
            }
            if tag.name == "tbl"
                && open_tables
                    .last()
                    .is_some_and(|(_, start)| *start == *top_start)
            {
                open_tables.pop();
            }
            stack.pop();
            continue;
        }
        let depth = stack.len();
        if tag.name == root_name && depth == 0 {
            root_start = Some(tag.start);
            if tag.self_closing {
                root_end = Some(tag.end);
            }
        }
        if tag.name == "tbl" && depth == 1 && matches!(root_name, "hdr" | "ftr") {
            table_ordinal += 1;
            tr_ordinal = 0;
            tc_ordinal = 0;
            open_tables.push((table_ordinal, tag.start));
        }
        if tag.name == "tr" && depth == 2 && !open_tables.is_empty() {
            tr_ordinal += 1;
            tc_ordinal = 0;
        }
        if tag.name == "tc" && depth == 3 && !open_tables.is_empty() {
            tc_ordinal += 1;
            cell_p_ordinal = 0;
        }
        if !tag.self_closing {
            stack.push((tag.name.clone(), tag.start));
        }
    }
    if !stack.is_empty() || root_start.is_none() {
        return Err(CoreError::Domain(format!(
            "{part_key} XML has unclosed elements"
        )));
    }
    if root_end.is_none() {
        return Err(CoreError::Domain(format!(
            "no {root_name} root in {part_key}"
        )));
    }
    let mut located: Vec<(String, usize, usize)> =
        Vec::with_capacity(paragraphs.len() + cell_paragraphs.len());
    for (index, (start, end)) in paragraphs.into_iter().enumerate() {
        located.push((format!("{part_key}.P{index}"), start, end));
    }
    located.extend(cell_paragraphs);
    Ok(located)
}

// ---------------------------------------------------------------------------
// Leaf / opaque extraction within one paragraph
// ---------------------------------------------------------------------------

/// Known inline children of a run (Python `_parse_run` `known_inline`).
pub const KNOWN_RUN_CHILDREN: [&str; 15] = [
    "t",
    "delText",
    "tab",
    "br",
    "cr",
    "noBreakHyphen",
    "softHyphen",
    "sym",
    "commentReference",
    "footnoteRef",
    "endnoteRef",
    "annotationRef",
    "separator",
    "continuationSeparator",
    "lastRenderedPageBreak",
];

/// Known container-level children of a paragraph (Python `_parse_container`
/// handled set: pPr/proofErr/r/hyperlink/revisions/anchors; anything else
/// is opaque). `proofErr` is opaque but is handled separately by callers.
pub const KNOWN_CONTAINER_CHILDREN: [&str; 12] = [
    "pPr",
    "proofErr",
    "r",
    "hyperlink",
    "ins",
    "del",
    "moveFrom",
    "moveTo",
    "bookmarkStart",
    "bookmarkEnd",
    "commentRangeStart",
    "commentRangeEnd",
];

/// The style hash of a run: SHA-256 of its raw rPr bytes ("" when absent).
fn style_sha256(rpr_bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(rpr_bytes))
}

/// One paragraph's leaf/opaque extraction result.
struct ParagraphExtract {
    leaves: Vec<ProseLeaf>,
    opaques: Vec<OpaqueBlock>,
}

/// Collect leaves and opaque blocks inside one paragraph element (mirror of
/// Python `_parse_run` / `_parse_container`):
/// - run children: `t` -> editable leaf candidate, `delText` -> locked leaf,
///   inline tokens -> skipped, anything else -> the WHOLE run is one opaque
///   block (Python's early-return rule);
/// - container children: pPr skipped, proofErr opaque, r processed,
///   hyperlink/ins/del/moveFrom/moveTo recursed, anchors skipped, anything
///   else (drawing, fldSimple, oMath, object, subDoc, inline sdt, ...)
///   opaque;
/// - a paragraph containing ANY opaque node is fully locked (Python
///   `editable = not contains_opaque(children)`).
fn extract_paragraph(
    node: &ElementNode,
    nodes: &[ElementNode],
    xml: &[u8],
    part_key: &str,
    paragraph_id: &str,
) -> ParagraphExtract {
    let mut leaves: Vec<ProseLeaf> = Vec::new();
    let mut opaques: Vec<OpaqueBlock> = Vec::new();
    let mut leaf_index = 0usize;
    let mut rpr_bytes: Vec<u8> = Vec::new();

    // One run node: classify children in order; on the first unknown child
    // the whole run becomes an opaque block (and leaves collected from it
    // stay locked).
    #[allow(clippy::too_many_arguments)]
    fn process_run(
        run: &ElementNode,
        nodes: &[ElementNode],
        xml: &[u8],
        leaves: &mut Vec<ProseLeaf>,
        opaques: &mut Vec<OpaqueBlock>,
        leaf_index: &mut usize,
        rpr_bytes: &mut Vec<u8>,
        paragraph_id: &str,
        part_key: &str,
    ) {
        let mut opaque = false;
        let mut rpr_range: (usize, usize) = (0, 0);
        for &child in &run.children {
            let child_node = &nodes[child];
            let name = child_node.name.as_str();
            if name == "rPr" {
                *rpr_bytes = xml[child_node.open_start..child_node.end].to_vec();
                rpr_range = (child_node.open_start, child_node.end);
            } else if name == "t" || name == "delText" {
                let text_start = child_node.open_end;
                let text_end = if child_node.children.is_empty() {
                    child_node.close_start
                } else {
                    // A w:t with element children cannot be byte-mapped
                    // provably: the run is locked.
                    opaque = true;
                    child_node.open_end
                };
                let (text, _) =
                    decode_text_segments(&xml[text_start..text_end]).unwrap_or_default();
                leaves.push(ProseLeaf {
                    part_key: part_key.to_string(),
                    paragraph_id: paragraph_id.to_string(),
                    leaf_index: *leaf_index,
                    editable: true, // locked later for delText / opaque paragraphs
                    text_start,
                    text_end,
                    open_end: child_node.open_end,
                    style_sha256: style_sha256(rpr_bytes),
                    rpr: String::from_utf8_lossy(rpr_bytes).into_owned(),
                    rpr_start: rpr_range.0,
                    rpr_end: rpr_range.1,
                    run_open_end: run.open_end,
                    text,
                });
                // delText is a deletion revision: locked in v1.
                if name == "delText" {
                    if let Some(leaf) = leaves.last_mut() {
                        leaf.editable = false;
                    }
                }
                *leaf_index += 1;
            } else if KNOWN_RUN_CHILDREN.contains(&name) {
                // inline token: not a text leaf, not opaque
            } else {
                // drawing / fldChar / pict / object / unknown: run opaque.
                opaque = true;
            }
        }
        if opaque {
            opaques.push(OpaqueBlock {
                part_key: part_key.to_string(),
                paragraph_id: paragraph_id.to_string(),
                tag: run.raw_name.clone(),
                start: run.open_start,
                end: run.end,
            });
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn process_container(
        node: &ElementNode,
        nodes: &[ElementNode],
        xml: &[u8],
        leaves: &mut Vec<ProseLeaf>,
        opaques: &mut Vec<OpaqueBlock>,
        leaf_index: &mut usize,
        rpr_bytes: &mut Vec<u8>,
        paragraph_id: &str,
        part_key: &str,
    ) {
        for &child in &node.children {
            let child_node = &nodes[child];
            let name = child_node.name.as_str();
            match name {
                "pPr" => {}
                "proofErr" => opaques.push(OpaqueBlock {
                    part_key: part_key.to_string(),
                    paragraph_id: paragraph_id.to_string(),
                    tag: child_node.raw_name.clone(),
                    start: child_node.open_start,
                    end: child_node.end,
                }),
                "r" => process_run(
                    child_node,
                    nodes,
                    xml,
                    leaves,
                    opaques,
                    leaf_index,
                    rpr_bytes,
                    paragraph_id,
                    part_key,
                ),
                "hyperlink" | "ins" | "del" | "moveFrom" | "moveTo" => process_container(
                    child_node,
                    nodes,
                    xml,
                    leaves,
                    opaques,
                    leaf_index,
                    rpr_bytes,
                    paragraph_id,
                    part_key,
                ),
                "bookmarkStart" | "bookmarkEnd" | "commentRangeStart" | "commentRangeEnd" => {}
                _ => opaques.push(OpaqueBlock {
                    part_key: part_key.to_string(),
                    paragraph_id: paragraph_id.to_string(),
                    tag: child_node.raw_name.clone(),
                    start: child_node.open_start,
                    end: child_node.end,
                }),
            }
        }
    }

    process_container(
        node,
        nodes,
        xml,
        &mut leaves,
        &mut opaques,
        &mut leaf_index,
        &mut rpr_bytes,
        paragraph_id,
        part_key,
    );
    // Paragraph-level edibility (Python `not contains_opaque`): any opaque
    // node in the paragraph locks every leaf.
    if !opaques.is_empty() {
        for leaf in leaves.iter_mut() {
            leaf.editable = false;
        }
    }
    ParagraphExtract { leaves, opaques }
}

// ---------------------------------------------------------------------------
// Package enumeration + global invariant validation
// ---------------------------------------------------------------------------

/// Enumerate the recursive prose surface of one DOCX package.
pub fn enumerate_package(path: &Path) -> Result<ProseInventory, CoreError> {
    let bytes = std::fs::read(path).map_err(CoreError::io)?;
    let file = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| CoreError::Message(format!("not a valid DOCX: {error}")))?;
    let mut part_xml: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for index in 0..archive.len() {
        let mut member = archive
            .by_index(index)
            .map_err(|error| CoreError::Message(format!("not a valid DOCX: {error}")))?;
        let name = member.name().to_string();
        if let Some(part_key) = part_key_from_path(&name) {
            let mut bytes = Vec::new();
            member.read_to_end(&mut bytes).map_err(CoreError::io)?;
            part_xml.insert(part_key, bytes);
        }
    }
    if !part_xml.contains_key("document") {
        return Err(CoreError::Message(
            "not a valid DOCX: missing word/document.xml".to_string(),
        ));
    }
    enumerate_parts(&part_xml)
}

/// Enumerate over already-read part bytes (shared with the edit path).
fn enumerate_parts(part_xml: &BTreeMap<String, Vec<u8>>) -> Result<ProseInventory, CoreError> {
    // Extract order: headers (sorted), document (body scan order), footers
    // (sorted), endnotes, footnotes, comments (mirror of
    // `parse_package_document`).
    let mut part_keys: Vec<String> = Vec::new();
    for key in part_xml.keys() {
        if key.starts_with("header") {
            part_keys.push(key.clone());
        }
    }
    part_keys.sort();
    part_keys.push("document".to_string());
    let mut footers: Vec<String> = Vec::new();
    for key in part_xml.keys() {
        if key.starts_with("footer") {
            footers.push(key.clone());
        }
    }
    footers.sort();
    part_keys.extend(footers);
    for suffix in ["endnotes", "footnotes", "comments"] {
        if part_xml.contains_key(suffix) {
            part_keys.push(suffix.to_string());
        }
    }

    let mut paragraphs: Vec<ParagraphEntry> = Vec::new();
    let mut leaves: Vec<ProseLeaf> = Vec::new();
    let mut opaques: Vec<OpaqueBlock> = Vec::new();
    for part_key in &part_keys {
        let xml = &part_xml[part_key];
        let tags = scan_tags(xml);
        let nodes = build_tree(&tags)?;
        let located = if part_key == "document" {
            scan_document(xml)?
        } else {
            scan_part(xml, part_key)?
        };
        for (paragraph_id, start, end) in located {
            // Find the paragraph's tree node by exact byte range.
            let Some(node) = nodes.iter().find(|node| {
                node.open_start == start
                    && (node.end == end || (node.self_closing && node.open_start == end))
            }) else {
                return Err(CoreError::Domain(format!(
                    "paragraph locator disagrees with parsed XML: {paragraph_id}"
                )));
            };
            let extract = extract_paragraph(node, &nodes, xml, part_key, &paragraph_id);
            let visible_text: String = extract
                .leaves
                .iter()
                .filter(|leaf| leaf.text_start != leaf.text_end)
                .map(|leaf| leaf.text.clone())
                .collect();
            let opaque_count = extract.opaques.len();
            let editable = extract.opaques.is_empty();
            paragraphs.push(ParagraphEntry {
                part_key: part_key.clone(),
                paragraph_id: paragraph_id.clone(),
                visible_text,
                editable,
                leaf_count: extract.leaves.len(),
                opaque_count,
            });
            leaves.extend(extract.leaves);
            opaques.extend(extract.opaques);
        }
    }
    Ok(ProseInventory {
        part_keys,
        paragraphs,
        leaves,
        opaques,
    })
}

/// Global invariant gate before an edited build (mirror of Python's
/// package guard): the package opens as a zip, `word/document.xml` exists,
/// and EVERY `.xml` part parses as well-formed XML (full parser, not just
/// the nesting-aware byte walker). Failures reject the whole build.
pub fn validate_package(path: &Path) -> Result<PackageFacts, CoreError> {
    let bytes = std::fs::read(path).map_err(CoreError::io)?;
    let file = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| CoreError::Message(format!("not a valid DOCX: {error}")))?;
    let mut parts = 0usize;
    let mut xml_parts = 0usize;
    let mut has_document_xml = false;
    for index in 0..archive.len() {
        let mut member = archive
            .by_index(index)
            .map_err(|error| CoreError::Message(format!("not a valid DOCX: {error}")))?;
        let name = member.name().to_string();
        parts += 1;
        if !name.ends_with(".xml") {
            continue;
        }
        xml_parts += 1;
        let mut bytes = Vec::new();
        member.read_to_end(&mut bytes).map_err(CoreError::io)?;
        roxmltree::Document::parse(std::str::from_utf8(&bytes).map_err(|_| {
            CoreError::Domain("prose-xml-invalid: part XML is not UTF-8".to_string())
        })?)
        .map_err(|error| {
            CoreError::Domain(format!(
                "prose-xml-invalid: part {name} is not well-formed XML: {error}"
            ))
        })?;
        if name == "word/document.xml" {
            has_document_xml = true;
        }
    }
    if !has_document_xml {
        return Err(CoreError::Message(
            "not a valid DOCX: missing word/document.xml".to_string(),
        ));
    }
    Ok(PackageFacts {
        parts,
        xml_parts,
        has_document_xml,
    })
}

// ---------------------------------------------------------------------------
// Island edits
// ---------------------------------------------------------------------------

/// Resolve one leaf inside the enumerated inventory; a leaf outside the
/// editable surface fails closed with the Python-mirrored diagnostic.
pub fn resolve_leaf(
    inventory: &ProseInventory,
    part: &str,
    paragraph_id: &str,
    leaf_index: usize,
) -> Result<ProseLeaf, CoreError> {
    let leaf = inventory
        .leaves
        .iter()
        .find(|leaf| {
            leaf.part_key == part
                && leaf.paragraph_id == paragraph_id
                && leaf.leaf_index == leaf_index
        })
        .cloned()
        .ok_or_else(|| {
            CoreError::Domain(format!(
                "invalid-edit: leaf not found: {part}:{paragraph_id}.{leaf_index}"
            ))
        })?;
    if !leaf.editable {
        return Err(CoreError::Domain(format!(
            "opaque-paragraph-mutated: {paragraph_id}: leaf {leaf_index} is inside \
             locked structure (opaque field/math/drawing interior or non-editable surface)"
        )));
    }
    Ok(leaf)
}

/// Validate one island edit against the template package: resolve the leaf,
/// prove the old text exists uniquely, and confirm the surgery byte range.
/// Returns the leaf plus the byte range to replace.
pub fn validate_edit(
    template: &Path,
    edit: &IslandEdit,
) -> Result<(ProseLeaf, (usize, usize)), CoreError> {
    let part = edit.part.clone();
    let part_bytes = read_part(template, &part)?;
    let inventory = enumerate_package(template)?;
    let leaf = resolve_leaf(&inventory, &part, &edit.paragraph_id, edit.leaf_index)?;
    let Some((byte_start, byte_end)) =
        find_old_byte_range(&part_bytes, leaf.text_start, leaf.text_end, &edit.old)?
    else {
        return Err(CoreError::Domain(format!(
            "invalid-edit: old text {:?} not found in leaf {}.{} of {}",
            edit.old, edit.paragraph_id, edit.leaf_index, part
        )));
    };
    Ok((leaf.clone(), (byte_start, byte_end)))
}

fn read_part(template: &Path, part: &str) -> Result<Vec<u8>, CoreError> {
    let bytes = std::fs::read(template).map_err(CoreError::io)?;
    let file = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| CoreError::Message(format!("not a valid DOCX: {error}")))?;
    let target = part_path(part);
    for index in 0..archive.len() {
        let mut member = archive
            .by_index(index)
            .map_err(|error| CoreError::Message(format!("not a valid DOCX: {error}")))?;
        if member.name() == target {
            let mut bytes = Vec::new();
            member.read_to_end(&mut bytes).map_err(CoreError::io)?;
            return Ok(bytes);
        }
    }
    Err(CoreError::Domain(format!(
        "invalid-edit: part not found: {target}"
    )))
}

/// Apply a list of island edits to the template package, producing the new
/// package bytes. Every edit is re-validated (leaf editable, old text
/// proven, opaque containment) and the global invariant gate runs first;
/// any failure returns Err with no output produced.
/// Apply direct or tracked island edits. Tracked edits replace one text run
/// with a `w:del` + `w:ins` pair while preserving the run properties.
pub fn apply_edits(template: &Path, edits: &[IslandEdit]) -> Result<Vec<u8>, CoreError> {
    if edits.is_empty() {
        return std::fs::read(template).map_err(CoreError::io);
    }
    validate_package(template)?;
    let package = std::fs::read(template).map_err(CoreError::io)?;
    let mut next_revision_id = max_revision_id(&package).saturating_add(1);
    // Group edits by part, preserving order within each part.
    let mut by_part: BTreeMap<String, Vec<&IslandEdit>> = BTreeMap::new();
    for edit in edits {
        by_part.entry(edit.part.clone()).or_default().push(edit);
    }
    let mut current = package;
    for (part, part_edits) in &by_part {
        let part_bytes = read_part_bytes(&current, &part_path(part))?;
        let inventory = enumerate_package_bytes(&current)?;
        let mut spans: Vec<(usize, usize, Vec<u8>)> = Vec::new();
        for edit in part_edits {
            let leaf = resolve_leaf(&inventory, part, &edit.paragraph_id, edit.leaf_index)?;
            let Some((byte_start, byte_end)) =
                find_old_byte_range(&part_bytes, leaf.text_start, leaf.text_end, &edit.old)?
            else {
                return Err(CoreError::Domain(format!(
                    "invalid-edit: old text {:?} not found in leaf {}.{} of {}",
                    edit.old, edit.paragraph_id, edit.leaf_index, part
                )));
            };
            if edit.tracked {
                let replacement = tracked_run_replacement(
                    TrackedEditContext {
                        part: &part_bytes,
                        leaf: &leaf,
                    },
                    TrackedEditContent {
                        old: &edit.old,
                        new: &edit.new,
                    },
                    TrackedEditRevision {
                        author: &edit.author,
                        date: &edit.date,
                        delete_id: next_revision_id,
                        insert_id: next_revision_id.saturating_add(1),
                    },
                )?;
                next_revision_id = next_revision_id.saturating_add(2);
                spans.push(replacement);
            } else {
                let replacement = xml_escape(&edit.new).into_bytes();
                spans.push((byte_start, byte_end, replacement));
                // Whitespace preservation: a text with leading/trailing spaces
                // needs xml:space="preserve" on the w:t start tag.
                if needs_xml_space(&edit.new) {
                    if let Some(tag_span) = maybe_inject_xml_space(&part_bytes, leaf.open_end) {
                        spans.push(tag_span);
                    }
                }
            }
        }
        // Apply spans right-to-left so earlier offsets stay valid.
        spans.sort_by_key(|(start, _, _)| *start);
        let mut new_part = part_bytes.clone();
        for (byte_start, byte_end, replacement) in spans.into_iter().rev() {
            new_part.splice(byte_start..byte_end, replacement);
        }
        current = patch_zip_member(&current, &part_path(part), &new_part)?;
    }
    Ok(current)
}

fn max_revision_id(package: &[u8]) -> u32 {
    let mut max_id = 0u32;
    let marker = b"w:id=\"";
    let mut start = 0usize;
    while let Some(offset) = package[start..]
        .windows(marker.len())
        .position(|w| w == marker)
    {
        let begin = start + offset + marker.len();
        let end = package[begin..]
            .iter()
            .position(|byte| !byte.is_ascii_digit())
            .map(|n| begin + n)
            .unwrap_or(package.len());
        if let Ok(value) = std::str::from_utf8(&package[begin..end])
            .unwrap_or("")
            .parse::<u32>()
        {
            max_id = max_id.max(value);
        }
        start = end;
        if start >= package.len() {
            break;
        }
    }
    max_id
}

// Grouped parameters to satisfy clippy::too_many_arguments
struct TrackedEditContext<'a> {
    part: &'a [u8],
    leaf: &'a ProseLeaf,
}

struct TrackedEditContent<'a> {
    old: &'a str,
    new: &'a str,
}

struct TrackedEditRevision<'a> {
    author: &'a str,
    date: &'a str,
    delete_id: u32,
    insert_id: u32,
}

fn tracked_run_replacement(
    ctx: TrackedEditContext<'_>,
    content: TrackedEditContent<'_>,
    revision: TrackedEditRevision<'_>,
) -> Result<(usize, usize, Vec<u8>), CoreError> {
    let before = &ctx.part[..ctx.leaf.text_start];
    let mut run_start = None;
    for index in 0..before.len().saturating_sub(3) {
        if &before[index..index + 4] == b"<w:r"
            && before
                .get(index + 4)
                .is_some_and(|byte| *byte == b' ' || *byte == b'>')
        {
            run_start = Some(index);
        }
    }
    let run_start = run_start
        .ok_or_else(|| CoreError::Domain("tracked-edit: owning run not found".to_string()))?;
    let run_end_rel = ctx.part[ctx.leaf.text_end..]
        .windows(6)
        .position(|w| w == b"</w:r>")
        .ok_or_else(|| CoreError::Domain("tracked-edit: owning run end not found".to_string()))?;
    let run_end = ctx.leaf.text_end + run_end_rel + 6;
    let run = &ctx.part[run_start..run_end];
    let t_count = run.windows(5).filter(|w| *w == b"<w:t>").count()
        + run.windows(5).filter(|w| *w == b"<w:t ").count();
    if t_count != 1 || run.windows(6).filter(|w| *w == b"</w:t>").count() != 1 {
        return Err(CoreError::Domain(
            "tracked-edit: replacement must target one text run".to_string(),
        ));
    }
    let rpr = run
        .windows(6)
        .position(|w| w == b"<w:rPr")
        .and_then(|start| {
            run[start..]
                .windows(8)
                .position(|w| w == b"</w:rPr>")
                .map(|end| String::from_utf8_lossy(&run[start..start + end + 8]).into_owned())
        })
        .unwrap_or_default();
    let prefix = ctx
        .leaf
        .text
        .split_once(content.old)
        .map(|(p, _)| p)
        .unwrap_or("");
    let suffix = ctx
        .leaf
        .text
        .split_once(content.old)
        .map(|(_, s)| s)
        .unwrap_or("");
    let attrs = |kind: &str, id: u32| {
        format!(
            "<w:{kind} w:id=\"{id}\" w:author=\"{}\" w:date=\"{}\">",
            xml_attr(revision.author),
            xml_attr(revision.date)
        )
    };
    let run_text = |tag: &str, text: &str| {
        let space = if text.starts_with(' ') || text.ends_with(' ') {
            " xml:space=\"preserve\""
        } else {
            ""
        };
        format!(
            "<w:r>{rpr}<w:{tag}{space}>{}</w:{tag}></w:r>",
            xml_escape(text)
        )
    };
    if !ctx.leaf.text.contains(content.old) {
        return Err(CoreError::Domain(
            "tracked-edit: old text is not in leaf".to_string(),
        ));
    }
    let mut replacement = String::new();
    if !prefix.is_empty() {
        replacement.push_str(&run_text("t", prefix));
    }
    replacement.push_str(&attrs("del", revision.delete_id));
    replacement.push_str(&run_text("delText", content.old));
    replacement.push_str("</w:del>");
    replacement.push_str(&attrs("ins", revision.insert_id));
    replacement.push_str(&run_text("t", content.new));
    replacement.push_str("</w:ins>");
    if !suffix.is_empty() {
        replacement.push_str(&run_text("t", suffix));
    }
    Ok((run_start, run_end, replacement.into_bytes()))
}

fn xml_attr(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Read one member's decoded bytes from raw package bytes.
fn read_part_bytes(package: &[u8], member: &str) -> Result<Vec<u8>, CoreError> {
    let file = std::io::Cursor::new(package.to_vec());
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| CoreError::Message(format!("not a valid DOCX: {error}")))?;
    for index in 0..archive.len() {
        let mut part = archive
            .by_index(index)
            .map_err(|error| CoreError::Message(format!("not a valid DOCX: {error}")))?;
        if part.name() == member {
            let mut bytes = Vec::new();
            part.read_to_end(&mut bytes).map_err(CoreError::io)?;
            return Ok(bytes);
        }
    }
    Err(CoreError::Domain(format!(
        "invalid-edit: part not found: {member}"
    )))
}

/// Enumerate over raw package bytes (shared helper).
pub fn enumerate_package_bytes(package: &[u8]) -> Result<ProseInventory, CoreError> {
    let file = std::io::Cursor::new(package.to_vec());
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| CoreError::Message(format!("not a valid DOCX: {error}")))?;
    let mut part_xml: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for index in 0..archive.len() {
        let mut member = archive
            .by_index(index)
            .map_err(|error| CoreError::Message(format!("not a valid DOCX: {error}")))?;
        let name = member.name().to_string();
        if let Some(part_key) = part_key_from_path(&name) {
            let mut bytes = Vec::new();
            member.read_to_end(&mut bytes).map_err(CoreError::io)?;
            part_xml.insert(part_key, bytes);
        }
    }
    if !part_xml.contains_key("document") {
        return Err(CoreError::Message(
            "not a valid DOCX: missing word/document.xml".to_string(),
        ));
    }
    enumerate_parts(&part_xml)
}

fn needs_xml_space(text: &str) -> bool {
    text.starts_with(' ') || text.ends_with(' ')
}

/// Inject `xml:space="preserve"` into the `w:t` start tag when absent.
/// Returns the tag-replacement span `(tag_start, open_end, new_tag_bytes)`
/// or None when the attribute is already present (or the tag cannot be
/// located).
fn maybe_inject_xml_space(part_bytes: &[u8], open_end: usize) -> Option<(usize, usize, Vec<u8>)> {
    let has_preserve = part_bytes[..open_end]
        .windows(18)
        .any(|window| window == b"xml:space=\"preserve\"");
    if has_preserve {
        return None;
    }
    // Find the tag start: scan back to the last '<'.
    let tag_start = part_bytes[..open_end]
        .iter()
        .rposition(|&byte| byte == b'<')?;
    let insert_at = part_bytes[tag_start..open_end]
        .iter()
        .rposition(|&byte| byte == b'>')
        .map(|position| tag_start + position)
        .unwrap_or(open_end);
    let mut out = Vec::with_capacity(open_end - tag_start + 19);
    out.extend_from_slice(&part_bytes[tag_start..insert_at]);
    out.extend_from_slice(b" xml:space=\"preserve\"");
    out.extend_from_slice(&part_bytes[insert_at..open_end]);
    Some((tag_start, open_end, out))
}

// ---------------------------------------------------------------------------
// Zip byte surgery: replace one member's data while every other byte of the
// package replays verbatim (including untouched members' deflate streams).
// ---------------------------------------------------------------------------

const EOCD_SIG: &[u8; 4] = b"PK\x05\x06";
const LOCAL_SIG: &[u8; 4] = b"PK\x03\x04";
const CENTRAL_SIG: &[u8; 4] = b"PK\x01\x02";
const DESCRIPTOR_SIG: &[u8; 4] = b"PK\x07\x08";
const FLAG_DATA_DESCRIPTOR: u16 = 0x0008;
const FLAG_ZIP64: u16 = 0x0001;

/// Replace the bytes of one zip member with `new_bytes` (same compression
/// method as the original), copying every other byte of the package
/// verbatim. The edited member's local header, data, (data descriptor),
/// central-directory entry, and the EOCD offset fields are the only bytes
/// that change; all other members' local headers and deflate streams are
/// untouched.
pub fn patch_zip_member(
    package: &[u8],
    member: &str,
    new_bytes: &[u8],
) -> Result<Vec<u8>, CoreError> {
    // --- End-of-central-directory ------------------------------------------
    let eocd_pos = find_eocd(package)
        .ok_or_else(|| CoreError::Domain("prose-xml-invalid: package has no EOCD".to_string()))?;
    if package.len() >= eocd_pos + 22 + 20
        && package[eocd_pos + 22..eocd_pos + 42].starts_with(b"PK\x06\x07")
    {
        return Err(CoreError::Domain(
            "prose-edit-unsupported: zip64 packages are not supported by the island patcher"
                .to_string(),
        ));
    }
    let read_u16 = |pos: usize| -> u16 { u16::from_le_bytes([package[pos], package[pos + 1]]) };
    let read_u32 = |pos: usize| -> u32 {
        u32::from_le_bytes([
            package[pos],
            package[pos + 1],
            package[pos + 2],
            package[pos + 3],
        ])
    };
    let write_u32 = |bytes: &mut [u8], pos: usize, value: u32| {
        bytes[pos..pos + 4].copy_from_slice(&value.to_le_bytes());
    };
    let cd_size = read_u32(eocd_pos + 12) as usize;
    let cd_offset = read_u32(eocd_pos + 16) as usize;
    if cd_offset + cd_size > package.len() {
        return Err(CoreError::Domain(
            "prose-xml-invalid: corrupt central directory".to_string(),
        ));
    }

    // --- Walk the central directory -----------------------------------------
    // entry: 4 sig + 42 fixed + name + extra + comment
    struct CdEntry {
        name: String,
        entry_pos: usize,  // start of the entry (sig)
        offset_pos: usize, // absolute pos of the local-header offset field
        local_offset: u32,
        crc_pos: usize,
        csize_pos: usize,
        usize_pos: usize,
    }
    let mut entries: Vec<CdEntry> = Vec::new();
    let mut pos = cd_offset;
    while pos < cd_offset + cd_size {
        if package[pos..pos + 4] != *CENTRAL_SIG {
            return Err(CoreError::Domain(
                "prose-xml-invalid: corrupt central directory entry".to_string(),
            ));
        }
        let flags = read_u16(pos + 8);
        if flags & FLAG_ZIP64 != 0 {
            return Err(CoreError::Domain(
                "prose-edit-unsupported: zip64 entries are not supported".to_string(),
            ));
        }
        let nlen = read_u16(pos + 28) as usize;
        let elen = read_u16(pos + 30) as usize;
        let clen = read_u16(pos + 32) as usize;
        let name = std::str::from_utf8(&package[pos + 46..pos + 46 + nlen])
            .map_err(|_| {
                CoreError::Domain("prose-xml-invalid: member name is not UTF-8".to_string())
            })?
            .to_string();
        entries.push(CdEntry {
            name,
            entry_pos: pos,
            offset_pos: pos + 42,
            local_offset: read_u32(pos + 42),
            crc_pos: pos + 16,
            csize_pos: pos + 20,
            usize_pos: pos + 24,
        });
        pos += 46 + nlen + elen + clen;
    }
    if pos != cd_offset + cd_size {
        return Err(CoreError::Domain(
            "prose-xml-invalid: central directory size mismatch".to_string(),
        ));
    }

    // --- Locate the edited member -------------------------------------------
    let edited = entries
        .iter()
        .find(|entry| entry.name == member)
        .ok_or_else(|| CoreError::Domain(format!("invalid-edit: part not found: {member}")))?;
    let local_start = edited.local_offset as usize;
    if package[local_start..local_start + 4] != *LOCAL_SIG {
        return Err(CoreError::Domain(
            "prose-xml-invalid: member local header not found".to_string(),
        ));
    }
    let local_flags = read_u16(local_start + 6);
    let method = read_u16(local_start + 8);
    let nlen = read_u16(local_start + 26) as usize;
    let elen = read_u16(local_start + 28) as usize;
    let data_start = local_start + 30 + nlen + elen;
    let old_csize = read_u32(edited.csize_pos) as usize;
    let data_descriptor_len = if local_flags & FLAG_DATA_DESCRIPTOR != 0 {
        if package.len() >= data_start + old_csize + 4
            && package[data_start + old_csize..data_start + old_csize + 4] == *DESCRIPTOR_SIG
        {
            16
        } else {
            12
        }
    } else {
        0
    };
    let data_end = data_start + old_csize + data_descriptor_len;

    // --- Compress the new member data ---------------------------------------
    let (new_data, new_csize, new_crc, new_usize) = match method {
        0 => {
            let crc = crc32fast::hash(new_bytes);
            (new_bytes.to_vec(), new_bytes.len(), crc, new_bytes.len())
        }
        8 => {
            use std::io::Write;
            let mut encoder =
                flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(new_bytes).map_err(CoreError::io)?;
            let compressed = encoder.finish().map_err(CoreError::io)?;
            let crc = crc32fast::hash(new_bytes);
            let new_csize = compressed.len();
            (compressed, new_csize, crc, new_bytes.len())
        }
        other => {
            return Err(CoreError::Domain(format!(
            "prose-edit-unsupported: member {member} uses unsupported compression method {other}"
        )))
        }
    };

    // --- New local header (same lengths, patched sizes/crc) ------------------
    let mut new_local = package[local_start..data_start].to_vec();
    if local_flags & FLAG_DATA_DESCRIPTOR == 0 {
        write_u32(&mut new_local, 14, new_crc);
        write_u32(&mut new_local, 18, new_csize as u32);
        write_u32(&mut new_local, 22, new_usize as u32);
    }

    // --- Patched data descriptor (when streaming) ----------------------------
    let mut new_descriptor: Vec<u8> = Vec::new();
    if data_descriptor_len > 0 {
        new_descriptor = package[data_start + old_csize..data_end].to_vec();
        if data_descriptor_len == 16 {
            write_u32(&mut new_descriptor, 4, new_crc);
            write_u32(&mut new_descriptor, 8, new_csize as u32);
            write_u32(&mut new_descriptor, 12, new_usize as u32);
        } else {
            write_u32(&mut new_descriptor, 0, new_crc);
            write_u32(&mut new_descriptor, 4, new_csize as u32);
            write_u32(&mut new_descriptor, 8, new_usize as u32);
        }
    }

    let delta = new_csize as i64 - old_csize as i64;
    let delta_abs = new_csize.abs_diff(old_csize);

    // --- Patched central directory -------------------------------------------
    let mut new_cd = Vec::with_capacity(cd_size + delta_abs);
    for entry in &entries {
        let start = entry.entry_pos;
        let end = if let Some(next) = entries.iter().find(|other| other.entry_pos > start) {
            next.entry_pos
        } else {
            cd_offset + cd_size
        };
        let mut entry_bytes = package[start..end].to_vec();
        let mut changed = false;
        if entry.name == member {
            write_u32(&mut entry_bytes, entry.crc_pos - start, new_crc);
            write_u32(&mut entry_bytes, entry.csize_pos - start, new_csize as u32);
            write_u32(&mut entry_bytes, entry.usize_pos - start, new_usize as u32);
            changed = true;
        }
        if (entry.local_offset as usize) >= data_end && delta != 0 {
            let new_offset = (entry.local_offset as i64 + delta) as u32;
            write_u32(&mut entry_bytes, entry.offset_pos - start, new_offset);
            changed = true;
        }
        if changed {
            new_cd.extend_from_slice(&entry_bytes);
        } else {
            new_cd.extend_from_slice(&package[start..end]);
        }
    }

    // --- Assemble -------------------------------------------------------------
    let mut out = Vec::with_capacity(package.len() + delta_abs);
    out.extend_from_slice(&package[..local_start]);
    out.extend_from_slice(&new_local);
    out.extend_from_slice(&new_data);
    if data_descriptor_len > 0 {
        out.extend_from_slice(&new_descriptor);
    }
    out.extend_from_slice(&package[data_end..cd_offset]);
    out.extend_from_slice(&new_cd);
    let new_cd_offset = cd_offset as i64 + delta;
    out.extend_from_slice(&package[eocd_pos..]);
    let eocd_in_out = out.len() - (package.len() - eocd_pos);
    write_u32(&mut out, eocd_in_out + 16, new_cd_offset as u32);
    Ok(out)
}

/// Locate the EOCD record (scan the last 64 KiB + 22 bytes backward).
fn find_eocd(package: &[u8]) -> Option<usize> {
    let window = package.len().saturating_sub(65557);
    let mut pos = package.len();
    while pos > window {
        pos -= 1;
        if package[pos..].starts_with(EOCD_SIG) {
            return Some(pos);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Workdir sidecar (islands.json)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Workdir sidecar (rpr_edits.json) -- the format_span lane
// ---------------------------------------------------------------------------

/// The `format_span` sidecar: character-formatting changes the build applies
/// by rewriting an existing run's `w:rPr` in place.
///
/// This is the third record the build understands, and it is deliberately not
/// one of the other two: `islands.json` changes text BYTES inside a leaf, and
/// `structure.json` synthesises or removes whole paragraphs. A format change
/// is neither -- it rewrites properties that sit BEFORE the text, which is why
/// `IslandEdit`'s `text_start..text_end` cannot express it.
pub const RPR_EDITS_SCHEMA: &str = "docx2typed-rpr-edits-1";
pub const RPR_EDITS_FILE: &str = "rpr_edits.json";

/// One recorded restyle.
///
/// Addressing is the same anchor `IslandEdit` uses -- a paragraph plus a
/// character span -- and NOT a run index. A run index is an index into a
/// structure the caller never sees, and it goes stale the moment any text
/// edit moves a run boundary; a text span survives edits elsewhere in the
/// paragraph. `style_id` is what the build applies; `attributes` is kept so
/// the record explains itself.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RprEdit {
    /// Part key: "document", "header1", ...
    pub part: String,
    pub paragraph_id: String,
    /// Character span within the paragraph's visible text.
    pub text_start: usize,
    pub text_end: usize,
    /// The partial override map that was requested, for audit and preview.
    #[serde(default)]
    pub attributes: serde_json::Value,
    /// The derived character style, already present in `styles.json`.
    pub style_id: String,
}

// ---------------------------------------------------------------------------
// typed.md serialization (#52)
// ---------------------------------------------------------------------------

/// The style a paragraph's plain text is measured against: the one covering the
/// most characters, ties broken by first appearance.
///
/// Mirrors `choose_base_style`. It matters because the markup only wraps a span
/// when its style DIFFERS from this, so getting it wrong changes every byte.
fn choose_base_style(segments: &[Segment], fallback: &str) -> String {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut first: BTreeMap<String, usize> = BTreeMap::new();
    let mut order = 0usize;
    for segment in segments {
        if segment.text.is_empty() {
            continue;
        }
        *counts.entry(segment.style_id.clone()).or_default() += segment.text.chars().count();
        first.entry(segment.style_id.clone()).or_insert(order);
        order += 1;
    }
    counts
        .into_iter()
        .max_by_key(|(style_id, count)| (*count, std::cmp::Reverse(first[style_id])))
        .map(|(style_id, _)| style_id)
        .unwrap_or_else(|| fallback.to_string())
}

/// XML-escape exactly as the reference's `xml_escape` does: three entities,
/// in this order, and nothing else.
fn xml_escape_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn attr_value(text: &str) -> String {
    format!("\"{}\"", xml_escape_text(text).replace('"', "&quot;"))
}

/// The body of one paragraph: merged adjacent runs, each wrapped in a `<span>`
/// only when its style differs from the paragraph's base style.
///
/// Mirrors `_node_to_markup` for the text case, which is the whole of it until
/// `_attrs_text`: the named keys first, in the given order, then the rest
/// sorted -- which is what makes `<docx-anchor id kind w:id w:name/>` come out
/// in the reference's order rather than the document's.
fn attrs_text(attrs: &BTreeMap<String, String>, first: &[&str]) -> String {
    let mut keys: Vec<&String> = first
        .iter()
        .filter_map(|key| attrs.get_key_value(*key).map(|(k, _)| k))
        .collect();
    let mut rest: Vec<&String> = attrs
        .keys()
        .filter(|key| !first.contains(&key.as_str()))
        .collect();
    rest.sort();
    keys.extend(rest);
    keys.iter()
        .map(|key| format!("{key}={}", attr_value(&attrs[*key])))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `_node_to_markup`, over the tree.
///
/// The tree is what makes this correct: a `<docx-range>` must sit where its
/// children sit and must hold them. A flat list put the range AFTER its own
/// text and left it empty -- measured against the reference before the tree
/// existed.
fn node_markup(node: &crate::token_table::TypedNode, base_style: &str) -> String {
    use crate::token_table::TypedNode;
    let head = |token_id: &str, kind: &str, attrs: &BTreeMap<String, String>| {
        let mut all = attrs.clone();
        all.insert("id".to_string(), token_id.to_string());
        all.insert("kind".to_string(), kind.to_string());
        attrs_text(&all, &["id", "kind"])
    };
    match node {
        TypedNode::Text {
            style_id,
            text,
            vertical,
        } => {
            let body = match vertical {
                Some(vertical) => {
                    let marker = if vertical == "superscript" { '^' } else { '_' };
                    format!(
                        "{marker}{{{}}}",
                        crate::document_search::escape_vertical_tag_body(text)
                    )
                }
                // The schema-2 grammar gives `^{` / `_{` meaning, so a literal
                // one the document already carried is escaped -- otherwise it
                // would be read as a vertical tag on the next load.
                None => crate::document_search::escape_vertical_markers(text),
            };
            let escaped = xml_escape_text(&body);
            if style_id == base_style {
                escaped
            } else {
                format!("<span data-s={}>{escaped}</span>", attr_value(style_id))
            }
        }
        TypedNode::Anchor {
            token_id,
            kind,
            attrs,
        } => {
            format!("<docx-anchor {}/>", head(token_id, kind, attrs))
        }
        TypedNode::Inline {
            token_id,
            kind,
            style_id,
            attrs,
        } => {
            let mut all = attrs.clone();
            all.insert("style".to_string(), style_id.clone());
            format!("<docx-inline {}/>", head(token_id, kind, &all))
        }
        TypedNode::Opaque {
            token_id,
            kind,
            attrs,
        } => {
            format!("<docx-opaque {}/>", head(token_id, kind, attrs))
        }
        TypedNode::Range {
            token_id,
            kind,
            attrs,
            children,
        } => {
            let inner: String = children
                .iter()
                .map(|c| node_markup(c, base_style))
                .collect();
            format!(
                "<docx-range {}>{inner}</docx-range>",
                head(token_id, kind, attrs)
            )
        }
        TypedNode::Revision {
            token_id,
            kind,
            attrs,
            children,
        } => {
            let inner: String = children
                .iter()
                .map(|c| node_markup(c, base_style))
                .collect();
            format!(
                "<docx-revision {}>{inner}</docx-revision>",
                head(token_id, kind, attrs)
            )
        }
    }
}

/// The body of one paragraph, from its node tree.
fn paragraph_markup(nodes: &[crate::token_table::TypedNode], base_style: &str) -> String {
    nodes
        .iter()
        .map(|node| node_markup(node, base_style))
        .collect()
}

// ---------------------------------------------------------------------------
// Vertical promotion (schema-2 `^{…}` / `_{…}`)
// ---------------------------------------------------------------------------

/// Python's `factor_vertical_style`: can this region's vertical alignment be
/// factored out losslessly?
///
/// Eligible iff removing **exactly one** `w:vertAlign` from the region's run
/// properties yields run properties canonically equal to the paragraph's base
/// style. Canonical identity, not a feature subset: `rpr_features` enumerates
/// only part of Word's `rPr` vocabulary, so a feature diff could ignore a
/// property. Returns the alignment when eligible, else `None` (the region keeps
/// its span).
fn factor_vertical_style(
    catalog: &StyleCatalog,
    style_id: &str,
    base_style_id: &str,
) -> Option<String> {
    if style_id.is_empty() || style_id == base_style_id {
        return None;
    }
    let region_rpr = catalog.rpr(style_id)?;
    // Cheap reject before parsing: almost every style carries no vertical
    // alignment at all, and this runs once per text node.
    if !region_rpr.contains("vertAlign") {
        return None;
    }
    let base_canonical = catalog.canonical(base_style_id)?;
    let fragment = RprFragment::parse(region_rpr).ok()?;
    let alignments: Vec<&ElementNode> = fragment
        .children
        .iter()
        .filter(|child| child.name == "vertAlign")
        .collect();
    let [alignment] = alignments.as_slice() else {
        return None; // none, or more than one: not a single factorable element
    };
    let value = tag_attribute_pairs(&fragment.bytes[alignment.open_start..alignment.end])
        .into_iter()
        .find(|(key, _)| key == "val")
        .map(|(_, value)| value)?;
    if value != "superscript" && value != "subscript" {
        return None;
    }
    let remainder = fragment.restyled("vertAlign", None);
    if canonical_rpr_json(&remainder).ok()? != base_canonical {
        return None; // something else differs too: keep the span
    }
    Some(value)
}

/// Python's `promote_vertical_alignment`: move eligible vertical alignment out
/// of styles and into the text.
///
/// Every text region whose style factors into `(vertical, base)` becomes
/// `(style=base, vertical)`, which the markup writes as `^{…}` / `_{…}`.
/// Returns the number of regions promoted — the migration evidence
/// `format.json` records. History inside a deletion (`w:del` / `w:moveFrom`) is
/// never rewritten.
fn promote_vertical_alignment(
    nodes: &mut [crate::token_table::TypedNode],
    base_style: &str,
    catalog: &StyleCatalog,
) -> usize {
    fn walk(
        nodes: &mut [crate::token_table::TypedNode],
        base_style: &str,
        catalog: &StyleCatalog,
        in_history: bool,
        promoted: &mut usize,
    ) {
        use crate::token_table::TypedNode;
        for node in nodes.iter_mut() {
            match node {
                TypedNode::Text {
                    style_id, vertical, ..
                } => {
                    if in_history {
                        continue;
                    }
                    // The extractor never sets `vertical`, so the reference's
                    // "already vertical" branch (re-factor an existing tag)
                    // is unreachable here.
                    if let Some(found) = factor_vertical_style(catalog, style_id, base_style) {
                        *vertical = Some(found);
                        *style_id = base_style.to_string();
                        *promoted += 1;
                    }
                }
                TypedNode::Range { children, .. } => {
                    walk(children, base_style, catalog, in_history, promoted);
                }
                TypedNode::Revision { kind, children, .. } => {
                    let history = in_history || kind == "delete" || kind == "move_from";
                    walk(children, base_style, catalog, history, promoted);
                }
                _ => {}
            }
        }
    }

    let mut promoted = 0usize;
    walk(nodes, base_style, catalog, false, &mut promoted);
    promoted
}

/// The TEXT nodes of a paragraph as canonical segments — what
/// `choose_base_style` counts, and nothing else.
fn text_segments(nodes: &[crate::token_table::TypedNode]) -> Vec<Segment> {
    nodes
        .iter()
        .filter_map(|node| match node {
            crate::token_table::TypedNode::Text { style_id, text, .. } => Some(Segment {
                start: 0,
                end: text.chars().count(),
                text: text.clone(),
                style_id: style_id.clone(),
                revision: None,
                comment_ids: Vec::new(),
                editable: true,
                visibility: crate::document_projection::Visibility {
                    original: true,
                    tracked: true,
                    final_view: true,
                },
            }),
            _ => None,
        })
        .collect()
}

/// typed.md lists paragraphs in DOCUMENT order — headers, body, footers, notes,
/// comments — while the trees are in TOKEN order, which is the parse order
/// (body first). Part rank ONLY, then the part key: the within-part order is
/// the document's and `sort_by_key` is stable, so it survives. Adding the
/// paragraph id here sorted them LEXICOGRAPHICALLY — P10 before P2 — and broke
/// three documents that had matched.
fn paragraph_rank(tree: &crate::token_table::ParagraphNodes) -> (u8, String) {
    let key = crate::token_table::typed_part_key(&tree.part);
    let rank = if key.starts_with("header") {
        0
    } else if key.is_empty() {
        1
    } else if key.starts_with("footer") {
        2
    } else if key == "footnotes" || key == "endnotes" {
        3
    } else {
        4
    };
    (rank, key)
}

/// One paragraph ready to render: its tree with vertical alignments promoted
/// into the text, and the base style both the promotion and the markup measured
/// it against.
pub struct PreparedParagraph<'a> {
    pub tree: &'a crate::token_table::ParagraphNodes,
    pub base_style: String,
    pub nodes: Vec<crate::token_table::TypedNode>,
}

/// Order the trees, assign each paragraph its base style, and promote vertical
/// alignments — the one pass `serialize_typed` and `format_paragraphs` share.
///
/// The base style is computed from the UNPROMOTED styles (the reference assigns
/// it during parsing, before `promote_vertical_alignment` runs), which is why
/// promotion cannot move it.
pub fn prepare_paragraphs<'a>(
    trees: &'a [crate::token_table::ParagraphNodes],
    catalog: &StyleCatalog,
) -> (Vec<PreparedParagraph<'a>>, usize) {
    let mut ordered: Vec<&crate::token_table::ParagraphNodes> = trees.iter().collect();
    ordered.sort_by_key(|tree| paragraph_rank(tree));
    // The body's first paragraph falls back to Normal; a PART paragraph falls
    // back to "", which is what the reference records — measured on
    // `comments.P0` and `header1.P0`, whose base is `""`.
    let mut fallback = crate::document_projection::default_style_id();
    let mut promoted = 0usize;
    let mut out = Vec::with_capacity(ordered.len());
    for tree in ordered {
        // The reference assigns `base_style` ONLY in the body's walk
        // (`parse_document_xml`); `parse_part_xml` calls `_parse_paragraph`
        // without it, so a header/footer/note/comment paragraph keeps the
        // dataclass default of "".
        let base_style = if crate::token_table::typed_part_key(&tree.part).is_empty() {
            let chosen = choose_base_style(&text_segments(&tree.nodes), &fallback);
            fallback = chosen.clone();
            chosen
        } else {
            String::new()
        };
        let mut nodes = tree.nodes.clone();
        promoted += promote_vertical_alignment(&mut nodes, &base_style, catalog);
        out.push(PreparedParagraph {
            tree,
            base_style,
            nodes,
        });
    }
    (out, promoted)
}

pub fn serialize_typed(
    source_name: &str,
    projection: &crate::document_projection::DocumentProjection,
    prepared: &[PreparedParagraph],
) -> String {
    let header = format!(
        "<!--@typed schema=\"2\" format=\"format.json\" styles=\"styles.json\" template=\"_template.docx\" source={}-->",
        attr_value(source_name)
    );
    let mut blocks = vec![header];
    let mut current_part = Some(String::new());
    // The TREES are the typed document, not the projection's paragraphs: the
    // projection deliberately excludes the comments part, and the reference
    // lists it last. Walking the trees also keeps the markup and the token ids
    // on one traversal.
    let _ = projection;
    for entry in prepared {
        let part = crate::token_table::typed_part_key(&entry.tree.part);
        if current_part.as_deref() != Some(part.as_str()) {
            blocks.push(format!("<!--@part key={}-->", attr_value(&part)));
            current_part = Some(part);
        }
        let body = paragraph_markup(&entry.nodes, &entry.base_style);
        let marker = format!(
            "<!--@p id={} base={}-->",
            attr_value(&entry.tree.paragraph_id),
            attr_value(&entry.base_style)
        );
        let mark = entry
            .tree
            .mark
            .as_ref()
            .map(|(kind, token_id, attrs)| {
                let mut all = attrs.clone();
                all.insert("kind".to_string(), kind.clone());
                all.insert("id".to_string(), token_id.clone());
                format!("\n<!--@mark {}-->", attrs_text(&all, &["kind", "id"]))
            })
            .unwrap_or_default();
        blocks.push(if body.is_empty() {
            format!("{marker}{mark}")
        } else {
            format!("{marker}{mark}\n{body}")
        });
    }
    let mut text = blocks.join("\n\n");
    text.push('\n');
    text
}

/// The `format.json` paragraph records, one per paragraph in extract order.
pub fn format_paragraphs(
    projection: &crate::document_projection::DocumentProjection,
    prepared: &[PreparedParagraph],
) -> Vec<serde_json::Value> {
    // The trees are the typed document, so they carry the paragraph order and
    // the nodes; the projection supplies nothing here that the tree lacks.
    let _ = projection;
    let mut out = Vec::with_capacity(prepared.len());
    for (index, entry) in prepared.iter().enumerate() {
        let paragraph_id = entry.tree.paragraph_id.clone();
        let part_key = crate::token_table::typed_part_key(&entry.tree.part);
        let base_style = entry.base_style.clone();
        // `editable` is "the paragraph contains no opaque node" -- the
        // reference's `not contains_opaque(children)`.
        let editable = entry
            .tree
            .nodes
            .iter()
            .all(|node| !matches!(node, crate::token_table::TypedNode::Opaque { .. }));
        // The reference: the paragraph mark's rPr, then the first text run,
        // then the base style. The projection does not surface the mark, so a
        // paragraph WITH text uses its first run, which is the same answer.
        let insertion_style = text_segments(&entry.nodes)
            .iter()
            .find(|segment| !segment.text.is_empty())
            .map(|segment| segment.style_id.clone())
            .unwrap_or_else(|| base_style.clone());
        out.push(serde_json::json!({
            "id": paragraph_id,
            "base_style": base_style,
            "insertion_style": insertion_style,
            "skeleton": crate::token_table::skeleton(&entry.nodes),
            "token_ids": crate::token_table::token_ids(&entry.nodes),
            "section_bearing": entry.tree.section_bearing,
            "editable": editable,
            "mark_revision": match &entry.tree.mark {
                Some((kind, token_id, attrs)) => serde_json::json!({
                    "kind": kind,
                    "token_id": token_id,
                    "attrs": attrs,
                }),
                None => serde_json::Value::Null,
            },
            "original_index": index,
            "part_key": part_key,
            // The note a note paragraph belongs to; "" everywhere else.
            "part_entry_id": entry.tree.part_entry_id,
        }));
    }
    out
}

/// Read the restyle sidecar of a workdir (missing file = no edits).
pub fn load_rpr_edits(root: &Path) -> Result<Vec<RprEdit>, CoreError> {
    let path = root.join(RPR_EDITS_FILE);
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let bytes = std::fs::read(&path).map_err(CoreError::io)?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| CoreError::Domain(format!("invalid rpr_edits.json: {error}")))?;
    if value.get("schema").and_then(|v| v.as_str()) != Some(RPR_EDITS_SCHEMA) {
        return Err(CoreError::Domain(format!(
            "incompatible rpr_edits.json schema: {}",
            value
                .get("schema")
                .and_then(|v| v.as_str())
                .unwrap_or("missing")
        )));
    }
    let edits = value
        .get("edits")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            CoreError::Domain("invalid rpr_edits.json: missing edits array".to_string())
        })?;
    let mut out = Vec::with_capacity(edits.len());
    for edit in edits {
        let parsed: RprEdit = serde_json::from_value(edit.clone())
            .map_err(|error| CoreError::Domain(format!("invalid rpr_edits.json edit: {error}")))?;
        out.push(parsed);
    }
    Ok(out)
}

/// Plan one `format_span`: resolve the addressed text to the runs it covers,
/// derive the style, and return the record to append.
///
/// Refusals, and why each is refused rather than guessed:
///
/// - `paragraph-not-found` / `text-not-found` / `text-ambiguous` — the same
///   selector contract the island lane uses, so one selector cannot mean two
///   things in two lanes.
/// - `format-noop` / `format-invalid-attribute` / `format-style-unavailable` —
///   from the primitive, which is where the "never invent a style variable"
///   rule lives.
/// - `format-span-crosses-paragraphs` — a span that leaves its paragraph is a
///   different edit than the caller asked for.
pub fn plan_format_span(
    root: &Path,
    paragraph_id: &str,
    old: &str,
    attributes: &serde_json::Map<String, serde_json::Value>,
    style_id_override: Option<&str>,
) -> Result<RprEdit, CoreError> {
    let template = root.join("_template.docx");
    let package = std::fs::read(&template).map_err(CoreError::io)?;
    let inventory = enumerate_package_bytes(&package)?;
    let leaves: Vec<&ProseLeaf> = inventory
        .leaves
        .iter()
        .filter(|leaf| leaf.paragraph_id == paragraph_id && leaf.editable)
        .collect();
    if leaves.is_empty() {
        return Err(CoreError::Domain(format!(
            "paragraph-not-found: paragraph {paragraph_id} is not in the document"
        )));
    }
    let paragraph_text: String = leaves
        .iter()
        .filter(|leaf| leaf.text_start != leaf.text_end)
        .map(|leaf| leaf.text.clone())
        .collect();
    let occurrences = paragraph_text.matches(old).count();
    if old.is_empty() || occurrences == 0 {
        return Err(CoreError::Domain(format!(
            "text-not-found: {paragraph_id}: text {old:?} not found in the paragraph"
        )));
    }
    if occurrences > 1 {
        return Err(CoreError::Domain(format!(
            "text-ambiguous: {paragraph_id}: text {old:?} appears {occurrences} times; use a longer unique span"
        )));
    }
    let start = paragraph_text.find(old).unwrap_or(0);
    let end = start + old.chars().count();

    // The runs the span covers. Character offsets are resolved against the
    // paragraph's visible text, the surface the read side publishes.
    let mut cursor = 0usize;
    let mut covered: Vec<&ProseLeaf> = Vec::new();
    for leaf in &leaves {
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
    if covered.is_empty() {
        return Err(CoreError::Domain(format!(
            "text-not-found: {paragraph_id}: {old:?} is not editable text"
        )));
    }

    // The reference derives ONE style for the span from the FIRST covered run's
    // properties, not one per run. Recorded because this spec originally said
    // the opposite and the correction matters to anyone reimplementing it.
    let base = covered[0].rpr.clone();
    let available = load_style_rprs_for(root)?;
    let style_id = match style_id_override {
        Some(requested) => {
            if !available.contains_key(requested) {
                return Err(CoreError::Domain(format!(
                    "format-style-unavailable: {requested} is not in styles.json"
                )));
            }
            requested.to_string()
        }
        None => {
            restyle_run_properties(&base, attributes, &available.keys().cloned().collect())?
                .style_id
        }
    };
    Ok(RprEdit {
        part: covered[0].part_key.clone(),
        paragraph_id: paragraph_id.to_string(),
        text_start: start,
        text_end: end,
        attributes: serde_json::Value::Object(attributes.clone()),
        style_id,
    })
}

/// Append a restyle to the workdir's sidecar (idempotent per paragraph+span:
/// a repeat of the same record does not duplicate).
pub fn record_format_span(root: &Path, edit: RprEdit) -> Result<usize, CoreError> {
    let mut edits = load_rpr_edits(root)?;
    let duplicate = edits.iter().any(|existing| {
        existing.part == edit.part
            && existing.paragraph_id == edit.paragraph_id
            && existing.text_start == edit.text_start
            && existing.text_end == edit.text_end
    });
    if !duplicate {
        edits.push(edit);
    }
    save_rpr_edits(root, &edits)?;
    Ok(edits.len())
}

/// `style_id -> w:rPr` for a workdir, the same map the build applies.
pub fn load_style_rprs_for(root: &Path) -> Result<BTreeMap<String, String>, CoreError> {
    let path = root.join("styles.json");
    if !path.is_file() {
        return Ok(BTreeMap::new());
    }
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).map_err(CoreError::io)?)
            .map_err(|error| CoreError::Domain(format!("invalid styles.json: {error}")))?;
    let mut out = BTreeMap::new();
    if let Some(styles) = value.get("styles").and_then(|v| v.as_object()) {
        for (style_id, entry) in styles {
            if let Some(rpr) = entry.get("rPr").and_then(|v| v.as_str()) {
                out.insert(style_id.clone(), rpr.to_string());
            }
        }
    }
    Ok(out)
}

/// Persist the restyle sidecar. An empty record REMOVES the file, matching the
/// structure sidecar, so "no formatting changes" has one representation.
pub fn save_rpr_edits(root: &Path, edits: &[RprEdit]) -> Result<(), CoreError> {
    let path = root.join(RPR_EDITS_FILE);
    if edits.is_empty() {
        if path.is_file() {
            std::fs::remove_file(&path).map_err(CoreError::io)?;
        }
        return Ok(());
    }
    let value = serde_json::json!({
        "schema": RPR_EDITS_SCHEMA,
        "edits": edits,
    });
    let mut json = serde_json::to_string_pretty(&value).expect("rpr edits serialize");
    json.push('\n');
    std::fs::write(&path, json).map_err(CoreError::io)
}

/// Apply recorded restyles to a package, in place.
///
/// Mirrors `apply_edits`' shape: read the canonical inventory once, resolve
/// each edit to the leaves its span covers, collect byte spans, then splice
/// them right-to-left so earlier offsets stay valid.
///
/// A run with no `w:rPr` gets one inserted at the run's open tag -- the
/// reference always writes the derived properties, even onto a run that
/// carried none.
///
/// Returns the new package and the paragraph ids it could NOT apply. The
/// caller MUST report those: a restyle whose paragraph a later structure edit
/// removed is stale, and dropping it silently is the defect class #26 named.
pub fn apply_rpr_edits(
    package: &[u8],
    edits: &[RprEdit],
    styles: &BTreeMap<String, String>,
) -> Result<(Vec<u8>, Vec<String>), CoreError> {
    if edits.is_empty() {
        return Ok((package.to_vec(), Vec::new()));
    }
    let mut by_part: BTreeMap<String, Vec<&RprEdit>> = BTreeMap::new();
    for edit in edits {
        by_part.entry(edit.part.clone()).or_default().push(edit);
    }
    let mut stale: Vec<String> = Vec::new();
    let mut current = package.to_vec();
    for (part, part_edits) in &by_part {
        let inventory = enumerate_package_bytes(&current)?;
        let mut spans: Vec<(usize, usize, Vec<u8>)> = Vec::new();
        for edit in part_edits {
            let Some(style_rpr) = styles.get(&edit.style_id) else {
                return Err(CoreError::Domain(format!(
                    "format-style-unavailable: rpr_edits.json names {} which styles.json does not hold",
                    edit.style_id
                )));
            };
            // The paragraph's visible text, exactly the surface the read side
            // publishes, so a span copied from it resolves here too.
            let paragraph_leaves: Vec<&ProseLeaf> = inventory
                .leaves
                .iter()
                .filter(|leaf| leaf.part_key == *part && leaf.paragraph_id == edit.paragraph_id)
                .collect();
            let paragraph_text: String = paragraph_leaves
                .iter()
                .filter(|leaf| leaf.text_start != leaf.text_end)
                .map(|leaf| leaf.text.clone())
                .collect();
            let length = paragraph_text.chars().count();
            if edit.text_end > length || edit.text_start >= edit.text_end {
                stale.push(edit.paragraph_id.clone());
                continue;
            }
            let mut cursor = 0usize;
            let mut targets: Vec<&ProseLeaf> = Vec::new();
            for leaf in &paragraph_leaves {
                if leaf.text_start == leaf.text_end {
                    continue;
                }
                let leaf_length = leaf.text.chars().count();
                let (leaf_start, leaf_end) = (cursor, cursor + leaf_length);
                cursor = leaf_end;
                if leaf_start < edit.text_end && leaf_end > edit.text_start {
                    targets.push(leaf);
                }
            }
            if targets.is_empty() {
                stale.push(edit.paragraph_id.clone());
                continue;
            }
            for leaf in targets {
                let (start, end) = if leaf.rpr_start == leaf.rpr_end {
                    // No rPr on the run: insert one immediately after the
                    // run's open tag, where an rPr belongs.
                    (leaf.run_open_end, leaf.run_open_end)
                } else {
                    (leaf.rpr_start, leaf.rpr_end)
                };
                spans.push((start, end, style_rpr.as_bytes().to_vec()));
            }
        }
        if spans.is_empty() {
            continue;
        }
        let part_bytes = read_part_bytes(&current, &part_path(part))?;
        spans.sort_by_key(|(start, _, _)| *start);
        let mut new_part = part_bytes.clone();
        for (start, end, replacement) in spans.into_iter().rev() {
            if end > new_part.len() || start > end {
                stale.push(part.clone());
                continue;
            }
            new_part.splice(start..end, replacement);
        }
        current = patch_zip_member(&current, &part_path(part), &new_part)?;
    }
    Ok((current, stale))
}

/// Read the island-edit sidecar of a workdir (missing file = no edits).
pub fn load_islands(root: &Path) -> Result<Vec<IslandEdit>, CoreError> {
    let path = root.join(ISLANDS_FILE);
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let bytes = std::fs::read(&path).map_err(CoreError::io)?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| CoreError::Domain(format!("invalid islands.json: {error}")))?;
    if value.get("schema").and_then(|v| v.as_str()) != Some(ISLANDS_SCHEMA) {
        return Err(CoreError::Domain(format!(
            "incompatible islands.json schema: {}",
            value
                .get("schema")
                .and_then(|v| v.as_str())
                .unwrap_or("missing")
        )));
    }
    let edits = value
        .get("edits")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            CoreError::Domain("invalid islands.json: missing edits array".to_string())
        })?;
    let mut out = Vec::with_capacity(edits.len());
    for edit in edits {
        let parsed: IslandEdit = serde_json::from_value(edit.clone())
            .map_err(|error| CoreError::Domain(format!("invalid islands.json edit: {error}")))?;
        if !parsed.old.is_empty() {
            out.push(parsed);
        }
    }
    Ok(out)
}

/// Persist the island-edit sidecar (indent-2 JSON + trailing newline,
/// matching the other workdir sidecars).
pub fn save_islands(root: &Path, edits: &[IslandEdit]) -> Result<(), CoreError> {
    let value = serde_json::json!({
        "schema": ISLANDS_SCHEMA,
        "edits": edits,
    });
    let mut json = serde_json::to_string_pretty(&value).expect("islands serialize");
    json.push('\n');
    std::fs::write(root.join(ISLANDS_FILE), json).map_err(CoreError::io)
}

/// Validate every recorded island edit against the workdir template
/// (the build-time invariant gate: leaf editable, old text proven, opaque
/// containment, per-part XML well-formedness). Any failure rejects the
/// whole build with no output.
pub fn validate_islands(template: &Path, edits: &[IslandEdit]) -> Result<(), CoreError> {
    if edits.is_empty() {
        return Ok(());
    }
    validate_package(template)?;
    for edit in edits {
        let _ = validate_edit(template, edit)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Structural edits (new / deleted paragraphs)
// ---------------------------------------------------------------------------
//
// An island edit replaces bytes inside an EXISTING leaf; a new paragraph has
// no baseline byte range at all (`base=""`), so it needs a synthesis path
// instead. Mirror of the Python reference (`typed_docx.py`
// `_render_paragraph` / `_new_paragraph_opening` / `_paragraph_placements`):
// the new `w:p` copies the inherit paragraph's opening tag (ephemeral
// attributes stripped) and its `<w:pPr>` verbatim, renders its own run, and
// is spliced before the next surviving baseline paragraph (or appended when
// none follows). A deleted paragraph is simply not rendered.

pub const STRUCTURE_SCHEMA: &str = "docx2typed-structure-1";
pub const STRUCTURE_FILE: &str = "structure.json";

/// Paragraph opening attributes that are per-extraction ephemeral: the
/// synthesised paragraph must not inherit another paragraph's revision or
/// identity markers (`typed_docx.py` `_EPHEMERAL_P_ATTRS`).
const EPHEMERAL_P_ATTRS: [&str; 7] = [
    "paraId",
    "textId",
    "rsidP",
    "rsidR",
    "rsidRDefault",
    "rsidDel",
    "rsidRPr",
];

/// One synthesised paragraph: rendered from the `inherit` baseline
/// paragraph and spliced before `insert_before` (None = append).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct NewParagraph {
    /// Part key ("document"); container parts are out of scope.
    pub part: String,
    /// Typed paragraph id — the draft temp id is kept (`N1`), never
    /// reallocated to a `P` slot (mirror of `original_index = -1`).
    pub paragraph_id: String,
    /// Baseline paragraph whose opening tag, `pPr`, and run properties are
    /// copied.
    pub inherit: String,
    /// Plain-text body (XML-escaped on write).
    pub body: String,
    /// Baseline id the new paragraph is spliced before; None = append.
    #[serde(default)]
    pub insert_before: Option<String>,
}

/// One removed baseline paragraph: not rendered, so its `w:p` disappears.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeletedParagraph {
    pub part: String,
    pub paragraph_id: String,
}

/// The structural-edit sidecar of one workdir.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct StructureEdits {
    #[serde(default)]
    pub new_paragraphs: Vec<NewParagraph>,
    #[serde(default)]
    pub deleted_paragraphs: Vec<DeletedParagraph>,
}

impl StructureEdits {
    pub fn is_empty(&self) -> bool {
        self.new_paragraphs.is_empty() && self.deleted_paragraphs.is_empty()
    }
}

/// Read the structural-edit sidecar of a workdir (missing file = none).
pub fn load_structure(root: &Path) -> Result<StructureEdits, CoreError> {
    let path = root.join(STRUCTURE_FILE);
    if !path.is_file() {
        return Ok(StructureEdits::default());
    }
    let bytes = std::fs::read(&path).map_err(CoreError::io)?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| CoreError::Domain(format!("invalid structure.json: {error}")))?;
    if value.get("schema").and_then(|v| v.as_str()) != Some(STRUCTURE_SCHEMA) {
        return Err(CoreError::Domain(format!(
            "incompatible structure.json schema: {}",
            value
                .get("schema")
                .and_then(|v| v.as_str())
                .unwrap_or("missing")
        )));
    }
    serde_json::from_value(value)
        .map_err(|error| CoreError::Domain(format!("invalid structure.json: {error}")))
}

/// Persist the structural-edit sidecar (indent-2 JSON + trailing newline,
/// matching the other workdir sidecars). A sidecar that records nothing is
/// removed instead of written: "missing file" and "no structural edits"
/// must not be two different states.
pub fn save_structure(root: &Path, edits: &StructureEdits) -> Result<(), CoreError> {
    let path = root.join(STRUCTURE_FILE);
    if edits.is_empty() {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(CoreError::io(error)),
        };
    }
    let mut value = serde_json::to_value(edits).expect("structure serialize");
    value["schema"] = serde_json::Value::String(STRUCTURE_SCHEMA.to_string());
    let mut json = serde_json::to_string_pretty(&value).expect("structure serialize");
    json.push('\n');
    std::fs::write(path, json).map_err(CoreError::io)
}

/// The protected-structure reason for one template paragraph, or None when
/// it is plain prose. Absent ids are also None (callers own existence
/// checks). Mirror of `typed_docx.py` `section_bearing or
/// _contains_structural(nodes)`.
pub fn paragraph_protection(
    workdir: &Path,
    paragraph_id: &str,
) -> Result<Option<String>, CoreError> {
    let package = std::fs::read(workdir.join("_template.docx")).map_err(CoreError::io)?;
    let part = read_part_bytes(&package, &part_path("document"))?;
    let located = scan_document(&part)?;
    let Some((start, end)) = located
        .iter()
        .find(|(id, _, _)| id == paragraph_id)
        .map(|(_, start, end)| (*start, *end))
    else {
        return Ok(None);
    };
    let inventory = enumerate_package_bytes(&package)?;
    Ok(structure_reason(
        &part,
        &inventory,
        paragraph_id,
        start,
        end,
    ))
}

/// Why a paragraph's own structure is immutable, or None when it is plain
/// prose. Section properties, tracked-revision containers, anchors, nested
/// tables, and opaque interiors (fields/math/drawings/proofErr) all refuse.
fn structure_reason(
    part: &[u8],
    inventory: &ProseInventory,
    paragraph_id: &str,
    start: usize,
    end: usize,
) -> Option<String> {
    for tag in scan_tags(&part[start..end]) {
        match tag.name.as_str() {
            "sectPr" => return Some("section properties".to_string()),
            "ins" | "del" | "moveFrom" | "moveTo" => return Some("tracked revisions".to_string()),
            "bookmarkStart" | "bookmarkEnd" | "commentRangeStart" | "commentRangeEnd" => {
                return Some("anchors".to_string())
            }
            "tbl" => return Some("nested tables".to_string()),
            _ => {}
        }
    }
    if inventory
        .opaques
        .iter()
        .any(|opaque| opaque.paragraph_id == paragraph_id)
    {
        return Some("structural content".to_string());
    }
    None
}

fn container_paragraph_error(paragraph_id: &str) -> CoreError {
    CoreError::Domain(format!(
        "table-structure-immutable: paragraphs cannot be added to or removed from tables, \
         text boxes, or header/footer/note parts ({paragraph_id}); container structure \
         operations are out of scope"
    ))
}

/// Validate every recorded structural edit against the workdir template:
/// the inherit/deleted paragraph must be an existing body paragraph with no
/// protected structure. Any failure rejects the whole build with no output.
pub fn validate_structure(template: &Path, edits: &StructureEdits) -> Result<(), CoreError> {
    if edits.is_empty() {
        return Ok(());
    }
    validate_package(template)?;
    let package = std::fs::read(template).map_err(CoreError::io)?;
    let part = read_part_bytes(&package, &part_path("document"))?;
    let located = scan_document(&part)?;
    let inventory = enumerate_package_bytes(&package)?;
    let range_of = |id: &str| {
        located
            .iter()
            .find(|(paragraph_id, _, _)| paragraph_id == id)
            .map(|(_, start, end)| (*start, *end))
    };
    for paragraph in &edits.new_paragraphs {
        if paragraph.part != "document" {
            return Err(container_paragraph_error(&paragraph.paragraph_id));
        }
        let Some((start, end)) = range_of(&paragraph.inherit) else {
            return Err(CoreError::Domain(format!(
                "invalid-structure: new paragraph requires an existing inherit paragraph: \
                 {} (inherits {})",
                paragraph.paragraph_id, paragraph.inherit
            )));
        };
        if !paragraph.inherit.starts_with('P') {
            return Err(container_paragraph_error(&paragraph.inherit));
        }
        if let Some(reason) = structure_reason(&part, &inventory, &paragraph.inherit, start, end) {
            return Err(CoreError::Domain(format!(
                "invalid-structure: new paragraph cannot inherit protected structure: {} \
                 (inherits {}: {reason})",
                paragraph.paragraph_id, paragraph.inherit
            )));
        }
        if let Some(anchor) = &paragraph.insert_before {
            if range_of(anchor).is_none() {
                return Err(CoreError::Domain(format!(
                    "invalid-structure: unknown insert_before paragraph: {anchor}"
                )));
            }
        }
    }
    for paragraph in &edits.deleted_paragraphs {
        if paragraph.part != "document" {
            return Err(container_paragraph_error(&paragraph.paragraph_id));
        }
        let Some((start, end)) = range_of(&paragraph.paragraph_id) else {
            return Err(CoreError::Domain(format!(
                "invalid-structure: deleted paragraph not found: {}",
                paragraph.paragraph_id
            )));
        };
        if !paragraph.paragraph_id.starts_with('P') {
            return Err(container_paragraph_error(&paragraph.paragraph_id));
        }
        if let Some(reason) =
            structure_reason(&part, &inventory, &paragraph.paragraph_id, start, end)
        {
            return Err(CoreError::Domain(format!(
                "invalid-structure: paragraph with protected structure cannot be deleted: {} \
                 ({reason})",
                paragraph.paragraph_id
            )));
        }
    }
    Ok(())
}

/// The byte offset a new paragraph with no following baseline paragraph is
/// appended at: the body-level `w:sectPr` start, else just before
/// `</w:body>` (mirror of Python's `template_count` slot, which lands after
/// every body-level unit and before the section properties).
fn body_append_offset(xml: &[u8]) -> Result<usize, CoreError> {
    fn find(nodes: &[ElementNode], index: usize, name: &str) -> Option<usize> {
        if nodes[index].name == name {
            return Some(index);
        }
        nodes[index]
            .children
            .iter()
            .find_map(|&child| find(nodes, child, name))
    }
    let tags = scan_tags(xml);
    let nodes = build_tree(&tags)?;
    let body = (0..nodes.len())
        .find_map(|root| find(&nodes, root, "body"))
        .ok_or_else(|| CoreError::Domain("document XML has no body element".to_string()))?;
    for &child in &nodes[body].children {
        if nodes[child].name == "sectPr" {
            return Ok(nodes[child].open_start);
        }
    }
    Ok(nodes[body].close_start)
}

/// Render one synthesised `w:p`: the inherit paragraph's opening tag with
/// ephemeral attributes stripped, its `pPr` verbatim, and one run carrying
/// the inherit paragraph's run properties (`typed_docx.py`
/// `_render_paragraph` + `_new_paragraph_opening`).
fn render_new_paragraph(
    part: &[u8],
    inventory: &ProseInventory,
    located: &[(String, usize, usize)],
    paragraph: &NewParagraph,
) -> Result<Vec<u8>, CoreError> {
    let (start, end) = located
        .iter()
        .find(|(id, _, _)| id == &paragraph.inherit)
        .map(|(_, start, end)| (*start, *end))
        .ok_or_else(|| {
            CoreError::Domain(format!(
                "invalid-structure: new paragraph requires an existing inherit paragraph: \
                 {} (inherits {})",
                paragraph.paragraph_id, paragraph.inherit
            ))
        })?;
    let inherit = &part[start..end];
    let tags = scan_tags(inherit);
    let nodes = build_tree(&tags)?;
    let root = nodes
        .first()
        .ok_or_else(|| CoreError::Domain("malformed paragraph XML".to_string()))?;
    let mut opening = inherit[..root.open_end].to_vec();
    if root.self_closing {
        // `<w:p .../>` cannot be reopened: drop the `/` to keep the tag open.
        opening.pop();
    }
    let ppr = root
        .children
        .iter()
        .find(|&&child| nodes[child].name == "pPr")
        .map(|&child| &inherit[nodes[child].open_start..nodes[child].end])
        .unwrap_or_default();
    let rpr = inventory
        .leaves
        .iter()
        .find(|leaf| leaf.paragraph_id == paragraph.inherit && leaf.leaf_index == 0)
        .map(|leaf| leaf.rpr.as_str())
        .unwrap_or("");
    let mut out = strip_ephemeral_p_attrs(&opening);
    out.extend_from_slice(ppr);
    if !paragraph.body.is_empty() {
        out.extend_from_slice(b"<w:r>");
        out.extend_from_slice(rpr.as_bytes());
        let space = if needs_xml_space(&paragraph.body) {
            " xml:space=\"preserve\""
        } else {
            ""
        };
        out.extend_from_slice(format!("<w:t{space}>").as_bytes());
        out.extend_from_slice(xml_escape(&paragraph.body).as_bytes());
        out.extend_from_slice(b"</w:t></w:r>");
    }
    out.extend_from_slice(b"</w:p>");
    Ok(out)
}

fn attr_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-')
}

/// Match one `\s+name\s*=\s*"value"` attribute at `start`, returning its end
/// offset and qname (mirror of `_P_ATTR_RE`; unquoted values do not match).
fn match_p_attr(bytes: &[u8], start: usize) -> Option<(usize, String)> {
    let mut index = start;
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    if index == start {
        return None;
    }
    let name_start = index;
    if !matches!(bytes.get(index), Some(byte) if byte.is_ascii_alphabetic() || *byte == b'_') {
        return None;
    }
    index += 1;
    while index < bytes.len() && attr_name_byte(bytes[index]) {
        index += 1;
    }
    if bytes.get(index) == Some(&b':') {
        index += 1;
        if !matches!(bytes.get(index), Some(byte) if byte.is_ascii_alphabetic() || *byte == b'_') {
            return None;
        }
        index += 1;
        while index < bytes.len() && attr_name_byte(bytes[index]) {
            index += 1;
        }
    }
    let name = String::from_utf8_lossy(&bytes[name_start..index]).into_owned();
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    if bytes.get(index) != Some(&b'=') {
        return None;
    }
    index += 1;
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    let quote = *bytes.get(index)?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    index += 1;
    while index < bytes.len() && bytes[index] != quote {
        index += 1;
    }
    if bytes.get(index) != Some(&quote) {
        return None;
    }
    index += 1;
    Some((index, name))
}

/// Copy one paragraph opening tag, dropping the ephemeral attributes
/// (mirror of `_new_paragraph_opening`).
fn strip_ephemeral_p_attrs(opening: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(opening.len());
    let mut index = 0usize;
    while index < opening.len() {
        if opening[index].is_ascii_whitespace() {
            if let Some((end, name)) = match_p_attr(opening, index) {
                let local = name.rsplit(':').next().unwrap_or(&name);
                if EPHEMERAL_P_ATTRS.contains(&local) {
                    index = end;
                    continue;
                }
                out.extend_from_slice(&opening[index..end]);
                index = end;
                continue;
            }
        }
        out.push(opening[index]);
        index += 1;
    }
    out
}

/// Splice the structural edits into one document part: deleted paragraphs
/// are dropped, synthesised paragraphs land before the next surviving
/// baseline paragraph (or at the append offset).
fn splice_structure(
    part: &[u8],
    inventory: &ProseInventory,
    edits: &StructureEdits,
) -> Result<Vec<u8>, CoreError> {
    let located = scan_document(part)?;
    let index_of: BTreeMap<&str, usize> = located
        .iter()
        .enumerate()
        .map(|(index, (id, _, _))| (id.as_str(), index))
        .collect();
    let deleted: BTreeSet<&str> = edits
        .deleted_paragraphs
        .iter()
        .map(|paragraph| paragraph.paragraph_id.as_str())
        .collect();
    let append_at = body_append_offset(part)?;
    let mut spans: Vec<(usize, usize, Vec<u8>)> = Vec::new();
    for paragraph in &edits.deleted_paragraphs {
        let index = index_of[paragraph.paragraph_id.as_str()];
        let (_, start, end) = &located[index];
        spans.push((*start, *end, Vec::new()));
    }
    for paragraph in &edits.new_paragraphs {
        let at = match &paragraph.insert_before {
            None => append_at,
            Some(anchor) => {
                // The anchor may itself have been deleted since the insert
                // was committed: fall forward to the next surviving one.
                let mut target = None;
                let mut index = index_of.get(anchor.as_str()).copied();
                while let Some(current) = index {
                    let (id, start, _) = &located[current];
                    if !deleted.contains(id.as_str()) {
                        target = Some(*start);
                        break;
                    }
                    index = Some(current + 1);
                }
                target.unwrap_or(append_at)
            }
        };
        spans.push((
            at,
            at,
            render_new_paragraph(part, inventory, &located, paragraph)?,
        ));
    }
    spans.sort_by_key(|(start, end, _)| (*start, *end));
    let mut out = Vec::with_capacity(part.len());
    let mut cursor = 0usize;
    for (start, end, replacement) in spans {
        if start < cursor || end < start {
            return Err(CoreError::Domain(
                "invalid-structure: overlapping paragraph splices".to_string(),
            ));
        }
        out.extend_from_slice(&part[cursor..start]);
        out.extend_from_slice(&replacement);
        cursor = end;
    }
    out.extend_from_slice(&part[cursor..]);
    Ok(out)
}

/// The full edited build: island leaf surgery first, then the structural
/// synthesis over the result. Both sides validate against the template
/// before any byte moves.
pub fn apply_build(
    template: &Path,
    islands: &[IslandEdit],
    structure: &StructureEdits,
    rpr_edits: &[RprEdit],
    styles: &BTreeMap<String, String>,
) -> Result<(Vec<u8>, Vec<String>), CoreError> {
    let package = apply_edits(template, islands)?;
    // Structural synthesis first: it decides which paragraphs exist, and a
    // restyle addresses paragraphs. Then the property lane over the result.
    let package = if structure.is_empty() {
        package
    } else {
        validate_structure(template, structure)?;
        let part = read_part_bytes(&package, &part_path("document"))?;
        let inventory = enumerate_package_bytes(&package)?;
        let new_part = splice_structure(&part, &inventory, structure)?;
        patch_zip_member(&package, &part_path("document"), &new_part)?
    };
    // A restyle whose paragraph a structure edit removed is reported, never
    // dropped silently and never resurrected (#26's class).
    apply_rpr_edits(&package, rpr_edits, styles)
}

// ---------------------------------------------------------------------------
// Character formatting: the `format_span` run-property lane
// ---------------------------------------------------------------------------

/// The main OOXML namespace (`w:`).
pub const WORD_NAMESPACE: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";

/// `CT_RPr` child sequence (ECMA-376 §17.3.2.28) — the single source of truth
/// for the order of `w:rPr` children.
///
/// Word rejects an `rPr` whose children are out of sequence. Python's first
/// `format_span` port shipped exactly that defect (fix #98): it *appended* the
/// synthesized `w:vertAlign`, which is schema-invalid whenever the base run
/// properties already carried a later element (`w:lang`, `w:rtl`,
/// `w:rPrChange`, ...). Every insertion in this lane goes through
/// [`insert_rpr_child`], which places the element in this sequence and never
/// appends.
pub const RPR_ORDER: [&str; 40] = [
    "rStyle",
    "rFonts",
    "b",
    "bCs",
    "i",
    "iCs",
    "caps",
    "smallCaps",
    "strike",
    "dstrike",
    "outline",
    "shadow",
    "emboss",
    "imprint",
    "noProof",
    "snapToGrid",
    "vanish",
    "webHidden",
    "color",
    "spacing",
    "w",
    "kern",
    "position",
    "sz",
    "szCs",
    "highlight",
    "u",
    "effect",
    "bdr",
    "shd",
    "fitText",
    "vertAlign",
    "rtl",
    "cs",
    "em",
    "lang",
    "eastAsianLayout",
    "specVanish",
    "oMath",
    "rPrChange",
];

/// Sort key of one `w:rPr` child; unknown properties sort last.
pub fn rpr_schema_position(local: &str) -> usize {
    RPR_ORDER
        .iter()
        .position(|name| *name == local)
        .unwrap_or(RPR_ORDER.len())
}

/// The outcome of one character restyle: the new run properties, their
/// canonical bytes, and the content-addressed character style they register
/// under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunRestyle {
    /// The restyled `w:rPr` element, ready to replace the run's own.
    pub rpr: String,
    /// Canonical bytes of `rpr` (`w:rPrChange` dropped) — the style identity.
    pub canonical_rpr: String,
    /// The `s_<sha256[:16]>` id of `canonical_rpr`.
    pub style_id: String,
}

impl RunRestyle {
    /// Register the derived character style under its id: `style_id` -> `rPr`.
    /// Content-addressed ids make this idempotent — an existing entry is left
    /// untouched, and the id is returned either way.
    pub fn register(&self, styles: &mut BTreeMap<String, String>) -> String {
        styles
            .entry(self.style_id.clone())
            .or_insert_with(|| self.rpr.clone());
        self.style_id.clone()
    }
}

/// One `<w:rPr>` fragment taken apart for byte-level surgery.
struct RprFragment {
    bytes: Vec<u8>,
    nodes: Vec<ElementNode>,
    root: ElementNode,
    children: Vec<ElementNode>,
}

/// The index of the single root element of one parsed fragment.
fn root_index(nodes: &[ElementNode]) -> Result<usize, CoreError> {
    let roots: Vec<usize> = nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| node.parent.is_none())
        .map(|(index, _)| index)
        .collect();
    match roots.as_slice() {
        [index] => Ok(*index),
        _ => Err(CoreError::Domain(
            "invalid-xml: the fragment must hold exactly one element".to_string(),
        )),
    }
}

/// The attribute pairs of one element's opening tag, as (local name, value)
/// in document order.
/// The `styles.json` payload for a document: one entry per distinct run
/// property set it carries, plus the seeded `Normal`.
///
/// This is the registry the format lane reads, and its ABSENCE is why
/// `format_span` was inert (#48): `restyle_run_properties` refuses
/// `format-style-unavailable` unless the derived id is already here, and the
/// extract path wrote this file empty on every document.
///
/// Mirrors the reference's population exactly: `_parse_run` calls
/// `registry.ensure` for every run's rPr (a run with none contributes the
/// empty rPr), and the document seeds `Normal` before the paragraph walk.
pub fn build_style_registry(catalog: &StyleCatalog) -> Result<serde_json::Value, CoreError> {
    let mut entries: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    for (style_id, (rpr_xml, canonical)) in &catalog.entries {
        let (label, features) = crate::document_projection::rpr_semantics_for(rpr_xml)?;
        let features_value: BTreeMap<String, serde_json::Value> = features
            .into_iter()
            .map(|(k, v)| {
                if BOOLEAN_RPR.contains(&k.as_str()) {
                    (k, serde_json::Value::Bool(v == "true"))
                } else {
                    (k, serde_json::Value::String(v))
                }
            })
            .collect();
        entries.insert(
            style_id.clone(),
            serde_json::json!({
                "rPr": strip_rpr_change(rpr_xml),
                "canonical": canonical,
                "label": label,
                "features": features_value,
            }),
        );
    }
    Ok(serde_json::json!({
        "schema": crate::STYLES_SCHEMA,
        "canonicalizer_version": crate::CANONICALIZER_VERSION,
        "styles": entries,
    }))
}

/// One `(style_id, rPr, canonical)` per distinct run property set the document
/// carries, the seeded `Normal` first.
///
/// The one enumeration both `styles.json` and the vertical-promotion predicate
/// read: the predicate has to canonicalise a style's rPr minus one
/// `w:vertAlign`, and doing that from a second, independently-built table is
/// how the two would drift.
fn style_fragments(package: &[u8]) -> Result<Vec<(String, String, String)>, CoreError> {
    let inventory = enumerate_package_bytes(package)?;
    let mut out: Vec<(String, String, String)> = Vec::new();
    let mut seen: BTreeMap<String, ()> = BTreeMap::new();
    let mut fragments: Vec<String> = vec![String::new()]; // the seeded Normal
    for leaf in &inventory.leaves {
        fragments.push(leaf.rpr.clone());
    }
    for fragment in fragments {
        let rpr_xml = if fragment.trim().is_empty() {
            format!(r#"<w:rPr xmlns:w="{WORD_NAMESPACE}"/>"#)
        } else {
            fragment
        };
        let canonical = canonical_rpr_json(&rpr_xml)?;
        let style_id = format!("s_{}", &bytes_sha256(canonical.as_bytes())[..16]);
        if seen.insert(style_id.clone(), ()).is_some() {
            continue;
        }
        out.push((style_id, rpr_xml, canonical));
    }
    Ok(out)
}

/// The style facts the vertical-promotion predicate needs: each style's rPr
/// and its canonical form, keyed by style id.
pub struct StyleCatalog {
    entries: BTreeMap<String, (String, String)>,
}

impl StyleCatalog {
    pub fn build(package: &[u8]) -> Result<Self, CoreError> {
        let mut entries = BTreeMap::new();
        for (style_id, rpr, canonical) in style_fragments(package)? {
            entries.insert(style_id, (rpr, canonical));
        }
        Ok(Self { entries })
    }

    fn rpr(&self, style_id: &str) -> Option<&str> {
        self.entries.get(style_id).map(|(rpr, _)| rpr.as_str())
    }

    fn canonical(&self, style_id: &str) -> Option<&str> {
        self.entries
            .get(style_id)
            .map(|(_, canonical)| canonical.as_str())
    }
}

/// The original rPr with `w:rPrChange` children removed: format history is
/// never duplicated into a style's rendered properties.
pub fn strip_rpr_change(rpr_xml: &str) -> String {
    let Ok(fragment) = RprFragment::parse(rpr_xml) else {
        return rpr_xml.to_string();
    };
    let mut open =
        String::from_utf8_lossy(&fragment.bytes[fragment.root.open_start..fragment.root.open_end])
            .into_owned();
    // The reference re-serialises through ElementTree, which emits the
    // namespace declaration on the element and closes an empty element as
    // ` />`. These bytes land in styles.json verbatim, so both matter.
    if !open.contains("xmlns:w=") {
        let declaration = format!(r#" xmlns:w="{WORD_NAMESPACE}""#);
        open = match open.find('>') {
            Some(at) => {
                let (head, tail) = open.split_at(at);
                let head = head.trim_end_matches('/');
                format!("{head}{declaration}/{tail}")
            }
            None => open,
        };
    }
    let mut out = String::new();
    out.push_str(open.trim_end_matches('>').trim_end_matches('/'));
    if fragment
        .children
        .iter()
        .all(|child| child.name == "rPrChange")
    {
        out.push_str(" />");
        return out;
    }
    out.push('>');
    for child in &fragment.children {
        if child.name == "rPrChange" {
            continue;
        }
        let rendered = String::from_utf8_lossy(&fragment.bytes[child.open_start..child.end]);
        // ElementTree closes an empty element as ` />`; the source document
        // may write `/>`. These bytes land in styles.json verbatim.
        out.push_str(&rendered.replace("/>", " />"));
    }
    out.push_str("</w:rPr>");
    out
}

fn tag_attribute_pairs(tag: &[u8]) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut index = 1; // past '<'
    while index < tag.len() && !tag[index].is_ascii_whitespace() {
        if tag[index] == b'/' || tag[index] == b'>' {
            return pairs;
        }
        index += 1;
    }
    while index < tag.len() {
        while index < tag.len() && tag[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= tag.len() || tag[index] == b'/' || tag[index] == b'>' {
            break;
        }
        let name_start = index;
        while index < tag.len() && tag[index] != b'=' && !tag[index].is_ascii_whitespace() {
            index += 1;
        }
        let name = String::from_utf8_lossy(&tag[name_start..index]).into_owned();
        while index < tag.len() && tag[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= tag.len() || tag[index] != b'=' {
            break;
        }
        index += 1;
        while index < tag.len() && tag[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= tag.len() || (tag[index] != b'"' && tag[index] != b'\'') {
            break;
        }
        let quote = tag[index];
        index += 1;
        let value_start = index;
        while index < tag.len() && tag[index] != quote {
            index += 1;
        }
        let value = String::from_utf8_lossy(&tag[value_start..index]).into_owned();
        index += 1;
        pairs.push((crate::xml_walker::local_name(&name), value));
    }
    pairs
}

// ---------------------------------------------------------------------------
// Python-compatible rPr canonicalisation (style identity)
// ---------------------------------------------------------------------------

/// A canonical value, mirroring the shape Python's `_canonical_element`
/// returns: a tuple of (name, attributes, text, children), where attributes is
/// a tuple of pairs. Tuples serialize as JSON arrays.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Canon {
    Text(String),
    Seq(Vec<Canon>),
}

/// Python's `repr()` for the canonical tuple, which is the SORT KEY for rPr
/// children. Reproduced because the sort decides the canonical string, and the
/// canonical string decides every style id.
///
/// Python's string rules: single quotes unless the text holds a `'` and no
/// `"`, in which case double quotes; `\` and the active quote are escaped.
fn canon_python_repr(value: &Canon) -> String {
    match value {
        Canon::Text(text) => {
            let quote = if text.contains('\'') && !text.contains('"') {
                '"'
            } else {
                '\''
            };
            let mut out = String::with_capacity(text.len() + 2);
            out.push(quote);
            for character in text.chars() {
                match character {
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    c if c == quote => {
                        out.push('\\');
                        out.push(c);
                    }
                    c => out.push(c),
                }
            }
            out.push(quote);
            out
        }
        Canon::Seq(items) => {
            let mut out = String::from("(");
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                out.push_str(&canon_python_repr(item));
            }
            // Python marks a one-element tuple with a trailing comma.
            if items.len() == 1 {
                out.push(',');
            }
            out.push(')');
            out
        }
    }
}

fn canon_to_json(value: &Canon) -> serde_json::Value {
    match value {
        Canon::Text(text) => serde_json::Value::String(text.clone()),
        Canon::Seq(items) => serde_json::Value::Array(items.iter().map(canon_to_json).collect()),
    }
}

/// `_BOOLEAN_RPR`: elements whose `w:val` is a boolean, so an explicit true is
/// noise and an explicit false removes the property entirely.
const BOOLEAN_RPR: [&str; 9] = [
    "b",
    "i",
    "strike",
    "outline",
    "shadow",
    "smallCaps",
    "caps",
    "vanish",
    "webHidden",
];
const FALSE_VALUES: [&str; 4] = ["false", "0", "off", "none"];
const TRUE_VALUES: [&str; 3] = ["true", "1", "on"];

/// The literal tag name of one element (`w:rFonts` from `<w:rFonts ...>`).
fn element_qname(bytes: &[u8], node: &ElementNode) -> String {
    let tag = &bytes[node.open_start..node.open_end];
    let mut end = 1;
    while end < tag.len() && !tag[end].is_ascii_whitespace() && tag[end] != b'>' && tag[end] != b'/'
    {
        end += 1;
    }
    String::from_utf8_lossy(&tag[1..end]).into_owned()
}

fn local_of(qname: &str) -> &str {
    qname.rsplit(':').next().unwrap_or(qname)
}

/// Python's `_canonical_element`. `None` means the element is dropped (a
/// boolean property explicitly false).
fn canonical_element(
    bytes: &[u8],
    nodes: &[ElementNode],
    node: &ElementNode,
    rpr: bool,
) -> Option<Canon> {
    let name = element_qname(bytes, node);
    let local = local_of(&name).to_string();
    let mut attributes: Vec<(String, String)> = Vec::new();
    for (key, value) in canonical_attribute_pairs(&bytes[node.open_start..node.end]) {
        let key_local = local_of(&key).to_string();
        if key_local.starts_with("rsid") {
            continue;
        }
        if rpr && BOOLEAN_RPR.contains(&local.as_str()) && key_local == "val" {
            let lowered = value.to_ascii_lowercase();
            if FALSE_VALUES.contains(&lowered.as_str()) {
                return None;
            }
            if TRUE_VALUES.contains(&lowered.as_str()) {
                continue;
            }
        }
        attributes.push((key, value));
    }
    attributes.sort();

    let mut children: Vec<Canon> = Vec::new();
    for &child in &node.children {
        if let Some(canonical) = canonical_element(bytes, nodes, &nodes[child], rpr) {
            children.push(canonical);
        }
    }
    if rpr {
        // Python: `children.sort(key=repr)`. The canonical form is meant to be
        // order-insensitive, which is why this is a repr sort and not the
        // schema order the synthesis lane uses.
        children.sort_by_key(canon_python_repr);
    }
    Some(Canon::Seq(vec![
        Canon::Text(name),
        Canon::Seq(
            attributes
                .into_iter()
                .map(|(key, value)| Canon::Seq(vec![Canon::Text(key), Canon::Text(value)]))
                .collect(),
        ),
        // `element.text or ""` -- run properties carry no text.
        Canon::Text(String::new()),
        Canon::Seq(children),
    ]))
}

/// Python's `canonical_rpr`: the rPr with `w:rPrChange` dropped, canonicalised
/// and serialized with `separators=(",", ":")`.
pub fn canonical_rpr_json(rpr_xml: &str) -> Result<String, CoreError> {
    let fragment = RprFragment::parse(rpr_xml)?;
    let mut children: Vec<Canon> = Vec::new();
    for child in &fragment.children {
        if child.name == "rPrChange" {
            continue;
        }
        if let Some(canonical) = canonical_element(&fragment.bytes, &fragment.nodes, child, true) {
            children.push(canonical);
        }
    }
    children.sort_by_key(canon_python_repr);
    let mut root_attributes: Vec<(String, String)> =
        canonical_attribute_pairs(&fragment.bytes[fragment.root.open_start..fragment.root.end])
            .into_iter()
            .filter(|(key, _)| !local_of(key).starts_with("rsid"))
            .collect();
    root_attributes.sort();
    let root = Canon::Seq(vec![
        Canon::Text(element_qname(&fragment.bytes, &fragment.root)),
        Canon::Seq(
            root_attributes
                .into_iter()
                .map(|(key, value)| Canon::Seq(vec![Canon::Text(key), Canon::Text(value)]))
                .collect(),
        ),
        Canon::Text(String::new()),
        Canon::Seq(children),
    ]);
    Ok(canonical_json(&canon_to_json(&root)))
}

/// Python's `style_id_for_rpr`: `s_` + the first 16 hex of the canonical form.
pub fn style_id_for_rpr(rpr_xml: &str) -> Result<String, CoreError> {
    let canonical = canonical_rpr_json(rpr_xml)?;
    Ok(format!("s_{}", &bytes_sha256(canonical.as_bytes())[..16]))
}

/// Attributes as the canonical form needs them: **full** names, with namespace
/// declarations skipped.
///
/// `tag_attribute_pairs` strips the prefix (it exists for local-name
/// comparisons), but Python's canonical form records `w:val`, not `val` —
/// measured: using the stripped form produced `["w:sz",[["val","24"]]]` where
/// the reference produces `["w:sz",[["w:val","24"]]]`, and every style id
/// diverged. `xmlns` is skipped because ElementTree consumes namespace
/// declarations, so they never reach `element.attrib`.
pub(crate) fn canonical_attribute_pairs(tag: &[u8]) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut index = 1; // past '<'
    while index < tag.len() && !tag[index].is_ascii_whitespace() {
        if tag[index] == b'/' || tag[index] == b'>' {
            return pairs;
        }
        index += 1;
    }
    while index < tag.len() {
        while index < tag.len() && tag[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= tag.len() || tag[index] == b'/' || tag[index] == b'>' {
            break;
        }
        let name_start = index;
        while index < tag.len() && tag[index] != b'=' && !tag[index].is_ascii_whitespace() {
            index += 1;
        }
        let name = String::from_utf8_lossy(&tag[name_start..index]).into_owned();
        while index < tag.len() && tag[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= tag.len() || tag[index] != b'=' {
            break;
        }
        index += 1;
        while index < tag.len() && tag[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= tag.len() || (tag[index] != b'"' && tag[index] != b'\'') {
            break;
        }
        let quote = tag[index];
        index += 1;
        let value_start = index;
        while index < tag.len() && tag[index] != quote {
            index += 1;
        }
        let value = String::from_utf8_lossy(&tag[value_start..index]).into_owned();
        index += 1;
        let local = local_of(&name);
        if local == "xmlns" || name.starts_with("xmlns:") {
            continue;
        }
        pairs.push((name, value));
    }
    pairs
}

/// Serialize one empty run-property element under `prefix`.
fn render_empty_element(prefix: &str, local: &str, attributes: &[(&str, &str)]) -> String {
    let mut out = format!("<{prefix}{local}");
    for (name, value) in attributes {
        out.push_str(&format!(r#" {prefix}{name}="{}""#, xml_attr(value)));
    }
    out.push_str("/>");
    out
}

/// One malformed-argument refusal (`format-invalid-attribute`).
fn invalid_attribute(detail: impl Into<String>) -> CoreError {
    CoreError::Domain(format!("format-invalid-attribute: {}", detail.into()))
}

impl RprFragment {
    /// Parse `rpr_xml`; `""` is the empty default run properties, exactly as
    /// Python's `_rpr_with_overrides` reads a run that carries no `rPr`.
    fn parse(rpr_xml: &str) -> Result<Self, CoreError> {
        let text = if rpr_xml.trim().is_empty() {
            format!(r#"<w:rPr xmlns:w="{WORD_NAMESPACE}"/>"#)
        } else {
            rpr_xml.to_string()
        };
        let bytes = text.into_bytes();
        let nodes = build_tree(&scan_tags(&bytes))?;
        let root = nodes[root_index(&nodes)?].clone();
        if root.name != "rPr" {
            return Err(CoreError::Domain(
                "invalid-rpr: run properties must be a single <w:rPr> element".to_string(),
            ));
        }
        let children = root
            .children
            .iter()
            .map(|&index| nodes[index].clone())
            .collect();
        Ok(Self {
            nodes: nodes.clone(),
            bytes,
            root,
            children,
        })
    }

    /// The namespace prefix the fragment writes (`w:` for `w:rPr`, `""` for a
    /// bare `rPr` relying on a default namespace declaration).
    fn prefix(&self) -> &str {
        match self.root.raw_name.find(':') {
            Some(index) => &self.root.raw_name[..=index],
            None => "",
        }
    }

    /// The fragment's bytes, verbatim.
    fn slice(&self, start: usize, end: usize) -> &str {
        if start >= end {
            return "";
        }
        // Offsets are tag boundaries inside a `&str`, so they always sit on
        // char boundaries and the slice is valid UTF-8.
        std::str::from_utf8(&self.bytes[start..end]).unwrap_or_default()
    }

    /// The opening tag in open form: a self-closing tag is expanded, because
    /// the rebuilt fragment carries children.
    fn open_form(&self) -> String {
        let raw = self.slice(self.root.open_start, self.root.open_end);
        let body = raw
            .trim_end()
            .trim_end_matches('>')
            .trim_end()
            .trim_end_matches('/')
            .trim_end();
        format!("{body}>")
    }

    /// The matching closing tag.
    fn close_form(&self) -> String {
        format!("</{}>", self.root.raw_name)
    }

    /// The child region: from the first child (or the end of the open tag) to
    /// the start of the closing tag.
    fn children_region(&self) -> (usize, usize) {
        let start = self
            .children
            .first()
            .map_or(self.root.open_end, |child| child.open_start);
        let end = if self.root.self_closing {
            self.root.open_end
        } else {
            self.root.close_start
        };
        (start, end)
    }

    /// The child carrying `local`, when the fragment has one.
    fn child(&self, local: &str) -> Option<&ElementNode> {
        self.children.iter().find(|child| child.name == local)
    }

    /// The attributes already in force for `local` — `None` when there is no
    /// such child, or when it is not an empty property element.
    fn child_attributes(&self, local: &str) -> Option<Vec<(String, String)>> {
        let child = self.child(local)?;
        if !child.children.is_empty() {
            return None;
        }
        Some(tag_attribute_pairs(
            &self.bytes[child.open_start..child.end],
        ))
    }

    /// Rebuild the fragment with every `local`-named child dropped and
    /// `element` (when given) inserted at `local`'s `CT_RPr` slot. Untouched
    /// children, and all text between them, keep their original bytes: only
    /// the moved element travels.
    fn restyled(&self, local: &str, element: Option<&str>) -> String {
        let (region_start, region_end) = self.children_region();
        let order = rpr_schema_position(local);
        let mut out = self.open_form();
        let mut cursor = region_start;
        let mut last_kept_end = region_start;
        let mut placed = false;
        for child in &self.children {
            if child.name == local {
                out.push_str(self.slice(cursor, child.open_start));
                cursor = child.end;
                continue;
            }
            if !placed && rpr_schema_position(&child.name) > order {
                if let Some(text) = element {
                    out.push_str(self.slice(cursor, child.open_start));
                    out.push_str(text);
                    cursor = child.open_start;
                    placed = true;
                }
            }
            out.push_str(self.slice(cursor, child.end));
            cursor = child.end;
            last_kept_end = child.end;
        }
        if let Some(text) = element {
            if !placed {
                // Sorts last: right after the final surviving child, before
                // whatever text trails it.
                let at = last_kept_end.max(cursor);
                out.push_str(self.slice(cursor, at));
                out.push_str(text);
                cursor = at;
            }
        }
        out.push_str(self.slice(cursor, region_end));
        out.push_str(&self.close_form());
        out
    }
}

/// Insert `child_xml` into `rpr_xml` at its `CT_RPr` schema position, replacing
/// any existing child of the same local name (mirror of Python's
/// `insert_rpr_child`). Existing children are never reordered — only the
/// inserted element moves to its slot.
pub fn insert_rpr_child(rpr_xml: &str, child_xml: &str) -> Result<String, CoreError> {
    let fragment = RprFragment::parse(rpr_xml)?;
    let child_bytes = child_xml.as_bytes();
    let nodes = build_tree(&scan_tags(child_bytes))?;
    let child = nodes[root_index(&nodes)?].clone();
    let element =
        std::str::from_utf8(&child_bytes[child.open_start..child.end]).unwrap_or_default();
    Ok(fragment.restyled(&child.name, Some(element)))
}

/// Remove every `local`-named child from `rpr_xml`; the boolean reports whether
/// anything was removed (`false` means the fragment replays unchanged).
pub fn remove_rpr_child(rpr_xml: &str, local: &str) -> Result<(String, bool), CoreError> {
    let fragment = RprFragment::parse(rpr_xml)?;
    if fragment.child(local).is_none() {
        return Ok((rpr_xml.to_string(), false));
    }
    Ok((fragment.restyled(local, None), true))
}

/// The canonical bytes of one `rPr`: opening tag, every child except
/// `w:rPrChange` (format history is not a format property), closing tag. This
/// is the byte sequence the projection hashes into `s_<sha256[:16]>` style
/// ids, so a restyled run lands on the id the read path already reports.
pub fn canonical_rpr_bytes(rpr_xml: &str) -> Result<Vec<u8>, CoreError> {
    let fragment = RprFragment::parse(rpr_xml)?;
    let mut out = Vec::new();
    out.extend_from_slice(
        fragment
            .slice(fragment.root.open_start, fragment.root.open_end)
            .as_bytes(),
    );
    for child in &fragment.children {
        if child.name == "rPrChange" {
            continue;
        }
        out.extend_from_slice(fragment.slice(child.open_start, child.end).as_bytes());
    }
    if !fragment.root.self_closing {
        out.extend_from_slice(
            fragment
                .slice(fragment.root.close_start, fragment.root.end)
                .as_bytes(),
        );
    }
    Ok(out)
}

/// Apply a **partial** attribute override to the run properties already in
/// force (mirror of Python `_rpr_with_overrides`).
///
/// Every property not named in `attributes` keeps its bytes and its slot;
/// each named one is placed by `CT_RPr` schema order. Returns `None` when the
/// request changes nothing — the second application of the same attributes.
///
/// Supported: `vertAlign` (`"superscript"`/`"subscript"`, or `""`/`"baseline"`/
/// `"none"`/`null` to clear a `w:vertAlign` already in force), `bold` and
/// `italic` (`true`/`false`). Anything else is `format-invalid-attribute`.
pub fn rpr_with_overrides(
    base_rpr: &str,
    attributes: &serde_json::Map<String, serde_json::Value>,
) -> Result<Option<String>, CoreError> {
    use serde_json::Value;

    let mut current: Option<String> = None;
    let mut changed = false;
    for (name, value) in attributes {
        let text = current.as_deref().unwrap_or(base_rpr);
        let fragment = RprFragment::parse(text)?;
        let prefix = fragment.prefix().to_string();
        let (local, element) = match name.as_str() {
            "vertAlign" => {
                let requested = match value {
                    Value::Null => None,
                    Value::String(text) if text.is_empty() => None,
                    Value::String(text) if matches!(text.as_str(), "baseline" | "none") => None,
                    Value::String(text) if matches!(text.as_str(), "superscript" | "subscript") => {
                        Some(text.clone())
                    }
                    Value::String(text) => {
                        return Err(invalid_attribute(format!(
                            "vertAlign must be superscript or subscript, got {text:?}"
                        )))
                    }
                    other => {
                        return Err(invalid_attribute(format!(
                            "vertAlign must be superscript or subscript, got {other}"
                        )))
                    }
                };
                let Some(align) = requested else {
                    // Clearing: a schema-2 vertical marker is not part of the
                    // run properties at all, so only a literal w:vertAlign is
                    // removed — a real mutation when one is there.
                    let (restyled, removed) = remove_rpr_child(text, "vertAlign")?;
                    if removed {
                        current = Some(restyled);
                        changed = true;
                    }
                    continue;
                };
                if fragment.child_attributes("vertAlign")
                    == Some(vec![("val".to_string(), align.clone())])
                {
                    continue; // already in force, verbatim
                }
                let element = render_empty_element(&prefix, "vertAlign", &[("val", &align)]);
                ("vertAlign".to_string(), element)
            }
            "bold" | "italic" => {
                let Value::Bool(enabled) = value else {
                    return Err(invalid_attribute(format!(
                        "{name} must be true or false, got {value}"
                    )));
                };
                let local = if name == "bold" { "b" } else { "i" };
                let attributes: Vec<(&str, &str)> = if *enabled {
                    Vec::new()
                } else {
                    vec![("val", "0")]
                };
                if fragment.child_attributes(local)
                    == Some(
                        attributes
                            .iter()
                            .map(|(name, value)| (name.to_string(), value.to_string()))
                            .collect(),
                    )
                {
                    continue;
                }
                (
                    local.to_string(),
                    render_empty_element(&prefix, local, &attributes),
                )
            }
            other => {
                return Err(invalid_attribute(format!(
                    "unsupported attribute {other:?}; supported: vertAlign \
                     (superscript|subscript|baseline), bold, italic"
                )))
            }
        };
        current = Some(fragment.restyled(&local, Some(&element)));
        changed = true;
    }
    Ok(if changed { current } else { None })
}

/// The `format_span` property lane: restyle the run properties already in
/// force on one span of text and derive the character style that results.
///
/// `available_styles` is the document's style inventory (`styles.json` must
/// mirror the source document): the derived variant is *reused* when the
/// document already carries it and refused with `format-style-unavailable`
/// when it does not — this engine never invents a style variable. Register the
/// result with [`RunRestyle::register`].
///
/// Failure codes: `format-noop` (the request produces the properties already
/// in force), `format-invalid-attribute`, `format-style-unavailable`. The
/// remaining `format_span` refusals need paragraph context (region verticals,
/// revision boundaries, span addressing) and belong to the caller.
pub fn restyle_run_properties(
    base_rpr: &str,
    attributes: &serde_json::Map<String, serde_json::Value>,
    available_styles: &BTreeSet<String>,
) -> Result<RunRestyle, CoreError> {
    let Some(rpr) = rpr_with_overrides(base_rpr, attributes)? else {
        return Err(CoreError::Domain(format!(
            "format-noop: the requested attributes already match the target's run properties ({})",
            serde_json::Value::Object(attributes.clone())
        )));
    };
    // The id must come from the SAME canonical form the registry is keyed by,
    // or a derived style can never be found in it. This was a real defect while
    // the two schemes coexisted: `restyle_run_properties` hashed raw XML bytes
    // while `style_id_for_rpr` hashed the reference's JSON form, so the
    // derived id never matched the registered one.
    let style_id = style_id_for_rpr(&rpr)?;
    if !available_styles.contains(&style_id) {
        return Err(CoreError::Domain(format!(
            "format-style-unavailable: this document has no run carrying {} on the target's base \
             formatting, and style variables cannot be invented (styles.json must mirror the source \
             document). Reuse an existing variant by passing style_id=..., or apply the formatting \
             once in Word and re-extract",
            serde_json::Value::Object(attributes.clone())
        )));
    }
    let canonical_rpr = canonical_rpr_json(&rpr)?;
    Ok(RunRestyle {
        rpr,
        canonical_rpr,
        style_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Map, Value};
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../corpus/release")
            .join(name)
    }

    fn ids(inventory: &ProseInventory) -> Vec<String> {
        inventory
            .paragraphs
            .iter()
            .map(|paragraph| paragraph.paragraph_id.clone())
            .collect()
    }

    #[test]
    fn new_paragraphs_strip_ephemeral_opening_attributes() {
        // `typed_docx.py` `_EPHEMERAL_P_ATTRS`: identity and revision marks
        // never travel into a synthesised paragraph.
        let ephemeral = b"<w:p w:rsidR=\"00A\" w:rsidRDefault=\"00B\" w14:paraId=\"1A2B\" \
                          w:rsidP=\"00C\">";
        assert_eq!(
            String::from_utf8(strip_ephemeral_p_attrs(ephemeral)).unwrap(),
            "<w:p>"
        );
        let kept = b"<w:p w:rsidR=\"00A\" w:style=\"Heading1\" w:rsidDel=\"00B\">";
        assert_eq!(
            String::from_utf8(strip_ephemeral_p_attrs(kept)).unwrap(),
            "<w:p w:style=\"Heading1\">"
        );
    }

    #[test]
    fn an_empty_structure_sidecar_is_removed() {
        let dir = std::env::temp_dir().join(format!("docx2typed-structure-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let edits = StructureEdits {
            new_paragraphs: vec![NewParagraph {
                part: "document".to_string(),
                paragraph_id: "N1".to_string(),
                inherit: "P0".to_string(),
                body: "正文".to_string(),
                insert_before: None,
            }],
            deleted_paragraphs: Vec::new(),
        };
        save_structure(&dir, &edits).expect("save");
        assert_eq!(load_structure(&dir).expect("load"), edits);
        save_structure(&dir, &StructureEdits::default()).expect("clear");
        assert!(!dir.join(STRUCTURE_FILE).is_file(), "empty sidecar removed");
        assert_eq!(
            load_structure(&dir).expect("load"),
            StructureEdits::default()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn structure_edits_synthesise_and_drop_paragraphs() {
        let template = fixture("plain.docx");
        let edits = StructureEdits {
            new_paragraphs: vec![NewParagraph {
                part: "document".to_string(),
                paragraph_id: "N1".to_string(),
                inherit: "P0".to_string(),
                body: "新段落 <文本>".to_string(),
                insert_before: Some("P1".to_string()),
            }],
            deleted_paragraphs: vec![DeletedParagraph {
                part: "document".to_string(),
                paragraph_id: "P3".to_string(),
            }],
        };
        validate_structure(&template, &edits).expect("valid structure edits");
        let (bytes, stale) =
            apply_build(&template, &[], &edits, &[], &BTreeMap::new()).expect("apply structure");
        assert!(stale.is_empty(), "no restyles were recorded: {stale:?}");
        let package = std::io::Cursor::new(bytes);
        let mut archive = zip::ZipArchive::new(package).expect("zip");
        let mut document = String::new();
        archive
            .by_name("word/document.xml")
            .expect("document part")
            .read_to_string(&mut document)
            .expect("utf8");
        // The new paragraph is spliced before P1 and XML-escaped.
        assert!(document.contains(
            "<w:p><w:r><w:t>新段落 &lt;文本&gt;</w:t></w:r></w:p>\
             <w:p><w:r><w:t>实施例1采用 250 mg 剂量。</w:t></w:r></w:p>"
        ));
        // The deleted paragraph is not rendered.
        assert!(!document.contains("结束段落。"));
        assert!(document.contains("重复句子内容 重复句子内容。"));
    }

    #[test]
    fn protected_structure_refuses_structural_edits() {
        let package = std::fs::read(fixture("complex.docx")).expect("fixture");
        let part = read_part_bytes(&package, &part_path("document")).expect("document part");
        let inventory = enumerate_package_bytes(&package).expect("inventory");
        let located = scan_document(&part).expect("scan");
        let reason = |id: &str| {
            let (start, end) = located
                .iter()
                .find(|(paragraph_id, _, _)| paragraph_id == id)
                .map(|(_, start, end)| (*start, *end))
                .expect("located paragraph");
            structure_reason(&part, &inventory, id, start, end)
        };
        // P17 carries the section properties; P12 carries a field.
        assert!(reason("P17").is_some_and(|why| why.contains("section properties")));
        assert!(reason("P12").is_some_and(|why| why.contains("structural content")));
        assert_eq!(reason("P0"), None);
        let edits = StructureEdits {
            new_paragraphs: Vec::new(),
            deleted_paragraphs: vec![DeletedParagraph {
                part: "document".to_string(),
                paragraph_id: "P17".to_string(),
            }],
        };
        let error = validate_structure(&fixture("complex.docx"), &edits).expect_err("must refuse");
        assert!(
            error
                .to_string()
                .contains("protected structure cannot be deleted"),
            "{error}"
        );
    }

    #[test]
    fn enumeration_matches_python_ids_on_fixtures() {
        // Frozen Python Reference typed.md paragraph id lists (extract on
        // 2026-08-14; scripts/typed_docx.py extract order: headers, body,
        // footers, endnotes, footnotes, comments).
        let expected: &[(&str, &[&str])] = &[
            ("plain.docx", &["P0", "P1", "P2", "P3", "P4", "P5"]),
            (
                "table.docx",
                &[
                    "P0",
                    "T0.R0.C0.P0",
                    "T0.R0.C1.P0",
                    "T0.R0.C2.P0",
                    "T0.R1.C0.P0",
                    "T0.R1.C1.P0",
                    "T1.R0.C0.P0",
                    "T0.R1.C2.P0",
                    "T0.R2.C0.P0",
                    "T0.R2.C1.P0",
                    "T0.R2.C2.P0",
                    "T2.R0.C0.P0",
                    "T2.R0.C1.P0",
                    "T2.R0.C2.P0",
                    "T2.R1.C0.P0",
                    "T2.R1.C1.P0",
                    "T2.R1.C2.P0",
                    "T2.R2.C0.P0",
                    "T2.R2.C1.P0",
                    "T2.R2.C2.P0",
                    "P1",
                ],
            ),
            ("boxes.docx", &["P0", "B0.P0", "P1", "P2"]),
            (
                "parts.docx",
                &[
                    "header1.P0",
                    "P0",
                    "P1",
                    "P2",
                    "footer1.P0",
                    "endnotes.P0",
                    "endnotes.P1",
                    "endnotes.P2",
                    "footnotes.P0",
                    "footnotes.P1",
                    "footnotes.P2",
                ],
            ),
            (
                "complex.docx",
                &[
                    "header1.P0",
                    "header2.P0",
                    "P0",
                    "P1",
                    "P2",
                    "P3",
                    "P4",
                    "P5",
                    "P6",
                    "P7",
                    "P8",
                    "T0.R0.C0.P0",
                    "T0.R0.C1.P0",
                    "T0.R0.C2.P0",
                    "T0.R0.C3.P0",
                    "T0.R1.C0.P0",
                    "T0.R1.C1.P0",
                    "T0.R1.C2.P0",
                    "T0.R1.C3.P0",
                    "T0.R2.C0.P0",
                    "T0.R2.C1.P0",
                    "T0.R2.C2.P0",
                    "T0.R2.C3.P0",
                    "T0.R3.C0.P0",
                    "T1.R0.C0.P0",
                    "T1.R0.C1.P0",
                    "T1.R1.C0.P0",
                    "T1.R1.C1.P0",
                    "T0.R3.C1.P0",
                    "T0.R3.C2.P0",
                    "T0.R3.C3.P0",
                    "P9",
                    "P10",
                    "P11",
                    "P12",
                    "P13",
                    "P14",
                    "P15",
                    "P16",
                    "P17",
                    "P18",
                    "P19",
                    "S0.P0",
                    "footer1.P0",
                    "endnotes.P0",
                    "endnotes.P1",
                    "endnotes.P2",
                    "footnotes.P0",
                    "footnotes.P1",
                    "footnotes.P2",
                    "comments.P0",
                ],
            ),
        ];
        for (name, expected_ids) in expected {
            let inventory = enumerate_package(&fixture(name)).expect("enumerate");
            let actual = ids(&inventory);
            assert_eq!(
                actual, *expected_ids,
                "paragraph ids differ for {name}: {actual:?}"
            );
        }
    }

    #[test]
    fn editable_surface_flags_match_python() {
        let inventory = enumerate_package(&fixture("complex.docx")).expect("enumerate");
        // P4 (drawing), P10 (proofErr), P12 (fldSimple), P15 (math) are
        // locked; plain prose paragraphs are editable.
        let locked: Vec<&str> = inventory
            .paragraphs
            .iter()
            .filter(|paragraph| !paragraph.editable)
            .map(|paragraph| paragraph.paragraph_id.as_str())
            .collect();
        assert!(
            locked.contains(&"P4"),
            "drawing paragraph locked: {locked:?}"
        );
        assert!(
            locked.contains(&"P10"),
            "proofErr paragraph locked: {locked:?}"
        );
        assert!(
            locked.contains(&"P12"),
            "fldSimple paragraph locked: {locked:?}"
        );
        assert!(
            locked.contains(&"P13"),
            "footnoteReference run locked: {locked:?}"
        );
        assert!(
            locked.contains(&"P14"),
            "endnoteReference run locked: {locked:?}"
        );
        assert!(
            locked.contains(&"footer1.P0"),
            "footer field locked: {locked:?}"
        );
        // Python parity: MATH-SLOT is editable (its oMath lives inside the
        // locked fldSimple paragraph P12).
        let p15 = inventory
            .paragraphs
            .iter()
            .find(|paragraph| paragraph.paragraph_id == "P15")
            .expect("P15");
        assert!(p15.editable);
        let p0 = inventory
            .paragraphs
            .iter()
            .find(|paragraph| paragraph.paragraph_id == "P0")
            .expect("P0");
        assert!(p0.editable);
        // The sdt paragraph is editable.
        let s0 = inventory
            .paragraphs
            .iter()
            .find(|paragraph| paragraph.paragraph_id == "S0.P0")
            .expect("S0.P0");
        assert!(s0.editable);
        assert_eq!(s0.visible_text, "structured document tag");
    }

    #[test]
    fn leaf_editable_flags_inside_opaque_are_locked() {
        let inventory = enumerate_package(&fixture("complex.docx")).expect("enumerate");
        for leaf in &inventory.leaves {
            if !leaf.editable {
                // Locked leaves must sit inside a locked paragraph.
                let paragraph = inventory
                    .paragraphs
                    .iter()
                    .find(|paragraph| paragraph.paragraph_id == leaf.paragraph_id)
                    .expect("paragraph");
                assert!(!paragraph.editable);
            }
        }
    }

    #[test]
    fn island_edit_patches_only_the_leaf_bytes() {
        let template = fixture("table.docx");
        let edit = IslandEdit {
            part: "document".to_string(),
            paragraph_id: "T0.R1.C1.P0".to_string(),
            leaf_index: 0,
            old: "PVA".to_string(),
            new: "PLBA".to_string(),
            tracked: false,
            author: String::new(),
            date: String::new(),
        };
        let out = apply_edits(&template, &[edit]).expect("apply edit");
        // Re-open the output: every part identical except document.xml.
        let template_manifest = zip_manifest_bytes(&std::fs::read(&template).unwrap());
        let out_manifest = zip_manifest_bytes(&out);
        let changed: Vec<&String> = template_manifest
            .iter()
            .filter(|(name, hash)| out_manifest.get(*name) != Some(*hash))
            .map(|(name, _)| name)
            .collect();
        assert_eq!(changed, vec!["word/document.xml"]);
        // The edited part differs ONLY in the target leaf's text range
        // (table.docx also contains an untouched "PVA" cell: T2.R2.C1.P0).
        let template_doc =
            read_part_bytes(&std::fs::read(&template).unwrap(), "word/document.xml").unwrap();
        let out_doc = read_part_bytes(&out, "word/document.xml").unwrap();
        let find = |haystack: &[u8], needle: &[u8]| {
            haystack
                .windows(needle.len())
                .position(|window| window == needle)
        };
        let template_pva = find(&template_doc, b"PVA").expect("PVA in template");
        let out_plba = find(&out_doc, b"PLBA").expect("PLBA in output");
        let out_pva = find(&out_doc, b"PVA").expect("untouched PVA remains");
        assert_eq!(
            template_pva, out_plba,
            "same byte offset, 3 bytes -> 4 bytes"
        );
        assert!(out_plba < out_pva, "edited cell precedes the untouched one");
        // Everything before and after the edited range is byte-identical.
        assert_eq!(&template_doc[..template_pva], &out_doc[..out_plba]);
        assert_eq!(&template_doc[template_pva + 3..], &out_doc[out_plba + 4..]);
    }

    #[test]
    fn island_edit_rejects_locked_leaf() {
        let template = fixture("complex.docx");
        // P12 is the fldSimple paragraph: locked.
        let edit = IslandEdit {
            part: "document".to_string(),
            paragraph_id: "P12".to_string(),
            leaf_index: 0,
            old: "FIELD".to_string(),
            new: "XXX".to_string(),
            tracked: false,
            author: String::new(),
            date: String::new(),
        };
        let error = apply_edits(&template, &[edit]).expect_err("must reject");
        assert!(error.to_string().contains("opaque-paragraph-mutated"));
    }

    #[test]
    fn island_edit_rejects_missing_old() {
        let template = fixture("plain.docx");
        let edit = IslandEdit {
            part: "document".to_string(),
            paragraph_id: "P0".to_string(),
            leaf_index: 0,
            old: "nonexistent text".to_string(),
            new: "x".to_string(),
            tracked: false,
            author: String::new(),
            date: String::new(),
        };
        let error = apply_edits(&template, &[edit]).expect_err("must reject");
        assert!(error.to_string().contains("invalid-edit"));
    }

    #[test]
    fn island_edit_rejects_ambiguous_old() {
        let template = fixture("plain.docx");
        // P5 text is "重复句子内容 重复句子内容。" — old occurs twice.
        let edit = IslandEdit {
            part: "document".to_string(),
            paragraph_id: "P5".to_string(),
            leaf_index: 0,
            old: "重复句子内容".to_string(),
            new: "x".to_string(),
            tracked: false,
            author: String::new(),
            date: String::new(),
        };
        let error = apply_edits(&template, &[edit]).expect_err("must reject");
        assert!(error.to_string().contains("prose-edit-ambiguous"));
    }

    #[test]
    fn patcher_handles_deflate_members() {
        // Build a small zip with a deflated member and patch it.
        use std::io::Write;
        let mut zip_bytes = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut zip_bytes));
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            writer.start_file("word/document.xml", options).unwrap();
            writer
                .write_all(br#"<w:document><w:body><w:p><w:r><w:t>hello world</w:t></w:r></w:p></w:body></w:document>"#)
                .unwrap();
            writer.start_file("word/styles.xml", options).unwrap();
            writer.write_all(br#"<w:styles/>"#).unwrap();
            writer.finish().unwrap();
        }
        let patched = patch_zip_member(
            &zip_bytes,
            "word/document.xml",
            br#"<w:document><w:body><w:p><w:r><w:t>hello there</w:t></w:r></w:p></w:body></w:document>"#,
        )
        .expect("patch");
        let original = zip_manifest_bytes(&zip_bytes);
        let after = zip_manifest_bytes(&patched);
        assert_eq!(
            after.get("word/styles.xml"),
            original.get("word/styles.xml")
        );
        assert_ne!(
            after.get("word/document.xml"),
            original.get("word/document.xml")
        );
        let content = read_part_bytes(&patched, "word/document.xml").unwrap();
        assert!(content.windows(11).any(|w| w == b"hello there"));
    }

    fn zip_manifest_bytes(package: &[u8]) -> BTreeMap<String, String> {
        let file = std::io::Cursor::new(package.to_vec());
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut manifest = BTreeMap::new();
        for index in 0..archive.len() {
            let mut member = archive.by_index(index).unwrap();
            let name = member.name().to_string();
            let mut bytes = Vec::new();
            member.read_to_end(&mut bytes).unwrap();
            use sha2::{Digest, Sha256};
            manifest.insert(name, hex::encode(Sha256::digest(&bytes)));
        }
        manifest
    }

    // -- format_span run-property lane --------------------------------------

    fn rpr(body: &str) -> String {
        format!(r#"<w:rPr xmlns:w="{WORD_NAMESPACE}">{body}</w:rPr>"#)
    }

    fn attributes(pairs: &[(&str, Value)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), value.clone()))
            .collect()
    }

    /// The rPr root's children, in document order.
    fn child_order(rpr_xml: &str) -> Vec<String> {
        let nodes = build_tree(&scan_tags(rpr_xml.as_bytes())).expect("well-formed rPr");
        let root = &nodes[root_index(&nodes).expect("one root")];
        root.children
            .iter()
            .map(|&index| nodes[index].name.clone())
            .collect()
    }

    /// Layer 1 of the #98 regression: every child sits in CT_RPr sequence.
    fn assert_schema_order(rpr_xml: &str) {
        let order = child_order(rpr_xml);
        let positions: Vec<usize> = order.iter().map(|name| rpr_schema_position(name)).collect();
        assert!(
            positions.windows(2).all(|pair| pair[0] <= pair[1]),
            "rPr children out of CT_RPr order: {order:?}"
        );
    }

    #[test]
    fn vertalign_is_inserted_in_schema_order() {
        // The #98 case verbatim: `w:lang` must follow `w:vertAlign`, so a
        // blind append is schema-invalid and Word can reject the document.
        let base = rpr(
            r#"<w:rFonts w:ascii="Times New Roman" w:eastAsia="宋体"/><w:sz w:val="21"/><w:szCs w:val="21"/><w:lang w:val="en-US" w:eastAsia="zh-CN"/>"#,
        );
        let restyled =
            rpr_with_overrides(&base, &attributes(&[("vertAlign", json!("superscript"))]))
                .expect("valid")
                .expect("changed");
        assert_schema_order(&restyled);
        assert_eq!(
            restyled,
            rpr(
                r#"<w:rFonts w:ascii="Times New Roman" w:eastAsia="宋体"/><w:sz w:val="21"/><w:szCs w:val="21"/><w:vertAlign w:val="superscript"/><w:lang w:val="en-US" w:eastAsia="zh-CN"/>"#
            )
        );
        assert_eq!(
            child_order(&restyled),
            ["rFonts", "sz", "szCs", "vertAlign", "lang"]
        );
    }

    #[test]
    fn format_history_stays_last() {
        let base =
            rpr(r#"<w:rFonts w:ascii="Arial"/><w:lang w:val="en-US"/><w:rPrChange w:id="1"/>"#);
        let restyled = rpr_with_overrides(&base, &attributes(&[("bold", json!(true))]))
            .expect("valid")
            .expect("changed");
        assert_schema_order(&restyled);
        assert_eq!(child_order(&restyled), ["rFonts", "b", "lang", "rPrChange"]);
        // Format history is not a format property: the style identity drops it.
        let canonical = canonical_rpr_bytes(&restyled).expect("canonical");
        assert!(!String::from_utf8_lossy(&canonical).contains("rPrChange"));
    }

    #[test]
    fn unnamed_run_properties_survive_untouched() {
        let base = rpr(r#"<w:rFonts w:ascii="Arial"/><w:sz w:val="24"/><w:lang w:val="en-US"/>"#);
        let restyled = rpr_with_overrides(&base, &attributes(&[("bold", json!(false))]))
            .expect("valid")
            .expect("changed");
        // Dropping the one synthesized element reproduces the base byte for
        // byte: every unnamed property kept its bytes *and* its slot.
        assert_eq!(restyled.replace(r#"<w:b w:val="0"/>"#, ""), base);
        assert_eq!(child_order(&restyled), ["rFonts", "b", "sz", "lang"]);
    }

    #[test]
    fn applying_the_same_attributes_twice_changes_nothing() {
        let base = rpr(r#"<w:sz w:val="21"/><w:lang w:val="en-US"/>"#);
        let request = attributes(&[("vertAlign", json!("subscript"))]);
        let once = rpr_with_overrides(&base, &request)
            .expect("valid")
            .expect("changed");
        assert_eq!(rpr_with_overrides(&once, &request).expect("valid"), None);

        // …and the lane reports the repeat as the frozen format-noop code.
        let inventory = BTreeSet::from([style_id_for_rpr(&once).expect("style id")]);
        let restyle = restyle_run_properties(&base, &request, &inventory).expect("first restyle");
        assert_eq!(restyle.rpr, once);
        let noop = restyle_run_properties(&restyle.rpr, &request, &inventory).expect_err("noop");
        assert!(noop.to_string().starts_with("format-noop:"), "{noop}");
    }

    #[test]
    fn registering_the_derived_style_is_idempotent() {
        let base = rpr(r#"<w:sz w:val="21"/>"#);
        let request = attributes(&[("italic", json!(true))]);
        let derived = style_id_for_rpr(&rpr(r#"<w:i/><w:sz w:val="21"/>"#)).expect("style id");
        let restyle = restyle_run_properties(&base, &request, &BTreeSet::from([derived.clone()]))
            .expect("variant in the document");
        assert_eq!(restyle.style_id, derived);

        let mut styles = BTreeMap::new();
        assert_eq!(restyle.register(&mut styles), restyle.style_id);
        let registered = styles.clone();
        restyle.register(&mut styles);
        assert_eq!(styles, registered);
        assert_eq!(styles.get(&restyle.style_id), Some(&restyle.rpr));
    }

    #[test]
    fn clearing_a_vertical_alignment_removes_it() {
        let base = rpr(
            r#"<w:rFonts w:ascii="Arial"/><w:vertAlign w:val="superscript"/><w:lang w:val="en-US"/>"#,
        );
        let cleared = rpr(r#"<w:rFonts w:ascii="Arial"/><w:lang w:val="en-US"/>"#);
        for request in [json!(null), json!("baseline"), json!("none"), json!("")] {
            let restyled = rpr_with_overrides(&base, &attributes(&[("vertAlign", request)]))
                .expect("valid")
                .expect("changed");
            assert_eq!(restyled, cleared);
            assert_schema_order(&restyled);
        }
        // Nothing to clear is a no-op, not a mutation.
        assert_eq!(
            rpr_with_overrides(&cleared, &attributes(&[("vertAlign", json!("none"))]))
                .expect("valid"),
            None
        );
        // A different alignment replaces the one in force, in its own slot.
        let swapped = rpr_with_overrides(&base, &attributes(&[("vertAlign", json!("subscript"))]))
            .expect("valid")
            .expect("changed");
        assert_eq!(
            swapped,
            rpr(
                r#"<w:rFonts w:ascii="Arial"/><w:vertAlign w:val="subscript"/><w:lang w:val="en-US"/>"#
            )
        );
    }

    #[test]
    fn a_run_without_properties_gets_an_explicit_rpr() {
        let restyled = rpr_with_overrides("", &attributes(&[("italic", json!(true))]))
            .expect("valid")
            .expect("changed");
        assert_eq!(restyled, rpr("<w:i/>"));
        assert_schema_order(&restyled);
    }

    #[test]
    fn unsupported_or_mistyped_attributes_are_refused() {
        let base = rpr(r#"<w:rFonts w:ascii="Arial"/>"#);
        for request in [
            attributes(&[("underline", json!(true))]),
            attributes(&[("vertAlign", json!("middle"))]),
            attributes(&[("vertAlign", json!(3))]),
            attributes(&[("bold", json!("yes"))]),
        ] {
            let error = rpr_with_overrides(&base, &request).expect_err("refused");
            assert!(
                error.to_string().starts_with("format-invalid-attribute:"),
                "{error}"
            );
        }
    }

    #[test]
    fn a_variant_the_document_cannot_express_is_refused() {
        let base = rpr(r#"<w:rFonts w:ascii="Arial"/>"#);
        let request = attributes(&[("bold", json!(true))]);
        let unavailable = restyle_run_properties(&base, &request, &BTreeSet::new())
            .expect_err("no such variant in the document");
        assert!(
            unavailable
                .to_string()
                .starts_with("format-style-unavailable:"),
            "{unavailable}"
        );

        // With the variant in the inventory it is reused, not invented.
        let target = rpr(r#"<w:rFonts w:ascii="Arial"/><w:b/>"#);
        let style_id = style_id_for_rpr(&target).expect("style id");
        let restyle = restyle_run_properties(&base, &request, &BTreeSet::from([style_id.clone()]))
            .expect("variant in the document");
        assert_eq!(restyle.rpr, target);
        assert_eq!(restyle.style_id, style_id);
    }

    #[test]
    fn insert_rpr_child_places_by_schema_position_and_replaces_in_place() {
        let base = rpr(r#"<w:rFonts w:ascii="Arial"/><w:lang w:val="en-US"/>"#);
        let inserted =
            insert_rpr_child(&base, r#"<w:vertAlign w:val="subscript"/>"#).expect("insert");
        assert_eq!(
            inserted,
            rpr(
                r#"<w:rFonts w:ascii="Arial"/><w:vertAlign w:val="subscript"/><w:lang w:val="en-US"/>"#
            )
        );
        assert_schema_order(&inserted);
        let replaced =
            insert_rpr_child(&inserted, r#"<w:vertAlign w:val="superscript"/>"#).expect("replace");
        assert_eq!(child_order(&replaced), ["rFonts", "vertAlign", "lang"]);
        assert!(replaced.contains(r#"<w:vertAlign w:val="superscript"/>"#));
        // A self-closing root is expanded; an empty base grows the default rPr.
        assert_eq!(
            insert_rpr_child(&rpr(""), "<w:b/>").expect("insert"),
            rpr("<w:b/>")
        );
        assert_eq!(
            remove_rpr_child(&replaced, "vertAlign").expect("remove").0,
            rpr(r#"<w:rFonts w:ascii="Arial"/><w:lang w:val="en-US"/>"#)
        );
        assert_eq!(
            remove_rpr_child(&base, "vertAlign").expect("remove"),
            (base, false)
        );
    }

    #[test]
    fn the_registry_unblocks_a_restyle_on_a_real_document() {
        // The end-to-end shape of #48: build the registry from a document,
        // then restyle one of its runs to a style the document already
        // carries. Before the registry existed this failed
        // `format-style-unavailable` on EVERY call, because `available_styles`
        // came from a styles.json written with an empty styles object.
        let package = std::fs::read(fixture("complex.docx")).expect("fixture");
        let registry = build_style_registry(&StyleCatalog::build(&package).expect("catalog"))
            .expect("registry");
        let styles = registry["styles"].as_object().expect("styles");
        assert!(
            styles.len() > 1,
            "a real document carries several run properties, got {}",
            styles.len()
        );
        let available: BTreeSet<String> = styles.keys().cloned().collect();

        let inventory = enumerate_package_bytes(&package).expect("inventory");
        let leaf = inventory
            .leaves
            .iter()
            .find(|leaf| !leaf.rpr.is_empty() && leaf.editable)
            .expect("a formatted editable run");
        // Derive the id of an EXISTING style, then ask the lane to produce it
        // from a different base: the lane must find it in the registry.
        let base = if leaf.rpr.is_empty() {
            format!(r#"<w:rPr xmlns:w="{WORD_NAMESPACE}"/>"#)
        } else {
            leaf.rpr.clone()
        };
        let base_id = style_id_for_rpr(&base).expect("base id");
        assert!(
            available.contains(&base_id),
            "the document's own run must be in the registry it produced"
        );
        let target =
            restyle_run_properties(&base, &attributes(&[("italic", json!(true))]), &available);
        match target {
            Ok(restyle) => {
                assert!(
                    available.contains(&restyle.style_id),
                    "a derived style must be one the registry holds"
                );
            }
            Err(error) => {
                // Refusing is correct when the document genuinely has no such
                // variant -- but it must be THAT refusal, not a registry miss.
                let text = error.to_string();
                assert!(
                    text.starts_with("format-noop") || text.starts_with("format-style-unavailable"),
                    "unexpected failure: {text}"
                );
            }
        }
    }

    #[test]
    fn the_registry_matches_the_reference_byte_for_byte() {
        // styles.json is a canonical asset: its bytes are hashed into the tree
        // digest, so serialization is part of the contract. Captured from the
        // reference for corpus/release/styled.docx.
        let package = std::fs::read(fixture("styled.docx")).expect("fixture");
        let registry = build_style_registry(&StyleCatalog::build(&package).expect("catalog"))
            .expect("registry");
        let mut text = serde_json::to_string_pretty(&registry).expect("serialize");
        text.push('\n');
        let expected = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/styles_styled_expectation.json"),
        )
        .expect("expectation");
        assert_eq!(
            text, expected,
            "styles.json must match the reference byte for byte"
        );
    }

    // -----------------------------------------------------------------------
    // format_span: the rPr restyle lane
    // -----------------------------------------------------------------------

    #[test]
    fn a_restyle_replaces_the_runs_properties_in_place() {
        let package = std::fs::read(fixture("styled.docx")).expect("fixture");
        let inventory = enumerate_package_bytes(&package).expect("inventory");
        let leaf = inventory
            .leaves
            .iter()
            .find(|leaf| !leaf.rpr.is_empty() && leaf.editable)
            .expect("an editable run with properties");
        let style_id = "s_test_style".to_string();
        let new_rpr = r#"<w:rPr><w:b/></w:rPr>"#.to_string();
        let mut styles = BTreeMap::new();
        styles.insert(style_id.clone(), new_rpr.clone());

        let text_len = leaf.text.chars().count();
        let edit = RprEdit {
            part: leaf.part_key.clone(),
            paragraph_id: leaf.paragraph_id.clone(),
            text_start: 0,
            text_end: text_len.max(1),
            attributes: json!({ "bold": true }),
            style_id: style_id.clone(),
        };
        let (out, stale) = apply_rpr_edits(&package, &[edit], &styles).expect("apply restyle");
        assert!(stale.is_empty(), "nothing stale: {stale:?}");

        // The target run now carries the new properties...
        let after = enumerate_package_bytes(&out).expect("inventory after");
        let restyled = after
            .leaves
            .iter()
            .find(|candidate| {
                candidate.paragraph_id == leaf.paragraph_id
                    && candidate.leaf_index == leaf.leaf_index
            })
            .expect("the run survived");
        assert_eq!(restyled.rpr, new_rpr, "the run's properties were rewritten");
        // ...and nothing else in the document changed.
        assert_eq!(
            after.paragraphs.len(),
            inventory.paragraphs.len(),
            "a restyle adds no paragraphs"
        );
    }

    #[test]
    fn a_run_without_properties_gets_them_inserted() {
        // The reference always writes the derived rPr, even onto a run that
        // carried none, so the writer must insert rather than skip.
        let package = std::fs::read(fixture("complex.docx")).expect("fixture");
        let inventory = enumerate_package_bytes(&package).expect("inventory");
        // complex.docx has 75 runs and 21 carrying rPr, so a bare editable run
        // is guaranteed. Asserted rather than skipped: a test that can
        // silently no-op is not a test.
        let bare = inventory
            .leaves
            .iter()
            .find(|leaf| leaf.rpr.is_empty() && leaf.editable && !leaf.text.is_empty())
            .expect("complex.docx must have a bare editable run");
        let mut styles = BTreeMap::new();
        styles.insert("s_x".to_string(), "<w:rPr><w:i/></w:rPr>".to_string());
        let edit = RprEdit {
            part: bare.part_key.clone(),
            paragraph_id: bare.paragraph_id.clone(),
            text_start: 0,
            text_end: bare.text.chars().count().max(1),
            attributes: json!({ "italic": true }),
            style_id: "s_x".to_string(),
        };
        let (out, stale) = apply_rpr_edits(&package, &[edit], &styles).expect("apply");
        assert!(stale.is_empty());
        let after = enumerate_package_bytes(&out).expect("inventory after");
        let restyled = after
            .leaves
            .iter()
            .find(|candidate| {
                candidate.paragraph_id == bare.paragraph_id
                    && candidate.leaf_index == bare.leaf_index
            })
            .expect("the run survived");
        assert_eq!(restyled.rpr, "<w:rPr><w:i/></w:rPr>", "an rPr was inserted");
    }

    #[test]
    fn a_span_covering_several_runs_restyles_each_of_them() {
        let package = std::fs::read(fixture("norm.docx")).expect("fixture");
        let inventory = enumerate_package_bytes(&package).expect("inventory");
        // A paragraph with more than one editable leaf.
        let paragraph_id = inventory
            .leaves
            .iter()
            .filter(|leaf| leaf.editable && !leaf.text.is_empty())
            .fold(BTreeMap::<String, usize>::new(), |mut acc, leaf| {
                *acc.entry(leaf.paragraph_id.clone()).or_default() += 1;
                acc
            })
            .into_iter()
            .find(|(_, count)| *count > 1)
            .map(|(id, _)| id);
        let Some(paragraph_id) = paragraph_id else {
            return;
        };
        let leaves: Vec<&ProseLeaf> = inventory
            .leaves
            .iter()
            .filter(|leaf| leaf.paragraph_id == paragraph_id && leaf.editable)
            .collect();
        let total: usize = leaves.iter().map(|leaf| leaf.text.chars().count()).sum();

        let mut styles = BTreeMap::new();
        styles.insert("s_multi".to_string(), "<w:rPr><w:u/></w:rPr>".to_string());
        let edit = RprEdit {
            part: leaves[0].part_key.clone(),
            paragraph_id: paragraph_id.clone(),
            text_start: 0,
            text_end: total,
            attributes: json!({ "underline": true }),
            style_id: "s_multi".to_string(),
        };
        let (out, stale) = apply_rpr_edits(&package, &[edit], &styles).expect("apply");
        assert!(stale.is_empty());
        let after = enumerate_package_bytes(&out).expect("inventory after");
        let restyled: Vec<&ProseLeaf> = after
            .leaves
            .iter()
            .filter(|leaf| leaf.paragraph_id == paragraph_id && leaf.editable)
            .collect();
        assert!(
            restyled
                .iter()
                .all(|leaf| leaf.rpr == "<w:rPr><w:u/></w:rPr>"),
            "every covered run was restyled, not just the first: {:?}",
            restyled
                .iter()
                .map(|leaf| leaf.rpr.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_restyle_whose_paragraph_is_gone_is_reported_stale() {
        // #26's class: never silent, and never resurrected.
        let package = std::fs::read(fixture("norm.docx")).expect("fixture");
        let mut styles = BTreeMap::new();
        styles.insert("s_y".to_string(), "<w:rPr><w:b/></w:rPr>".to_string());
        let edit = RprEdit {
            part: "document".to_string(),
            paragraph_id: "P_does_not_exist".to_string(),
            text_start: 0,
            text_end: 3,
            attributes: json!({ "bold": true }),
            style_id: "s_y".to_string(),
        };
        let (out, stale) = apply_rpr_edits(&package, &[edit], &styles).expect("apply");
        assert_eq!(stale, vec!["P_does_not_exist".to_string()]);
        assert_eq!(out, package, "a stale restyle changes nothing");
    }

    #[test]
    fn the_sidecar_round_trips_and_an_empty_one_is_removed() {
        let dir = std::env::temp_dir().join(format!(
            "docx2typed-rpr-edits-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("scratch");
        assert!(load_rpr_edits(&dir)
            .expect("missing file is no edits")
            .is_empty());

        let edits = vec![RprEdit {
            part: "document".to_string(),
            paragraph_id: "P1".to_string(),
            text_start: 2,
            text_end: 5,
            attributes: json!({ "vertAlign": "superscript" }),
            style_id: "s_z".to_string(),
        }];
        save_rpr_edits(&dir, &edits).expect("save");
        assert_eq!(load_rpr_edits(&dir).expect("load"), edits);

        save_rpr_edits(&dir, &[]).expect("clear");
        assert!(
            !dir.join(RPR_EDITS_FILE).is_file(),
            "an empty record removes the file, so 'no formatting changes' has one representation"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
