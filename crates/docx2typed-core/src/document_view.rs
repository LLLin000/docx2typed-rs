//! Core read API of the `document_read` tool: the `edit.md` projection read
//! back as one virtual text file (mirror of Python `scripts/mcp_server.py`
//! `document_read` / `_read_edit` / `_paragraph_blocks` / `_block_ident` /
//! `_block_body` / `_visible_text`, for the views this slice implements:
//! `content`, `outline`, `auto`).
//!
//! ## Frozen invariant: `content` is the diff base
//!
//! [`DocumentReadView::content`] is the exact projection text a later
//! `document_patch` diffs against. It is rebuilt from the parsed blocks
//! (never read byte-for-byte from disk), so it is byte-reproducible: reading
//! the same draft twice yields identical `content`, and re-splitting
//! `content` and rendering it again is the identity. Hunks locate their
//! target by block marker id + exact body text, so a windowed read is a
//! valid diff base too — window bounds only decide which blocks are in the
//! string.
//!
//! ## Views
//!
//! - `content`: the whole projection verbatim; with `anchor` (a paragraph id
//!   from a previous read/search) the surrounding `before`/`after` block
//!   window. Out-of-range bounds clamp to the draft, never error.
//! - `outline`: one line per paragraph: the marker, the first 80 visible
//!   characters, and the visible length.
//! - `auto`: `content` for small drafts, `outline` once the projection
//!   exceeds 40 000 characters (>40k, Python `len` semantics: scalars).
//!
//! `spans`, `issues`, and `capabilities` are not part of this slice; they
//! fail with [`INVALID_VIEW_CODE`] rather than silently returning a wrong
//! view.
//!
//! Read-only: the workdir is only ever opened for reading.

use std::fs;
use std::path::Path;

use serde::Serialize;

use crate::edit_state::{classify_edit_state, py_splitlines, PROJECTION_FILE};
use crate::CoreError;

/// Stable diagnostic code for a `view` outside the implemented set.
pub const INVALID_VIEW_CODE: &str = "document-read-invalid-view";
/// Stable diagnostic code for an `anchor` that is not a paragraph id of the
/// draft.
pub const PARAGRAPH_NOT_FOUND_CODE: &str = "paragraph-not-found";

/// Projection length above which `view="auto"` switches to the outline
/// (Python `len(full_text) > 40_000`).
pub const AUTO_OUTLINE_THRESHOLD: usize = 40_000;
/// Visible characters of each paragraph in the outline view.
pub const OUTLINE_PREVIEW_CHARS: usize = 80;

const TOKEN_START: char = '\u{27e6}';
const TOKEN_END: char = '\u{27e7}';
/// `^ _ { } \` — the characters a vertical tag escapes.
const VERTICAL_ESCAPES: [char; 5] = ['^', '_', '{', '}', '\\'];

/// One `document_read` reply (the `docx2typed-result-1` data payload).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DocumentReadView {
    /// Resolved view: `content` or `outline` (`auto` is never echoed back).
    pub view: String,
    /// Opaque token to pass back as a later patch's `base_revision`
    /// (`edit.state.json`'s canonical edit-body hash).
    pub revision: String,
    /// Freshness state of the draft: clean | dirty | stale-clean | conflict.
    pub state: String,
    /// Paragraph blocks of the WHOLE draft (not just the window).
    pub paragraphs: usize,
    /// The virtual-file text; also the diff base for `document_patch`.
    pub content: String,
    /// First marker id of the returned content (None when there is none).
    pub first_id: Option<String>,
    /// Last marker id of the returned content (None when there is none).
    pub last_id: Option<String>,
}

/// Read the workdir's `edit.md` projection, mirroring Python
/// `document_read`. `anchor` windows the selected blocks to
/// `[index - before, index + after]`, clamped to the draft.
pub fn document_read_view(
    workdir: &Path,
    anchor: Option<&str>,
    before: usize,
    after: usize,
    view: &str,
) -> Result<DocumentReadView, CoreError> {
    if !matches!(view, "content" | "outline" | "auto") {
        return Err(domain(format!(
            "{INVALID_VIEW_CODE}: view must be content, outline, or auto, got '{view}'"
        )));
    }
    let state = classify_edit_state(workdir)?;
    let text = fs::read_to_string(workdir.join(PROJECTION_FILE)).map_err(CoreError::io)?;
    let (header, blocks) = split_edit_blocks(&text)?;
    let full_text = format!("{header}\n\n{}\n", blocks.join("\n\n"));
    let mut selected = blocks.clone();
    if let Some(anchor) = anchor {
        let index = blocks
            .iter()
            // Mirrors Python's `(_block_ident(block) or ("", ""))[1] == anchor`.
            .position(|block| {
                block_ident(block)
                    .map(|(_, id)| id)
                    .as_deref()
                    .unwrap_or("")
                    == anchor
            })
            .ok_or_else(|| {
                domain(format!(
                    "{PARAGRAPH_NOT_FOUND_CODE}: anchor {anchor} not found in the draft"
                ))
            })?;
        let start = index.saturating_sub(before);
        let end = index
            .saturating_add(after)
            .saturating_add(1)
            .min(blocks.len());
        selected = blocks[start..end].to_vec();
    }
    let resolved = if view == "auto" {
        if full_text.chars().count() > AUTO_OUTLINE_THRESHOLD {
            "outline"
        } else {
            "content"
        }
    } else {
        view
    };
    let content = if resolved == "outline" {
        let mut lines: Vec<String> = vec![header.clone()];
        for block in &selected {
            let ident_text = match block_ident(block) {
                Some((kind, id)) => format!("<!--@{kind} id=\"{id}\"-->"),
                None => String::new(),
            };
            let visible = visible_text(&block_body(block))?;
            let length = visible.chars().count();
            let preview: String = visible.chars().take(OUTLINE_PREVIEW_CHARS).collect();
            let ellipsis = if length > OUTLINE_PREVIEW_CHARS {
                "\u{2026}"
            } else {
                ""
            };
            lines.push(format!("{ident_text} {preview}{ellipsis} [{length} chars]"));
        }
        format!("{}\n", lines.join("\n"))
    } else if anchor.is_some() {
        format!("{header}\n\n{}\n", selected.join("\n\n"))
    } else {
        full_text
    };
    let ident_source: &[String] = if anchor.is_some() || resolved == "outline" {
        &selected
    } else {
        &blocks
    };
    let idents: Vec<String> = ident_source
        .iter()
        .filter_map(|block| block_ident(block).map(|(_, id)| id))
        .collect();
    Ok(DocumentReadView {
        view: resolved.to_string(),
        revision: state.edit_body_sha256,
        state: state.state,
        paragraphs: blocks.len(),
        content,
        first_id: idents.first().cloned(),
        last_id: idents.last().cloned(),
    })
}

fn domain(message: String) -> CoreError {
    CoreError::Domain(message)
}

// ---------------------------------------------------------------------------
// edit.md grammar (mirror of `_paragraph_blocks` / `_block_ident` /
// `_block_body` / `_visible_text`)
// ---------------------------------------------------------------------------

/// Split the draft into (header line, paragraph blocks), mirroring Python
/// `_paragraph_blocks`: leading blank lines are dropped, the header must
/// start with `<!--@edit`, and a block starts at a `<!--@p` / `<!--@new` /
/// `<!--@delete` marker line (blank lines are skipped).
fn split_edit_blocks(text: &str) -> Result<(String, Vec<String>), CoreError> {
    let mut lines = py_splitlines(text);
    while lines
        .first()
        .map(|line| line.trim().is_empty())
        .unwrap_or(false)
    {
        lines.remove(0);
    }
    if lines.is_empty() || !lines[0].starts_with("<!--@edit") {
        return Err(domain(
            "edit-header-missing: edit.md must start with an @edit header".to_string(),
        ));
    }
    let header = lines[0].clone();
    let mut blocks: Vec<String> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for line in &lines[1..] {
        let stripped = line.trim();
        if stripped.starts_with("<!--@p")
            || stripped.starts_with("<!--@new")
            || stripped.starts_with("<!--@delete")
        {
            if !current.is_empty() {
                blocks.push(current.join("\n"));
                current.clear();
            }
            current.push(line);
        } else if !stripped.is_empty() {
            current.push(line);
        }
    }
    if !current.is_empty() {
        blocks.push(current.join("\n"));
    }
    Ok((header, blocks))
}

/// `(kind, id)` of a block marker, mirroring Python `_block_ident`:
/// `("p", id)` for `<!--@p id="...">`, `("new", temp)` for
/// `<!--@new temp="...">`, `("delete", id)` for `<!--@delete id="...">`.
fn block_ident(block: &str) -> Option<(String, String)> {
    let line = block.lines().next().unwrap_or("").trim();
    let rest = line.strip_prefix("<!--@")?;
    for kind in ["p", "new", "delete"] {
        let Some(rest) = rest
            .strip_prefix(kind)
            .and_then(|rest| rest.strip_prefix(' '))
        else {
            continue;
        };
        for attribute in ["id", "temp"] {
            let Some(rest) = rest
                .strip_prefix(attribute)
                .and_then(|rest| rest.strip_prefix("=\""))
            else {
                continue;
            };
            let value: String = rest.chars().take_while(|ch| *ch != '"').collect();
            if !value.is_empty() {
                return Some((kind.to_string(), value));
            }
        }
    }
    None
}

/// Body of one block: every line after the marker line (Python
/// `_block_body`).
fn block_body(block: &str) -> String {
    match block.split_once('\n') {
        Some((_, body)) => body.to_string(),
        None => String::new(),
    }
}

/// Python `_visible_text`: the token-free text an editor sees. Placeholders
/// become their display marker (`⇉` tab, `↵` break, `⟦kind⟧` otherwise) and
/// vertical tags keep their inner prose.
fn visible_text(body: &str) -> Result<String, CoreError> {
    let chars: Vec<char> = body.chars().collect();
    let mut out = String::new();
    let mut cursor = 0usize;
    loop {
        let Some(start) = find_char(&chars, TOKEN_START, cursor) else {
            if cursor < chars.len() {
                out.push_str(&plain_prose(&chars[cursor..])?);
            }
            return Ok(out);
        };
        if start > cursor {
            out.push_str(&plain_prose(&chars[cursor..start])?);
        }
        let end = find_char(&chars, TOKEN_END, start + 1).ok_or_else(|| {
            domain("edit-grammar-invalid: unclosed placeholder in draft".to_string())
        })?;
        let token: String = chars[start..=end].iter().collect();
        out.push_str(&token_marker(&token));
        cursor = end + 1;
    }
}

fn find_char(chars: &[char], needle: char, from: usize) -> Option<usize> {
    chars
        .iter()
        .skip(from)
        .position(|ch| *ch == needle)
        .map(|offset| offset + from)
}

/// Display marker of one `⟦...⟧` placeholder (Python `_token_marker` +
/// the `_visible_text` keyword dispatch).
fn token_marker(token: &str) -> String {
    let fallback = "\u{27e6}?\u{27e7}".to_string();
    let Some(rest) = token.strip_prefix(TOKEN_START) else {
        return fallback;
    };
    for keyword in ["token", "range-start", "range-end"] {
        let Some(attrs) = rest.strip_prefix(keyword) else {
            continue;
        };
        if keyword == "range-end" {
            return TOKEN_END.to_string();
        }
        return match kind_attribute(attrs) {
            Some(kind) => match kind {
                "tab" => "\u{21b9}".to_string(),
                "br" | "cr" => "\u{21b5}".to_string(),
                other => format!("\u{27e6}{other}\u{27e7}"),
            },
            None => fallback,
        };
    }
    fallback
}

/// `re.search(r'kind="([^"]+)"', attrs)` — first `kind="…"` with a non-empty
/// value.
fn kind_attribute(attrs: &str) -> Option<&str> {
    let mut cursor = 0usize;
    while let Some(offset) = attrs[cursor..].find("kind=\"") {
        let value_start = cursor + offset + "kind=\"".len();
        let rest = &attrs[value_start..];
        let value = rest.split('"').next().unwrap_or("");
        if !value.is_empty() {
            return Some(value);
        }
        cursor = value_start;
    }
    None
}

/// One plain-prose chunk: escape-validated, vertical tags stripped of their
/// markers (Python `_plain_prose` = `_validate_escaped_prose` +
/// `_split_vertical_tags`).
fn plain_prose(raw: &[char]) -> Result<String, CoreError> {
    let text = validate_escaped_prose(raw)?;
    let mut out = String::new();
    for segment in split_vertical_tags(&text)? {
        out.push_str(&segment);
    }
    Ok(out)
}

/// Python `_validate_escaped_prose`: `\\` -> `\`, `\u27E6`/`\u27E7` ->
/// `⟦`/`⟧`, anything else fails closed.
fn validate_escaped_prose(text: &[char]) -> Result<Vec<char>, CoreError> {
    let mut out: Vec<char> = Vec::with_capacity(text.len());
    let mut index = 0usize;
    while index < text.len() {
        let ch = text[index];
        if ch != '\\' {
            if ch == TOKEN_START || ch == TOKEN_END {
                return Err(domain(
                    "edit-grammar-invalid: literal \u{27e6}/\u{27e7} must use \
                     \\u27E6/\\u27E7 escapes"
                        .to_string(),
                ));
            }
            out.push(ch);
            index += 1;
            continue;
        }
        if text.get(index + 1) == Some(&'\\') {
            out.push('\\');
            index += 2;
            continue;
        }
        let end = (index + 6).min(text.len());
        let segment: String = text[index + 1..end]
            .iter()
            .collect::<String>()
            .to_lowercase();
        if segment == "u27e6" {
            out.push(TOKEN_START);
            index += 6;
            continue;
        }
        if segment == "u27e7" {
            out.push(TOKEN_END);
            index += 6;
            continue;
        }
        return Err(domain(
            "edit-grammar-invalid: invalid escape sequence".to_string(),
        ));
    }
    Ok(out)
}

/// Python `_split_vertical_tags`, prose only: `^{…}` / `_{…}` keep their
/// inner text, a backslash escapes `^ _ { } \`.
fn split_vertical_tags(text: &[char]) -> Result<Vec<String>, CoreError> {
    let mut segments: Vec<String> = Vec::new();
    let mut buffer = String::new();
    let mut index = 0usize;
    while index < text.len() {
        let ch = text[index];
        if ch == '\\'
            && text
                .get(index + 1)
                .is_some_and(|next| VERTICAL_ESCAPES.contains(next))
        {
            buffer.push(text[index + 1]);
            index += 2;
            continue;
        }
        if is_vertical_marker(ch) && text.get(index + 1) == Some(&'{') {
            if !buffer.is_empty() {
                segments.push(std::mem::take(&mut buffer));
            }
            let (inner, next) = read_vertical_tag(text, index + 2)?;
            segments.push(inner);
            index = next;
            continue;
        }
        buffer.push(ch);
        index += 1;
    }
    if !buffer.is_empty() {
        segments.push(buffer);
    }
    Ok(segments)
}

fn is_vertical_marker(ch: char) -> bool {
    ch == '^' || ch == '_'
}

/// Python `_read_vertical_tag`: one tag's prose and the index just past its
/// closing brace.
fn read_vertical_tag(text: &[char], start: usize) -> Result<(String, usize), CoreError> {
    let mut body = String::new();
    let mut index = start;
    while index < text.len() {
        let ch = text[index];
        if ch == '\\'
            && text
                .get(index + 1)
                .is_some_and(|next| VERTICAL_ESCAPES.contains(next))
        {
            body.push(text[index + 1]);
            index += 2;
            continue;
        }
        if ch == '}' {
            if body.is_empty() {
                return Err(domain(
                    "vertical-tag-empty: a vertical tag carries no text".to_string(),
                ));
            }
            return Ok((body, index + 1));
        }
        if is_vertical_marker(ch) && text.get(index + 1) == Some(&'{') {
            return Err(domain(
                "vertical-tag-nested: a vertical tag cannot contain another tag".to_string(),
            ));
        }
        body.push(ch);
        index += 1;
    }
    Err(domain(
        "vertical-tag-unclosed: a vertical tag is missing its closing brace".to_string(),
    ))
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
                "docx2typed-document-view-{name}-{}-{nanos}",
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
    /// binds the typed hash, plus the matching `edit.state.json` sidecar. The
    /// blocks are whatever the caller renders under the header.
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

    fn five_blocks() -> String {
        (1..=5)
            .map(|index| paragraph_block(&format!("P{index}"), &format!("body-{index}")))
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// The frozen invariant: `content` is byte-reproducible, so it can serve
    /// as `document_patch`'s diff base — two reads agree, and re-splitting
    /// and re-rendering the returned text is the identity.
    #[test]
    fn content_is_a_reproducible_patch_base() {
        let scratch = workdir("content", &five_blocks());
        let first = document_read_view(scratch.path(), None, 5, 8, "content").expect("read");
        let second = document_read_view(scratch.path(), None, 5, 8, "content").expect("read again");
        assert_eq!(first, second);
        assert_eq!(first.view, "content");
        assert_eq!(first.paragraphs, 5);
        assert_eq!(first.first_id.as_deref(), Some("P1"));
        assert_eq!(first.last_id.as_deref(), Some("P5"));
        assert_eq!(first.revision.len(), 64);
        assert_eq!(first.state, "clean");

        // Re-parsing the returned projection reproduces it exactly.
        let (header, blocks) = split_edit_blocks(&first.content).expect("re-split");
        assert_eq!(blocks.len(), 5);
        assert_eq!(
            format!("{header}\n\n{}\n", blocks.join("\n\n")),
            first.content
        );
        assert!(first.content.ends_with('\n'));
    }

    /// Windows clamp to the draft at both ends instead of erroring.
    #[test]
    fn window_bounds_clamp_to_the_draft() {
        let scratch = workdir("window", &five_blocks());
        let path = scratch.path();

        let head = document_read_view(path, Some("P1"), 5, 1, "content").expect("head window");
        assert_eq!(head.first_id.as_deref(), Some("P1"));
        assert_eq!(head.last_id.as_deref(), Some("P2"));
        assert_eq!(head.paragraphs, 5, "paragraphs counts the whole draft");
        assert_eq!(head.content.matches("<!--@p id=").count(), 2);

        let tail = document_read_view(path, Some("P5"), 1, 8, "content").expect("tail window");
        assert_eq!(tail.first_id.as_deref(), Some("P4"));
        assert_eq!(tail.last_id.as_deref(), Some("P5"));

        let middle = document_read_view(path, Some("P3"), 1, 1, "content").expect("middle window");
        assert_eq!(middle.first_id.as_deref(), Some("P2"));
        assert_eq!(middle.last_id.as_deref(), Some("P4"));

        // Bounds far past either end cover the whole draft, no error.
        let whole = document_read_view(path, Some("P3"), 1000, 1000, "content").expect("clamped");
        assert_eq!(
            whole.content,
            document_read_view(path, None, 0, 0, "content")
                .unwrap()
                .content
        );
    }

    #[test]
    fn unknown_anchor_reports_paragraph_not_found() {
        let scratch = workdir("anchor", &five_blocks());
        let error = document_read_view(scratch.path(), Some("P99"), 5, 8, "content")
            .expect_err("unknown anchor");
        assert_eq!(
            error.to_string(),
            "paragraph-not-found: anchor P99 not found in the draft"
        );
    }

    #[test]
    fn unknown_view_reports_document_read_invalid_view() {
        let scratch = workdir("view", &five_blocks());
        let error =
            document_read_view(scratch.path(), None, 5, 8, "spans").expect_err("unsupported view");
        assert!(error
            .to_string()
            .starts_with(&format!("{INVALID_VIEW_CODE}: ")));
        // Python rejects the view before it ever looks for the anchor.
        let error = document_read_view(scratch.path(), Some("P99"), 5, 8, "issues")
            .expect_err("unsupported view wins");
        assert!(error
            .to_string()
            .starts_with(&format!("{INVALID_VIEW_CODE}: ")));
    }

    #[test]
    fn outline_is_one_line_per_paragraph() {
        let blocks = [
            paragraph_block("P1", "alpha^{2}_x"),
            paragraph_block(
                "P2",
                &format!("{}\u{27e6}token kind=\"tab\"\u{27e7}", "b".repeat(90)),
            ),
        ]
        .join("\n\n");
        let scratch = workdir("outline", &blocks);
        let view = document_read_view(scratch.path(), None, 5, 8, "outline").expect("outline");
        assert_eq!(view.view, "outline");
        let lines: Vec<&str> = view.content.trim_end().lines().collect();
        assert_eq!(lines.len(), 3, "header + one line per paragraph");
        assert!(lines[0].starts_with("<!--@edit"));
        assert_eq!(lines[1], "<!--@p id=\"P1\"--> alpha2_x [8 chars]");
        assert!(lines[2].starts_with("<!--@p id=\"P2\"--> bbbb"));
        assert!(lines[2].ends_with(" [91 chars]"), "{}", lines[2]);
        assert!(lines[2].contains('\u{2026}'), "long preview ellipsis");
        assert_eq!(view.first_id.as_deref(), Some("P1"));
        assert_eq!(view.last_id.as_deref(), Some("P2"));
    }

    /// `auto` is content for a small draft and outline past 40k projection
    /// characters (Python `len(full_text) > 40_000`).
    #[test]
    fn auto_switches_to_outline_past_the_threshold() {
        let small = workdir("auto-small", &five_blocks());
        let view = document_read_view(small.path(), None, 5, 8, "auto").expect("auto small");
        assert_eq!(view.view, "content");
        assert!(view.content.contains("body-3"));

        let big = workdir(
            "auto-big",
            &paragraph_block("P1", &"x".repeat(AUTO_OUTLINE_THRESHOLD + 100)),
        );
        let view = document_read_view(big.path(), None, 5, 8, "auto").expect("auto big");
        assert_eq!(view.view, "outline");
        assert!(view.content.contains("[40100 chars]"), "{}", view.content);
    }

    /// `_visible_text` on the shapes the projection actually emits.
    #[test]
    fn visible_text_matches_the_python_projection() {
        assert_eq!(visible_text("plain prose").unwrap(), "plain prose");
        assert_eq!(visible_text("a^{2} + b_{1}").unwrap(), "a2 + b1");
        assert_eq!(
            visible_text(
                "tab\u{27e6}token kind=\"tab\"\u{27e7}br\u{27e6}token kind=\"br\"\u{27e7}"
            )
            .unwrap(),
            "tab\u{21b9}br\u{21b5}"
        );
        assert_eq!(
            visible_text("\u{27e6}range-start kind=\"ins-start\" id=\"1\"\u{27e7}x\u{27e6}range-end id=\"1\"\u{27e7}")
                .unwrap(),
            "\u{27e6}ins-start\u{27e7}x\u{27e7}"
        );
        assert_eq!(
            visible_text("\u{27e6}token id=\"1\"\u{27e7}").unwrap(),
            "\u{27e6}?\u{27e7}"
        );
        // Escapes and vertical-tag faults mirror the Python grammar.
        assert_eq!(visible_text("\\u27E6 literal").unwrap(), "\u{27e6} literal");
        assert!(visible_text("\u{27e6}unclosed").is_err());
        assert!(visible_text("\\q").is_err());
        assert!(visible_text("^{}").is_err());
        assert!(visible_text("^{a").is_err());
    }

    #[test]
    fn block_ident_reads_every_marker_kind() {
        assert_eq!(
            block_ident("<!--@p id=\"P1\" base=\"\"/>\nbody"),
            Some(("p".to_string(), "P1".to_string()))
        );
        assert_eq!(
            block_ident("<!--@new temp=\"T1\"/>\nbody"),
            Some(("new".to_string(), "T1".to_string()))
        );
        assert_eq!(
            block_ident("<!--@delete id=\"P2\"/>\n"),
            Some(("delete".to_string(), "P2".to_string()))
        );
        assert_eq!(block_ident("\n<!--@p id=\"P3\"/>"), None);
        assert_eq!(block_ident("<!--@p id=\"\"/>\n"), None);
    }
}
