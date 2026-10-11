//! Core search API of the `document_search` tool: full-text search over the
//! `edit.md` projection, mirroring Python `scripts/mcp_server.py`
//! `document_search` / `_body_boundaries` / `_annotated_slice` /
//! `_token_boundary_kind` / `_fold_text` / `_region_at` / `_span_map_from` /
//! `_scope_paragraph_ids` / `_encode_match_ref`.
//!
//! ## Matching runs on token-free visible text
//!
//! Inline structure markers (revision edges, comment refs, bookmarks, rPr
//! changes) are zero-width cut points in the coordinate system patches match
//! in, so a query that reads as continuous across one still hits — but the
//! hit says so honestly: `patchable_as_single_hunk` is `false`, the crossed
//! markers are named in `boundary_crossings`, the touched `span_indices` are
//! listed, `region` is `mixed`, and the sub-spans that ARE each patchable are
//! handed over ready to send in `patchable_spans`. Only a hit inside ONE
//! region is a ready-to-send patch `old` (`matched_text`).
//!
//! Width/punctuation variants are tolerated the way patches tolerate them
//! ([`fold_text`] is character-local and length-preserving, so a folded hit
//! maps straight back onto the visible text); the returned `matched_text` is
//! always the ORIGINAL text, re-rendered through the edit grammar so it is a
//! valid patch `old` even when it sits inside `^{…}` / `_{…}` tags.
//!
//! ## Shape
//!
//! Each hit is the enclosing paragraph block — its `@p` anchor id, its
//! `prev_id` / `next_id` for `document_read` windows, the block `text` (full
//! text when it fits in `context_chars`, otherwise windows around each
//! occurrence) — never a bare list of ids. `offset` pages through matching
//! BLOCKS (`total_blocks` says how many), while `total_matches` counts
//! occurrences.
//!
//! ## Scope
//!
//! `scope` narrows the search exactly like `document_replace` does (`all` by
//! default, `body`, `comments`, a part prefix such as `header1` / `T0`, or
//! one paragraph id) and resolves through the same
//! [`SCOPE_INVALID_CODE`] error.
//!
//! Read-only: the workdir is never mutated.
//!
//! ## Declared deferral
//!
//! `counts.inside_deleted_tracked_changes` is always `0`: Rust workdirs
//! extracted by this fork carry a header-only `typed.md` and a paragraph-less
//! `format.json` (see `docx2typed-review::draft`), so the projection holds no
//! deleted revision prose to count. Python, whose `typed.md` carries the
//! revision AST, may report a non-zero count with an explanatory note.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::document_view::{
    block_body, block_ident, find_char, kind_attribute, plain_prose, read_vertical_tag,
    split_edit_blocks, validate_escaped_prose, TOKEN_END, TOKEN_START, VERTICAL_ESCAPE_CHARS,
};
use crate::edit_state::{classify_edit_state, PROJECTION_FILE};
use crate::CoreError;

/// Stable diagnostic code for an empty `query`.
pub const EMPTY_QUERY_CODE: &str = "document-search-empty-query";
/// Stable diagnostic code for a `scope` that names no paragraph (the code
/// `document_replace` raises for the same resolution).
pub const SCOPE_INVALID_CODE: &str = "replace-scope-invalid";

/// Separator between occurrence windows inside one excerpt.
const WINDOW_SEPARATOR: &str = "\n\u{22ef}\n";

const HOW_TO_EDIT: &str = "each occurrence carries match_ref: edit exactly that spot with \
document_patch({\"hunks\": [{\"match_ref\": <ref>, \"new\": <text>}]}) or \
format_span(match_ref=<ref>, attributes={...}); use document_replace only when you mean to \
change every match";

/// One matching occurrence of the query inside a paragraph block.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Occurrence {
    /// Offset of the hit in the block's token-free visible text.
    pub offset: usize,
    /// The hit as it appears in the draft (a ready-to-send patch `old` when
    /// `patchable_as_single_hunk`); falls back to the visible text when the
    /// hit crosses an inline token.
    pub matched_text: String,
    /// `baseline`, `insert`, or `mixed` when the hit crosses a boundary.
    pub region: String,
    /// True only when the hit lies inside ONE revision region.
    pub patchable_as_single_hunk: bool,
    /// Markers crossed strictly inside the hit.
    pub boundary_crossings: Vec<String>,
    /// True when the draft's text differs from the query only by folding.
    pub normalized: bool,
    /// Version-bound address of this occurrence.
    pub match_ref: String,
    /// Sub-spans of a crossing hit that are each patchable, ready to send.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patchable_spans: Option<Vec<PatchableSpan>>,
}

/// One ready-to-send sub-span of a crossing hit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PatchableSpan {
    /// The sub-span's exact draft text (`old` for a patch).
    pub text: String,
    /// Version-bound address of the sub-span.
    pub match_ref: String,
}

/// One matching paragraph block, with its neighbours for read windows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MatchBlock {
    /// Paragraph id (the block's `@p` anchor).
    pub id: String,
    /// Block kind (always `p` for a search hit).
    pub kind: String,
    /// Occurrences inside this block.
    pub matches: usize,
    /// Every occurrence, first one first.
    pub occurrences: Vec<Occurrence>,
    /// Full block text, or occurrence windows when it exceeds `context_chars`.
    pub text: String,
    /// The first occurrence's `matched_text`.
    pub matched_text: String,
    /// The first occurrence's offset.
    pub offset: usize,
    /// The first occurrence's region.
    pub region: String,
    /// The first occurrence's folded-match flag.
    pub normalized: bool,
    /// True only when the first occurrence lies inside one revision region.
    pub patchable_as_single_hunk: bool,
    /// Markers the first occurrence crosses.
    pub boundary_crossings: Vec<String>,
    /// Indices of the editable spans the first occurrence touches.
    pub span_indices: Vec<usize>,
    /// Version-bound address of the first occurrence.
    pub match_ref: String,
    /// Previous block's id, for `document_read` windows.
    pub prev_id: Option<String>,
    /// Next block's id, for `document_read` windows.
    pub next_id: Option<String>,
}

/// Match accounting: what is editable, and what the projection holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SearchCounts {
    /// Editable occurrences (the `matches` total).
    pub editable: usize,
    /// Occurrences that exist only inside deleted tracked changes.
    pub inside_deleted_tracked_changes: usize,
    /// `editable` + `inside_deleted_tracked_changes`.
    pub total_in_projection: usize,
    /// Explains a non-zero `inside_deleted_tracked_changes`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// One `document_search` reply (the `docx2typed-result-1` data payload).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DocumentSearch {
    /// The query as asked.
    pub query: String,
    /// The resolved scope argument.
    pub scope: String,
    /// Block offset this page starts at.
    pub offset: usize,
    /// Match accounting.
    pub counts: SearchCounts,
    /// Number of matching BLOCKS before paging.
    pub total_blocks: usize,
    /// How to turn a `match_ref` into an edit.
    pub how_to_edit: String,
    /// Revision the addresses are bound to (`edit.state.json` body hash).
    pub revision: String,
    /// Freshness state of the draft: clean | dirty | stale-clean | conflict.
    pub state: String,
    /// Number of matching OCCURRENCES before paging.
    pub total_matches: usize,
    /// Number of blocks in `matches`.
    pub returned_blocks: usize,
    /// The matching blocks, this page only.
    pub matches: Vec<MatchBlock>,
}

fn domain(message: impl Into<String>) -> CoreError {
    CoreError::Domain(message.into())
}

/// Search the workdir's `edit.md` projection, mirroring Python
/// `document_search`. `offset` / `limit` page through matching BLOCKS.
pub fn document_search(
    workdir: &Path,
    query: &str,
    context_chars: usize,
    case_sensitive: bool,
    limit: usize,
    offset: usize,
    scope: &str,
) -> Result<DocumentSearch, CoreError> {
    if query.is_empty() {
        return Err(domain(format!(
            "{EMPTY_QUERY_CODE}: query must not be empty"
        )));
    }
    let state = classify_edit_state(workdir)?;
    let text = fs::read_to_string(workdir.join(PROJECTION_FILE)).map_err(CoreError::io)?;
    let (_header, blocks) = split_edit_blocks(&text)?;
    let allowed = scope_paragraph_ids(&blocks, scope)?;
    let needle = if case_sensitive {
        query.to_string()
    } else {
        query.to_lowercase()
    };
    // Exact pass first, then the folded pass (folding is 1:1, so folded offsets
    // map straight back onto the visible text).
    let needle_chars: Vec<char> = needle.chars().collect();
    let folded_needle: Vec<char> = fold_text(&needle).chars().collect();
    let mut entries: Vec<MatchBlock> = Vec::new();
    let mut total = 0usize;
    for (index, block) in blocks.iter().enumerate() {
        let Some((kind, id)) = block_ident(block) else {
            continue;
        };
        if kind != "p" || !allowed.contains(&id) {
            continue;
        }
        let body = block_body(block);
        let (flat, boundaries) = body_boundaries(&body)?;
        let flat_chars: Vec<char> = flat.chars().collect();
        let haystack_text = if case_sensitive {
            flat.clone()
        } else {
            flat.to_lowercase()
        };
        let haystack: Vec<char> = haystack_text.chars().collect();
        let folded_haystack: Vec<char> = fold_text(&haystack_text).chars().collect();
        let mut offsets: Vec<usize> = Vec::new();
        for (candidate, hay) in [
            (&needle_chars, &haystack),
            (&folded_needle, &folded_haystack),
        ] {
            if candidate.is_empty() {
                continue;
            }
            let mut cursor = 0usize;
            while let Some(found) = find_chars(hay, candidate, cursor) {
                if !offsets.contains(&found) {
                    offsets.push(found);
                }
                cursor = found + 1;
            }
            if !offsets.is_empty() {
                break;
            }
        }
        offsets.sort_unstable();
        total += offsets.len();
        if offsets.is_empty() {
            continue;
        }
        let hit_length = needle_chars.len();
        let excerpt = excerpt(&flat_chars, &offsets, hit_length, context_chars);
        let spans = editable_spans(&flat_chars, &boundaries);
        let mut occurrences: Vec<Occurrence> = Vec::new();
        for &occurrence_offset in &offsets {
            let occ_end = occurrence_offset + hit_length;
            let semantic_text: String = flat_chars[occurrence_offset..occ_end].iter().collect();
            let occ_text = annotated_slice(&body, occurrence_offset, occ_end)?
                .unwrap_or_else(|| semantic_text.clone());
            let mut crossed: Vec<String> = boundaries
                .iter()
                .filter(|(boundary_offset, _)| {
                    occurrence_offset < *boundary_offset && *boundary_offset < occ_end
                })
                .map(|(_, kind)| kind.clone())
                .collect();
            crossed.sort();
            crossed.dedup();
            let mut regions = vec![
                region_at(&boundaries, occurrence_offset),
                region_at(&boundaries, occurrence_offset.max(occ_end - 1)),
            ];
            regions.sort_unstable();
            regions.dedup();
            let mut occurrence = Occurrence {
                offset: occurrence_offset,
                matched_text: occ_text,
                region: if crossed.is_empty() {
                    regions[0].to_string()
                } else {
                    "mixed".to_string()
                },
                patchable_as_single_hunk: crossed.is_empty(),
                boundary_crossings: crossed.clone(),
                normalized: semantic_text != query,
                match_ref: encode_match_ref(
                    &id,
                    occurrence_offset,
                    occ_end,
                    &semantic_text,
                    &state.edit_body_sha256,
                ),
                patchable_spans: None,
            };
            if !crossed.is_empty() {
                // The occurrence spans regions, so its own ref will be refused:
                // hand over the sub-spans that ARE patchable, ready to send.
                occurrence.patchable_spans = Some(
                    spans
                        .iter()
                        .filter(|span| {
                            span.start >= occurrence_offset
                                && span.end <= occ_end
                                && !span.text.is_empty()
                        })
                        .map(|span| PatchableSpan {
                            text: span.text.clone(),
                            match_ref: encode_match_ref(
                                &id,
                                span.start,
                                span.end,
                                &span.text,
                                &state.edit_body_sha256,
                            ),
                        })
                        .collect(),
                );
            }
            occurrences.push(occurrence);
        }
        let first = &occurrences[0];
        let hit_start = first.offset;
        let hit_end = hit_start + hit_length;
        let (first_text, first_region, first_patchable, first_crossings) = (
            first.matched_text.clone(),
            first.region.clone(),
            first.patchable_as_single_hunk,
            first.boundary_crossings.clone(),
        );
        let flat_hit: String = flat_chars[hit_start..hit_end].iter().collect();
        let normalized = if case_sensitive {
            flat_hit != needle
        } else {
            flat_hit.to_lowercase() != needle
        };
        entries.push(MatchBlock {
            id: id.clone(),
            kind,
            matches: offsets.len(),
            span_indices: spans
                .iter()
                .filter(|span| span.start < hit_end && span.end > hit_start)
                .map(|span| span.index)
                .collect(),
            occurrences,
            text: excerpt,
            matched_text: first_text,
            offset: hit_start,
            region: first_region,
            normalized,
            patchable_as_single_hunk: first_patchable,
            boundary_crossings: first_crossings,
            match_ref: encode_match_ref(
                &id,
                hit_start,
                hit_end,
                &flat_hit,
                &state.edit_body_sha256,
            ),
            prev_id: neighbour_id(&blocks, index, -1),
            next_id: neighbour_id(&blocks, index, 1),
        });
    }
    let total_blocks = entries.len();
    let page_start = offset.min(total_blocks);
    let page_end = (page_start + limit.max(1)).min(total_blocks);
    let returned = entries[page_start..page_end].to_vec();
    Ok(DocumentSearch {
        query: query.to_string(),
        scope: scope.to_string(),
        offset,
        counts: SearchCounts {
            editable: total,
            inside_deleted_tracked_changes: 0,
            total_in_projection: total,
            note: None,
        },
        total_blocks,
        how_to_edit: HOW_TO_EDIT.to_string(),
        revision: state.edit_body_sha256,
        state: state.state,
        total_matches: total,
        returned_blocks: returned.len(),
        matches: returned,
    })
}

/// `(kind, id)` of the block at `index + step`, when that block has one.
fn neighbour_id(blocks: &[String], index: usize, step: isize) -> Option<String> {
    let neighbour = index.checked_add_signed(step)?;
    block_ident(blocks.get(neighbour)?).map(|(_, id)| id)
}

/// The block excerpt: the whole visible text when it fits in `context_chars`,
/// otherwise one window around each occurrence.
fn excerpt(flat: &[char], offsets: &[usize], hit_length: usize, context_chars: usize) -> String {
    if flat.len() <= context_chars {
        return flat.iter().collect();
    }
    let half = context_chars / 2;
    let windows: Vec<String> = offsets
        .iter()
        .map(|hit_offset| {
            let window_start = hit_offset.saturating_sub(half);
            let window_end = (hit_offset + hit_length + half).min(flat.len());
            let slice: String = flat[window_start..window_end].iter().collect();
            let prefix = if window_start > 0 { "\u{2026}" } else { "" };
            let suffix = if window_end < flat.len() {
                "\u{2026}"
            } else {
                ""
            };
            format!("{prefix}{slice}{suffix}")
        })
        .collect();
    windows.join(WINDOW_SEPARATOR)
}

/// One editable span: a run of visible text `document_patch` can match,
/// cut ONLY at revision-control boundaries (Python `_span_map_from` without
/// the style-region half, which the search output does not use).
struct Span {
    index: usize,
    start: usize,
    end: usize,
    text: String,
}

fn editable_spans(flat_chars: &[char], boundaries: &[(usize, String)]) -> Vec<Span> {
    let mut cuts: BTreeSet<usize> = BTreeSet::from([0, flat_chars.len()]);
    for (offset, _) in boundaries {
        if *offset > 0 && *offset < flat_chars.len() {
            cuts.insert(*offset);
        }
    }
    let ordered: Vec<usize> = cuts.into_iter().collect();
    let mut spans: Vec<Span> = Vec::new();
    for pair in ordered.windows(2) {
        let (start, end) = (pair[0], pair[1]);
        let text: String = flat_chars[start..end].iter().collect();
        if text.is_empty() {
            continue;
        }
        spans.push(Span {
            index: spans.len(),
            start,
            end,
            text,
        });
    }
    spans
}

// ---------------------------------------------------------------------------
// Token-free visible text + boundary markers (Python `_body_boundaries` /
// `_split_chunks` / `_token_boundary_kind`)
// ---------------------------------------------------------------------------

/// Flat visible text plus zero-width revision-control boundaries, mirroring
/// Python `_body_boundaries`: every inline token is an atomic cut point at the
/// visible offset where it sits, and contributes no visible width.
pub fn body_boundaries(body: &str) -> Result<(String, Vec<(usize, String)>), CoreError> {
    let chars: Vec<char> = body.chars().collect();
    let mut flat = String::new();
    let mut boundaries: Vec<(usize, String)> = Vec::new();
    let mut cursor = 0usize;
    loop {
        let Some(start) = find_char(&chars, TOKEN_START, cursor) else {
            if cursor < chars.len() {
                flat.push_str(&plain_prose(&chars[cursor..])?);
            }
            return Ok((flat, boundaries));
        };
        if start > cursor {
            flat.push_str(&plain_prose(&chars[cursor..start])?);
        }
        let end = find_char(&chars, TOKEN_END, start + 1)
            .ok_or_else(|| domain("edit-grammar-invalid: unclosed placeholder in draft"))?;
        let token: String = chars[start..=end].iter().collect();
        boundaries.push((flat.chars().count(), token_boundary_kind(&token)));
        cursor = end + 1;
    }
}

/// Python `_token_boundary_kind`: name the flavour of an inline token so a
/// refusal can say WHAT blocks the span.
fn token_boundary_kind(chunk: &str) -> String {
    if let Some(rest) = chunk.strip_prefix(TOKEN_START) {
        let (closing, rest) = match rest.strip_prefix('/') {
            Some(rest) => (true, rest),
            None => (false, rest),
        };
        for keyword in ["insert", "move-to", "move-from"] {
            if let Some(after) = rest.strip_prefix(keyword) {
                // `\b`: the keyword must end at a non-word character.
                let boundary = after
                    .chars()
                    .next()
                    .map(|ch| !(ch.is_alphanumeric() || ch == '_'))
                    .unwrap_or(true);
                if boundary {
                    return format!("{keyword}-{}", if closing { "end" } else { "start" });
                }
            }
        }
        if rest.starts_with("revision-gap") {
            return "revision-gap".to_string();
        }
    }
    match kind_attribute(chunk) {
        Some(kind) => format!("token:{kind}"),
        None => "token:unknown".to_string(),
    }
}

/// Python `_region_at`: `baseline` / `insert` for a visible offset.
pub fn region_at(boundaries: &[(usize, String)], offset: usize) -> &'static str {
    let mut depth = 0i64;
    for (boundary_offset, kind) in boundaries {
        if *boundary_offset > offset {
            break;
        }
        match kind.as_str() {
            "insert-start" => depth += 1,
            "insert-end" => depth = (depth - 1).max(0),
            _ => {}
        }
    }
    if depth > 0 {
        "insert"
    } else {
        "baseline"
    }
}

// ---------------------------------------------------------------------------
// Matching tolerance (Python `_fold_text`)
// ---------------------------------------------------------------------------

/// Width/punctuation/space folding used ONLY for matching, mirroring Python
/// `_fold_text`: full-width ASCII folds to ASCII, CJK punctuation folds to its
/// ASCII counterpart, exotic spaces fold to U+0020, micro sign folds to mu.
/// Every fold is character-local and length-preserving, so a folded hit maps
/// straight back onto the visible text.
pub(crate) fn fold_text(text: &str) -> String {
    text.chars().map(fold_char).collect()
}

fn fold_char(ch: char) -> char {
    match ch {
        '\u{ff0c}' => ',',
        '\u{3002}' => '.',
        '\u{3001}' => ',',
        '\u{ff1b}' => ';',
        '\u{ff1a}' => ':',
        '\u{ff1f}' => '?',
        '\u{ff01}' => '!',
        '\u{ff08}' => '(',
        '\u{ff09}' => ')',
        '\u{3010}' => '[',
        '\u{3011}' => ']',
        '\u{300a}' => '<',
        '\u{300b}' => '>',
        '\u{201c}' => '"',
        '\u{201d}' => '"',
        '\u{2018}' => '\'',
        '\u{2019}' => '\'',
        '\u{2014}' => '-',
        '\u{2013}' => '-',
        '\u{2212}' => '-',
        '\u{2015}' => '-',
        '\u{2012}' => '-',
        '\u{ff5e}' => '~',
        '\u{301c}' => '~',
        '\u{ff05}' => '%',
        '\u{ff0b}' => '+',
        '\u{ff0d}' => '-',
        '\u{ff0f}' => '/',
        '\u{ff3c}' => '\\',
        '\u{ff1d}' => '=',
        '\u{ff1c}' => '<',
        '\u{ff1e}' => '>',
        '\u{ff03}' => '#',
        '\u{ff06}' => '&',
        '\u{ff0a}' => '*',
        '\u{ff20}' => '@',
        '\u{ff5c}' => '|',
        '\u{ff3e}' => '^',
        '\u{ff3f}' => '_',
        '\u{ff40}' => '`',
        '\u{ff02}' => '"',
        '\u{ff07}' => '\'',
        '\u{3000}' => ' ',
        '\u{00a0}' => ' ',
        '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}' | '\u{2007}'
        | '\u{2008}' | '\u{2009}' | '\u{200a}' | '\u{202f}' | '\u{205f}' => ' ',
        '\u{00b5}' => '\u{3bc}',
        other => {
            let code = other as u32;
            if (0xff01..=0xff5e).contains(&code) {
                // Full-width ASCII block: every code point is its ASCII
                // counterpart plus 0xFEE0.
                char::from_u32(code - 0xfee0).unwrap_or(other)
            } else {
                other
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Annotated slices (Python `_annotated_slice` / `_prose_units` /
// `_render_prose_units`)
// ---------------------------------------------------------------------------

/// A slice of the draft body's ORIGINAL prose, re-rendered through the edit
/// grammar so it is a valid patch `old`; `None` when the slice would cross an
/// inline token (Python `_annotated_slice`).
fn annotated_slice(body: &str, start: usize, end: usize) -> Result<Option<String>, CoreError> {
    let chars: Vec<char> = body.chars().collect();
    let mut out = String::new();
    let mut cursor = 0usize;
    let mut index = 0usize;
    loop {
        let Some(token_start) = find_char(&chars, TOKEN_START, index) else {
            if index < chars.len() {
                append_slice(&chars[index..], start, end, &mut cursor, &mut out)?;
            }
            return Ok(Some(out));
        };
        if token_start > index {
            append_slice(
                &chars[index..token_start],
                start,
                end,
                &mut cursor,
                &mut out,
            )?;
        }
        if start < cursor && cursor < end {
            return Ok(None);
        }
        let token_end = find_char(&chars, TOKEN_END, token_start + 1)
            .ok_or_else(|| domain("edit-grammar-invalid: unclosed placeholder in draft"))?;
        index = token_end + 1;
    }
}

/// Consume one escaped prose chunk: append its `[start, end)` slice, advance
/// the visible offset by the chunk's whole visible width.
fn append_slice(
    raw: &[char],
    start: usize,
    end: usize,
    cursor: &mut usize,
    out: &mut String,
) -> Result<(), CoreError> {
    let units = prose_units(raw)?;
    let local_start = start.saturating_sub(*cursor);
    let local_end = end.saturating_sub(*cursor).min(units.len());
    if local_start < local_end {
        out.push_str(&render_prose_units(&units[local_start..local_end]));
    }
    *cursor += units.len();
    Ok(())
}

/// One visible character plus the vertical annotation in force on it
/// (Python `_prose_units`).
type Unit = (char, Option<&'static str>);

fn prose_units(raw: &[char]) -> Result<Vec<Unit>, CoreError> {
    let text = validate_escaped_prose(raw)?;
    let mut units: Vec<Unit> = Vec::with_capacity(text.len());
    let mut index = 0usize;
    while index < text.len() {
        let ch = text[index];
        if ch == '\\'
            && text
                .get(index + 1)
                .is_some_and(|next| VERTICAL_ESCAPE_CHARS.contains(next))
        {
            units.push((text[index + 1], None));
            index += 2;
            continue;
        }
        if let Some(direction) = vertical_direction(ch) {
            if text.get(index + 1) == Some(&'{') {
                let (inner, next) = read_vertical_tag(&text, index + 2)?;
                units.extend(
                    inner
                        .chars()
                        .map(|inner_char| (inner_char, Some(direction))),
                );
                index = next;
                continue;
            }
        }
        units.push((ch, None));
        index += 1;
    }
    Ok(units)
}

fn vertical_direction(ch: char) -> Option<&'static str> {
    match ch {
        '^' => Some("superscript"),
        '_' => Some("subscript"),
        _ => None,
    }
}

/// Render text units back to escaped edit prose (Python
/// `_render_prose_units`), so a tagged match round-trips as `^{…}` / `_{…}`.
fn render_prose_units(units: &[Unit]) -> String {
    let mut out = String::new();
    let mut cursor = 0usize;
    while cursor < units.len() {
        let vertical = units[cursor].1;
        let mut end = cursor + 1;
        while end < units.len() && units[end].1 == vertical {
            end += 1;
        }
        let text: String = units[cursor..end].iter().map(|(ch, _)| *ch).collect();
        match vertical {
            Some(direction) => {
                out.push(if direction == "superscript" { '^' } else { '_' });
                out.push('{');
                out.push_str(&escape_prose(&escape_vertical_tag_body(&text)));
                out.push('}');
            }
            None => out.push_str(&escape_prose(&escape_vertical_markers(&text))),
        }
        cursor = end;
    }
    out
}

fn escape_prose(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            TOKEN_START => out.push_str("\\u27E6"),
            TOKEN_END => out.push_str("\\u27E7"),
            other => out.push(other),
        }
    }
    out
}

pub(crate) fn escape_vertical_markers(text: &str) -> String {
    if !text.contains(['^', '_', '\\']) {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    for (index, ch) in chars.iter().enumerate() {
        let following = chars.get(index + 1).copied();
        if *ch == '\\' && following.is_some_and(|next| VERTICAL_ESCAPE_CHARS.contains(&next)) {
            out.push_str("\\\\");
            continue;
        }
        if (*ch == '^' || *ch == '_') && following == Some('{') {
            out.push('\\');
        }
        out.push(*ch);
    }
    out
}

pub(crate) fn escape_vertical_tag_body(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        if VERTICAL_ESCAPE_CHARS.contains(&ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

// ---------------------------------------------------------------------------
// Scope resolution (Python `_scope_paragraph_ids`) and match refs
// ---------------------------------------------------------------------------

/// Resolve a scope to the paragraph ids it allows, mirroring Python
/// `_scope_paragraph_ids`: `body` (default), `all`, `comments`, a part prefix
/// (`header1`, `T0`), or one paragraph id.
pub fn scope_paragraph_ids(blocks: &[String], scope: &str) -> Result<Vec<String>, CoreError> {
    let ids: Vec<String> = blocks
        .iter()
        .filter_map(|block| block_ident(block))
        .filter(|(kind, _)| kind == "p")
        .map(|(_, id)| id)
        .collect();
    let stripped = if scope == "/" {
        "all"
    } else {
        scope.strip_prefix('/').unwrap_or(scope)
    };
    match stripped {
        "body" | "" => {
            return Ok(ids
                .into_iter()
                .filter(|id| {
                    id.strip_prefix('P').is_some_and(|digits| {
                        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
                    })
                })
                .collect());
        }
        "all" => return Ok(ids),
        "comments" => {
            return Ok(ids
                .into_iter()
                .filter(|id| id.starts_with("comments."))
                .collect());
        }
        _ => {}
    }
    if ids.iter().any(|id| id == stripped) {
        return Ok(vec![stripped.to_string()]);
    }
    let prefixed: Vec<String> = ids
        .iter()
        .filter(|id| id.starts_with(&format!("{stripped}.")))
        .cloned()
        .collect();
    if !prefixed.is_empty() {
        return Ok(prefixed);
    }
    Err(domain(format!(
        "{SCOPE_INVALID_CODE}: unknown scope '{scope}'; use body (default), all, comments, \
         a part prefix (header1, footer1, footnotes, endnotes, T0), or one paragraph id"
    )))
}

/// Python `_encode_match_ref`: revision + paragraph + offsets + text hash,
/// bound so a mutation is independent of copied text.
pub fn encode_match_ref(
    paragraph_id: &str,
    start: usize,
    end: usize,
    text: &str,
    revision: &str,
) -> String {
    let digest = Sha256::digest(text.as_bytes());
    let payload = format!(
        "{{\"revision\":{},\"paragraph_id\":{},\"start\":{start},\"end\":{end},\"sha256\":{}}}",
        json_string(revision),
        json_string(paragraph_id),
        json_string(&hex::encode(&digest[..8])),
    );
    format!("ref_{}", base64_urlsafe(payload.as_bytes()))
}

fn json_string(value: &str) -> String {
    serde_json::Value::String(value.to_string()).to_string()
}

/// URL-safe base64 without padding (Python
/// `base64.urlsafe_b64encode(...).rstrip("=")`).
fn base64_urlsafe(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let second = *chunk.get(1).unwrap_or(&0);
        let third = *chunk.get(2).unwrap_or(&0);
        let packed = (u32::from(chunk[0]) << 16) | (u32::from(second) << 8) | u32::from(third);
        for (slot, shift) in [(0usize, 18u32), (1, 12), (2, 6), (3, 0)] {
            if slot <= chunk.len() {
                out.push(ALPHABET[((packed >> shift) & 63) as usize] as char);
            }
        }
    }
    out
}

/// Inverse of [`base64_urlsafe`]. Python re-pads to a multiple of four before
/// decoding, so a reference round-trips across the two engines.
fn base64_urlsafe_decode(text: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut lookup = [255u8; 256];
    for (index, byte) in ALPHABET.iter().enumerate() {
        lookup[*byte as usize] = index as u8;
    }
    let mut out = Vec::with_capacity(text.len() * 3 / 4 + 3);
    let mut packed: u32 = 0;
    let mut bits = 0u32;
    for byte in text.bytes() {
        let value = lookup[byte as usize];
        if value == 255 {
            return None;
        }
        packed = (packed << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((packed >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

/// One decoded `match_ref` payload — the opaque, version-bound span address
/// `document_search` hands out and `document_patch` consumes. Byte-compatible
/// with Python's `_encode_match_ref`/`_decode_match_ref`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatchRefPayload {
    pub revision: String,
    pub paragraph_id: String,
    /// Char offsets into the paragraph's flattened visible text.
    pub start: usize,
    pub end: usize,
    /// `sha256(text)[:16]` of the addressed text.
    pub sha256: String,
}

/// Stable code for an unreadable span reference.
pub const MATCH_REF_INVALID: &str = "match-ref-invalid";

/// Decode a `match_ref`. Mirrors Python's `_decode_match_ref`: the prefix and
/// every payload key are required, and anything unreadable fails closed rather
/// than resolving to a guessed span.
pub fn decode_match_ref(reference: &str) -> Result<MatchRefPayload, CoreError> {
    let invalid = |detail: &str| CoreError::Domain(format!("{MATCH_REF_INVALID}: {detail}"));
    let Some(body) = reference.strip_prefix("ref_") else {
        return Err(invalid(&format!("not a span reference: {reference:?}")));
    };
    let raw = base64_urlsafe_decode(body)
        .ok_or_else(|| invalid(&format!("span reference is corrupt: {reference:?}")))?;
    let text = String::from_utf8(raw)
        .map_err(|_| invalid(&format!("span reference is corrupt: {reference:?}")))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| invalid(&format!("span reference is corrupt: {reference:?}")))?;
    let field = |name: &str| {
        value
            .get(name)
            .ok_or_else(|| invalid(&format!("span reference is missing {name:?}")))
    };
    Ok(MatchRefPayload {
        revision: field("revision")?
            .as_str()
            .ok_or_else(|| invalid("revision is not a string"))?
            .to_string(),
        paragraph_id: field("paragraph_id")?
            .as_str()
            .ok_or_else(|| invalid("paragraph_id is not a string"))?
            .to_string(),
        start: field("start")?
            .as_u64()
            .ok_or_else(|| invalid("start is not an integer"))? as usize,
        end: field("end")?
            .as_u64()
            .ok_or_else(|| invalid("end is not an integer"))? as usize,
        sha256: field("sha256")?
            .as_str()
            .ok_or_else(|| invalid("sha256 is not a string"))?
            .to_string(),
    })
}

/// `sha256(text)[:16]`, the digest a `match_ref` binds to its text.
pub fn match_ref_digest(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    hex::encode(&digest[..8])
}

/// Stable code for a reference that no longer addresses what it was issued for.
pub const MATCH_REF_STALE: &str = "match-ref-stale";

/// A `match_ref` resolved against the current draft.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedMatchRef {
    pub paragraph_id: String,
    /// The exact visible text the reference addresses, now.
    pub text: String,
    /// Char offset of that text inside the paragraph's visible text.
    pub start: usize,
}

/// Resolve a span reference to `(paragraph_id, exact text)` against the
/// CURRENT draft — mirror of Python's `_resolve_match_ref`, including its
/// recoverability: a stale reference reports what the document says at those
/// offsets now, so the caller can re-issue in one step instead of
/// re-searching.
///
/// The reference binds the draft revision, the paragraph, the char offsets and
/// a digest of the addressed text. Any disagreement is `match-ref-stale`, not
/// a guessed span: editing the wrong place silently is the failure mode this
/// address exists to prevent.
pub fn resolve_match_ref(workdir: &Path, reference: &str) -> Result<ResolvedMatchRef, CoreError> {
    let payload = decode_match_ref(reference)?;
    let current = classify_edit_state(workdir)?.edit_body_sha256;
    let view = crate::document_view::document_read_view(workdir, None, 0, 0, "content")?;
    let (_header, blocks) = crate::document_view::split_edit_blocks(&view.content)?;
    let body = blocks
        .iter()
        .find(|block| {
            crate::document_view::block_ident(block)
                .map(|(_, id)| id == payload.paragraph_id)
                .unwrap_or(false)
        })
        .map(|block| crate::document_view::block_body(block));
    let Some(body) = body else {
        return Err(stale(
            &format!(
                "{} is no longer in the draft; re-run document_search",
                payload.paragraph_id
            ),
            &payload,
            &current,
            "",
            0,
        ));
    };
    let (flat, _boundaries) = body_boundaries(&body)?;
    let chars: Vec<char> = flat.chars().collect();
    if payload.start >= payload.end || payload.end > chars.len() {
        let tail = if payload.start < chars.len() {
            chars[payload.start..].iter().collect::<String>()
        } else {
            String::new()
        };
        return Err(stale(
            &format!("span reference points outside {}", payload.paragraph_id),
            &payload,
            &current,
            &tail,
            payload.start,
        ));
    }
    let candidate: String = chars[payload.start..payload.end].iter().collect();
    if match_ref_digest(&candidate) != payload.sha256 {
        return Err(stale(
            &format!(
                "the text at {}[{}:{}] changed since the reference was issued",
                payload.paragraph_id, payload.start, payload.end
            ),
            &payload,
            &current,
            &candidate,
            payload.start,
        ));
    }
    Ok(ResolvedMatchRef {
        paragraph_id: payload.paragraph_id,
        text: candidate,
        start: payload.start,
    })
}

/// A refusal that hands back what the document says at those offsets now.
fn stale(
    reason: &str,
    payload: &MatchRefPayload,
    current_revision: &str,
    current_text: &str,
    offset: usize,
) -> CoreError {
    let details = serde_json::json!({
        "current_revision": current_revision,
        "ref_revision": payload.revision,
        "paragraph_id": payload.paragraph_id,
        "offset": offset,
        "current_text": current_text,
        "expected_sha256": payload.sha256,
    });
    CoreError::Domain(format!("{MATCH_REF_STALE}: {reason} | {details}"))
}

/// First char offset of `needle` in `haystack` at or after `from`, mirroring
/// Python `str.find` (char offsets, not byte offsets).
fn find_chars(haystack: &[char], needle: &[char], from: usize) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    (from..=haystack.len() - needle.len())
        .find(|start| haystack[*start..*start + needle.len()] == *needle)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Scratch workdir under the OS temp dir, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "docx2typed-document-search-{name}-{}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("scratch dir");
            Scratch(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A real-shaped workdir: `typed.md` + an `edit.md` whose `@edit` header
    /// binds the typed hash, plus the matching `edit.state.json` sidecar.
    fn workdir(name: &str, blocks: &str) -> Scratch {
        let scratch = Scratch::new(name);
        std::fs::write(scratch.path().join("typed.md"), b"# typed\n").expect("typed.md");
        let typed_sha256 =
            docx2typed_protocol::file_sha256(&scratch.path().join("typed.md")).expect("typed hash");
        let header = format!(
            "<!--@edit schema=\"1\" sync-contract=\"1\" base-typed-sha256=\"{typed_sha256}\" \
             base-projection-sha256=\"PLACEHOLDER\" segmentation=\"uax29-c1-1/unicode-16.0.0\"-->"
        );
        let body_hash =
            crate::edit_state::edit_body_sha256(&format!("{header}\n{blocks}")).expect("body hash");
        let header = header.replace("PLACEHOLDER", &body_hash);
        std::fs::write(
            scratch.path().join(PROJECTION_FILE),
            format!("{header}\n{blocks}"),
        )
        .expect("edit.md");
        let state = serde_json::json!({
            "schema": "typed-clean-edit-state-1",
            "edit_schema_version": 1,
            "sync_contract_version": 1,
            "segmentation_contract": "uax29-c1-1/unicode-16.0.0",
            "base_typed_sha256": typed_sha256,
            "base_projection_sha256": body_hash,
        });
        std::fs::write(
            scratch.path().join("edit.state.json"),
            serde_json::to_vec_pretty(&state).expect("state json"),
        )
        .expect("edit.state.json");
        scratch
    }

    fn paragraph_block(id: &str, body: &str) -> String {
        format!("<!--@p id=\"{id}\" base=\"\"/>\n{body}")
    }

    fn search(name: &str, blocks: &str, query: &str) -> (Scratch, DocumentSearch) {
        let scratch = workdir(name, blocks);
        let reply = document_search(scratch.path(), query, 3000, false, 20, 0, "all")
            .expect("document_search");
        (scratch, reply)
    }

    /// A block whose visible text reads continuously across a revision edge:
    /// the insert marker sits between `Alpha ` and `world`, so a query that
    /// spans it still hits — as a NON-patchable, mixed-region occurrence with
    /// the crossed marker named and the patchable sub-span handed over.
    const REVISION_EDGE_BODY: &str =
        "Alpha \u{27e6}insert id=\"9\" kind=\"insert\"\u{27e7}world\u{27e6}/insert id=\"9\"\u{27e7}wide beta.";

    #[test]
    fn matches_across_a_revision_edge_without_breaking_the_query() {
        let blocks = paragraph_block("P1", REVISION_EDGE_BODY);
        let (_scratch, reply) = search("edge", &blocks, "worldwide");

        assert_eq!(reply.total_matches, 1);
        assert_eq!(reply.total_blocks, 1);
        let block = &reply.matches[0];
        assert_eq!(block.id, "P1");
        assert_eq!(block.offset, 6);
        assert_eq!(block.matched_text, "worldwide", "the marker is invisible");
        assert_eq!(block.region, "mixed");
        assert!(!block.patchable_as_single_hunk);
        assert_eq!(block.boundary_crossings, vec!["insert-end".to_string()]);
        assert_eq!(block.span_indices, vec![1, 2]);

        let occurrence = &block.occurrences[0];
        assert_eq!(occurrence.matched_text, "worldwide");
        assert!(!occurrence.patchable_as_single_hunk);
        assert!(occurrence.match_ref.starts_with("ref_"));
        // The crossing occurrence hands over its patchable sub-spans.
        let spans = occurrence
            .patchable_spans
            .as_ref()
            .expect("patchable spans");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "world");
        assert!(spans[0].match_ref.starts_with("ref_"));
    }

    /// A query that spans the insert START is crossing too; one inside the
    /// inserted region is a single-hunk, ready-to-send patch old.
    #[test]
    fn patchable_as_single_hunk_true_inside_one_region() {
        let blocks = paragraph_block("P1", REVISION_EDGE_BODY);
        let (_scratch, reply) = search("region", &blocks, "world");
        let block = &reply.matches[0];
        assert_eq!(block.id, "P1");
        assert_eq!(block.region, "insert");
        assert!(block.patchable_as_single_hunk);
        assert!(block.boundary_crossings.is_empty());
        assert_eq!(block.matched_text, "world");
        assert_eq!(block.span_indices, vec![1]);

        let (_scratch, crossing) = search("region-start", &blocks, "alpha world");
        assert!(!crossing.matches[0].patchable_as_single_hunk);
        assert_eq!(
            crossing.matches[0].boundary_crossings,
            vec!["insert-start".to_string()]
        );
    }

    /// Inline structure markers that are not revision edges cross a hit the
    /// same honest way, and the token contributes no visible width.
    #[test]
    fn a_non_revision_marker_is_still_a_crossing() {
        let blocks = paragraph_block(
            "P1",
            "pre\u{27e6}token id=\"7\" kind=\"tab\"\u{27e7}post here",
        );
        let (_scratch, reply) = search("token", &blocks, "epos");
        let block = &reply.matches[0];
        assert_eq!(block.matched_text, "epos");
        assert!(!block.patchable_as_single_hunk);
        assert_eq!(block.boundary_crossings, vec!["token:tab".to_string()]);
        assert_eq!(block.region, "mixed");
        assert_eq!(block.span_indices, vec![0, 1]);
    }

    /// Width/punctuation variants match the way patches tolerate them, and the
    /// returned text is the DRAFT's original, not the folded query.
    #[test]
    fn width_and_punctuation_variants_match() {
        let blocks = paragraph_block(
            "P1",
            "\u{7ed3}\u{8bba}\u{ff1a}\u{603b}\u{4ea7}\u{91cf}\u{ff0d}5\u{00b5}m",
        );
        // ASCII colon + ASCII minus + ASCII micro vs the document's full-width
        // punctuation and U+00B5.
        let (_scratch, reply) = search(
            "fold",
            &blocks,
            "\u{7ed3}\u{8bba}:\u{603b}\u{4ea7}\u{91cf}-5\u{3bc}m",
        );
        let block = &reply.matches[0];
        assert_eq!(
            block.matched_text,
            "\u{7ed3}\u{8bba}\u{ff1a}\u{603b}\u{4ea7}\u{91cf}\u{ff0d}5\u{00b5}m",
            "the draft's original text, not the folded query"
        );
        assert!(block.normalized, "the hit needed folding");
        assert_eq!(
            block.offset, 0,
            "folding is 1:1, so the offset maps back onto the visible text"
        );
        assert_eq!(block.matches, 1);
    }

    /// A hit inside `^{…}` comes back re-rendered through the edit grammar, so
    /// `matched_text` is a ready-to-send patch `old`.
    #[test]
    fn matched_text_keeps_vertical_tags() {
        let blocks = paragraph_block("P1", "Cu^{2+} and Mn");
        let (_scratch, reply) = search("vertical", &blocks, "2+");
        let block = &reply.matches[0];
        assert_eq!(block.matched_text, "^{2+}");
        assert!(block.patchable_as_single_hunk);
        // The unannotated address text is the visible text.
        assert_eq!(
            block.match_ref,
            encode_match_ref(&block.id, 2, 4, "2+", &reply.revision)
        );
    }

    /// A block longer than `context_chars` returns a window around each
    /// occurrence instead of the whole text.
    #[test]
    fn long_blocks_window_around_each_occurrence() {
        let body = "0123456789 target 9876543210";
        let blocks = paragraph_block("P1", body);
        let scratch = workdir("window", &blocks);
        let reply =
            document_search(scratch.path(), "target", 10, false, 20, 0, "all").expect("search");
        let block = &reply.matches[0];
        assert_eq!(block.text, "\u{2026}6789 target 9876\u{2026}");
        assert_ne!(block.text, body);

        // A window that reaches the block edge carries no ellipsis on that
        // side (only the truncated side is marked).
        let reply = document_search(scratch.path(), "012", 6, false, 20, 0, "all").expect("search");
        assert_eq!(reply.matches[0].text, "012345\u{2026}");

        // Two windows when the block holds two occurrences.
        let reply = document_search(scratch.path(), "0", 6, false, 20, 0, "all").expect("search");
        let block = &reply.matches[0];
        assert!(
            block.text.contains("\n\u{22ef}\n"),
            "one window per occurrence"
        );
    }

    /// `limit` / `offset` page through matching BLOCKS, and neighbours carry
    /// the ids `document_read` windows need.
    #[test]
    fn limit_and_offset_page_through_matching_blocks() {
        let blocks = (1..=3)
            .map(|index| paragraph_block(&format!("P{index}"), "needle in here"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let scratch = workdir("paging", &blocks);

        let first =
            document_search(scratch.path(), "needle", 3000, false, 1, 0, "all").expect("page 1");
        assert_eq!(first.total_blocks, 3);
        assert_eq!(first.total_matches, 3);
        assert_eq!(first.returned_blocks, 1);
        assert_eq!(first.offset, 0);
        assert_eq!(first.matches[0].id, "P1");
        assert_eq!(first.matches[0].prev_id, None);
        assert_eq!(first.matches[0].next_id.as_deref(), Some("P2"));

        let second =
            document_search(scratch.path(), "needle", 3000, false, 1, 1, "all").expect("page 2");
        assert_eq!(second.returned_blocks, 1);
        assert_eq!(second.offset, 1);
        assert_eq!(second.matches[0].id, "P2");
        assert_eq!(second.matches[0].prev_id.as_deref(), Some("P1"));
        assert_eq!(second.matches[0].next_id.as_deref(), Some("P3"));

        let past_the_end =
            document_search(scratch.path(), "needle", 3000, false, 1, 9, "all").expect("page 9");
        assert_eq!(past_the_end.returned_blocks, 0);
        assert_eq!(past_the_end.total_blocks, 3, "total_blocks is pre-paging");
    }

    #[test]
    fn empty_result_is_an_empty_page_not_an_error() {
        let blocks = paragraph_block("P1", "nothing to see");
        let (_scratch, reply) = search("empty", &blocks, "absent");
        assert!(reply.matches.is_empty());
        assert_eq!(reply.total_matches, 0);
        assert_eq!(reply.total_blocks, 0);
        assert_eq!(reply.returned_blocks, 0);
        assert_eq!(reply.counts.editable, 0);
        assert_eq!(reply.counts.total_in_projection, 0);
        assert_eq!(reply.counts.inside_deleted_tracked_changes, 0);
        assert_eq!(reply.state, "clean");
        assert_eq!(reply.revision.len(), 64);
    }

    /// `scope` narrows to body / one id / a part prefix; an unknown scope is
    /// the stable `replace-scope-invalid` code.
    #[test]
    fn scope_narrows_and_reports_a_stable_code() {
        let blocks = [
            paragraph_block("P1", "needle body"),
            paragraph_block("header1.P1", "needle header"),
            paragraph_block("T0.R1.C2.P0", "needle cell"),
        ]
        .join("\n\n");
        let scratch = workdir("scope", &blocks);

        let body =
            document_search(scratch.path(), "needle", 3000, false, 20, 0, "body").expect("body");
        assert_eq!(body.total_blocks, 1);
        assert_eq!(body.matches[0].id, "P1");

        let all =
            document_search(scratch.path(), "needle", 3000, false, 20, 0, "all").expect("all");
        assert_eq!(all.total_blocks, 3);

        let one = document_search(scratch.path(), "needle", 3000, false, 20, 0, "header1.P1")
            .expect("id");
        assert_eq!(one.matches[0].id, "header1.P1");

        let part =
            document_search(scratch.path(), "needle", 3000, false, 20, 0, "T0").expect("part");
        assert_eq!(part.total_blocks, 1);
        assert_eq!(part.matches[0].id, "T0.R1.C2.P0");

        let error = document_search(scratch.path(), "needle", 3000, false, 20, 0, "nope")
            .expect_err("unknown scope");
        assert!(
            matches!(&error, CoreError::Domain(message) if message.starts_with(SCOPE_INVALID_CODE)),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn empty_query_is_refused() {
        let blocks = paragraph_block("P1", "anything");
        let scratch = workdir("query", &blocks);
        let error = document_search(scratch.path(), "", 3000, false, 20, 0, "all")
            .expect_err("empty query");
        assert!(
            matches!(&error, CoreError::Domain(message) if message.starts_with(EMPTY_QUERY_CODE)),
            "unexpected error: {error}"
        );
    }

    /// Case sensitivity is honoured, `matches` counts occurrences inside one
    /// block, and `document_read`'s neighbours come from ALL blocks (including
    /// non-matching ones).
    #[test]
    fn case_sensitivity_and_per_block_occurrence_counts() {
        let blocks = [
            paragraph_block("P1", "target twice: target"),
            paragraph_block("P2", "Target upper"),
        ]
        .join("\n\n");
        let scratch = workdir("case", &blocks);

        let insensitive =
            document_search(scratch.path(), "target", 3000, false, 20, 0, "all").expect("folded");
        assert_eq!(insensitive.total_blocks, 2);
        assert_eq!(insensitive.total_matches, 3);
        assert_eq!(insensitive.matches[0].matches, 2);
        assert!(
            !insensitive.matches[1].normalized,
            "a case-only difference is not a folded match"
        );
        assert_eq!(insensitive.matches[1].prev_id.as_deref(), Some("P1"));

        let sensitive =
            document_search(scratch.path(), "target", 3000, true, 20, 0, "all").expect("exact");
        assert_eq!(sensitive.total_blocks, 1);
        assert_eq!(sensitive.total_matches, 2);
        assert!(!sensitive.matches[0].normalized);

        let folded =
            document_search(scratch.path(), "TARGET", 3000, false, 20, 0, "all").expect("upper");
        assert_eq!(folded.total_matches, 3);
    }
}
