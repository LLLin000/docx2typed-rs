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
pub fn inventory_paragraphs(workdir: &Path) -> Result<Vec<InventoryParagraph>, CollaborationError> {
    let package = workdir.join("_template.docx");
    if !package.is_file() {
        return Err(CollaborationError::new(
            "workdir-invalid",
            format!("_template.docx not found in {}", workdir.to_string_lossy()),
        ));
    }
    let inventory = docx2typed_core::prose::enumerate_package(&package)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    let mut paragraphs = Vec::new();
    for paragraph in inventory.paragraphs {
        // Body paragraphs carry the document part key (Rust inventory) or an
        // empty key (Python); container/part paragraphs are out of scope.
        if paragraph.part_key != "document" && !paragraph.part_key.is_empty() {
            continue;
        }
        paragraphs.push(InventoryParagraph {
            id: paragraph.paragraph_id.clone(),
            visible_text: paragraph.visible_text.clone(),
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
    let text = read_draft(workdir)?;
    let blocks = paragraph_blocks(&text)?;
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

/// Python `_visible_text` for a token-free draft body: the body itself
/// (escaping is identity for plain text without `\`, `⟦`, `⟧`).
pub fn visible_text(body: &str) -> String {
    body.to_string()
}

/// Parse one block marker line into (kind, id, extra attrs).
fn marker_of(block: &str) -> Option<(String, String, String)> {
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

/// The changed paragraph ids of one draft: the draft body vs the effective
/// baseline (committed typed.md record where present, else the template
/// inventory body). New blocks are always changed; delete markers are
/// changed only when the id existed in the baseline.
fn compute_changed(workdir: &Path, blocks: &[String]) -> Result<Vec<String>, CollaborationError> {
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
                let body = block_body(block);
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
    let index = find_block(&blocks, "p", paragraph_id).ok_or_else(|| {
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
    replace_text_with_options(workdir, paragraph_id, old, new, false, None, None, None)
}

pub fn replace_text_with_options(
    workdir: &Path,
    paragraph_id: &str,
    old: &str,
    new: &str,
    tracked: bool,
    author: Option<&str>,
    date: Option<&str>,
    leaf_index: Option<usize>,
) -> Result<Value, CollaborationError> {
    crate::collab::ensure_agent_ready(workdir)?;
    let (header, mut blocks) = read_blocks(workdir)?;
    let index = find_block(&blocks, "p", paragraph_id).ok_or_else(|| {
        CollaborationError::new(
            "paragraph-not-found",
            format!("paragraph {paragraph_id} not found in the draft"),
        )
    })?;
    let marker = blocks[index].split('\n').next().unwrap_or("").to_string();
    let body = block_body(&blocks[index]);
    let new_body = if let Some(leaf_index) = leaf_index {
        let occurrence = leaf_occurrence(workdir, paragraph_id, leaf_index, old)?;
        replace_nth_in_body(&body, old, new, occurrence, paragraph_id)?
    } else {
        replace_in_body(&body, old, new, paragraph_id)?
    };
    blocks[index] = if new_body.is_empty() {
        marker
    } else {
        format!("{marker}\n{new_body}")
    };
    record_island_operation(
        workdir,
        paragraph_id,
        old,
        new,
        tracked,
        author,
        date,
        leaf_index,
    )?;
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
fn record_island_operation(
    workdir: &Path,
    paragraph_id: &str,
    old: &str,
    new: &str,
    tracked: bool,
    author: Option<&str>,
    date: Option<&str>,
    leaf_index: Option<usize>,
) -> Result<(), CollaborationError> {
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
    let mut islands = docx2typed_core::prose::load_islands(workdir)
        .map_err(|error| CollaborationError::new("workdir-invalid", error.to_string()))?;
    islands.push(docx2typed_core::prose::IslandEdit {
        part,
        paragraph_id: paragraph_id.to_string(),
        leaf_index: leaf.leaf_index,
        old: old.to_string(),
        new: new.to_string(),
        tracked,
        author: author.unwrap_or("").to_string(),
        date: date.unwrap_or("").to_string(),
    });
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
    let index = find_block(&blocks, "p", after_id).ok_or_else(|| {
        CollaborationError::new(
            "paragraph-not-found",
            format!("paragraph {after_id} not found in the draft"),
        )
    })?;
    let resolved_inherit = inherit.unwrap_or(after_id).to_string();
    let mut max_temp = 0usize;
    for block in &blocks {
        if let Some((kind, id, _)) = marker_of(block) {
            if kind == "new" {
                if let Some(number) = id.strip_prefix('N').and_then(|n| n.parse().ok()) {
                    max_temp = max_temp.max(number);
                }
            }
        }
    }
    let temp = format!("N{}", max_temp + 1);
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
    let index = find_block(&blocks, "p", paragraph_id).ok_or_else(|| {
        CollaborationError::new(
            "paragraph-not-found",
            format!("paragraph {paragraph_id} not found in the draft"),
        )
    })?;
    blocks.remove(index);
    blocks.push(format!("<!--@delete id=\"{paragraph_id}\"-->"));
    write_draft(workdir, &header, &blocks)?;
    Ok(serde_json::json!({
        "paragraph_id": paragraph_id,
        "draft": "dirty",
        "next": "commit_sync",
    }))
}

/// `diff_preview` payload: the freshness state plus per-paragraph changed
/// ids when dirty. Hunk/style-ownership reporting is a declared tracer
/// deferral (the tracer draft model carries no style regions).
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
    for block in &blocks {
        let Some((kind, id, marker)) = marker_of(block) else {
            continue;
        };
        match kind.as_str() {
            "p" => {
                let body = block_body(block);
                let marker_line = if marker.contains("base=\"\"") {
                    format!("<!--@p id=\"{id}\"-->")
                } else {
                    marker.clone()
                };
                typed.push_str(&format!(
                    "{marker_line}
{body}
"
                ));
            }
            "new" => {
                // Formal paragraph allocation is a declared tracer
                // deferral; new paragraphs commit under their temp id.
                let body = block_body(block);
                typed.push_str(&format!(
                    "<!--@p id=\"{id}\" base=\"\"-->
{body}
"
                ));
            }
            "delete" => {
                typed.push_str(&format!("<!--@delete id=\"{id}\"-->"));
                typed.push('\n');
            }
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
            kept.push(format!("<!--@p id=\"{id}\"-->"));
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
    let mut recorded = 0usize;
    for id in &changed {
        let Some(new_body) =
            parse_typed_paragraphs(&fs::read_to_string(workdir.join("typed.md")).map_err(
                |error| CollaborationError::new("workdir-unreadable", error.to_string()),
            )?)
            .remove(id)
        else {
            continue;
        };
        let Some(old_body) = before.get(id) else {
            continue;
        };
        if *old_body == new_body {
            continue;
        }
        if islands.iter().any(|edit| edit.paragraph_id == *id) {
            continue;
        }
        islands.retain(|edit| edit.paragraph_id != *id);
        islands.push(docx2typed_core::prose::IslandEdit {
            part: "document".to_string(),
            paragraph_id: id.clone(),
            leaf_index: 0,
            old: old_body.clone(),
            new: new_body,
            tracked,
            author: author.unwrap_or("").to_string(),
            date: date.unwrap_or("").to_string(),
        });
        recorded += 1;
    }
    if recorded > 0 {
        docx2typed_core::prose::save_islands(workdir, &islands)
            .map_err(|error| CollaborationError::new("workdir-unreadable", error.to_string()))?;
    }
    Ok(changed)
}

/// Parse committed typed.md paragraph records: id -> body, plus the
/// deleted id set (marker grammar `<!--@p id="X"-->` / `<!--@delete id="X"-->`).
fn parse_typed_paragraphs(text: &str) -> std::collections::BTreeMap<String, String> {
    let mut records: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    let mut current: Option<String> = None;
    let mut body_lines: Vec<String> = Vec::new();
    let flush = |records: &mut std::collections::BTreeMap<String, String>,
                 current: &mut Option<String>,
                 body_lines: &mut Vec<String>| {
        if let Some(id) = current.take() {
            records.insert(
                id,
                body_lines.join(
                    "
",
                ),
            );
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
        } else if trimmed.starts_with("<!--@delete") {
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
