//! MCP draft projection model (issue #60) — the edit.md work surface the
//! draft tools (`list_paragraphs`, `get_paragraph`, `replace_text`,
//! `insert_paragraph`, `delete_paragraph`, `diff_preview`, `commit_sync`,
//! `revert`) operate on, mirroring `scripts/edit.py`
//! (`refresh_edit_projection`) and the Python MCP draft tools.
//!
//! Rust workdirs extracted by this fork carry a header-only `typed.md` and a
//! paragraph-less `format.json`; the paragraph surface comes from the prose
//! inventory (`enumerate` over `_template.docx`). The projection binds
//! `edit.state.json` (`typed-clean-edit-state-1`) to the generated `edit.md`
//! exactly like the core `edit_state` freshness classifier expects, so
//! `workdir_status` and `diff_preview` reuse the frozen four-state
//! classification (clean / dirty / stale-clean / conflict). Style-region
//! bookkeeping and placeholder tokens are a declared tracer deferral: draft
//! bodies are plain visible text with `base=""` markers.
//!
//! edit.md grammar (mirror of the Python projection): an `@edit` comment
//! header plus `<!--@p id="P0" base="..."/>` / `<!--@new temp="N1"
//! inherit="P0"/>` / `<!--@delete id="P0"/>` paragraph blocks.

use std::fs;
use std::path::{Path, PathBuf};

use docx2typed_core::edit_state::{
    classify_edit_state, edit_body_sha256, parse_edit_header, py_splitlines, EditState,
};
use docx2typed_protocol::{bytes_sha256, file_sha256, resolve_path};
use serde_json::{json, Value};

use crate::collab::{block_body, find_block, paragraph_blocks, CollaborationError};

pub const PROJECTION_FILE: &str = "edit.md";
pub const STATE_FILE: &str = "edit.state.json";
pub const SEGMENTATION_CONTRACT: &str = "uax29-c1-1/unicode-16.0.0";

/// A draft paragraph record (Python `list_paragraphs` shape).
#[derive(Clone, Debug)]
pub struct DraftParagraph {
    pub id: String,
    pub kind: String,
    pub summary: String,
    pub chars: usize,
    pub tokens: usize,
    pub deleted: bool,
}

/// One enumerated paragraph from `_template.docx` (prose inventory view).
#[derive(Clone, Debug)]
pub struct InventoryParagraph {
    pub id: String,
    pub visible_text: String,
}

/// Enumerate the workdir's document body via the prose inventory (mirror of
/// `enumerate` over the template). Only body paragraphs (empty part key)
/// are listed — container/part paragraphs are out of the editable surface.
/// Every paragraph of every part, keyed as typed.md keys them.
///
/// `inventory_paragraphs` deliberately narrows to the BODY, because the draft
/// projection is body-only. This check needs the other end of the comparison:
/// typed.md carries header, footer and note paragraphs too, and a baseline
/// that lacks them reads each one as an unrecorded edit. That was latent until
/// #52 -- typed.md had no paragraphs at all, so nothing ever failed to match.
fn inventory_paragraphs_all_parts(
    workdir: &Path,
) -> Result<Vec<InventoryParagraph>, CollaborationError> {
    let package = workdir.join("_template.docx");
    let bytes = fs::read(&package)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    // The TREE, not `enumerate_package`'s `visible_text`: the latter is built
    // from `w:t` leaves alone, so a paragraph of pure separators reads as empty
    // while typed.md reads `[opaque:separator]`. The tree gives the reference's
    // `visible_text` for every node kind, which is what the comparison needs.
    let trees = docx2typed_core::token_table::paragraph_nodes(&bytes)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    Ok(trees
        .into_iter()
        .map(|tree| InventoryParagraph {
            id: tree.paragraph_id.clone(),
            visible_text: docx2typed_core::token_table::visible_text(&tree.nodes),
        })
        .collect())
}

pub fn inventory_paragraphs(workdir: &Path) -> Result<Vec<InventoryParagraph>, CollaborationError> {
    let package = workdir.join("_template.docx");
    if !package.is_file() {
        return Err(CollaborationError::new(
            "workdir-invalid",
            format!("_template.docx not found in {}", workdir.to_string_lossy()),
        ));
    }
    let bytes = fs::read(&package)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    let trees = docx2typed_core::token_table::paragraph_nodes(&bytes)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    let mut paragraphs = Vec::new();
    for tree in trees {
        if tree.part != "document" && tree.part != "word/document.xml" && !tree.part.is_empty() {
            continue;
        }
        paragraphs.push(InventoryParagraph {
            id: tree.paragraph_id.clone(),
            visible_text: docx2typed_core::token_table::project_nodes(&tree.nodes),
        });
    }
    Ok(paragraphs)
}

/// State-file JSON the projection writes (`typed-clean-edit-state-1`).
fn state_json(typed_sha256: &str, projection_sha256: &str) -> Value {
    serde_json::json!({
        "schema": "typed-clean-edit-state-1",
        "edit_schema_version": 1,
        "sync_contract_version": 1,
        "segmentation_contract": "uax29-c1-1/unicode-16.0.0",
        "base_typed_sha256": typed_sha256,
        "base_projection_sha256": projection_sha256,
    })
}

/// Render one paragraph block from its inventory record.
fn render_block(paragraph: &InventoryParagraph) -> String {
    format!(
        "<!--@p id=\"{}\" base=\"\"/>\n{}",
        paragraph.id, paragraph.visible_text
    )
}

/// Render the full edit.md projection from the template inventory, mirroring
/// `scripts/edit.py` `refresh_edit_projection` (header + ordered blocks).
fn render_projection(
    workdir: &Path,
    paragraphs: &[InventoryParagraph],
) -> Result<(String, String), CollaborationError> {
    let typed_sha256 = file_sha256(&workdir.join("typed.md"))
        .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))?;
    let mut body = String::new();
    for (index, paragraph) in paragraphs.iter().enumerate() {
        if index > 0 {
            body.push('\n');
        }
        body.push_str(&render_block(paragraph));
    }
    let header = format!(
        "<!--@edit schema=\"1\" sync-contract=\"1\" base-typed-sha256=\"{}\" \
         base-projection-sha256=\"{}\" segmentation=\"{}\"-->",
        typed_sha256, "PLACEHOLDER", SEGMENTATION_CONTRACT
    );
    // The base projection hash binds the RENDERED body (header excluded),
    // mirroring Python's two-step (render -> hash -> write header).
    let body_hash = edit_body_sha256(&format!("{header}\n{body}"))
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    let header = format!(
        "<!--@edit schema=\"1\" sync-contract=\"1\" base-typed-sha256=\"{}\" \
         base-projection-sha256=\"{}\" segmentation=\"{}\"-->",
        typed_sha256, body_hash, SEGMENTATION_CONTRACT
    );
    Ok((format!("{header}\n{body}"), typed_sha256))
}

/// Create the edit projection (edit.md + edit.state.json) from the template
/// inventory when absent; otherwise leave it untouched. Mutation path used
/// by the draft tools; returns the existing or fresh `EditState`.
pub fn ensure_projection(workdir: &Path) -> Result<EditState, CollaborationError> {
    if workdir.join(STATE_FILE).is_file() {
        return classify_edit_state(workdir)
            .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()));
    }
    let paragraphs = inventory_paragraphs(workdir)?;
    let (text, typed_sha256) = render_projection(workdir, &paragraphs)?;
    let body_hash = edit_body_sha256(&text)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    fs::write(workdir.join(PROJECTION_FILE), text)
        .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))?;
    let state = state_json(&typed_sha256, &body_hash);
    fs::write(
        workdir.join(STATE_FILE),
        serde_json::to_vec_pretty(&state).expect("serializes"),
    )
    .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))?;
    classify_edit_state(workdir)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))
}

/// Read the draft projection text; error when the projection was never
/// initialized (mirror of `_read_edit` + `session.require` semantics).
pub fn read_draft(workdir: &Path) -> Result<String, CollaborationError> {
    let path = workdir.join(PROJECTION_FILE);
    if !path.is_file() {
        return Err(CollaborationError::new(
            "workdir-not-open",
            "no draft projection; call workdir_open (or a draft tool) first",
        ));
    }
    fs::read_to_string(&path)
        .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))
}

/// Read the draft and split it into (header, blocks).
pub fn read_blocks(workdir: &Path) -> Result<(String, Vec<String>), CollaborationError> {
    split_blocks(&read_draft(workdir)?)
}

/// Split projection text into (header, blocks).
fn split_blocks(text: &str) -> Result<(String, Vec<String>), CollaborationError> {
    let blocks = paragraph_blocks(text)?;
    let header = text
        .lines()
        .find(|line| line.trim_start().starts_with("<!--@edit"))
        .unwrap_or("")
        .to_string();
    Ok((header, blocks))
}

/// Write the draft back (header + blocks), normalizing to `\n` endings.
pub fn write_draft(
    workdir: &Path,
    header: &str,
    blocks: &[String],
) -> Result<(), CollaborationError> {
    let mut out = String::new();
    out.push_str(header);
    out.push('\n');
    for block in blocks {
        out.push('\n');
        out.push_str(block);
    }
    out.push('\n');
    fs::write(workdir.join(PROJECTION_FILE), out)
        .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))
}

/// A draft body rendered as typed.md markup.
///
/// The two files speak DIFFERENT dialects and both match the reference:
/// `edit.md` carries `⟦token id="N0" kind="unsupported-run" tag="…"⟧` and
/// `typed.md` carries `<docx-opaque id="N0" kind="unsupported-run" tag="…"/>`.
/// The commit writes typed.md from the draft's bodies, so without this the
/// committed file would carry the draft's dialect -- and a later commit would
/// compare the two and record a spurious edit for every token.
///
/// `tab`/`br`/`cr` render as their inline markers, everything else as its own
/// element, which is what `_node_to_markup` does for each node kind.
fn typed_markup_from_draft(body: &str) -> String {
    let chars: Vec<char> = body.chars().collect();
    let mut out = String::new();
    let mut index = 0usize;
    while index < chars.len() {
        if chars[index] != '\u{27e6}' {
            out.push(chars[index]);
            index += 1;
            continue;
        }
        let Some(close) = chars[index..].iter().position(|c| *c == '\u{27e7}') else {
            out.push(chars[index]);
            index += 1;
            continue;
        };
        let token: String = chars[index + 1..index + close].iter().collect();
        index += close + 1;
        // `token id="…" kind="…" …` / `range-start …` / `range-end`
        let (keyword, attrs) = match token.split_once(' ') {
            Some((keyword, attrs)) => (keyword.to_string(), attrs.to_string()),
            None => (token.clone(), String::new()),
        };
        let kind = attrs
            .split("kind=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .unwrap_or("")
            .to_string();
        match keyword.as_str() {
            "range-end" => out.push_str("</docx-range>"),
            "range-start" => {
                let id = attrs
                    .split("id=\"")
                    .nth(1)
                    .and_then(|rest| rest.split('"').next())
                    .unwrap_or("");
                out.push_str(&format!("<docx-range id=\"{id}\" kind=\"{kind}\">"));
            }
            _ => match kind.as_str() {
                "tab"
                | "br"
                | "cr"
                | "empty-run"
                | "separator"
                | "continuationSeparator"
                | "noBreakHyphen"
                | "softHyphen"
                | "sym"
                | "dayShort" => {
                    let id = attrs
                        .split("id=\"")
                        .nth(1)
                        .and_then(|rest| rest.split('"').next())
                        .unwrap_or("");
                    let style = attrs
                        .split("style=\"")
                        .nth(1)
                        .and_then(|rest| rest.split('"').next())
                        .unwrap_or("");
                    let style_attr = if style.is_empty() {
                        String::new()
                    } else {
                        format!(" style=\"{style}\"")
                    };
                    out.push_str(&format!(
                        "<docx-inline id=\"{id}\" kind=\"{kind}\"{style_attr}/>"
                    ));
                }
                _ => {
                    let id = attrs
                        .split("id=\"")
                        .nth(1)
                        .and_then(|rest| rest.split('"').next())
                        .unwrap_or("");
                    out.push_str(&format!("<docx-opaque id=\"{id}\" kind=\"{kind}\""));
                    for pair in attrs.split(' ') {
                        if let Some((key, value)) = pair.split_once('=') {
                            if key == "id" || key == "kind" {
                                continue;
                            }
                            out.push_str(&format!(" {key}={value}"));
                        }
                    }
                    out.push_str("/>");
                }
            },
        }
    }
    out
}

/// Python `_visible_text` for a token-free draft body: the body itself
/// (escaping is identity for plain text without `\`, `⟦`, `⟧`).
pub fn visible_text(body: &str) -> String {
    body.to_string()
}

/// Index of the block addressing a paragraph id, whichever marker carries it.
///
/// A baseline paragraph lives in `<!--@p id="P3"-->`; one inserted in this
/// session lives in `<!--@new temp="N1"-->` until a commit reallocates the id.
/// Addressing a paragraph by id must find both, or every edit aimed at a
/// freshly inserted paragraph fails with `paragraph-not-found` in the window
/// between the insert and the commit — the window an agent works in.
pub(crate) fn find_paragraph_block(blocks: &[String], paragraph_id: &str) -> Option<usize> {
    let p_marker = format!("<!--@p id=\"{paragraph_id}\"");
    let new_marker = format!("<!--@new temp=\"{paragraph_id}\"");
    blocks
        .iter()
        .position(|block| block.starts_with(&p_marker) || block.starts_with(&new_marker))
}

/// Parse one block marker line into (kind, id, extra attrs).
pub(crate) fn marker_of(block: &str) -> Option<(String, String, String)> {
    let line = block.split('\n').next().unwrap_or("").trim();
    if let Some(rest) = line.strip_prefix("<!--@p id=\"") {
        let id = rest.split('"').next().unwrap_or("");
        return Some(("p".to_string(), id.to_string(), line.to_string()));
    }
    if let Some(rest) = line.strip_prefix("<!--@new ") {
        let attrs = rest.trim_end_matches("-->");
        let temp = attrs
            .split("temp=\"")
            .nth(1)
            .and_then(|part| part.split('"').next())
            .unwrap_or("");
        return Some(("new".to_string(), temp.to_string(), line.to_string()));
    }
    if let Some(rest) = line.strip_prefix("<!--@delete id=\"") {
        let id = rest.split('"').next().unwrap_or("");
        return Some(("delete".to_string(), id.to_string(), line.to_string()));
    }
    None
}

/// One attribute value out of a marker line (`id`, `temp`, `inherit`).
fn marker_attr(marker: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    marker
        .split(&needle)
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .map(str::to_string)
}

/// The committed typed.md marker for one draft block: a plain template
/// paragraph records `<!--@p id="X"-->`; a synthesised paragraph keeps
/// `base=""` and its `inherit` anchor, so a later commit re-synthesises it
/// from the same baseline instead of dropping it (PR 3).
fn committed_marker(id: &str, marker: &str) -> String {
    match marker_attr(marker, "inherit") {
        Some(inherit) if !inherit.is_empty() => {
            // A new paragraph states `inherit` ONLY. Python's marker grammar
            // requires exactly one of `base`/`inherit` (`typed_core` parse:
            // "paragraph marker requires exactly one of base or inherit"), and
            // `base` addresses baseline style this paragraph does not have.
            format!("<!--@p id=\"{id}\" inherit=\"{inherit}\"/>")
        }
        _ => format!("<!--@p id=\"{id}\"-->"),
    }
}

/// The inherit anchor recorded for a draft paragraph (the `@new` block's
/// `inherit` attribute, or a synthesised `@p` block's).
fn draft_inherit(blocks: &[String], id: &str) -> Option<String> {
    blocks.iter().find_map(|block| {
        let (_, block_id, marker) = marker_of(block)?;
        (block_id == id)
            .then(|| marker_attr(&marker, "inherit"))
            .flatten()
    })
}

/// The insertion anchor of a synthesised paragraph: the next following
/// surviving baseline paragraph in draft order, else None (append) —
/// mirror of Python's `_paragraph_placements`.
fn next_live_paragraph(blocks: &[String], id: &str) -> Option<String> {
    let mut deleted: Vec<String> = Vec::new();
    for block in blocks {
        if let Some((kind, block_id, _)) = marker_of(block) {
            if kind == "delete" {
                deleted.push(block_id);
            }
        }
    }
    let start = blocks
        .iter()
        .position(|block| marker_of(block).is_some_and(|(_, block_id, _)| block_id == id))?;
    for block in &blocks[start + 1..] {
        if let Some((kind, block_id, _)) = marker_of(block) {
            if kind == "p" && !deleted.contains(&block_id) {
                return Some(block_id);
            }
        }
    }
    None
}

/// The next free draft temp id (`N<n>`) for a synthesised block: one above
/// every temp id the draft already carries.
fn next_temp_id(blocks: &[String]) -> String {
    let max_temp = blocks
        .iter()
        .filter_map(|block| {
            let (kind, id, _) = marker_of(block)?;
            (kind == "new")
                .then(|| id.strip_prefix('N')?.parse::<usize>().ok())
                .flatten()
        })
        .max()
        .unwrap_or(0);
    format!("N{}", max_temp + 1)
}

/// The changed paragraph ids of one draft: the draft body vs the effective
/// baseline (committed typed.md record where present, else the template
/// inventory body). New blocks are always changed; delete markers are
/// changed only when the id existed in the baseline.
pub(crate) fn compute_changed(
    workdir: &Path,
    blocks: &[String],
) -> Result<Vec<String>, CollaborationError> {
    let typed_path = workdir.join("typed.md");
    let mut old_paragraphs: std::collections::BTreeMap<String, String> = if typed_path.is_file() {
        let text = fs::read_to_string(&typed_path)
            .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))?;
        parse_typed_paragraphs(&text)
    } else {
        std::collections::BTreeMap::new()
    };
    for paragraph in inventory_paragraphs(workdir)? {
        old_paragraphs
            .entry(paragraph.id)
            .or_insert(paragraph.visible_text);
    }
    let mut changed: Vec<String> = Vec::new();
    for block in blocks {
        let Some((kind, id, _)) = marker_of(block) else {
            continue;
        };
        match kind.as_str() {
            "p" => {
                // Compare like with like. The draft speaks the token dialect
                // (`⟦token id="N0" kind="empty-run"⟧`) while `old_paragraphs`
                // holds typed.md reduced to VISIBLE TEXT, so a raw draft body
                // never equals its own record for any paragraph carrying an
                // inline token — every such paragraph read as "changed", and a
                // real patent showed 5 of them (P7, P17, P58, P68, P81) with no
                // edit at all. Reduce the draft the same way first.
                let body = typed_body_visible_text(&typed_markup_from_draft(&block_body(block)));
                let prior = old_paragraphs.get(&id).cloned().unwrap_or_default();
                if prior != body {
                    changed.push(id);
                }
            }
            "new" => changed.push(id),
            "delete" if old_paragraphs.contains_key(&id) => changed.push(id),
            _ => {}
        }
    }
    changed.sort();
    changed.dedup();
    Ok(changed)
}

/// `list_paragraphs` payload: every draft paragraph block.
pub fn list_paragraphs(workdir: &Path) -> Result<Value, CollaborationError> {
    if !workdir.join(PROJECTION_FILE).is_file() {
        let paragraphs = inventory_paragraphs(workdir)?;
        let items: Vec<Value> = paragraphs
            .iter()
            .map(|paragraph| {
                let visible = &paragraph.visible_text;
                json!({
                    "id": paragraph.id,
                    "kind": "p",
                    "summary": visible.chars().take(60).collect::<String>(),
                    "chars": visible.chars().count(),
                    "tokens": 0,
                    "deleted": false,
                })
            })
            .collect();
        return Ok(serde_json::json!({ "paragraphs": items }));
    }
    let (_, blocks) = read_blocks(workdir)?;
    let mut paragraphs: Vec<Value> = Vec::new();
    for block in &blocks {
        let Some((kind, id, _)) = marker_of(block) else {
            continue;
        };
        match kind.as_str() {
            "p" | "new" => {
                let body = block_body(block);
                let visible = visible_text(&body);
                paragraphs.push(serde_json::json!({
                    "id": id,
                    "kind": kind,
                    "summary": visible.chars().take(60).collect::<String>(),
                    "chars": visible.chars().count(),
                    "tokens": body.matches('\u{27e6}').count(),
                    "deleted": false,
                }));
            }
            "delete" => {
                paragraphs.push(serde_json::json!({
                    "id": id,
                    "kind": "delete",
                    "summary": "[deleted]",
                    "chars": 0,
                    "tokens": 0,
                    "deleted": true,
                }));
            }
            _ => {}
        }
    }
    Ok(serde_json::json!({ "paragraphs": paragraphs }))
}

/// `get_paragraph` payload: draft text plus the single style region of the
/// tracer (styles.json is empty; style identity is a declared deferral).
pub fn get_paragraph(workdir: &Path, paragraph_id: &str) -> Result<Value, CollaborationError> {
    if !workdir.join(PROJECTION_FILE).is_file() {
        let paragraphs = inventory_paragraphs(workdir)?;
        let paragraph = paragraphs
            .iter()
            .find(|paragraph| paragraph.id == paragraph_id)
            .ok_or_else(|| {
                CollaborationError::new(
                    "paragraph-not-found",
                    format!("paragraph {paragraph_id} not found in the draft"),
                )
            })?;
        let plain = paragraph.visible_text.clone();
        return Ok(json!({
            "paragraph_id": paragraph_id,
            "text": plain,
            "plain": plain,
            "tokens": 0,
            "styles": [{
                "text": plain,
                "style_id": "",
                "description": "",
                "rpr": Value::Null,
            }],
        }));
    }
    let (_, blocks) = read_blocks(workdir)?;
    let index = find_paragraph_block(&blocks, paragraph_id).ok_or_else(|| {
        CollaborationError::new(
            "paragraph-not-found",
            format!("paragraph {paragraph_id} not found in the draft"),
        )
    })?;
    let body = block_body(&blocks[index]);
    let plain = visible_text(&body);
    Ok(serde_json::json!({
        "paragraph_id": paragraph_id,
        "text": body,
        "plain": plain,
        "tokens": body.matches('\u{27e6}').count(),
        "styles": [{
            "text": plain,
            "style_id": "",
            "description": "",
            "rpr": Value::Null,
        }],
    }))
}

/// Replace exactly one occurrence of `old` in the paragraph body (Python
/// `_replace_in_body` with start_offset=None): unique occurrence required,
/// no placeholder markers allowed in `old`. Returns the new body.
pub fn replace_in_body(
    body: &str,
    old: &str,
    new: &str,
    paragraph_id: &str,
) -> Result<String, CollaborationError> {
    if old.contains('\u{27e6}') || old.contains('\u{27e7}') {
        return Err(CollaborationError::new(
            "text-not-found",
            format!("{paragraph_id}: old must be visible text without placeholder markers"),
        ));
    }
    let count = body.matches(old).count();
    if count == 0 {
        return Err(CollaborationError::new(
            "text-not-found",
            format!("{paragraph_id}: text {old:?} not found in paragraph"),
        ));
    }
    if count > 1 {
        return Err(CollaborationError::new(
            "text-ambiguous",
            format!("{paragraph_id}: text {old:?} appears {count} times; provide a longer unique context"),
        ));
    }
    let position = body.find(old).expect("count > 0 implies a match");
    let mut out = String::with_capacity(body.len() + new.len());
    out.push_str(&body[..position]);
    out.push_str(new);
    out.push_str(&body[position + old.len()..]);
    Ok(out)
}

/// The `replace_text` tool body: preflight, locate the paragraph, prove a
/// single occurrence, write the draft back, and record the exact leaf-local
/// operation for the build sidecar.
pub fn replace_text(
    workdir: &Path,
    paragraph_id: &str,
    old: &str,
    new: &str,
) -> Result<Value, CollaborationError> {
    replace_text_with_options(
        workdir,
        TextEdit {
            paragraph_id,
            old,
            new,
            leaf_index: None,
        },
        EditTracking {
            tracked: false,
            author: None,
            date: None,
        },
    )
}

/// The text replacement one draft operation performs, plus the addressing
/// needed to select the right occurrence within the paragraph.
#[derive(Clone, Copy)]
pub struct TextEdit<'a> {
    pub paragraph_id: &'a str,
    pub old: &'a str,
    pub new: &'a str,
    pub leaf_index: Option<usize>,
}

/// How an operation is recorded: a plain body edit, or a tracked revision
/// that carries an author and date.
#[derive(Clone, Copy)]
pub struct EditTracking<'a> {
    pub tracked: bool,
    pub author: Option<&'a str>,
    pub date: Option<&'a str>,
}

pub fn replace_text_with_options(
    workdir: &Path,
    edit: TextEdit<'_>,
    tracking: EditTracking<'_>,
) -> Result<Value, CollaborationError> {
    crate::collab::ensure_agent_ready(workdir)?;
    let (header, mut blocks) = read_blocks(workdir)?;
    let paragraph_id = edit.paragraph_id;
    let index = find_paragraph_block(&blocks, paragraph_id).ok_or_else(|| {
        CollaborationError::new(
            "paragraph-not-found",
            format!("paragraph {paragraph_id} not found in the draft"),
        )
    })?;
    let marker = blocks[index].split('\n').next().unwrap_or("").to_string();
    let body = block_body(&blocks[index]);
    let new_body = if let Some(leaf_index) = edit.leaf_index {
        let occurrence = leaf_occurrence(workdir, paragraph_id, leaf_index, edit.old)?;
        replace_nth_in_body(&body, edit.old, edit.new, occurrence, paragraph_id)?
    } else {
        replace_in_body(&body, edit.old, edit.new, paragraph_id)?
    };
    blocks[index] = if new_body.is_empty() {
        marker
    } else {
        format!("{marker}\n{new_body}")
    };
    record_island_operation(workdir, edit, tracking)?;
    write_draft(workdir, &header, &blocks)?;
    Ok(serde_json::json!({
        "paragraph_id": paragraph_id,
        "draft": "dirty",
        "next": "diff_preview to inspect style ownership, then commit_sync",
    }))
}
fn leaf_occurrence(
    workdir: &Path,
    paragraph_id: &str,
    leaf_index: usize,
    old: &str,
) -> Result<usize, CollaborationError> {
    let package = fs::read(workdir.join("_template.docx"))
        .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))?;
    let inventory = docx2typed_core::prose::enumerate_package_bytes(&package)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    let part = docx2typed_core::prose::part_for_paragraph_id(paragraph_id)
        .ok_or_else(|| CollaborationError::new("invalid-edit", "unknown paragraph part"))?;
    Ok(inventory
        .leaves
        .iter()
        .filter(|leaf| {
            leaf.part_key == part
                && leaf.paragraph_id == paragraph_id
                && leaf.leaf_index < leaf_index
                && leaf.text == old
        })
        .count())
}

fn replace_nth_in_body(
    body: &str,
    old: &str,
    new: &str,
    occurrence: usize,
    paragraph_id: &str,
) -> Result<String, CollaborationError> {
    let mut start = 0usize;
    for _ in 0..occurrence {
        let position = body[start..].find(old).ok_or_else(|| {
            CollaborationError::new(
                "text-not-found",
                format!("{paragraph_id}: occurrence of {old:?} not found"),
            )
        })?;
        start += position + old.len();
    }
    let position = body[start..].find(old).ok_or_else(|| {
        CollaborationError::new(
            "text-not-found",
            format!("{paragraph_id}: occurrence of {old:?} not found"),
        )
    })? + start;
    let mut out = String::with_capacity(body.len() + new.len());
    out.push_str(&body[..position]);
    out.push_str(new);
    out.push_str(&body[position + old.len()..]);
    Ok(out)
}

/// Apply one literal or regex replacement to a run of text, honouring the
/// case-sensitivity flag. Shared by the leaf-island and structure-body paths
/// so both produce byte-identical results for the same input.
fn apply_replacement(
    regex: &Option<regex::Regex>,
    find: &str,
    replace: &str,
    text: &str,
    case_sensitive: bool,
) -> String {
    if let Some(re) = regex {
        return re.replace_all(text, replace).to_string();
    }
    if case_sensitive {
        return text.replace(find, replace);
    }
    let lower = text.to_lowercase();
    let lower_find = find.to_lowercase();
    let mut out = String::new();
    let mut cursor = 0usize;
    while let Some(pos) = lower[cursor..].find(&lower_find) {
        out.push_str(&text[cursor..cursor + pos]);
        out.push_str(replace);
        cursor += pos + lower_find.len();
    }
    out.push_str(&text[cursor..]);
    out
}

/// True when one leaf of the (already enumerated) inventory carries the whole
/// visible match.
///
/// `offset` and `length` are in visible characters of the FLATTENED paragraph,
/// which is the coordinate space the match plan uses. Each leaf's visible text
/// is walked in order to find the leaf the span falls in; a span that starts in
/// one leaf and ends in another has no single owner and is refused (it cannot
/// be rewritten without splitting a run).
fn leaf_in_inventory_covers(
    inventory: &docx2typed_core::prose::ProseInventory,
    paragraph_id: &str,
    offset: usize,
    length: usize,
) -> bool {
    if length == 0 {
        return false;
    }
    let Some(part) = docx2typed_core::prose::part_for_paragraph_id(paragraph_id) else {
        return false;
    };
    let span_end = offset + length;
    let mut cursor = 0usize;
    for leaf in inventory
        .leaves
        .iter()
        .filter(|leaf| leaf.part_key == part && leaf.paragraph_id == paragraph_id)
    {
        let leaf_start = cursor;
        let leaf_end = cursor + leaf.text.chars().count();
        cursor = leaf_end;
        if offset >= leaf_start && span_end <= leaf_end {
            return true;
        }
    }
    false
}

/// The leaf index that carries the whole visible span, or None.
///
/// The same walk as [`leaf_in_inventory_covers`], returning the index so a
/// handle-addressed edit can name its leaf instead of re-searching the text.
fn leaf_index_for_span(
    workdir: &Path,
    paragraph_id: &str,
    offset: usize,
    length: usize,
) -> Option<usize> {
    if length == 0 {
        return None;
    }
    let package = fs::read(workdir.join("_template.docx")).ok()?;
    let inventory = docx2typed_core::prose::enumerate_package_bytes(&package).ok()?;
    let part = docx2typed_core::prose::part_for_paragraph_id(paragraph_id)?;
    let span_end = offset + length;
    let mut cursor = 0usize;
    for leaf in inventory
        .leaves
        .iter()
        .filter(|leaf| leaf.part_key == part && leaf.paragraph_id == paragraph_id)
    {
        let leaf_start = cursor;
        let leaf_end = cursor + leaf.text.chars().count();
        cursor = leaf_end;
        if offset >= leaf_start && span_end <= leaf_end {
            return Some(leaf.leaf_index);
        }
    }
    None
}

/// The template leaf one island edit lands in, or None for a synthesised
/// paragraph: it has no baseline leaf to replace (it is rendered from the
/// inherit paragraph at build time and rewritten in the structure sidecar at
/// commit instead — PR 3). Fails closed with `cross-leaf-edit` when no single
/// leaf carries `old`. Read-only: the batch path resolves every hunk's leaf
/// before anything is written.
fn island_leaf_for(
    workdir: &Path,
    paragraph_id: &str,
    old: &str,
    leaf_index: Option<usize>,
) -> Result<Option<(String, usize)>, CollaborationError> {
    // A paragraph synthesised in this session has no baseline leaf to rewrite:
    // it is rendered from the structure sidecar at build time. That is true the
    // moment it is inserted, not only once `commit_sync` has written the
    // sidecar — the draft carries the `@new` block and the temp id alone, so an
    // edit aimed at it in that window must resolve to "no leaf", not to an
    // `unknown paragraph part` refusal.
    if paragraph_id.starts_with('N') {
        return Ok(None);
    }
    if docx2typed_core::prose::load_structure(workdir)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?
        .new_paragraphs
        .iter()
        .any(|paragraph| paragraph.paragraph_id == paragraph_id)
    {
        return Ok(None);
    }
    let package = fs::read(workdir.join("_template.docx"))
        .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))?;
    let inventory = docx2typed_core::prose::enumerate_package_bytes(&package)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    let part = docx2typed_core::prose::part_for_paragraph_id(paragraph_id)
        .ok_or_else(|| CollaborationError::new("invalid-edit", "unknown paragraph part"))?;
    let leaf = inventory
        .leaves
        .iter()
        .find(|leaf| {
            leaf.part_key == part
                && leaf.paragraph_id == paragraph_id
                && leaf_index.map_or(leaf.text.contains(old), |index| leaf.leaf_index == index)
        })
        .ok_or_else(|| {
            CollaborationError::new(
                "cross-leaf-edit",
                format!("{paragraph_id}: replacement must stay within one text leaf"),
            )
        })?;
    Ok(Some((part, leaf.leaf_index)))
}

fn record_island_operation(
    workdir: &Path,
    edit: TextEdit<'_>,
    tracking: EditTracking<'_>,
) -> Result<(), CollaborationError> {
    let TextEdit {
        paragraph_id,
        old,
        new,
        leaf_index,
    } = edit;
    let Some((part, leaf_index)) = island_leaf_for(workdir, paragraph_id, old, leaf_index)? else {
        return Ok(());
    };
    append_island_edits(
        workdir,
        &[docx2typed_core::prose::IslandEdit {
            part,
            paragraph_id: paragraph_id.to_string(),
            leaf_index,
            old: old.to_string(),
            new: new.to_string(),
            tracked: tracking.tracked,
            author: tracking.author.unwrap_or("").to_string(),
            date: tracking.date.unwrap_or("").to_string(),
        }],
    )
}

/// Append island edits in ONE sidecar write (load -> extend -> save).
fn append_island_edits(
    workdir: &Path,
    edits: &[docx2typed_core::prose::IslandEdit],
) -> Result<(), CollaborationError> {
    if edits.is_empty() {
        return Ok(());
    }
    let mut islands = docx2typed_core::prose::load_islands(workdir)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    islands.extend_from_slice(edits);
    docx2typed_core::prose::save_islands(workdir, &islands)
        .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))
}

/// The `insert_paragraph` tool body: block insert after `after_id`.
pub fn insert_paragraph(
    workdir: &Path,
    after_id: &str,
    text: &str,
    inherit: Option<&str>,
) -> Result<Value, CollaborationError> {
    if after_id.starts_with('T')
        || after_id.starts_with('B')
        || after_id.contains('.')
        || inherit
            .map(|value| value.starts_with('T') || value.starts_with('B') || value.contains('.'))
            .unwrap_or(false)
    {
        return Err(CollaborationError::new(
            "table-structure-immutable",
            "paragraphs cannot be inserted into tables, text boxes, or \
             header/footer/note parts; container structure operations are out of scope",
        ));
    }
    crate::collab::ensure_agent_ready(workdir)?;
    let (header, mut blocks) = read_blocks(workdir)?;
    let index = find_paragraph_block(&blocks, after_id).ok_or_else(|| {
        CollaborationError::new(
            "paragraph-not-found",
            format!("paragraph {after_id} not found in the draft"),
        )
    })?;
    let resolved_inherit = inherit.unwrap_or(after_id).to_string();
    // Fail closed before the draft is touched: a synthesised paragraph
    // copies its inherit paragraph's opening and pPr, so that paragraph
    // must be plain prose (no section properties, revisions, anchors, or
    // opaque interiors).
    require_unprotected(
        workdir,
        &resolved_inherit,
        "new paragraph cannot inherit protected structure",
    )?;
    let temp = next_temp_id(&blocks);
    let block = format!("<!--@new temp=\"{temp}\" inherit=\"{resolved_inherit}\"/>\n{text}");
    blocks.insert(index + 1, block);
    write_draft(workdir, &header, &blocks)?;
    Ok(serde_json::json!({
        "temp_id": temp,
        "inherit": resolved_inherit,
        "draft": "dirty",
        "next": "commit_sync allocates the formal paragraph ID",
    }))
}

/// The `delete_paragraph` tool body: remove the block, append a delete
/// marker.
pub fn delete_paragraph(workdir: &Path, paragraph_id: &str) -> Result<Value, CollaborationError> {
    if paragraph_id.starts_with('T')
        || paragraph_id.starts_with('B')
        || (paragraph_id.contains('.') && !paragraph_id.starts_with('P'))
    {
        return Err(CollaborationError::new(
            "table-structure-immutable",
            "container and part paragraphs cannot be deleted; container \
             structure operations are out of scope",
        ));
    }
    crate::collab::ensure_agent_ready(workdir)?;
    let (header, mut blocks) = read_blocks(workdir)?;
    let index = find_paragraph_block(&blocks, paragraph_id).ok_or_else(|| {
        CollaborationError::new(
            "paragraph-not-found",
            format!("paragraph {paragraph_id} not found in the draft"),
        )
    })?;
    // Fail closed before the draft is touched: a deleted paragraph is
    // simply not rendered, which would silently drop any protected
    // structure it carries (section properties, revisions, anchors).
    require_unprotected(
        workdir,
        paragraph_id,
        "paragraph with protected structure cannot be deleted",
    )?;
    blocks.remove(index);
    blocks.push(format!("<!--@delete id=\"{paragraph_id}\"-->"));
    write_draft(workdir, &header, &blocks)?;
    Ok(serde_json::json!({
        "paragraph_id": paragraph_id,
        "draft": "dirty",
        "next": "commit_sync",
    }))
}

// ---------------------------------------------------------------------------
// document_patch: the batch edit primitive
//
// The file-like editing surface over replace / insert / delete. Exactly one of
// `hunks` / `diff`; a paragraph may carry several non-overlapping replace hunks
// (like `git apply`), overlapping spans are rejected, and the whole batch
// applies or nothing does.
// ---------------------------------------------------------------------------

/// The hunk keys the tool accepts (Python `_normalize_patch_hunks` known set,
/// sorted): an unrecognised key would be silently ignored, so it is refused.
const PATCH_HUNK_KEYS: [&str; 10] = [
    "delete",
    "inherit",
    "insert_after",
    "leaf_index",
    "match_ref",
    "new",
    "old",
    "paragraph_id",
    "start",
    "text",
];

/// One deferred per-leaf replacement: `(paragraph, leaf, char start, char
/// length, replacement text)`. Several occurrences can land in one run, and the
/// island record rewrites the whole leaf, so they are merged before recording.
type LeafSpan = (String, usize, usize, usize, String);

/// One replacement inside a leaf: `(char start, char length, replacement)`.
type LeafReplacement = (usize, usize, String);

/// One normalized patch hunk.
#[derive(Clone, Debug)]
enum PatchHunk {
    Replace {
        paragraph_id: String,
        old: String,
        new: String,
        /// Char offset of `old` inside the paragraph's visible text, when the
        /// caller addressed the span with a `match_ref`. A handle names an
        /// exact span, so the patch must use it rather than re-searching the
        /// text — the text can legitimately occur twice in one paragraph
        /// (measured on a real patent: 13 of 34 hunks), and a search would
        /// then refuse a span the handle proved unique.
        start: Option<usize>,
        /// The template leaf carrying the span, when a handle named it. The
        /// island record addresses that leaf directly rather than re-searching
        /// the text for it.
        leaf_index: Option<usize>,
    },
    Insert {
        insert_after: String,
        text: String,
        inherit: Option<String>,
    },
    Delete {
        paragraph_id: String,
    },
}

impl PatchHunk {
    /// The paragraph this hunk names (the anchor paragraph for an insert).
    fn paragraph_id(&self) -> &str {
        match self {
            PatchHunk::Replace { paragraph_id, .. } => paragraph_id,
            PatchHunk::Insert { insert_after, .. } => insert_after,
            PatchHunk::Delete { paragraph_id } => paragraph_id,
        }
    }
}

/// One rejected hunk. A batch reports EVERY broken hunk in one call, so the
/// caller can fix all of them and resend once (mirror of Python's `failed()`).
struct HunkProblem {
    hunk: usize,
    paragraph_id: String,
    code: String,
    message: String,
}

impl HunkProblem {
    fn error(&self) -> CollaborationError {
        CollaborationError::new(
            self.code.clone(),
            format!(
                "hunk #{} ({}): {}",
                self.hunk, self.paragraph_id, self.message
            ),
        )
    }
}

fn hunk_problem(
    problems: &mut Vec<HunkProblem>,
    hunk: usize,
    target: &PatchHunk,
    error: CollaborationError,
) {
    problems.push(HunkProblem {
        hunk,
        paragraph_id: target.paragraph_id().to_string(),
        code: error.code,
        message: error.detail,
    });
}

/// One replace span: start offset, old text, new text.
type ReplaceSpan = (usize, String, String);

/// A paragraph's replace spans in first-appearance order.
type PendingSpans = Vec<(String, Vec<ReplaceSpan>)>;

/// The validated batch, ready to write: the new block list plus the island
/// edits its replaces record. Nothing here touches the workdir.
struct PatchPlan {
    blocks: Vec<String>,
    applied: Vec<Value>,
    islands: Vec<docx2typed_core::prose::IslandEdit>,
}

/// Python `_require_body_structure_mutable`: container and part paragraphs are
/// addressed by their container tools, never by a body patch hunk.
fn require_body_structure_mutable(paragraph_id: &str) -> Result<(), CollaborationError> {
    if paragraph_id.starts_with('T') || paragraph_id.starts_with('B') || paragraph_id.contains('.')
    {
        return Err(CollaborationError::new(
            "table-structure-immutable",
            format!(
                "{paragraph_id}: paragraphs inside tables, text boxes, or parts cannot be patched \
                 from the body surface; use the container-specific tools"
            ),
        ));
    }
    Ok(())
}

/// Python `_require_comment_text_opt_in`: comment paragraphs are annotation
/// content, not document prose.
fn require_comment_text_opt_in(
    paragraph_id: &str,
    allow_comment_text: bool,
) -> Result<(), CollaborationError> {
    if paragraph_id.starts_with("comments.") && !allow_comment_text {
        return Err(CollaborationError::new(
            "comment-text-requires-opt-in",
            format!(
                "{paragraph_id}: comment text is annotation content, not document prose; pass \
                 allow_comment_text=true only when the user explicitly asked to edit reviewer \
                 comment text"
            ),
        ));
    }
    Ok(())
}

/// The fail-closed protection gate shared with `insert_paragraph` and
/// `delete_paragraph`: the build cannot reproduce a protected paragraph's
/// structure, so a draft carrying one could never commit.
fn require_unprotected(
    workdir: &Path,
    paragraph_id: &str,
    verb: &str,
) -> Result<(), CollaborationError> {
    if let Some(reason) = docx2typed_core::prose::paragraph_protection(workdir, paragraph_id)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?
    {
        return Err(CollaborationError::new(
            "invalid-structure",
            format!("{verb}: {paragraph_id} ({reason})"),
        ));
    }
    Ok(())
}

/// Expand `{"match_ref": …, "new": …}` hunks into ordinary replace hunks.
///
/// `document_search` hands out these references so a caller can edit an exact
/// spot without copying text; resolving one here turns it into
/// `{paragraph_id, old, new}`, which is what the rest of the lane already
/// understands. Resolution validates the reference against the current draft,
/// so a span that moved is reported (`match-ref-stale`, carrying what the
/// document says at those offsets now) instead of landing on the wrong text.
fn expand_match_ref_hunks(
    workdir: &Path,
    hunks: &[Value],
) -> Result<Vec<Value>, CollaborationError> {
    let mut expanded = Vec::with_capacity(hunks.len());
    for (index, hunk) in hunks.iter().enumerate() {
        let Some(object) = hunk.as_object() else {
            // Let normalization report the shape error.
            expanded.push(hunk.clone());
            continue;
        };
        let Some(reference) = object.get("match_ref").and_then(Value::as_str) else {
            expanded.push(hunk.clone());
            continue;
        };
        let Some(new) = object.get("new").and_then(Value::as_str) else {
            return Err(CollaborationError::new(
                "invalid-arguments",
                format!("hunks[{index}]: match_ref hunk needs 'new' (the replacement text)"),
            ));
        };
        let resolved = docx2typed_core::document_search::resolve_match_ref(workdir, reference)
            .map_err(|error| {
                // Core domain failures carry `code: detail`; split so the
                // caller sees the stable code rather than one long message.
                let message = error.to_string();
                match message.split_once(':') {
                    Some((code, detail)) if !code.contains(' ') => {
                        CollaborationError::new(code, detail.trim())
                    }
                    _ => CollaborationError::new("workdir-invalid", message),
                }
            })?;
        expanded.push(json!({
            "paragraph_id": resolved.paragraph_id,
            "old": resolved.text,
            "new": new,
            // The handle's own anchor: the commit applies the span at this
            // offset instead of re-finding the text, and records the island
            // against the leaf that carries it.
            "start": resolved.start,
            "leaf_index": leaf_index_for_span(
                workdir,
                &resolved.paragraph_id,
                resolved.start,
                resolved.text.chars().count(),
            ),
        }));
    }
    Ok(expanded)
}

/// Normalize raw hunk objects into typed hunks (mirror of
/// `_normalize_patch_hunks`): unknown keys, missing fields, and an unusable
/// shape are `invalid-arguments` before anything is read or written.
fn normalize_patch_hunks(hunks: &[Value]) -> Result<Vec<PatchHunk>, CollaborationError> {
    let invalid = |message: String| CollaborationError::new("invalid-arguments", message);
    let mut normalized: Vec<PatchHunk> = Vec::with_capacity(hunks.len());
    for (index, hunk) in hunks.iter().enumerate() {
        let Some(object) = hunk.as_object() else {
            return Err(invalid(format!("hunks[{index}] must be an object")));
        };
        let unknown: Vec<String> = object
            .keys()
            .filter(|key| !PATCH_HUNK_KEYS.contains(&key.as_str()))
            .map(|key| format!("{key:?}"))
            .collect();
        if !unknown.is_empty() {
            return Err(invalid(format!(
                "hunks[{index}]: unknown key(s) {}; known keys are {} — an unrecognised key would \
                 be silently ignored, so nothing was applied",
                unknown.join(", "),
                PATCH_HUNK_KEYS.join(", ")
            )));
        }
        let string = |key: &str| object.get(key).and_then(Value::as_str);
        if object.contains_key("match_ref") {
            // Expanded before normalization (see `expand_match_ref_hunks`);
            // reaching here means the caller used the key on its own.
            return Err(invalid(format!(
                "hunks[{index}]: match_ref hunk needs 'new' (the replacement text)"
            )));
        } else if object.contains_key("paragraph_id") {
            let paragraph_id = string("paragraph_id").filter(|value| !value.is_empty());
            let Some(paragraph_id) = paragraph_id else {
                return Err(invalid(format!(
                    "hunks[{index}]: replace hunk needs a non-empty paragraph_id"
                )));
            };
            let Some(new) = string("new") else {
                return Err(invalid(format!(
                    "hunks[{index}]: replace hunk needs 'new' (use an empty string only when you \
                     really mean to delete the text; use a delete hunk to drop a paragraph)"
                )));
            };
            let Some(old) = string("old").filter(|value| !value.is_empty()) else {
                return Err(invalid(format!(
                    "hunks[{index}]: replace hunk needs string old (non-empty) and new"
                )));
            };
            let start = object
                .get("start")
                .and_then(Value::as_u64)
                .map(|v| v as usize);
            let leaf_index = object
                .get("leaf_index")
                .and_then(Value::as_u64)
                .map(|v| v as usize);
            normalized.push(PatchHunk::Replace {
                paragraph_id: paragraph_id.to_string(),
                old: old.to_string(),
                new: new.to_string(),
                start,
                leaf_index,
            });
        } else if object.contains_key("insert_after") {
            let Some(insert_after) = string("insert_after").filter(|value| !value.is_empty())
            else {
                return Err(invalid(format!(
                    "hunks[{index}]: insert hunk needs a non-empty insert_after"
                )));
            };
            let Some(text) = string("text").filter(|value| !value.is_empty()) else {
                return Err(invalid(format!(
                    "hunks[{index}]: insert hunk needs non-empty text"
                )));
            };
            let inherit = match object.get("inherit") {
                None | Some(Value::Null) => None,
                Some(value) => {
                    let Some(inherit) = value.as_str().filter(|value| !value.is_empty()) else {
                        return Err(invalid(format!(
                            "hunks[{index}]: insert hunk inherit must be a string"
                        )));
                    };
                    Some(inherit.to_string())
                }
            };
            normalized.push(PatchHunk::Insert {
                insert_after: insert_after.to_string(),
                text: text.to_string(),
                inherit,
            });
        } else if object.contains_key("delete") {
            let Some(paragraph_id) = string("delete").filter(|value| !value.is_empty()) else {
                return Err(invalid(format!(
                    "hunks[{index}]: delete hunk needs a non-empty paragraph id"
                )));
            };
            normalized.push(PatchHunk::Delete {
                paragraph_id: paragraph_id.to_string(),
            });
        } else {
            return Err(invalid(format!(
                "hunks[{index}]: needs one of paragraph_id (replace), insert_after, or delete"
            )));
        }
    }
    Ok(normalized)
}

/// The char offset a `match_ref`-derived hunk carries, if any.
///
/// `normalize_patch_hunks` keeps the offset on the `PatchHunk::Replace`
/// variant; the validation arm reads it back through this helper so the
/// anchor can be checked against the live visible text before it is used.
fn anchor_from_hunk(hunk: &PatchHunk) -> Option<usize> {
    match hunk {
        PatchHunk::Replace { start, .. } => *start,
        _ => None,
    }
}

/// The `⟦...⟧` inline token chunks of a draft body: flat visible text plus the
/// zero-width boundaries an inline token cuts into it (mirror of
/// `_body_boundaries`, offsets in characters). A draft that carries no token —
/// every draft this build produces — has no boundary at all.
fn body_boundaries(body: &str) -> (String, Vec<(usize, String)>) {
    let mut flat = String::new();
    let mut boundaries: Vec<(usize, String)> = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find('\u{27e6}') {
        flat.push_str(&rest[..start]);
        let tail = &rest[start..];
        let Some(end) = tail.find('\u{27e7}') else {
            break; // unclosed token: the rest is text, not a boundary
        };
        boundaries.push((
            flat.chars().count(),
            token_boundary_kind(&tail[..end + '\u{27e7}'.len_utf8()]),
        ));
        rest = &tail[end + '\u{27e7}'.len_utf8()..];
    }
    flat.push_str(rest);
    (flat, boundaries)
}

/// Python `_token_boundary_kind`: name the flavour of one inline token so a
/// refusal can say WHAT blocks the span (revision control vs format history vs
/// an anchor) instead of a bare text-not-found.
fn token_boundary_kind(token: &str) -> String {
    let inner = token.trim_start_matches('\u{27e6}');
    let (closing, name) = match inner.strip_prefix('/') {
        Some(rest) => (true, rest),
        None => (false, inner),
    };
    for candidate in ["insert", "move-to", "move-from"] {
        if let Some(rest) = name.strip_prefix(candidate) {
            let ends_name = rest
                .chars()
                .next()
                .is_none_or(|ch| !(ch.is_alphanumeric() || ch == '_'));
            if ends_name {
                return format!("{candidate}{}", if closing { "-end" } else { "-start" });
            }
        }
    }
    if inner.starts_with("revision-gap") {
        return "revision-gap".to_string();
    }
    match marker_attr(token, "kind").filter(|kind| !kind.is_empty()) {
        Some(kind) => format!("token:{kind}"),
        None => "token:unknown".to_string(),
    }
}

/// A refusal when `old` is contiguous only after the inline tokens are
/// stripped — i.e. it strictly spans a revision-control (or protected) marker.
/// Boundaries exactly at the span edges do not count: a span covering a whole
/// revision body is one editable span (mirror of `_span_crosses_boundary`).
fn span_crosses_boundary(paragraph_id: &str, body: &str, old: &str) -> Option<CollaborationError> {
    if old.contains('\u{27e6}') || old.contains('\u{27e7}') {
        return None;
    }
    let (flat, boundaries) = body_boundaries(body);
    if boundaries.is_empty() {
        return None;
    }
    let position = flat.find(old)?;
    let start = flat[..position].chars().count();
    let end = start + old.chars().count();
    let mut crossed: Vec<String> = boundaries
        .iter()
        .filter(|(offset, _)| *offset > start && *offset < end)
        .map(|(_, kind)| kind.clone())
        .collect();
    crossed.sort();
    crossed.dedup();
    if crossed.is_empty() {
        return None;
    }
    // Revision control, however the projection spells it: bare insert/move
    // labels, the gap marker, or a revision kind carried inside a range token.
    // Anchors and format history are NOT revisions, and calling them one sends
    // the agent hunting for a revision that is not there.
    let revision_flavours: Vec<&String> = crossed
        .iter()
        .filter(|name| match name.strip_prefix("token:") {
            Some(kind) => matches!(kind, "insert" | "delete" | "move_from" | "move_to"),
            None => true,
        })
        .collect();
    let (code, why) = if revision_flavours.is_empty() {
        (
            "edit-span-crosses-protected-marker",
            format!(
                "the span crosses protected inline markers ({}) — format history / anchors are \
                 atomic, so the visible text on the two sides is not one editable run",
                crossed.join(", ")
            ),
        )
    } else {
        (
            "edit-span-crosses-revision-boundary",
            format!(
                "the span crosses revision-control markers ({}), so the engine cannot decide \
                 which revision owns the new text",
                revision_flavours
                    .iter()
                    .map(|name| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
    };
    Some(CollaborationError::new(
        code,
        format!(
            "{paragraph_id}: old is visible in the paragraph but {why}. Re-issue the edit as hunks \
             that each copy ONE span's text (never a cross-boundary run); if the new text cannot \
             be partitioned without guessing, stop and ask the user."
        ),
    ))
}

/// Replace `old` at the recorded offset. Spans are applied in descending
/// offset order, so every offset validated against the pre-batch body is still
/// exact when its turn comes (mirror of `_replace_in_body(start_offset=...)`).
fn replace_at(
    body: &str,
    start: usize,
    old: &str,
    new: &str,
    paragraph_id: &str,
) -> Result<String, CollaborationError> {
    let Some(rest) = body.get(start..) else {
        return Err(CollaborationError::new(
            "text-not-found",
            format!("{paragraph_id}: offset {start} is outside the paragraph"),
        ));
    };
    if !rest.starts_with(old) {
        return Err(CollaborationError::new(
            "text-not-found",
            format!("{paragraph_id}: text {old:?} is no longer at offset {start}"),
        ));
    }
    let mut out = String::with_capacity(body.len() + new.len());
    out.push_str(&body[..start]);
    out.push_str(new);
    out.push_str(&rest[old.len()..]);
    Ok(out)
}

/// Validate EVERY hunk against the in-memory block list, then apply all of them
/// to a copy of it. Nothing is written here — this is the atomicity mechanism:
/// a rejected batch fails before the draft is touched, so the outcome is
/// all-or-nothing by construction instead of by snapshot-and-restore.
///
/// Deletes land first, then inserts, then replaces anchored by their start
/// offset in descending order, so earlier applications never shift later ones
/// (mirror of Python `_apply_document_hunks`).
fn plan_patch(
    workdir: &Path,
    blocks: &[String],
    hunks: &[PatchHunk],
    allow_comment_text: bool,
    tracking: EditTracking<'_>,
) -> Result<PatchPlan, CollaborationError> {
    let mut problems: Vec<HunkProblem> = Vec::new();
    let mut delete_ids: Vec<String> = Vec::new();
    let mut insert_specs: Vec<(String, String, Option<String>)> = Vec::new();
    let mut pending: PendingSpans = Vec::new();
    let mut islands: Vec<docx2typed_core::prose::IslandEdit> = Vec::new();
    // Deferred per-leaf replacements: (paragraph, leaf, char_start, char_len,
    // replacement). Several occurrences can land in ONE run, and each island
    // record rewrites the WHOLE leaf — recording them separately would make the
    // second expect a leaf text the first already changed. They are merged into
    // one whole-leaf edit per leaf below, which is also what makes the count
    // the Facade reports equal the count the build applies.
    let mut leaf_spans: Vec<LeafSpan> = Vec::new();
    let mut applied: Vec<Value> = Vec::new();

    for (index, hunk) in hunks.iter().enumerate() {
        let number = index + 1;
        match hunk {
            PatchHunk::Replace {
                paragraph_id,
                old,
                new,
                start: _,
                leaf_index,
            } => {
                let Some(block_index) = find_paragraph_block(blocks, paragraph_id) else {
                    hunk_problem(
                        &mut problems,
                        number,
                        hunk,
                        CollaborationError::new(
                            "paragraph-not-found",
                            format!("paragraph {paragraph_id} not found in the draft"),
                        ),
                    );
                    continue;
                };
                if let Err(error) = require_comment_text_opt_in(paragraph_id, allow_comment_text) {
                    hunk_problem(&mut problems, number, hunk, error);
                    continue;
                }
                if delete_ids.contains(paragraph_id) {
                    hunk_problem(
                        &mut problems,
                        number,
                        hunk,
                        CollaborationError::new(
                            "document-patch-paragraph-repeated",
                            format!(
                                "{paragraph_id}: cannot replace and delete the same paragraph in \
                                 one patch"
                            ),
                        ),
                    );
                    continue;
                }
                if old.contains('\u{27e6}') || old.contains('\u{27e7}') {
                    hunk_problem(
                        &mut problems,
                        number,
                        hunk,
                        CollaborationError::new(
                            "placeholder-in-edit-span",
                            format!(
                                "{paragraph_id}: old includes a read-only placeholder token; select \
                                 contiguous editable text on one side of the placeholder instead"
                            ),
                        ),
                    );
                    continue;
                }
                if old == new {
                    hunk_problem(
                        &mut problems,
                        number,
                        hunk,
                        CollaborationError::new(
                            "patch-noop",
                            format!("{paragraph_id}: old and new are identical; nothing to change"),
                        ),
                    );
                    continue;
                }
                let body = block_body(&blocks[block_index]);
                let visible = visible_text(&body);
                // A hunk that arrived through a `match_ref` carries the
                // handle's own char offset. That anchor is authoritative: the
                // text can occur twice in one paragraph while the handle still
                // addresses exactly one span (measured on a real patent, 13 of
                // 34 occurrences). Verify the anchor, do not re-search it.
                // The handle's offset is in CHARS of the visible text; the
                // span application below indexes BYTES of the block body, and
                // the leaf walk indexes CHARS again. For CJK text bytes and
                // chars diverge (one char is three bytes), so the anchor is
                // validated in char space and both forms are kept.
                let anchored_chars: Option<usize> = anchor_from_hunk(hunk).and_then(|start| {
                    let chars: Vec<char> = visible.chars().collect();
                    let end = start + old.chars().count();
                    if end > chars.len() || chars[start..end].iter().collect::<String>() != *old {
                        return None;
                    }
                    Some(start)
                });
                let anchored = anchored_chars.map(|start| {
                    let chars: Vec<char> = visible.chars().collect();
                    chars[..start].iter().map(|c| c.len_utf8()).sum::<usize>()
                });
                let start = if let Some(start) = anchored {
                    start
                } else {
                    let count = visible.matches(old.as_str()).count();
                    if count != 1 {
                        // A span that is contiguous only once the inline tokens
                        // are stripped is not one editable run: name the
                        // boundary instead of reporting a bare text-not-found.
                        if let Some(error) = span_crosses_boundary(paragraph_id, &body, old) {
                            hunk_problem(&mut problems, number, hunk, error);
                            continue;
                        }
                        let error = if count > 1 {
                            CollaborationError::new(
                                "text-ambiguous",
                                format!(
                                    "{paragraph_id}: {old:?} appears {count} times — resend with a \
                                     longer unique span"
                                ),
                            )
                        } else {
                            CollaborationError::new(
                                "text-not-found",
                                format!("{paragraph_id}: {old:?} not found in paragraph"),
                            )
                        };
                        hunk_problem(&mut problems, number, hunk, error);
                        continue;
                    }
                    visible
                        .find(old.as_str())
                        .expect("count == 1 implies a match")
                };
                let spans = match pending.iter().position(|(id, _)| id == paragraph_id) {
                    Some(index) => &mut pending[index].1,
                    None => {
                        pending.push((paragraph_id.clone(), Vec::new()));
                        &mut pending.last_mut().expect("just pushed a paragraph").1
                    }
                };
                let overlaps = spans.iter().any(|(other_start, other_old, _)| {
                    start < other_start + other_old.len() && *other_start < start + old.len()
                });
                if overlaps {
                    hunk_problem(
                        &mut problems,
                        number,
                        hunk,
                        CollaborationError::new(
                            "patch-hunks-invalid",
                            format!(
                                "{paragraph_id}: two replace hunks overlap at offset {start}; \
                                 merge them into one hunk"
                            ),
                        ),
                    );
                    continue;
                }
                // The build's island record must land inside ONE text leaf, so
                // the leaf is resolved (read-only) with the rest of validation.
                match island_leaf_for(workdir, paragraph_id, old, *leaf_index) {
                    Ok(Some((_part, resolved_leaf))) => {
                        // Defer: merge with any other occurrence in this leaf
                        // so one whole-leaf edit carries them all. The leaf walk
                        // indexes CHARS, so a text-searched hunk converts its
                        // byte offset before deferring.
                        let char_start =
                            anchored_chars.unwrap_or_else(|| visible[..start].chars().count());
                        leaf_spans.push((
                            paragraph_id.clone(),
                            resolved_leaf,
                            char_start,
                            old.chars().count(),
                            new.to_string(),
                        ));
                    }
                    Ok(None) => {}
                    Err(error) => {
                        hunk_problem(&mut problems, number, hunk, error);
                        continue;
                    }
                }
                if spans.is_empty() {
                    applied.push(json!({ "kind": "replace", "paragraph_id": paragraph_id }));
                }
                spans.push((start, old.clone(), new.clone()));
            }
            PatchHunk::Insert {
                insert_after,
                text,
                inherit,
            } => {
                let resolved_inherit = inherit.as_deref().unwrap_or(insert_after);
                let failure = require_comment_text_opt_in(insert_after, allow_comment_text)
                    .err()
                    .or_else(|| require_body_structure_mutable(insert_after).err())
                    .or_else(|| require_body_structure_mutable(resolved_inherit).err())
                    // The Rust build synthesises the paragraph from its inherit
                    // anchor, so a protected anchor is refused HERE rather than
                    // written into a draft that can never commit.
                    .or_else(|| {
                        require_unprotected(
                            workdir,
                            resolved_inherit,
                            "new paragraph cannot inherit protected structure",
                        )
                        .err()
                    })
                    .or_else(|| {
                        find_block(blocks, "p", insert_after).is_none().then(|| {
                            CollaborationError::new(
                                "paragraph-not-found",
                                format!("paragraph {insert_after} not found in the draft"),
                            )
                        })
                    });
                if let Some(error) = failure {
                    hunk_problem(&mut problems, number, hunk, error);
                    continue;
                }
                insert_specs.push((insert_after.clone(), text.clone(), inherit.clone()));
            }
            PatchHunk::Delete { paragraph_id } => {
                let mut failure = require_comment_text_opt_in(paragraph_id, allow_comment_text)
                    .err()
                    .or_else(|| require_body_structure_mutable(paragraph_id).err());
                if failure.is_none() && pending.iter().any(|(id, _)| id == paragraph_id) {
                    failure = Some(CollaborationError::new(
                        "document-patch-paragraph-repeated",
                        format!(
                            "{paragraph_id}: cannot replace and delete the same paragraph in one \
                             patch"
                        ),
                    ));
                }
                if failure.is_none() && find_block(blocks, "p", paragraph_id).is_none() {
                    failure = Some(CollaborationError::new(
                        "paragraph-not-found",
                        format!("paragraph {paragraph_id} not found in the draft"),
                    ));
                }
                if failure.is_none() {
                    failure = require_unprotected(
                        workdir,
                        paragraph_id,
                        "paragraph with protected structure cannot be deleted",
                    )
                    .err();
                }
                if let Some(error) = failure {
                    hunk_problem(&mut problems, number, hunk, error);
                    continue;
                }
                delete_ids.push(paragraph_id.clone());
                applied.push(json!({ "kind": "delete", "paragraph_id": paragraph_id }));
            }
        }
    }

    if !problems.is_empty() {
        // One broken hunk keeps its own diagnostic code; several broken hunks
        // surface together as one report.
        if problems.len() == 1 {
            return Err(problems[0].error());
        }
        let listed = problems
            .iter()
            .map(|problem| {
                format!(
                    "hunk #{} ({}): {}",
                    problem.hunk, problem.paragraph_id, problem.code
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        return Err(CollaborationError::new(
            "patch-hunks-invalid",
            format!(
                "{} of {} hunks are invalid — fix all of them and resend in ONE call: {listed}",
                problems.len(),
                hunks.len()
            ),
        ));
    }

    let mut blocks = blocks.to_vec();
    // Deletes first (ids, never block indices), then inserts, then replaces.
    for paragraph_id in &delete_ids {
        let Some(index) = find_paragraph_block(&blocks, paragraph_id) else {
            return Err(CollaborationError::new(
                "paragraph-not-found",
                format!("paragraph {paragraph_id} not found in the draft"),
            ));
        };
        blocks.remove(index);
        blocks.push(format!("<!--@delete id=\"{paragraph_id}\"-->"));
    }
    let mut insert_offsets: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    for (after_id, text, inherit) in &insert_specs {
        let Some(index) = find_paragraph_block(&blocks, after_id) else {
            return Err(CollaborationError::new(
                "paragraph-not-found",
                format!("paragraph {after_id} not found in the draft"),
            ));
        };
        let resolved_inherit = inherit.as_deref().unwrap_or(after_id);
        let temp = next_temp_id(&blocks);
        let offset = insert_offsets.get(after_id).copied().unwrap_or(0);
        blocks.insert(
            index + 1 + offset,
            format!("<!--@new temp=\"{temp}\" inherit=\"{resolved_inherit}\"/>\n{text}"),
        );
        insert_offsets.insert(after_id.clone(), offset + 1);
        applied.push(json!({
            "kind": "insert",
            "after_id": after_id,
            "temp_id": temp,
        }));
    }
    for (paragraph_id, spans) in &pending {
        let Some(index) = find_paragraph_block(&blocks, paragraph_id) else {
            return Err(CollaborationError::new(
                "paragraph-not-found",
                format!("paragraph {paragraph_id} not found in the draft"),
            ));
        };
        let marker = blocks[index].split('\n').next().unwrap_or("").to_string();
        let mut body = block_body(&blocks[index]);
        let mut ordered: Vec<&ReplaceSpan> = spans.iter().collect();
        ordered.sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));
        for (start, old, new) in ordered {
            body = replace_at(&body, *start, old, new, paragraph_id)?;
        }
        blocks[index] = if body.is_empty() {
            marker
        } else {
            format!("{marker}\n{body}")
        };
        for entry in applied.iter_mut() {
            if entry.get("kind").and_then(Value::as_str) == Some("replace")
                && entry.get("paragraph_id").and_then(Value::as_str) == Some(paragraph_id.as_str())
            {
                entry["hunks"] = json!(spans.len());
            }
        }
    }
    // One island per (paragraph, leaf), carrying EVERY replacement that lands
    // in that leaf. The island record rewrites the whole run, so two records
    // for the same leaf would each expect the pre-edit text and the second
    // would silently fail to apply — measured on a real patent: 34 occurrences
    // reported, 28 applied. Merging makes the reported count the applied count.
    leaf_spans.sort_by(|a, b| {
        a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(b.2.cmp(&a.2)) // descending offset within a leaf
    });
    let mut merged: std::collections::BTreeMap<(String, usize), Vec<LeafReplacement>> =
        std::collections::BTreeMap::new();
    for (paragraph_id, leaf_index, start, len, replacement) in leaf_spans {
        merged
            .entry((paragraph_id, leaf_index))
            .or_default()
            .push((start, len, replacement));
    }
    for ((paragraph_id, leaf_index), spans) in merged {
        // A leaf the walk cannot map is a refusal, never a half-recorded edit:
        // an island with an empty `old` would silently do nothing at build time
        // while the call still reported success.
        let (record_old, record_new) =
            merged_leaf_island(workdir, &paragraph_id, leaf_index, &spans).ok_or_else(|| {
                // The two cases are worth telling apart. An offset that cannot
                // be mapped usually means the paragraph already carries an
                // UNCOMMITTED edit that changed its length: the search reports
                // draft offsets while the island must address the template's
                // runs, and once the lengths differ those are different
                // coordinate spaces. Naming it tells the agent to commit first
                // instead of guessing.
                if paragraph_lengths_differ(workdir, &paragraph_id) {
                    CollaborationError::new(
                        "paragraph-has-uncommitted-length-change",
                        format!(
                            "{paragraph_id} already carries an uncommitted edit that changed its \
                             length, so draft offsets no longer address the template's runs; run \
                             commit_sync first, then edit again. Nothing was written"
                        ),
                    )
                } else {
                    CollaborationError::new(
                        "invalid-edit",
                        format!(
                            "{paragraph_id}: cannot map {} replacement(s) onto leaf {leaf_index}; \
                             nothing was written",
                            spans.len()
                        ),
                    )
                }
            })?;
        islands.push(docx2typed_core::prose::IslandEdit {
            part: "document".to_string(),
            paragraph_id,
            leaf_index,
            old: record_old,
            new: record_new,
            tracked: tracking.tracked,
            author: tracking.author.unwrap_or("").to_string(),
            date: tracking.date.unwrap_or("").to_string(),
        });
    }
    Ok(PatchPlan {
        blocks,
        applied,
        islands,
    })
}

/// True when a paragraph's DRAFT text and its template runs have different
/// visible lengths, i.e. an uncommitted edit already changed the text size.
fn paragraph_lengths_differ(workdir: &Path, paragraph_id: &str) -> bool {
    let package = match fs::read(workdir.join("_template.docx")) {
        Ok(bytes) => bytes,
        Err(_) => return false,
    };
    let Ok(inventory) = docx2typed_core::prose::enumerate_package_bytes(&package) else {
        return false;
    };
    let Some(part) = docx2typed_core::prose::part_for_paragraph_id(paragraph_id) else {
        return false;
    };
    let template_len: usize = inventory
        .leaves
        .iter()
        .filter(|leaf| leaf.part_key == part && leaf.paragraph_id == paragraph_id)
        .map(|leaf| leaf.text.chars().count())
        .sum();
    let Ok((_, blocks)) = read_blocks(workdir) else {
        return false;
    };
    let draft_len = blocks
        .iter()
        .find(|block| marker_of(block).is_some_and(|(_, id, _)| id == paragraph_id))
        .map(|block| visible_text(&block_body(block)).chars().count())
        .unwrap_or(template_len);
    draft_len != template_len
}

/// The whole-leaf form of one island edit, carrying every replacement that
/// lands in that leaf (offsets are char positions inside the paragraph's
/// visible text, applied right-to-left so earlier anchors never shift).
fn merged_leaf_island(
    workdir: &Path,
    paragraph_id: &str,
    leaf_index: usize,
    spans: &[LeafReplacement],
) -> Option<(String, String)> {
    let package = fs::read(workdir.join("_template.docx")).ok()?;
    let inventory = docx2typed_core::prose::enumerate_package_bytes(&package).ok()?;
    let part = docx2typed_core::prose::part_for_paragraph_id(paragraph_id)?;
    let mut cursor = 0usize;
    for leaf in inventory
        .leaves
        .iter()
        .filter(|leaf| leaf.part_key == part && leaf.paragraph_id == paragraph_id)
    {
        let mut chars: Vec<char> = leaf.text.chars().collect();
        let leaf_start = cursor;
        cursor += chars.len();
        if leaf.leaf_index != leaf_index {
            continue;
        }
        let mut ordered: Vec<&LeafReplacement> = spans.iter().collect();
        ordered.sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));
        for (start, len, replacement) in ordered {
            let local = start.checked_sub(leaf_start)?;
            let end = local + len;
            if end > chars.len() {
                return None;
            }
            let mut next: Vec<char> = Vec::with_capacity(chars.len() + replacement.len());
            next.extend_from_slice(&chars[..local]);
            next.extend(replacement.chars());
            next.extend_from_slice(&chars[end..]);
            chars = next;
        }
        return Some((leaf.text.clone(), chars.into_iter().collect()));
    }
    None
}

/// The draft's opaque revision token: the canonical edit-body hash of
/// `edit.md` — the value `document_read` returns and `base_revision` checks.
fn current_revision(workdir: &Path) -> Result<String, CollaborationError> {
    classify_edit_state(workdir)
        .map(|state| state.edit_body_sha256)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))
}

/// The immediately-previous committed view: the projection hash the last commit
/// bound in `edit.state.json`. A caller that read before that commit still
/// holds a legitimate (if older) revision token.
fn base_projection_sha256(workdir: &Path) -> String {
    fs::read_to_string(workdir.join(STATE_FILE))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|state| {
            state
                .get("base_projection_sha256")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default()
}

/// True when every hunk still resolves in the CURRENT draft, i.e. a stale
/// base_revision is harmless because the intervening change touched other
/// paragraphs (mirror of `_hunks_still_applicable`).
fn hunks_still_applicable(blocks: &[String], hunks: &[PatchHunk]) -> bool {
    for hunk in hunks {
        let Some(index) = find_paragraph_block(blocks, hunk.paragraph_id()) else {
            return false;
        };
        if let PatchHunk::Replace { old, .. } = hunk {
            let body = block_body(&blocks[index]);
            if visible_text(&body).matches(old.as_str()).count() != 1 {
                return false;
            }
        }
    }
    true
}

/// `base_revision` rides INSIDE the transaction: an exact retry of a completed
/// patch replays from the store ledger without reaching this check, so a lost
/// response can always be safely re-sent (mirror of Python's in-transaction
/// check).
fn check_base_revision(
    workdir: &Path,
    token: &str,
    blocks: &[String],
    hunks: &[PatchHunk],
    warnings: &mut Vec<String>,
) -> Result<(), CollaborationError> {
    let current = current_revision(workdir)?;
    if token == current {
        return Ok(());
    }
    let previous = base_projection_sha256(workdir);
    if token != previous {
        return Err(CollaborationError::new(
            "stale-document-view",
            format!(
                "base_revision {token:?} matches neither the current draft ({current:?}) nor the \
                 previous committed view ({previous:?}) — re-read with document_read/document_search \
                 and re-patch (current_revision is in data.current_revision)"
            ),
        ));
    }
    if hunks_still_applicable(blocks, hunks) {
        warnings.push(
            "stale-document-view-healed: the draft changed since your read but every hunk anchor \
             still resolves; the patch was applied against the current draft"
                .to_string(),
        );
        return Ok(());
    }
    Err(CollaborationError::new(
        "stale-document-view",
        format!(
            "base_revision {token:?} does not match the current draft ({current:?}) and at least \
             one hunk no longer resolves — re-read with document_read/document_search and re-patch \
             (current revision is in data.current_revision)"
        ),
    ))
}

/// The `document_patch` tool body: apply a batch of edits to the draft in one
/// atomic call — the file-like editing surface over replace / insert / delete.
///
/// Exactly one of `hunks` / `diff`; the unified-diff form is not implemented in
/// this build and fails with `invalid-arguments`. A paragraph may carry several
/// non-overlapping replace hunks (like `git apply`); overlapping spans are
/// rejected. Every hunk is validated against the in-memory block list before
/// anything is written, so a rejected batch leaves the draft byte-identical.
/// Writes the draft only — run diff_preview, then commit_sync.
pub fn document_patch(
    workdir: &Path,
    hunks: Option<&[Value]>,
    diff: Option<&str>,
    base_revision: Option<&str>,
    allow_comment_text: bool,
    tracking: EditTracking<'_>,
) -> Result<Value, CollaborationError> {
    if hunks.is_none() == diff.is_none() {
        return Err(CollaborationError::new(
            "invalid-arguments",
            "provide exactly one of hunks or diff",
        ));
    }
    if diff.is_some() {
        return Err(CollaborationError::new(
            "invalid-arguments",
            "the unified-diff form of document_patch is not implemented yet; pass hunks \
             (replace: {paragraph_id, old, new}; insert: {insert_after, text}; delete: {delete})",
        ));
    }
    crate::collab::ensure_agent_ready(workdir)?;
    // A `match_ref` hunk names a span instead of copying its text, so it is
    // expanded to an ordinary replace hunk here — against the CURRENT draft,
    // which is where a stale reference is caught rather than silently applied
    // to the wrong offset.
    let expanded = expand_match_ref_hunks(workdir, hunks.unwrap_or_default())?;
    let normalized = normalize_patch_hunks(&expanded)?;
    if normalized.is_empty() {
        return Err(CollaborationError::new(
            "invalid-arguments",
            "hunks must not be empty",
        ));
    }
    let original = read_draft(workdir)?;
    let (header, blocks) = split_blocks(&original)?;
    let revision_before = current_revision(workdir)?;
    let mut warnings: Vec<String> = Vec::new();
    if let Some(token) = base_revision {
        check_base_revision(workdir, token, &blocks, &normalized, &mut warnings)?;
    }
    let plan = plan_patch(workdir, &blocks, &normalized, allow_comment_text, tracking)?;
    write_draft(workdir, &header, &plan.blocks)?;
    if let Err(error) = append_island_edits(workdir, &plan.islands) {
        // edit.md is the contract surface: a sidecar failure must not leave a
        // batch the caller believes failed sitting in the draft.
        let _ = fs::write(workdir.join(PROJECTION_FILE), &original);
        return Err(error);
    }
    let revision_after = current_revision(workdir)?;
    let mut affected: Vec<String> = plan
        .applied
        .iter()
        .filter_map(|entry| {
            entry
                .get("paragraph_id")
                .or_else(|| entry.get("temp_id"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    affected.sort();
    affected.dedup();
    Ok(json!({
        "applied": plan.applied,
        "affected_paragraph_ids": affected,
        "document_state": {
            "revision_before": revision_before,
            "revision_after": revision_after,
            "draft": "dirty",
        },
        "warnings": warnings,
        "draft": "dirty",
        "next": "diff_preview to inspect style ownership, then commit_sync",
    }))
}

/// `diff_preview` payload: the freshness state plus per-paragraph changed
/// ids when dirty. Hunk/style-ownership reporting is a declared tracer
/// deferral (the tracer draft model carries no style regions).
fn replace_in_body_at_visible(
    body: &str,
    start_offset: usize,
    old: &str,
    new: &str,
    paragraph_id: &str,
) -> Result<String, CollaborationError> {
    const TOKEN_START: char = '\u{27e6}';
    const TOKEN_END: char = '\u{27e7}';

    let chars: Vec<char> = body.chars().collect();
    let old_chars: Vec<char> = old.chars().collect();
    let mut out = String::new();
    let mut cursor = 0usize;
    let mut visible_offset = 0usize;
    let mut replaced = false;

    while cursor < chars.len() {
        if chars[cursor] == TOKEN_START {
            let Some(end_rel) = chars[cursor..].iter().position(|c| *c == TOKEN_END) else {
                return Err(CollaborationError::new(
                    "edit-grammar-invalid",
                    "unclosed placeholder in draft",
                ));
            };
            out.extend(&chars[cursor..cursor + end_rel + 1]);
            cursor += end_rel + 1;
            continue;
        }
        let next_token = chars[cursor..]
            .iter()
            .position(|c| *c == TOKEN_START)
            .unwrap_or(chars.len() - cursor);
        let prose_chars = &chars[cursor..cursor + next_token];
        let prose_len = prose_chars.len();
        if !replaced
            && visible_offset <= start_offset
            && start_offset + old_chars.len() <= visible_offset + prose_len
        {
            let local_start = start_offset - visible_offset;
            if prose_chars[local_start..local_start + old_chars.len()] == old_chars[..] {
                out.extend(&prose_chars[..local_start]);
                out.push_str(new);
                out.extend(&prose_chars[local_start + old_chars.len()..]);
                replaced = true;
                cursor += next_token;
                visible_offset += prose_len;
                continue;
            }
        }
        out.extend(prose_chars);
        visible_offset += prose_len;
        cursor += next_token;
    }
    if !replaced {
        return Err(CollaborationError::new(
            "text-not-found",
            format!("{paragraph_id}: text {old:?} not found at visible offset {start_offset}"),
        ));
    }
    Ok(out)
}

#[derive(Clone, Debug)]
pub struct DocumentReplaceArgs<'a> {
    pub find: &'a str,
    pub replace: &'a str,
    pub scope: &'a str,
    pub regex: bool,
    pub case_sensitive: bool,
    pub expected_matches: Option<usize>,
    pub allow_comment_text: bool,
    pub base_revision: Option<&'a str>,
    pub dry_run: bool,
}

/// The parts a `document_replace` call can actually reach, and a plain-language
/// statement of that reach.
///
/// The draft projection carries ONE part today — the main document story. The
/// header, footer, footnote, endnote and comment parts are extracted and built
/// but are not part of the editable projection, so a replace cannot touch them.
/// Reporting this explicitly matters more than the limit itself: an agent that
/// reads only `matches: 0` has no way to tell "the word is absent" from "the
/// word is in a part this call never scans".
fn replacement_coverage(scope: &str) -> Value {
    let part = match scope {
        s if s.starts_with("header")
            || s.starts_with("footer")
            || s.starts_with("footnotes")
            || s.starts_with("endnotes")
            || s.starts_with("comments") =>
        {
            s.split('.').next().unwrap_or(s)
        }
        _ => "document",
    };
    let known = ["header1", "footer1", "footnotes", "endnotes", "comments"];
    let unscanned: Vec<&str> = known.iter().copied().filter(|p| *p != part).collect();
    serde_json::json!({
        "scanned_parts": [part],
        "unscanned_parts": unscanned,
        "note": "the editable projection covers the main document story only; \
                 header, footer, footnote, endnote, comment and text-box parts are \
                 built but not reachable by this call, so a `0` match count does not \
                 mean the phrase is absent from the file",
    })
}

pub fn document_replace(
    workdir: &Path,
    args: DocumentReplaceArgs<'_>,
    tracking: EditTracking<'_>,
) -> Result<Value, CollaborationError> {
    if args.find.is_empty() {
        return Err(CollaborationError::new(
            "replace-find-empty",
            "find must not be empty",
        ));
    }
    let original = read_draft(workdir)?;
    let (header, mut blocks) = split_blocks(&original)?;
    let revision_before = current_revision(workdir)?;
    if let Some(base_rev) = args.base_revision {
        if base_rev != revision_before {
            return Err(CollaborationError::new(
                "stale-document-view",
                format!(
                    "base_revision {base_rev:?} does not match the current draft ({revision_before:?})"
                ),
            ));
        }
    }
    // The replace scope is the DRAFT's own paragraphs, not just the baseline's:
    // a paragraph inserted earlier in this session is a
    // `<!--@new temp="N1"-->` block, and `scope_paragraph_ids` only names
    // `<!--@p` records — so a straight call silently ignores it (measured: the
    // reference reports 0 matches there too). Ask the scope for the baseline
    // ids, then union in every draft block the scope's family covers, so
    // "replace everywhere" means everywhere in the document the agent sees.
    let baseline_scope = docx2typed_core::document_search::scope_paragraph_ids(&blocks, args.scope)
        .map_err(|e| CollaborationError::new("replace-scope-invalid", e.to_string()))?;
    let allowed: Vec<String> = {
        let mut ids = baseline_scope.clone();
        let universe = matches!(args.scope, "body" | "" | "/body" | "/" | "all");
        if universe {
            for block in &blocks {
                if let Some((kind, id, _)) = marker_of(block) {
                    if (kind == "new" || kind == "p") && !ids.contains(&id) {
                        ids.push(id);
                    }
                }
            }
        }
        ids
    };
    if (args.scope == "comments"
        || args.scope == "/comments"
        || allowed.iter().any(|id| id.starts_with("comments.")))
        && !args.allow_comment_text
    {
        return Err(CollaborationError::new(
            "comment-text-requires-opt-in",
            "scope touches comment paragraphs; pass allow_comment_text=true only when the user explicitly asked to edit reviewer comment text",
        ));
    }

    let mut plan: Vec<Value> = Vec::new();
    let mut unsafe_matches: Vec<Value> = Vec::new();
    let inventory = docx2typed_core::prose::enumerate_package(&workdir.join("_template.docx")).ok();

    let regex_pattern = if args.regex {
        Some(
            regex::RegexBuilder::new(args.find)
                .case_insensitive(!args.case_sensitive)
                .build()
                .map_err(|e| {
                    CollaborationError::new(
                        "replace-regex-invalid",
                        format!("invalid regular expression: {e}"),
                    )
                })?,
        )
    } else {
        None
    };

    let needle = if args.case_sensitive {
        args.find.to_string()
    } else {
        args.find.to_lowercase()
    };

    for block in &blocks {
        let Some((kind, id, _)) = marker_of(block) else {
            continue;
        };
        if kind != "p" && kind != "new" {
            continue;
        }
        if !allowed.contains(&id) {
            continue;
        }
        let body = block_body(block);
        let (flat, boundaries) = match docx2typed_core::document_search::body_boundaries(&body) {
            Ok(res) => res,
            Err(e) => {
                return Err(CollaborationError::new(
                    "edit-grammar-invalid",
                    e.to_string(),
                ))
            }
        };
        if flat.is_empty() {
            continue;
        }

        let flat_chars: Vec<char> = flat.chars().collect();
        let mut matches: Vec<(usize, String)> = Vec::new();
        if let Some(re) = &regex_pattern {
            for m in re.find_iter(&flat) {
                let char_offset = flat[..m.start()].chars().count();
                matches.push((char_offset, m.as_str().to_string()));
            }
        } else {
            let haystack: Vec<char> = if args.case_sensitive {
                flat_chars.clone()
            } else {
                flat.to_lowercase().chars().collect()
            };
            let needle_chars: Vec<char> = needle.chars().collect();
            let mut cursor = 0usize;
            while cursor + needle_chars.len() <= haystack.len() {
                if haystack[cursor..cursor + needle_chars.len()] == needle_chars[..] {
                    let matched: String = flat_chars[cursor..cursor + needle_chars.len()]
                        .iter()
                        .collect();
                    matches.push((cursor, matched));
                    cursor += needle_chars.len();
                } else {
                    cursor += 1;
                }
            }
        }

        for (offset, matched_text) in matches {
            let end = offset + matched_text.chars().count();
            let crossed: Vec<String> = boundaries
                .iter()
                .filter(|(b_off, _)| *b_off > offset && *b_off < end)
                .map(|(_, k)| k.clone())
                .collect();
            let region = if !crossed.is_empty() {
                "mixed".to_string()
            } else {
                docx2typed_core::document_search::region_at(&boundaries, offset).to_string()
            };
            let match_ref = docx2typed_core::document_search::encode_match_ref(
                &id,
                offset,
                end,
                &matched_text,
                &revision_before,
            );
            let entry = serde_json::json!({
                "paragraph_id": id,
                "offset": offset,
                "matched_text": matched_text,
                "region": region,
                "patchable": crossed.is_empty(),
                "normalized": false,
                "match_ref": match_ref,
            });
            // A match the flattened paragraph text can see may still span
            // several `<w:t>` leaves: `("长效润", "滑材料")` yields a visible
            // `润滑` that no single leaf contains. Editing it would require
            // splitting a run, which is exactly the surgery that produced
            // Python's `unbound prefix` corruption on the same input. Refuse
            // it loudly instead of reporting a match that cannot be applied.
            let synthesised = kind == "new";
            if !synthesised
                && !inventory.as_ref().is_some_and(|inv| {
                    leaf_in_inventory_covers(inv, &id, offset, matched_text.chars().count())
                })
            {
                let mut u = entry.clone();
                u["reason"] = serde_json::json!("match-spans-leaves");
                u["capability"] = serde_json::json!("word.text.replace.cross-leaf");
                u["detail"] = serde_json::json!(
                    "the visible match spans more than one text run; rewriting it would \
                     split a run and cannot be done losslessly"
                );
                unsafe_matches.push(u);
            } else if !crossed.is_empty() {
                let mut u = entry.clone();
                u["reason"] = serde_json::json!("edit-span-crosses-revision-boundary");
                u["boundary_crossings"] = serde_json::json!(crossed);
                unsafe_matches.push(u);
            } else if region == "insert" && !tracking.tracked {
                let mut u = entry.clone();
                u["reason"] = serde_json::json!("revision-text-mutated-in-direct-mode");
                u["fix"] = serde_json::json!({ "track": true });
                unsafe_matches.push(u);
            } else {
                plan.push(entry);
            }
        }
    }

    let total = plan.len() + unsafe_matches.len();
    if !unsafe_matches.is_empty() {
        let cross_leaf: Vec<Value> = unsafe_matches
            .iter()
            .filter(|u| u.get("reason") == Some(&serde_json::json!("match-spans-leaves")))
            .cloned()
            .collect();
        if !cross_leaf.is_empty() {
            return Err(CollaborationError::new(
                "replace-cross-leaf-matches",
                format!(
                    "{} of {total} matches span more than one text run and cannot be rewritten \
                     losslessly; nothing was written. Address each side of the run boundary \
                     individually, or use a surrounding span that fits in one run. \
                     cross_leaf_matches={}",
                    cross_leaf.len(),
                    serde_json::to_string(&cross_leaf).unwrap_or_default()
                ),
            ));
        }
        let mode_fix = unsafe_matches.iter().any(|u| {
            u.get("reason") == Some(&serde_json::json!("revision-text-mutated-in-direct-mode"))
        });
        if mode_fix {
            return Err(CollaborationError::new(
                "revision-text-mutated-in-direct-mode",
                "matches sit inside existing tracked insertions and direct mode cannot change revision text; resend with track=true",
            ));
        }
        return Err(CollaborationError::new(
            "replace-unsafe-matches",
            format!(
                "matches sit across a revision boundary and cannot be rewritten in one pass; \
                 nothing was written. unsafe={}",
                serde_json::to_string(&unsafe_matches).unwrap_or_default()
            ),
        ));
    }

    if let Some(expected) = args.expected_matches {
        if total != expected {
            return Err(CollaborationError::new(
                "replace-expected-matches-mismatch",
                format!("expected {expected} match(es) but found {total}; nothing was written"),
            ));
        }
    }

    if total == 0 {
        return Ok(serde_json::json!({
            "scope": args.scope,
        "coverage": replacement_coverage(args.scope),
            "coverage": replacement_coverage(args.scope),
            "find": args.find,
            "matches": 0,
            "changed": 0,
            "noop_reason": "no-match",
            "match_plan": [],
            "document_state": {
                "revision_before": revision_before,
                "revision_after": revision_before,
                "draft": "clean",
            },
        }));
    }

    if args.dry_run {
        return Ok(serde_json::json!({
            "scope": args.scope,
        "coverage": replacement_coverage(args.scope),
            "coverage": replacement_coverage(args.scope),
            "find": args.find,
            "matches": total,
            "changed": 0,
            "dry_run": true,
            "match_plan": plan,
            "document_state": {
                "revision_before": revision_before,
                "revision_after": revision_before,
                "draft": "clean",
            },
        }));
    }

    let mut by_p: std::collections::BTreeMap<String, Vec<(usize, String)>> =
        std::collections::BTreeMap::new();
    for entry in &plan {
        let pid = entry["paragraph_id"].as_str().unwrap().to_string();
        let off = entry["offset"].as_u64().unwrap() as usize;
        let matched = entry["matched_text"].as_str().unwrap().to_string();
        by_p.entry(pid).or_default().push((off, matched));
    }

    for block in &mut blocks {
        let Some((kind, id, marker)) = marker_of(block) else {
            continue;
        };
        if kind != "p" && kind != "new" {
            continue;
        }
        let Some(mut matches) = by_p.remove(&id) else {
            continue;
        };
        matches.sort_by_key(|(off, _)| std::cmp::Reverse(*off));
        let mut body = block_body(block);
        for (off, matched) in matches {
            body = replace_in_body_at_visible(&body, off, &matched, args.replace, &id)?;
        }
        *block = if body.is_empty() {
            marker
        } else {
            format!("{marker}\n{body}")
        };
    }

    // Record the edits now, exactly as `replace_text` does: leaf-level island
    // records for baseline paragraphs (the build applies them to the template
    // bytes) and a structure-sidecar body update for paragraphs synthesised in
    // this session (which have no baseline leaf to rewrite). `commit_sync`
    // then settles `typed.md` from this draft; without these records a match
    // would be reported as changed yet silently dropped by the build — the
    // failure the reference ships in two forms (stale inserted paragraphs and
    // `unbound prefix` corruption on a cross-run span, both measured).
    let mut island_edits: Vec<docx2typed_core::prose::IslandEdit> = Vec::new();
    if let Some(inv) = &inventory {
        for entry in &plan {
            let pid = entry["paragraph_id"].as_str().unwrap_or("");
            if blocks.iter().any(|block| {
                marker_of(block).is_some_and(|(kind, id, _)| kind == "new" && id == pid)
            }) {
                continue;
            }
            for leaf in inv
                .leaves
                .iter()
                .filter(|l| l.part_key == "document" && l.paragraph_id == pid)
            {
                let hit = if let Some(re) = &regex_pattern {
                    re.is_match(&leaf.text)
                } else if args.case_sensitive {
                    leaf.text.contains(args.find)
                } else {
                    leaf.text.to_lowercase().contains(&args.find.to_lowercase())
                };
                if !hit {
                    continue;
                }
                let old_text = leaf.text.clone();
                let new_text = apply_replacement(
                    &regex_pattern,
                    args.find,
                    args.replace,
                    &old_text,
                    args.case_sensitive,
                );
                if old_text == new_text
                    || island_edits
                        .iter()
                        .any(|e| e.paragraph_id == pid && e.leaf_index == leaf.leaf_index)
                {
                    continue;
                }
                island_edits.push(docx2typed_core::prose::IslandEdit {
                    part: "document".to_string(),
                    paragraph_id: pid.to_string(),
                    leaf_index: leaf.leaf_index,
                    old: old_text,
                    new: new_text,
                    tracked: tracking.tracked,
                    author: tracking.author.unwrap_or("").to_string(),
                    date: tracking.date.unwrap_or("").to_string(),
                });
            }
        }
    }
    // Rewrite the synthesised paragraphs' bodies in the structure sidecar.
    let mut structure = docx2typed_core::prose::load_structure(workdir)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    let mut structure_changed = false;
    let new_ids: Vec<String> = plan
        .iter()
        .filter_map(|e| e["paragraph_id"].as_str().map(str::to_string))
        .filter(|id| {
            blocks.iter().any(|block| {
                marker_of(block).is_some_and(|(kind, bid, _)| kind == "new" && &bid == id)
            })
        })
        .collect();
    for pid in new_ids {
        let Some(paragraph) = structure
            .new_paragraphs
            .iter_mut()
            .find(|p| p.paragraph_id == pid)
        else {
            // The paragraph was inserted in this session but its structure
            // record is still draft-only; `commit_sync` synthesises it from the
            // draft, so the body update rides along in the draft block.
            continue;
        };
        let updated = apply_replacement(
            &regex_pattern,
            args.find,
            args.replace,
            &paragraph.body,
            args.case_sensitive,
        );
        if paragraph.body != updated {
            paragraph.body = updated;
            structure_changed = true;
        }
    }
    // Every changed paragraph must be recorded by a sidecar the build reads.
    let recorded: std::collections::BTreeSet<String> = island_edits
        .iter()
        .map(|e| e.paragraph_id.clone())
        .chain(
            structure
                .new_paragraphs
                .iter()
                .map(|p| p.paragraph_id.clone()),
        )
        .collect();
    // A paragraph synthesised in this session has no baseline leaf and no
    // structure record yet: `commit_sync` writes it from the draft `@new`
    // block, so the draft block itself satisfies the requirement.
    let draft_new: std::collections::BTreeSet<String> = blocks
        .iter()
        .filter_map(|block| {
            let (kind, id, _) = marker_of(block)?;
            (kind == "new").then_some(id)
        })
        .collect();
    let uncovered: Vec<String> = plan
        .iter()
        .filter_map(|e| e["paragraph_id"].as_str().map(str::to_string))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter(|id| !recorded.contains(id) && !draft_new.contains(id))
        .collect();
    if !uncovered.is_empty() {
        return Err(CollaborationError::new(
            "replace-unrecorded-match",
            format!(
                "{} match(es) cannot be recorded for the build: {}; nothing was written",
                uncovered.len(),
                uncovered.join(", ")
            ),
        ));
    }
    if !island_edits.is_empty() {
        append_island_edits(workdir, &island_edits)?;
    }
    if structure_changed {
        docx2typed_core::prose::save_structure(workdir, &structure)
            .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))?;
    }
    write_draft(workdir, &header, &blocks)?;
    let revision_after = current_revision(workdir)?;
    Ok(serde_json::json!({
        "scope": args.scope,
        "coverage": replacement_coverage(args.scope),
        "find": args.find,
        "matches": total,
        "changed": total,
        "match_plan": plan,
        "document_state": {
            "revision_before": revision_before,
            "revision_after": revision_after,
            "draft": "dirty",
        },
    }))
}
pub fn diff_preview(workdir: &Path) -> Result<Value, CollaborationError> {
    let state = if workdir.join(STATE_FILE).is_file() {
        classify_edit_state(workdir)
            .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?
    } else {
        EditState {
            state: "clean".to_string(),
            typed_sha256: String::new(),
            edit_body_sha256: String::new(),
        }
    };
    if state.state != "dirty" {
        return Ok(serde_json::json!({ "state": state.state, "changes": [] }));
    }
    let (_, blocks) = read_blocks(workdir)?;
    let changed = compute_changed(workdir, &blocks)?;
    Ok(serde_json::json!({
        "state": "dirty",
        "edit_mode": "direct",
        "changed_paragraph_ids": changed,
        "hunks": [],
        "warnings": [],
    }))
}

/// `commit_sync` body: apply the draft projection to typed.md and reset the
/// projection binding so the workdir is clean again. The canonical
/// typed.md records `<!--@p id=... base=...-->` blocks with visible text —
/// the tracer's typed record (mirror of Python's `sync_edit_projection`
/// surface, without style regeneration). Only paragraphs whose draft body
/// differs from the committed typed record are reported as changed; a
/// no-change draft returns an empty list without touching typed.md.
pub fn apply_projection(workdir: &Path) -> Result<Vec<String>, CollaborationError> {
    apply_projection_with_options(workdir, false, None, None)
}

pub fn apply_projection_with_options(
    workdir: &Path,
    tracked: bool,
    author: Option<&str>,
    date: Option<&str>,
) -> Result<Vec<String>, CollaborationError> {
    if !workdir.join(PROJECTION_FILE).is_file() {
        // No draft: a clean no-op commit (mirror of Python's sync on a
        // clean workdir, which reports no changed paragraphs).
        return Ok(Vec::new());
    }
    let (_, blocks) = read_blocks(workdir)?;
    let changed = compute_changed(workdir, &blocks)?;
    if changed.is_empty() {
        return Ok(changed);
    }
    // Formal paragraph ids for this commit's new blocks, in draft order —
    // mirror of Python's `_next_paragraph_id` (`edit_sync.py:1028`): the next
    // free `P<n>` above every id already in use. `used` is shared across the
    // commit, so two inserts become P1 and P2 rather than both P1.
    //
    // Python reallocates the draft's temp id (`N1`) to this formal id at
    // commit; the temp id is draft-local bookkeeping and never survives into
    // `typed.md`.
    let mut formal_ids: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    {
        let mut used: std::collections::BTreeSet<String> = blocks
            .iter()
            .filter_map(|block| {
                let (kind, id, _) = marker_of(block)?;
                (kind != "new").then_some(id)
            })
            .collect();
        for block in &blocks {
            let Some((kind, temp, _)) = marker_of(block) else {
                continue;
            };
            if kind != "new" {
                continue;
            }
            let next = used
                .iter()
                .filter_map(|id| id.strip_prefix('P').and_then(|n| n.parse::<i64>().ok()))
                .max()
                .map_or(0, |max| max + 1);
            let formal = format!("P{next}");
            used.insert(formal.clone());
            formal_ids.insert(temp, formal);
        }
    }
    // Everything downstream addresses paragraphs by their committed id.
    let formal_to_temp: std::collections::BTreeMap<String, String> = formal_ids
        .iter()
        .map(|(temp, formal)| (formal.clone(), temp.clone()))
        .collect();
    let changed: Vec<String> = changed
        .into_iter()
        .map(|id| formal_ids.get(&id).cloned().unwrap_or(id))
        .collect();
    // Capture the pre-commit paragraph bodies so committed text edits can be
    // recorded as island edits (the build/verify sidecar). Whole-paragraph
    // replacement maps to leaf_index 0; validate_edit rejects it at build
    // time when the old text spans multiple leaves — fail-closed, not silent.
    let typed_before = fs::read_to_string(workdir.join("typed.md"))
        .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))?;
    let mut before = parse_typed_paragraphs(&typed_before);
    // Rust-extracted workdirs carry a header-only typed.md; the pre-commit
    // bodies live in the template inventory (same source compute_changed uses).
    for paragraph in inventory_paragraphs(workdir)? {
        before.entry(paragraph.id).or_insert(paragraph.visible_text);
    }
    let typed_path = workdir.join("typed.md");
    let mut typed = String::new();
    let mut wrote_header = false;
    if typed_path.is_file() {
        let text = fs::read_to_string(&typed_path)
            .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))?;
        for line in text.lines() {
            if line.trim_start().starts_with("<!--@typed") && !wrote_header {
                typed.push_str(line);
                typed.push('\n');
                wrote_header = true;
            }
        }
    }
    if !wrote_header {
        typed.push_str(
            "<!--@typed schema=\"1\" format=\"format.json\" styles=\"styles.json\" \
             template=\"_template.docx\" source=\"mcp-commit\"/>
",
        );
    }
    // A delete marker outlives the draft only for a BASELINE paragraph.
    // Deleting a paragraph that was synthesised in this lineage simply removes
    // it, so the marker would be stale bookkeeping addressing an id that exists
    // nowhere. The reference drops it: after insert -> commit -> delete ->
    // commit, Python's typed.md carries no `@delete` at all. `before` cannot
    // answer this question (it is seeded from the previous typed.md, which
    // already lists synthesised ids), so the template inventory is the source.
    let baseline_ids: std::collections::BTreeSet<String> = inventory_paragraphs(workdir)?
        .into_iter()
        .map(|paragraph| paragraph.id)
        .collect();
    for block in &blocks {
        let Some((kind, id, marker)) = marker_of(block) else {
            continue;
        };
        match kind.as_str() {
            "p" => {
                // The draft speaks `⟦token …⟧`; typed.md speaks `<docx-opaque
                // …/>`. Convert at the write, so the committed file is in the
                // reference's dialect and a later commit does not diff the two.
                let body = typed_markup_from_draft(&block_body(block));
                typed.push_str(&format!("{}\n{body}\n", committed_marker(&id, &marker)));
            }
            "new" => {
                // A new block is committed under its FORMAL id (Python
                // reallocates the temp id at commit), with its inherit anchor
                // riding along so a later commit can re-synthesise the
                // paragraph from the same baseline.
                let body = typed_markup_from_draft(&block_body(block));
                let formal = formal_ids.get(&id).map(String::as_str).unwrap_or(&id);
                typed.push_str(&format!("{}\n{body}\n", committed_marker(formal, &marker)));
            }
            "delete" if baseline_ids.contains(&id) => {
                typed.push_str(&format!("<!--@delete id=\"{id}\"-->"));
                typed.push('\n');
            }
            // A deleted non-baseline paragraph leaves no trace: it never
            // existed in the baseline, so there is nothing to record.
            _ => {}
        }
    }
    fs::write(&typed_path, typed)
        .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))?;
    // Rebind the projection to the NEW typed.md: edit.md is now the
    // canonical projection (its body is unchanged), so only the sidecar
    // hashes move — the workdir classifies clean again.
    let typed_sha256 = file_sha256(&typed_path)
        .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))?;
    // Rebind the projection: regenerate the @edit header with the NEW
    // base-typed-sha256 (mirror of Python's projection refresh after sync)
    // while keeping the draft body — header, sidecar, and typed.md then
    // agree and the workdir classifies clean.
    let draft_text = fs::read_to_string(workdir.join(PROJECTION_FILE))
        .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))?;
    let lines = py_splitlines(&draft_text);
    // Canonicalize the committed draft (mirror of the Python post-sync
    // projection, which is regenerated from the synced document): new-block
    // markers become canonical `<!--@p` records (formal paragraph
    // allocation stays deferred), and committed delete markers are dropped.
    // A repeat commit is then a clean no-op.
    let mut kept: Vec<String> = Vec::with_capacity(lines.len());
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if index > 0 && trimmed.starts_with("<!--@delete") {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("<!--@new temp=\"") {
            let id = rest.split('"').next().unwrap_or("");
            let formal = formal_ids.get(id).map(String::as_str).unwrap_or(id);
            kept.push(committed_marker(formal, trimmed));
            continue;
        }
        kept.push(line.clone());
    }
    let mut lines = kept;
    // Rebind the @edit header to the NEW typed.md; the body hash binds the
    // canonicalized draft body (header excluded), so header, sidecar, and
    // typed.md agree and the workdir classifies clean again.
    let body_hash = edit_body_sha256(&lines.join("\n"))
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    if let Some(first) = lines.first_mut() {
        *first = format!(
            "<!--@edit schema=\"1\" sync-contract=\"1\" base-typed-sha256=\"{}\" \
             base-projection-sha256=\"{}\" segmentation=\"{}\"-->",
            typed_sha256, body_hash, SEGMENTATION_CONTRACT
        );
    }
    let final_text = lines.join("\n");
    fs::write(workdir.join(PROJECTION_FILE), final_text)
        .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))?;
    let state = state_json(&typed_sha256, &body_hash);
    fs::write(
        workdir.join(STATE_FILE),
        serde_json::to_vec_pretty(&state).expect("serializes"),
    )
    .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))?;
    // Record the committed text changes as island edits so build_docx applies
    // them to the template bytes and verify_output re-checks them. Existing
    // island records for the same paragraph are replaced (latest commit wins).
    // Store replay re-runs this mutation on an already-committed copy where
    // typed.md is final and `changed` recomputes empty — skip the save there
    // so the recorded edits survive byte-identical replay.
    let mut islands = docx2typed_core::prose::load_islands(workdir)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    let mut structure = docx2typed_core::prose::load_structure(workdir)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    let mut recorded = 0usize;
    let mut structured = false;
    let inventory = docx2typed_core::prose::enumerate_package(&workdir.join("_template.docx")).ok();
    let committed = parse_typed_paragraphs(
        &fs::read_to_string(workdir.join("typed.md"))
            .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))?,
    );
    for id in &changed {
        let Some(new_body) = committed.get(id) else {
            // No committed @p record: the draft removed this paragraph.
            // Deletion has no baseline byte range to rewrite either, so it
            // is recorded as a structural edit (not rendered => gone).
            if structure
                .new_paragraphs
                .iter()
                .any(|paragraph| &paragraph.paragraph_id == id)
            {
                // A paragraph that was itself synthesised is dropped from
                // the sidecar instead of becoming a deletion.
                structure
                    .new_paragraphs
                    .retain(|paragraph| &paragraph.paragraph_id != id);
                structured = true;
            } else if !structure
                .deleted_paragraphs
                .iter()
                .any(|paragraph| &paragraph.paragraph_id == id)
            {
                structure
                    .deleted_paragraphs
                    .push(docx2typed_core::prose::DeletedParagraph {
                        part: "document".to_string(),
                        paragraph_id: id.clone(),
                    });
                structured = true;
            }
            continue;
        };
        if let Some(paragraph) = structure
            .new_paragraphs
            .iter_mut()
            .find(|paragraph| &paragraph.paragraph_id == id)
        {
            // Re-editing a synthesised paragraph rewrites its body in place.
            if &paragraph.body != new_body {
                paragraph.body = new_body.clone();
                structured = true;
            }
            continue;
        }
        let Some(old_body) = before.get(id) else {
            // Not in the baseline: a synthesised paragraph. Its inherit
            // anchor and insertion point come from the draft order, which is
            // keyed by the DRAFT temp id — `id` here is already the formal id.
            let temp = formal_to_temp
                .get(id)
                .map(String::as_str)
                .unwrap_or(id.as_str());
            structure
                .new_paragraphs
                .push(docx2typed_core::prose::NewParagraph {
                    part: "document".to_string(),
                    paragraph_id: id.clone(),
                    inherit: draft_inherit(&blocks, temp).unwrap_or_default(),
                    body: new_body.clone(),
                    insert_before: next_live_paragraph(&blocks, temp),
                });
            structured = true;
            continue;
        };
        if old_body == new_body {
            continue;
        }
        if islands.iter().any(|edit| edit.paragraph_id == *id) {
            continue;
        }
        let is_single_leaf = inventory.as_ref().is_some_and(|inv| {
            let mut leaves = inv
                .leaves
                .iter()
                .filter(|l| l.part_key == "document" && &l.paragraph_id == id);
            let first = leaves.next();
            let second = leaves.next();
            second.is_none() && first.is_some_and(|l| &l.text == old_body)
        });
        if is_single_leaf {
            islands.retain(|edit| edit.paragraph_id != *id);
            islands.push(docx2typed_core::prose::IslandEdit {
                part: "document".to_string(),
                paragraph_id: id.clone(),
                leaf_index: 0,
                old: old_body.clone(),
                new: new_body.clone(),
                tracked,
                author: author.unwrap_or("").to_string(),
                date: date.unwrap_or("").to_string(),
            });
            recorded += 1;
        } else {
            let next_anchor = next_live_paragraph(&blocks, id);
            if !structure
                .deleted_paragraphs
                .iter()
                .any(|p| p.paragraph_id == *id)
            {
                structure
                    .deleted_paragraphs
                    .push(docx2typed_core::prose::DeletedParagraph {
                        part: "document".to_string(),
                        paragraph_id: id.clone(),
                    });
            }
            structure.new_paragraphs.retain(|p| p.paragraph_id != *id);
            structure
                .new_paragraphs
                .push(docx2typed_core::prose::NewParagraph {
                    part: "document".to_string(),
                    paragraph_id: id.clone(),
                    inherit: id.clone(),
                    body: new_body.clone(),
                    insert_before: next_anchor,
                });
            structured = true;
        }
    }
    if recorded > 0 {
        docx2typed_core::prose::save_islands(workdir, &islands)
            .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))?;
    }
    if structured {
        docx2typed_core::prose::save_structure(workdir, &structure)
            .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))?;
    }
    Ok(changed)
}

/// True when `typed.md` records a paragraph the build cannot reproduce from
/// the template alone.
///
/// The build applies two sidecars — `islands.json` (byte surgery inside an
/// existing paragraph) and `structure.json` (synthesised / removed
/// paragraphs). With neither present it simply replays the template, which is
/// safe exactly when every paragraph `typed.md` carries matches the baseline.
///
/// This is the question the build guard must ask. Comparing `typed.md`
/// *bytes* against the extract-time file is too coarse: after an insert and a
/// subsequent delete of that same paragraph, `typed.md` legitimately differs
/// from the header-only extract state (committed baseline bodies were
/// materialised into it) while holding no edit at all — refusing that build
/// would be a false positive.
///
/// A body that differs from the baseline, or an id the baseline does not
/// carry, is a genuine unrecorded edit.
pub fn has_unrecorded_typed_edits(workdir: &Path) -> Result<bool, CollaborationError> {
    let typed = fs::read_to_string(workdir.join("typed.md"))
        .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))?;
    let recorded = parse_typed_paragraphs(&typed);
    let baseline: std::collections::BTreeMap<String, String> =
        inventory_paragraphs_all_parts(workdir)?
            .into_iter()
            .map(|paragraph| (paragraph.id, paragraph.visible_text))
            .collect();
    for (id, body) in &recorded {
        match baseline.get(id) {
            Some(baseline_body) => {
                if baseline_body != body {
                    return Ok(true);
                }
            }
            None => return Ok(true),
        }
    }
    Ok(false)
}

/// Parse committed typed.md paragraph records: id -> body, plus the
/// deleted id set (marker grammar `<!--@p id="X"-->` / `<!--@delete id="X"-->`).
/// The visible (final-view) text of one typed.md paragraph body.
///
/// Mirrors `visible_text`: spans and ranges pass their children through,
/// revisions pass children ONLY for insert/move_to (deletions are hidden), an
/// inline `tab`/`br`/`cr` becomes whitespace, and an opaque block becomes its
/// `[opaque:kind]` placeholder.
///
/// This was latent until #52: typed.md had no paragraph bodies, so the parser
/// never had markup to strip, and `has_unrecorded_typed_edits` compared the
/// markup against the template's plain text and declared every workdir edited.
fn typed_body_visible_text(body: &str) -> String {
    let bytes: Vec<char> = body.chars().collect();
    let mut out = String::new();
    let mut index = 0usize;
    let mut stack: Vec<String> = Vec::new();
    while index < bytes.len() {
        if bytes[index] == '<' {
            let Some(close) = bytes[index..].iter().position(|c| *c == '>') else {
                out.push(bytes[index]);
                index += 1;
                continue;
            };
            let tag: String = bytes[index..index + close + 1].iter().collect();
            let lowered = tag.to_ascii_lowercase();
            if let Some(name) = lowered
                .strip_prefix("</")
                .and_then(|rest| rest.split(|c: char| c.is_whitespace() || c == '>').next())
            {
                let _ = name;
                stack.pop();
            } else if lowered.starts_with("<span") || lowered.starts_with("<docx-range") {
                stack.push("pass".to_string());
            } else if lowered.starts_with("<docx-revision") {
                let passes =
                    lowered.contains("kind=\"insert\"") || lowered.contains("kind=\"move_to\"");
                stack.push(if passes { "pass".into() } else { "skip".into() });
            } else if lowered.starts_with("<docx-anchor") {
                // AnchorNode contributes no text.
                if !lowered.ends_with("/>") {
                    stack.push("skip".to_string());
                }
            } else if lowered.starts_with("<docx-inline") {
                if lowered.contains("kind=\"tab\"") {
                    out.push('\t');
                } else if lowered.contains("kind=\"br\"") || lowered.contains("kind=\"cr\"") {
                    out.push('\n');
                }
            } else if lowered.starts_with("<docx-opaque") {
                let kind = lowered
                    .split("kind=\"")
                    .nth(1)
                    .and_then(|rest| rest.split('"').next())
                    .unwrap_or("");
                out.push_str(&format!("[opaque:{kind}]"));
            }
            index += close + 1;
            continue;
        }
        let skipped = stack.iter().any(|state| state == "skip");
        let character = bytes[index];
        if !skipped {
            // `^{text}` / `_{text}`: the schema-2 vertical marker is markup, the
            // text inside it is the text.
            if (character == '^' || character == '_') && bytes.get(index + 1) == Some(&'{') {
                index += 2;
                let mut depth = 1usize;
                while index < bytes.len() && depth > 0 {
                    match bytes[index] {
                        '{' => depth += 1,
                        '}' => depth -= 1,
                        c if depth > 0 => out.push(c),
                        _ => {}
                    }
                    index += 1;
                }
                continue;
            }
            out.push(character);
        }
        index += 1;
    }
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn parse_typed_paragraphs(text: &str) -> std::collections::BTreeMap<String, String> {
    let mut records: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    let mut current: Option<String> = None;
    let mut body_lines: Vec<String> = Vec::new();
    let flush = |records: &mut std::collections::BTreeMap<String, String>,
                 current: &mut Option<String>,
                 body_lines: &mut Vec<String>| {
        if let Some(id) = current.take() {
            // typed.md writes `marker\nbody\n`, so the trailing empty line is
            // part of the FILE, not of the body -- joining it in added a
            // newline the committed form does not have.
            let body = body_lines.join("\n");
            records.insert(id, typed_body_visible_text(body.trim_end_matches('\n')));
            body_lines.clear();
        }
    };
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("<!--@p id=\"") {
            flush(&mut records, &mut current, &mut body_lines);
            let id = trimmed
                .strip_prefix("<!--@p id=\"")
                .and_then(|rest| rest.split('"').next())
                .unwrap_or("")
                .to_string();
            current = Some(id);
        } else if trimmed.starts_with("<!--@delete") || trimmed.starts_with("<!--@part") {
            flush(&mut records, &mut current, &mut body_lines);
        } else if trimmed.starts_with("<!--@mark") {
            // A paragraph-level revision mark is an annotation on the
            // paragraph, not a terminator.
            continue;
        } else if trimmed.starts_with("<!--@") {
            flush(&mut records, &mut current, &mut body_lines);
        } else if current.is_some() && !trimmed.is_empty() {
            body_lines.push(line.to_string());
        }
    }
    flush(&mut records, &mut current, &mut body_lines);
    records
}

/// `revert` body: discard the draft and regenerate the projection from the
/// template inventory (Python `refresh_edit_projection(discard=True)`).
pub fn discard_projection(workdir: &Path) -> Result<(), CollaborationError> {
    let paragraphs = inventory_paragraphs(workdir)?;
    let (text, typed_sha256) = render_projection(workdir, &paragraphs)?;
    let body_hash = edit_body_sha256(&text)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    fs::write(workdir.join(PROJECTION_FILE), text)
        .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))?;
    let state = state_json(&typed_sha256, &body_hash);
    fs::write(
        workdir.join(STATE_FILE),
        serde_json::to_vec_pretty(&state).expect("serializes"),
    )
    .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))?;
    Ok(())
}

/// Read-only freshness of the opened workdir (`workdir_status`): state plus
/// the edit body hash. A workdir without a projection reports clean with a
/// null body hash (Rust-extracted workdirs have no edit.md until a draft
/// tool initializes one).
pub fn workdir_status(workdir: &Path) -> Result<Value, CollaborationError> {
    let path = resolve_path(workdir);
    let state = if path.join(STATE_FILE).is_file() {
        classify_edit_state(&path)
            .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?
    } else {
        EditState {
            state: "clean".to_string(),
            typed_sha256: String::new(),
            edit_body_sha256: String::new(),
        }
    };
    let body_hash = if path.join(STATE_FILE).is_file() {
        Value::String(state.edit_body_sha256.clone())
    } else {
        Value::Null
    };
    Ok(serde_json::json!({
        "state": state.state,
        "edit_body_sha256": body_hash,
    }))
}

/// Unused-import guard: `parse_edit_header` and `bytes_sha256` are part of
/// the projection grammar surface kept for future header verification.
#[allow(dead_code)]
fn _header_guard(text: &str) -> Option<String> {
    parse_edit_header(text)
        .ok()
        .map(|attrs| attrs.get("base-typed-sha256").cloned().unwrap_or_default())
}

#[allow(dead_code)]
fn _bytes_hash(bytes: &[u8]) -> String {
    bytes_sha256(bytes)
}

/// Path normalization used by the store mutation wrappers.
pub fn absolute(workdir: &Path) -> PathBuf {
    resolve_path(workdir)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An inline token cuts the visible text at a zero-width boundary, so a
    /// span that reads contiguously once the tokens are stripped is not one
    /// editable run.
    #[test]
    fn body_boundaries_cuts_at_inline_tokens() {
        let (flat, boundaries) =
            body_boundaries("ab\u{27e6}insert\u{27e7}cd\u{27e6}/insert\u{27e7}ef");
        assert_eq!(flat, "abcdef");
        assert_eq!(
            boundaries,
            vec![
                (2, "insert-start".to_string()),
                (4, "insert-end".to_string())
            ]
        );
        // A body with no token has no boundary at all.
        assert_eq!(body_boundaries("plain prose").1, Vec::new());
    }

    #[test]
    fn span_crosses_boundary_names_the_flavour() {
        let body = "ab\u{27e6}insert\u{27e7}cd\u{27e6}/insert\u{27e7}ef";
        // "bc" reads contiguous in the flat text but strictly spans the marker.
        let error = span_crosses_boundary("P0", body, "bc").expect("boundary crossed");
        assert_eq!(error.code, "edit-span-crosses-revision-boundary");
        // Boundaries exactly at the span edges do not count.
        assert!(span_crosses_boundary("P0", body, "ab").is_none());
        assert!(span_crosses_boundary("P0", body, "ef").is_none());
        assert!(span_crosses_boundary("P0", body, "cd").is_none());
        // An anchor or format-history token is not a revision: say so.
        let error = span_crosses_boundary("P0", "x\u{27e6}kind=\"hyperlink\"\u{27e7}y", "xy")
            .expect("protected marker crossed");
        assert_eq!(error.code, "edit-span-crosses-protected-marker");
        // A span that is not contiguous in the flat text is simply not found.
        assert!(span_crosses_boundary("P0", body, "zz").is_none());
    }

    /// The batch primitive's argument surface, exercised without a workdir.
    #[test]
    fn normalize_patch_hunks_shapes() {
        let hunks = normalize_patch_hunks(&[json!({
            "paragraph_id": "P0", "old": "a", "new": "b",
        })])
        .expect("replace normalizes");
        assert_eq!(hunks.len(), 1);
        assert!(matches!(hunks[0], PatchHunk::Replace { .. }));
        let hunks = normalize_patch_hunks(&[json!({"insert_after": "P0", "text": "x"})])
            .expect("insert normalizes");
        assert!(matches!(hunks[0], PatchHunk::Insert { inherit: None, .. }));
        let hunks = normalize_patch_hunks(&[json!({"delete": "P4"})]).expect("delete normalizes");
        assert!(matches!(hunks[0], PatchHunk::Delete { .. }));
        for bad in [
            json!({"paragraph_id": "P0", "old": "a", "typo": "b"}),
            json!({"paragraph_id": "P0", "old": "a"}),
            json!({"paragraph_id": "P0", "old": "", "new": "b"}),
            json!({"insert_after": "P0"}),
            json!({"match_ref": "ref_x", "new": "b"}),
            json!({"old": "a", "new": "b"}),
            json!("not an object"),
        ] {
            let error =
                normalize_patch_hunks(std::slice::from_ref(&bad)).expect_err("invalid hunk");
            assert_eq!(error.code, "invalid-arguments", "{bad}");
        }
    }
}

#[cfg(test)]
mod visible_text_tests {
    use super::typed_body_visible_text;

    #[test]
    fn the_vertical_markers_are_markup_and_their_text_is_text() {
        // The shape typed.md actually writes, measured on complex.docx's P3.
        assert_eq!(
            typed_body_visible_text("highlight^{ superscript²}_{ subscript₂} code"),
            "highlight superscript² subscript₂ code"
        );
    }

    #[test]
    fn spans_and_anchors_reduce_the_way_visible_text_does() {
        assert_eq!(
            typed_body_visible_text("<span data-s=\"s_x\">bold</span> plain"),
            "bold plain"
        );
        assert_eq!(
            typed_body_visible_text("<docx-anchor id=\"N0\" kind=\"bookmark-start\"/>x"),
            "x"
        );
        assert_eq!(
            typed_body_visible_text(
                "<docx-opaque id=\"N1\" kind=\"unsupported-run\" tag=\"w:drawing\"/>y"
            ),
            "[opaque:unsupported-run]y"
        );
    }

    #[test]
    fn a_deletion_revision_hides_its_text_and_an_insertion_shows_it() {
        assert_eq!(
            typed_body_visible_text(
                "<docx-revision id=\"N0\" kind=\"delete\">gone</docx-revision>kept"
            ),
            "kept"
        );
        assert_eq!(
            typed_body_visible_text("<docx-revision id=\"N1\" kind=\"insert\">new</docx-revision>"),
            "new"
        );
    }
}
