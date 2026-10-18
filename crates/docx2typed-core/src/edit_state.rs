//! Hash-bound clean-edit freshness classification (issue #56 slice),
//! mirroring `scripts/edit.py` (`classify_edit_state` /
//! `edit_body_sha256` / header grammar) for schema-1 workdirs.
//!
//! Freshness is computed from the authoritative `edit.state.json` sidecar,
//! never the visible `edit.md` header: the header is a human-readable
//! mirror only, and any disagreement is `edit-header-tampered`. The four
//! states (clean / dirty / stale-clean / conflict) derive from comparing
//! `typed.md` and the canonical `edit.md` body against the pinned
//! `base_typed_sha256` / `base_projection_sha256`.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use docx2typed_protocol::bytes_sha256;

use crate::CoreError;

pub const EDIT_STATE_SCHEMA: &str = "typed-clean-edit-state-1";
pub const EDIT_SCHEMA_VERSION: i64 = 1;
pub const SYNC_CONTRACT_VERSION: i64 = 1;
pub const SEGMENTATION_CONTRACT: &str = "uax29-c1-1/unicode-16.0.0";
/// The single editable-view rendering rule (ADR 0051) the frozen baseline and
/// the draft are both rendered by. Emitted into `edit.state.json` at every
/// projection init; `classify_edit_state` refuses a workdir whose
/// `baseline.view` exists without a matching marker, before any mutation.
pub const EDITABLE_VIEW_CONTRACT: &str = "editable-view-1";

pub const STATE_FILE: &str = "edit.state.json";
pub const PROJECTION_FILE: &str = "edit.md";

/// Freshness classification result (the subset inspect/migrate/build need).
#[derive(Clone, Debug, PartialEq)]
pub struct EditState {
    /// clean | dirty | stale-clean | conflict
    pub state: String,
    pub typed_sha256: String,
    pub edit_body_sha256: String,
}

fn domain(message: impl Into<String>) -> CoreError {
    CoreError::Domain(message.into())
}

/// Python-compatible `str.splitlines()`: splits on `\n`, `\r\n`, `\r`,
/// `\x0b`, `\x0c`, `\x1c`, `\x1d`, `\x1e`, `\x85`, `\u2028`, `\u2029`
/// (the edit body hash normalizes CRLF through exactly this splitter).
pub fn py_splitlines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                lines.push(std::mem::take(&mut current));
            }
            '\n' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}' | '\u{2028}'
            | '\u{2029}' => {
                lines.push(std::mem::take(&mut current));
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// SHA-256 of the canonical edit body: header excluded, line endings
/// normalized (`splitlines()` + `"\n".join`), mirroring Python's
/// `edit_body_sha256`.
pub fn edit_body_sha256(text: &str) -> Result<String, CoreError> {
    let mut lines = py_splitlines(text);
    while lines
        .first()
        .map(|line| line.trim().is_empty())
        .unwrap_or(false)
    {
        lines.remove(0);
    }
    let first = lines
        .first()
        .ok_or_else(|| domain("edit-header-missing: edit.md must start with an @edit header"))?;
    if !first.starts_with("<!--@edit") {
        return Err(domain(
            "edit-header-missing: edit.md must start with an @edit header",
        ));
    }
    Ok(bytes_sha256(lines[1..].join("\n").as_bytes()))
}

/// XML entity validation + unescape for attribute values (`&lt;` `&gt;`
/// `&amp;` only), mirroring `scripts/typed_core.py` `xml_unescape`.
pub fn xml_unescape(text: &str) -> Result<String, CoreError> {
    let mut cursor = 0usize;
    while let Some(relative) = text[cursor..].find('&') {
        let marker = cursor + relative;
        let rest = &text[marker..];
        if !(rest.starts_with("&lt;") || rest.starts_with("&gt;") || rest.starts_with("&amp;")) {
            return Err(domain(format!(
                "unknown or unescaped entity at offset {marker}"
            )));
        }
        cursor = marker + 1;
    }
    Ok(text
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&"))
}

/// `key="value"` attribute list (no whitespace inside values, backslash
/// escapes kept literally — Python's `_ATTR_RE`), mirroring
/// `scripts/typed_core.py` `parse_attributes`.
pub fn parse_attributes(raw: &str) -> Result<BTreeMap<String, String>, CoreError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(BTreeMap::new());
    }
    let chars: Vec<char> = raw.chars().collect();
    let mut attrs = BTreeMap::new();
    let mut cursor = 0usize;
    let invalid = |chunk: &str| domain(format!("invalid attribute syntax: {chunk}"));
    while cursor < chars.len() {
        while cursor < chars.len() && chars[cursor].is_whitespace() {
            cursor += 1;
        }
        if cursor >= chars.len() {
            break;
        }
        let chunk_start = cursor;
        if !(chars[cursor].is_ascii_alphabetic() || chars[cursor] == '_' || chars[cursor] == ':') {
            return Err(invalid(&chars[chunk_start..].iter().collect::<String>()));
        }
        cursor += 1;
        while cursor < chars.len()
            && (chars[cursor].is_ascii_alphanumeric()
                || matches!(chars[cursor], '_' | '.' | ':' | '-'))
        {
            cursor += 1;
        }
        let name: String = chars[chunk_start..cursor].iter().collect();
        if cursor >= chars.len() || chars[cursor] != '=' {
            return Err(invalid(&chars[chunk_start..].iter().collect::<String>()));
        }
        cursor += 1;
        if cursor >= chars.len() || chars[cursor] != '"' {
            return Err(invalid(&chars[chunk_start..].iter().collect::<String>()));
        }
        cursor += 1;
        let mut value = String::new();
        loop {
            if cursor >= chars.len() {
                return Err(invalid(&chars[chunk_start..].iter().collect::<String>()));
            }
            let ch = chars[cursor];
            if ch == '"' {
                cursor += 1;
                break;
            }
            if ch == '\\' {
                if cursor + 1 >= chars.len() {
                    return Err(invalid(&chars[chunk_start..].iter().collect::<String>()));
                }
                value.push(chars[cursor + 1]);
                cursor += 2;
                continue;
            }
            value.push(ch);
            cursor += 1;
        }
        let value = xml_unescape(&value)?;
        if attrs.contains_key(&name) {
            return Err(domain(format!("duplicate attribute: {name}")));
        }
        attrs.insert(name, value);
    }
    Ok(attrs)
}

/// The `@edit` header line of `edit.md`, parsed into attributes. Mirrors
/// Python's `_HEADER_RE` + `parse_attributes`; the surrounding grammar
/// (paragraph markers, placeholders) is not re-parsed in this slice.
pub fn parse_edit_header(text: &str) -> Result<BTreeMap<String, String>, CoreError> {
    let mut lines = py_splitlines(text);
    while lines
        .first()
        .map(|line| line.trim().is_empty())
        .unwrap_or(false)
    {
        lines.remove(0);
    }
    let first = lines
        .first()
        .ok_or_else(|| domain("edit-header-missing: edit.md is empty"))?;
    let group = header_group(first)
        .ok_or_else(|| domain("edit-header-missing: edit.md must start with an @edit header"))?;
    parse_attributes(group)
}

/// `^<!--@edit(.*?)-->$`: the first `-->` that ends the line is the group
/// boundary (lazy semantics — Python backtracks to a later `-->` only when
/// the first does not reach end-of-string).
fn header_group(line: &str) -> Option<&str> {
    if !line.starts_with("<!--@edit") {
        return None;
    }
    let mut rest = &line["<!--@edit".len()..];
    while let Some(end) = rest.find("-->") {
        if end + 3 == rest.len() {
            return Some(&rest[..end]);
        }
        rest = &rest[end + 3..];
    }
    None
}

/// Freshness classification from the authoritative sidecar, mirroring
/// `scripts/edit.py` `classify_edit_state` (header/state binding checks +
/// the four-state freshness decision). `typed.md` must be readable.
pub fn classify_edit_state(root: &Path) -> Result<EditState, CoreError> {
    let state_path = root.join(STATE_FILE);
    if !state_path.exists() {
        return Err(domain(
            "edit-state-missing: edit.state.json not found; run `docx2typed edit refresh --init` \
             to create the projection and authoritative state",
        ));
    }
    let bytes = fs::read(&state_path).map_err(CoreError::io)?;
    let state: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
        domain(format!(
            "edit-state-incompatible: cannot read edit.state.json: {error}"
        ))
    })?;
    if !state.is_object()
        || state.get("schema").and_then(serde_json::Value::as_str) != Some(EDIT_STATE_SCHEMA)
    {
        return Err(domain(
            "edit-state-incompatible: unexpected edit.state.json schema",
        ));
    }
    for (key, expected) in [
        ("edit_schema_version", EDIT_SCHEMA_VERSION),
        ("sync_contract_version", SYNC_CONTRACT_VERSION),
    ] {
        if state.get(key).and_then(serde_json::Value::as_i64) != Some(expected) {
            return Err(domain(format!(
                "edit-state-incompatible: {key} mismatch in edit.state.json"
            )));
        }
    }
    if state
        .get("segmentation_contract")
        .and_then(serde_json::Value::as_str)
        != Some(SEGMENTATION_CONTRACT)
    {
        return Err(domain(
            "edit-state-incompatible: segmentation_contract mismatch in edit.state.json",
        ));
    }
    // Editable-view version gate: `editable_view_contract` is a version LABEL
    // in `edit.state.json` naming the rule the frozen baseline was rendered
    // under — not a digest, and not proof that a hand-modified `baseline.view`
    // is intact. A PRESENT marker that is absent from the known set is foreign
    // metadata, never legacy, so it refuses whether or not `baseline.view`
    // currently exists: accepting it would let one lifecycle's rule silently
    // govern another's bytes. A MISSING marker is legacy ONLY when no
    // `baseline.view` exists either — the workdir predates the freeze and
    // reads keep the template-render fallback exactly as before. A missing
    // marker beside an existing baseline is a mixed generation and refuses
    // with a fresh-extract diagnosis, before any mutation.
    match state
        .get("editable_view_contract")
        .and_then(serde_json::Value::as_str)
    {
        Some(marker) if marker == EDITABLE_VIEW_CONTRACT => {}
        Some(marker) => {
            return Err(domain(format!(
                "edit-state-incompatible: editable_view_contract {marker:?} in edit.state.json is not \
                 a known editable-view rule; re-extract the source into a fresh workdir to \
                 re-establish the baseline",
            )));
        }
        None if root.join("baseline.view").is_file() => {
            return Err(domain(
                "edit-state-incompatible: edit.state.json has no editable_view_contract but \
                 baseline.view exists — the frozen baseline is from a different lifecycle \
                 generation; re-extract the source into a fresh workdir to re-establish the baseline",
            ));
        }
        None => {}
    }
    for key in ["base_typed_sha256", "base_projection_sha256"] {
        let value = state
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(domain(format!(
                "edit-binding-mismatch: invalid {key} in edit.state.json"
            )));
        }
    }
    let edit_path = root.join(PROJECTION_FILE);
    if !edit_path.exists() {
        return Err(domain(
            "edit-state-missing: edit.md not found; run `docx2typed edit refresh --init`",
        ));
    }
    let text = fs::read_to_string(&edit_path).map_err(CoreError::io)?;
    let header = parse_edit_header(&text)?;
    let required = [
        "schema",
        "sync-contract",
        "base-typed-sha256",
        "base-projection-sha256",
        "segmentation",
    ];
    if required.iter().any(|key| !header.contains_key(*key)) || header.len() != required.len() {
        return Err(domain(
            "edit-header-missing: @edit header must declare schema, sync-contract, \
             base-typed-sha256, base-projection-sha256, segmentation",
        ));
    }
    if header.get("schema").map(String::as_str) != Some("1")
        || header.get("sync-contract").map(String::as_str) != Some("1")
    {
        return Err(domain(
            "edit-state-incompatible: unsupported edit schema or sync contract",
        ));
    }
    if header.get("segmentation").map(String::as_str) != Some(SEGMENTATION_CONTRACT) {
        return Err(domain(
            "edit-state-incompatible: unsupported segmentation contract",
        ));
    }
    for key in ["base-typed-sha256", "base-projection-sha256"] {
        let value = header.get(key).map(String::as_str).unwrap_or("");
        if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(domain(format!(
                "edit-grammar-invalid: {key} must be a SHA-256 hex digest"
            )));
        }
    }
    for (key, expected) in [
        ("schema", "1".to_string()),
        ("sync-contract", "1".to_string()),
        (
            "base-typed-sha256",
            state
                .get("base_typed_sha256")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string(),
        ),
        (
            "base-projection-sha256",
            state
                .get("base_projection_sha256")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string(),
        ),
        ("segmentation", SEGMENTATION_CONTRACT.to_string()),
    ] {
        if header.get(key) != Some(&expected) {
            return Err(domain(format!(
                "edit-header-tampered: edit.md header {key} does not match edit.state.json"
            )));
        }
    }
    let typed_hash =
        docx2typed_protocol::file_sha256(&root.join("typed.md")).map_err(CoreError::io)?;
    let body_hash = edit_body_sha256(&text)?;
    let base_typed = state
        .get("base_typed_sha256")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let base_body = state
        .get("base_projection_sha256")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let freshness = if typed_hash == base_typed && body_hash == base_body {
        "clean"
    } else if typed_hash == base_typed {
        "dirty"
    } else if body_hash == base_body {
        "stale-clean"
    } else {
        "conflict"
    };
    Ok(EditState {
        state: freshness.to_string(),
        typed_sha256: typed_hash,
        edit_body_sha256: body_hash,
    })
}
