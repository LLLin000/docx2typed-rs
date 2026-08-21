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

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use serde::{Deserialize, Serialize};

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
        for &child in &run.children {
            let child_node = &nodes[child];
            let name = child_node.name.as_str();
            if name == "rPr" {
                *rpr_bytes = xml[child_node.open_start..child_node.end].to_vec();
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
                    &part_bytes,
                    &leaf,
                    &edit.old,
                    &edit.new,
                    &edit.author,
                    &edit.date,
                    next_revision_id,
                    next_revision_id.saturating_add(1),
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

fn tracked_run_replacement(
    part: &[u8],
    leaf: &ProseLeaf,
    old: &str,
    new: &str,
    author: &str,
    date: &str,
    delete_id: u32,
    insert_id: u32,
) -> Result<(usize, usize, Vec<u8>), CoreError> {
    let before = &part[..leaf.text_start];
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
    let run_end_rel = part[leaf.text_end..]
        .windows(6)
        .position(|w| w == b"</w:r>")
        .ok_or_else(|| CoreError::Domain("tracked-edit: owning run end not found".to_string()))?;
    let run_end = leaf.text_end + run_end_rel + 6;
    let run = &part[run_start..run_end];
    let t_count = run.windows(5).filter(|w| *w == b"<w:t>").count()
        + run.windows(5).filter(|w| *w == b"<w:t ").count();
    if t_count != 1 || run.windows(6).filter(|w| *w == b"</w:t>").count() != 1 {
        return Err(CoreError::Domain(
            "tracked-edit: replacement must target one text run".to_string(),
        ));
    }
    let rpr = run
        .windows(5)
        .position(|w| w == b"<w:rPr")
        .and_then(|start| {
            run[start..]
                .windows(8)
                .position(|w| w == b"</w:rPr>")
                .map(|end| String::from_utf8_lossy(&run[start..start + end + 8]).into_owned())
        })
        .unwrap_or_default();
    let prefix = leaf.text.split_once(old).map(|(p, _)| p).unwrap_or("");
    let suffix = leaf.text.split_once(old).map(|(_, s)| s).unwrap_or("");
    let attrs = |kind: &str, id: u32| {
        format!(
            "<w:{kind} w:id=\"{id}\" w:author=\"{}\" w:date=\"{}\">",
            xml_attr(author),
            xml_attr(date)
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
    if !leaf.text.contains(old) {
        return Err(CoreError::Domain(
            "tracked-edit: old text is not in leaf".to_string(),
        ));
    }
    let mut replacement = String::new();
    if !prefix.is_empty() {
        replacement.push_str(&run_text("t", prefix));
    }
    replacement.push_str(&attrs("del", delete_id));
    replacement.push_str(&run_text("delText", old));
    replacement.push_str("</w:del>");
    replacement.push_str(&attrs("ins", insert_id));
    replacement.push_str(&run_text("t", new));
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

#[cfg(test)]
mod tests {
    use super::*;
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
}
