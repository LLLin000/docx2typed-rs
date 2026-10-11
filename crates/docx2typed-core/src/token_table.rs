//! The typed document's token table (#52).
//!
//! Python's parser allocates `N0`, `N1`, ... from ONE counter as it walks the
//! document, and `typed.md` renders only some of those tokens. `revisions.docx`
//! shows why that matters: its markup uses N0, N1, N3, N4, N5, N6, N7 -- **N2
//! is missing**, because it belongs to a token the markup never prints. So the
//! ids cannot be derived from what is rendered; the traversal has to be
//! reproduced.
//!
//! Two ordering rules decide the ids, and both are counter-intuitive:
//!
//! 1. **`document.xml` is parsed FIRST**, then the header/footer/note parts
//!    (`parse_package_document`). But `typed.md` LISTS headers first. So token
//!    order is not paragraph order, and `parts.docx` has body tokens N0/N1
//!    while its header paragraph appears above them.
//! 2. **Within a paragraph the container walks before the paragraph mark**, so
//!    a mark revision's token is allocated last (`_parse_paragraph`).
//!
//! This module reproduces `_parse_container` / `_parse_run` in that order.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::prose::{part_path, scan_document, scan_part};
use crate::xml_walker::{build_tree, scan_tags, ElementNode};
use crate::CoreError;

/// One allocated token: what the parser saw, in the order it saw it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenRecord {
    /// `N0`, `N1`, ... -- assigned by the single counter, never renumbered.
    pub id: String,
    /// `hyperlink`, `revision`, `bookmark-start`, `opaque`, `unsupported-run`,
    /// `paragraph-mark`, `rpr-change`, `empty-run`, or an inline element name.
    pub kind: String,
    pub attrs: BTreeMap<String, String>,
    pub part: String,
    pub paragraph_id: String,
    /// Byte range of the token's element inside its part.
    pub start: usize,
    pub end: usize,
    /// `raw`: the element's own XML, for the kinds that publish it.
    pub raw: String,
    /// `open` / `close`: the tag pair, for a range or a revision.
    pub open: String,
    pub close: String,
    /// The run's style, for an inline or a run-shaped token.
    pub style_id: String,
}

/// One node of a paragraph's typed tree.
///
/// The flat token list was not enough to render the markup: `_node_to_markup`
/// walks a TREE, so a `<docx-range>` has to hold its children and has to sit
/// where they sit. Measured against the reference, a flat list put the range
/// AFTER its own text and left it empty:
///
///   py  : 超链接：<docx-range id="N0" kind="hyperlink" r:id="rId9">点击此处</docx-range>
///   flat: 超链接：点击此处<docx-range id="N0" kind="hyperlink" r:id="rId9"></docx-range>
///
/// The ids still come from the one counter, allocated in the same order, so
/// the flat table and this tree agree by construction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TypedNode {
    Text {
        style_id: String,
        text: String,
        /// Vertical alignment as a dimension of its own, not part of the style
        /// (`^{…}` / `_{…}` in a schema-2 typed source). `None` means none.
        ///
        /// The extractor leaves this `None`; `promote_vertical_alignment` is
        /// what factors an eligible (vertical, base) style pair into it.
        vertical: Option<String>,
    },
    /// bookmark/comment anchors: no children, no style.
    Anchor {
        token_id: String,
        kind: String,
        attrs: BTreeMap<String, String>,
    },
    /// tab/br/cr and friends: no children, carries the run's style.
    Inline {
        token_id: String,
        kind: String,
        style_id: String,
        attrs: BTreeMap<String, String>,
    },
    /// An opaque interior (a drawing, a field, math).
    Opaque {
        token_id: String,
        kind: String,
        attrs: BTreeMap<String, String>,
    },
    /// A hyperlink or other range: carries its children.
    Range {
        token_id: String,
        kind: String,
        attrs: BTreeMap<String, String>,
        children: Vec<TypedNode>,
    },
    /// A tracked revision: carries its children.
    Revision {
        token_id: String,
        kind: String,
        attrs: BTreeMap<String, String>,
        children: Vec<TypedNode>,
    },
}

impl TypedNode {
    pub fn token_id(&self) -> Option<&str> {
        match self {
            TypedNode::Text { .. } => None,
            TypedNode::Anchor { token_id, .. }
            | TypedNode::Inline { token_id, .. }
            | TypedNode::Opaque { token_id, .. }
            | TypedNode::Range { token_id, .. }
            | TypedNode::Revision { token_id, .. } => Some(token_id),
        }
    }

    /// `_token_ids`: ranges and revisions first, then their children.
    pub fn collect_token_ids(&self, out: &mut Vec<(String, String)>) {
        match self {
            TypedNode::Text { .. } => {}
            TypedNode::Range {
                token_id,
                kind,
                children,
                ..
            }
            | TypedNode::Revision {
                token_id,
                kind,
                children,
                ..
            } => {
                out.push((token_id.clone(), kind.clone()));
                for child in children {
                    child.collect_token_ids(out);
                }
            }
            TypedNode::Anchor { token_id, kind, .. }
            | TypedNode::Inline { token_id, kind, .. }
            | TypedNode::Opaque { token_id, kind, .. } => {
                out.push((token_id.clone(), kind.clone()));
            }
        }
    }
}

/// Remove every `xmlns:PREFIX="URI"` / `xmlns="URI"` attribute from a fragment.
///
/// ElementTree consumes namespace declarations at parse time, so they never
/// reach the serialised output; `with_namespace` re-emits the ones the subtree
/// actually uses, hoisted onto the root. Without this, a declaration the source
/// wrote on a NESTED element stayed put and was duplicated by the hoist.
fn strip_xmlns_declarations(fragment: &str) -> String {
    let mut out = String::with_capacity(fragment.len());
    let mut cursor = 0usize;
    while let Some(at) = fragment[cursor..].find("xmlns") {
        let start = cursor + at;
        // Only an attribute NAME start counts: whitespace before, `:`/`=`
        // after (so `<w:t>xmlns:x` in text is left alone).
        let before = fragment[..start].chars().next_back();
        if !matches!(before, Some(c) if c.is_whitespace()) {
            out.push_str(&fragment[cursor..start + "xmlns".len()]);
            cursor = start + "xmlns".len();
            continue;
        }
        let after = &fragment[start + "xmlns".len()..];
        let Some(eq) = after.find('=') else {
            out.push_str(&fragment[cursor..]);
            return out;
        };
        if !(after.starts_with(':') || after.starts_with('='))
            || after[..eq].contains(|c: char| c.is_whitespace())
        {
            out.push_str(&fragment[cursor..start + "xmlns".len()]);
            cursor = start + "xmlns".len();
            continue;
        }
        let rest = &after[eq + 1..];
        let quote = rest.chars().next().unwrap_or(' ');
        if quote != '"' && quote != '\'' {
            out.push_str(&fragment[cursor..start + "xmlns".len()]);
            cursor = start + "xmlns".len();
            continue;
        }
        let Some(end) = rest[1..].find(quote) else {
            out.push_str(&fragment[cursor..]);
            return out;
        };
        // Drop the declaration together with the whitespace that separated it.
        out.push_str(&fragment[cursor..start]);
        if out.ends_with(char::is_whitespace) {
            out.pop();
        }
        cursor = start + "xmlns".len() + eq + 1 + 1 + end + 1;
    }
    out.push_str(&fragment[cursor..]);
    out
}

/// The element's opening tag with the namespaces ElementTree would emit.
///
/// `etree_xml` re-serialises through ElementTree, which writes every namespace
/// declaration the subtree uses onto the ROOT of the fragment -- measured on
/// `w:fldSimple`, whose reference `raw` is
/// `<w:fldSimple xmlns:w="..." w:instr=" PAGE ">`. The source bytes carry the
/// declaration on the document root instead, so it has to be re-added.
fn with_namespace(
    tag: &str,
    namespaces: &BTreeMap<String, String>,
    unknown: &mut BTreeMap<String, String>,
) -> String {
    if !tag.starts_with('<') || tag.starts_with("</") {
        return tag.to_string();
    }
    // ElementTree consumes namespace declarations at PARSE time, so they never
    // reach the serialised fragment -- including one the source wrote on a
    // NESTED element (complex.docx's `<wp:inline xmlns:a=...>`). Leaving them
    // in place both kept the source prefixes and duplicated the declarations
    // the hoist below re-emits; the whole-subtree `contains("xmlns")` bail-out
    // this replaces skipped the hoist entirely, which is why the drawing token
    // came out as raw source bytes.
    let stripped = strip_xmlns_declarations(tag);
    let tag = stripped.as_str();
    let Some(name_end) = tag.find(|c: char| c.is_whitespace() || c == '>' || c == '/') else {
        return tag.to_string();
    };
    let Some((own_prefix, _)) = tag[1..name_end].split_once(':') else {
        return tag.to_string();
    };

    // Which prefix maps to which URI, and which of those ElementTree knows.
    const REGISTERED: [(&str, &str); 4] = [
        (
            "w",
            "http://schemas.openxmlformats.org/wordprocessingml/2006/main",
        ),
        (
            "r",
            "http://schemas.openxmlformats.org/officeDocument/2006/relationships",
        ),
        (
            "w15",
            "http://schemas.microsoft.com/office/word/2012/wordml",
        ),
        (
            "w16du",
            "http://schemas.microsoft.com/office/word/2023/wordml/word16du",
        ),
    ];
    // Every prefix used ANYWHERE in the subtree, in ElementTree's order: per
    // element in document order, its TAG then its ATTRIBUTES. Reading the
    // attributes of the root alone missed `r:embed` on a nested `<a:blip>`,
    // and `r` never made it into the declaration list.
    let mut used: Vec<String> = Vec::new();
    let push = |prefix: &str, used: &mut Vec<String>| {
        if !used.iter().any(|entry| entry == prefix) {
            used.push(prefix.to_string());
        }
    };
    let bytes = tag.as_bytes();
    let mut cursor = 0usize;
    while let Some(at) = tag[cursor..].find('<') {
        let open = cursor + at;
        // The end of this tag, skipping quoted attribute values.
        let mut index = open + 1;
        let mut quote: Option<u8> = None;
        while index < bytes.len() {
            let byte = bytes[index];
            match quote {
                Some(open_quote) => {
                    if byte == open_quote {
                        quote = None;
                    }
                }
                None => {
                    if byte == b'"' || byte == b'\'' {
                        quote = Some(byte);
                    } else if byte == b'>' {
                        break;
                    }
                }
            }
            index += 1;
        }
        let end = index.min(bytes.len());
        let raw = &bytes[open..end];
        let skip = usize::from(raw.get(1) == Some(&b'/'));
        let name: String = String::from_utf8_lossy(&raw[1 + skip..])
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == ':' || *c == '_' || *c == '-')
            .collect();
        if let Some((found, _)) = name.split_once(':') {
            push(found, &mut used);
        }
        for (key, _) in crate::prose::canonical_attribute_pairs(raw) {
            if let Some((attribute_prefix, _)) = key.split_once(':') {
                push(attribute_prefix, &mut used);
            }
        }
        cursor = end.max(open + 1);
    }

    // Rename every unregistered prefix, in the order ElementTree met them.
    let mut renames: Vec<(String, String, String)> = Vec::new();
    let mut seen_uris: Vec<String> = Vec::new();
    for prefix in &used {
        let Some(uri) = namespaces.get(prefix) else {
            continue;
        };
        if REGISTERED.iter().any(|(_, known_uri)| known_uri == uri) {
            if !seen_uris.contains(uri) {
                seen_uris.push(uri.clone());
            }
            continue;
        }
        let assigned = match unknown.get(uri) {
            Some(existing) => existing.clone(),
            None => {
                let next = format!("ns{}", seen_uris.len());
                unknown.insert(uri.clone(), next.clone());
                next
            }
        };
        if !seen_uris.contains(uri) {
            seen_uris.push(uri.clone());
        }
        renames.push((prefix.clone(), assigned, uri.clone()));
    }

    let mut body = tag[name_end..].to_string();
    for (prefix, assigned, _) in &renames {
        body = body.replace(&format!("{prefix}:"), &format!("{assigned}:"));
    }
    let renamed_own = renames
        .iter()
        .find(|(prefix, _, _)| prefix == own_prefix)
        .map(|(_, assigned, _)| assigned.clone())
        .unwrap_or_else(|| own_prefix.to_string());

    let mut out = format!(
        "<{renamed_own}:{local}",
        local = tag[1..name_end]
            .split_once(':')
            .map(|(_, l)| l)
            .unwrap_or("")
    );
    // ElementTree writes every declaration on the serialised ROOT, SORTED BY
    // PREFIX (`_serialize_xml`: `sorted(namespaces.items(), key=lambda x:
    // x[1])`). Emitting "renames then registered" happened to match the
    // fixtures whose only registered prefix was `w`; complex.docx needs
    // ns1/ns2/ns3 before r before w.
    let mut declarations: Vec<(String, String)> = Vec::new();
    for (_, assigned, uri) in &renames {
        if !declarations.iter().any(|(prefix, _)| prefix == assigned) {
            declarations.push((assigned.clone(), uri.clone()));
        }
    }
    for (prefix, known_uri) in REGISTERED {
        if !used.iter().any(|entry| entry == prefix) {
            continue;
        }
        if let Some(found) = namespaces
            .iter()
            .find(|(_, value)| value.as_str() == known_uri)
            .map(|(key, _)| key.clone())
        {
            if !declarations.iter().any(|(prefix, _)| *prefix == found) {
                declarations.push((found, known_uri.to_string()));
            }
        }
    }
    declarations.sort();
    for (prefix, uri) in &declarations {
        out.push_str(&format!(" xmlns:{prefix}=\"{uri}\""));
    }
    // ElementTree closes an empty element as ` />`.
    out.push_str(&body.replace("/>", " />"));
    if !body.ends_with("/>") {
        for (prefix, assigned, _) in &renames {
            out = out.replace(&format!("</{prefix}:"), &format!("</{assigned}:"));
        }
    }
    out
}

/// The enclosing `w:footnote`/`w:endnote`'s `w:id`, for a paragraph inside one.
///
/// `locate_part_xml` records `current_entry or ""` per paragraph, so a note
/// paragraph names the note it belongs to and every other paragraph names
/// nothing. The values are the fixture's own note ids (-1, 0, 1), which is why
/// they look like indices.
pub fn part_entry_id(part: &str, paragraph_start: usize, xml: &[u8]) -> String {
    let key = typed_part_key(part);
    if key != "footnotes" && key != "endnotes" && key != "comments" {
        return String::new();
    }
    let head = String::from_utf8_lossy(&xml[..paragraph_start.min(xml.len())]);
    // The LAST opening entry tag before the paragraph is the one containing it.
    // Comments count too: `locate_part_xml` treats `comment` alongside
    // `footnote`/`endnote`, and anchors.docx's comments.P0 records "0".
    let mut best: Option<(usize, String)> = None;
    for marker in ["<w:footnote ", "<w:endnote ", "<w:comment "] {
        let Some(at) = head.rfind(marker) else {
            continue;
        };
        let rest = &head[at + marker.len()..];
        let Some(id_at) = rest.find("w:id=\"") else {
            continue;
        };
        let after = &rest[id_at + 6..];
        let Some(end) = after.find('"') else { continue };
        let better = match &best {
            Some((best_at, _)) => at > *best_at,
            None => true,
        };
        if better {
            best = Some((at, after[..end].to_string()));
        }
    }
    best.map(|(_, id)| id).unwrap_or_default()
}

/// `visible_text`: the final-view text of a paragraph's nodes.
///
/// Mirrors the reference exactly -- text passes through, a range passes its
/// children, a revision passes them only for insert/move_to, an inline `tab`
/// becomes a tab and `br`/`cr` a newline, and an opaque becomes
/// `[opaque:kind]`.
///
/// The inventory's own `visible_text` is built from `w:t` leaves alone, which
/// is why a note paragraph of pure separators read as EMPTY while typed.md read
/// `[opaque:separator]` -- and why the two sides disagreed about whether
/// anything had been edited at all.
pub fn visible_text(nodes: &[TypedNode]) -> String {
    let mut out = String::new();
    for node in nodes {
        match node {
            TypedNode::Text { text, .. } => out.push_str(text),
            TypedNode::Range { children, .. } => out.push_str(&visible_text(children)),
            TypedNode::Revision { kind, children, .. } => {
                if kind == "insert" || kind == "move_to" {
                    out.push_str(&visible_text(children));
                }
            }
            TypedNode::Inline { kind, .. } => match kind.as_str() {
                "tab" => out.push('\t'),
                "br" | "cr" => out.push('\n'),
                _ => {}
            },
            TypedNode::Opaque { kind, .. } => out.push_str(&format!("[opaque:{kind}]")),
            TypedNode::Anchor { .. } => {}
        }
    }
    out
}

const PROJECTION_TOKEN_START: char = '\u{27e6}';
const PROJECTION_TOKEN_END: char = '\u{27e7}';

fn escape_projection_text(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            PROJECTION_TOKEN_START => out.push_str("\\\u{27e6}"),
            PROJECTION_TOKEN_END => out.push_str("\\\u{27e7}"),
            other => out.push(other),
        }
    }
    out
}

fn projection_attr_value(value: &str) -> String {
    let escaped = value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;");
    format!("\"{escaped}\"")
}

fn render_node_attrs(attrs: &BTreeMap<String, String>) -> String {
    let mut ordered: Vec<(String, String)> = Vec::new();
    if let Some(v) = attrs.get("id") {
        ordered.push(("id".to_string(), v.clone()));
    }
    if let Some(v) = attrs.get("kind") {
        ordered.push(("kind".to_string(), v.clone()));
    }
    for (k, v) in attrs {
        if k != "id" && k != "kind" {
            ordered.push((k.clone(), v.clone()));
        }
    }
    ordered
        .into_iter()
        .map(|(k, v)| format!("{k}={}", projection_attr_value(&v)))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn project_node(node: &TypedNode) -> String {
    match node {
        TypedNode::Text { text, .. } => escape_projection_text(text),
        TypedNode::Anchor {
            token_id,
            kind,
            attrs,
        } => {
            let mut map = attrs.clone();
            map.insert("id".to_string(), token_id.clone());
            map.insert("kind".to_string(), kind.clone());
            let rendered = render_node_attrs(&map);
            format!("{PROJECTION_TOKEN_START}token {rendered}{PROJECTION_TOKEN_END}")
        }
        TypedNode::Inline {
            token_id,
            kind,
            style_id,
            attrs,
        } => {
            let mut map = attrs.clone();
            map.insert("id".to_string(), token_id.clone());
            map.insert("kind".to_string(), kind.clone());
            if !style_id.is_empty() {
                map.insert("style".to_string(), style_id.clone());
            }
            let rendered = render_node_attrs(&map);
            format!("{PROJECTION_TOKEN_START}token {rendered}{PROJECTION_TOKEN_END}")
        }
        TypedNode::Opaque {
            token_id,
            kind,
            attrs,
        } => {
            let mut map = attrs.clone();
            map.insert("id".to_string(), token_id.clone());
            map.insert("kind".to_string(), kind.clone());
            let rendered = render_node_attrs(&map);
            format!("{PROJECTION_TOKEN_START}token {rendered}{PROJECTION_TOKEN_END}")
        }
        TypedNode::Range {
            token_id,
            kind,
            attrs,
            children,
        } => {
            let mut map = attrs.clone();
            map.insert("id".to_string(), token_id.clone());
            map.insert("kind".to_string(), kind.clone());
            let rendered = render_node_attrs(&map);
            let inner = project_nodes(children);
            format!(
                "{PROJECTION_TOKEN_START}range-start {rendered}{PROJECTION_TOKEN_END}{inner}{PROJECTION_TOKEN_START}range-end id={}{PROJECTION_TOKEN_END}",
                projection_attr_value(token_id)
            )
        }
        TypedNode::Revision {
            token_id,
            kind,
            attrs,
            children,
        } => {
            if kind == "insert" || kind == "move_to" {
                let kind_name = if kind == "insert" {
                    "insert"
                } else {
                    "move-to"
                };
                let mut map = attrs.clone();
                map.insert("id".to_string(), token_id.clone());
                let mut ordered: Vec<(String, String)> = vec![("id".to_string(), token_id.clone())];
                for (k, v) in &map {
                    if k != "id" {
                        ordered.push((k.clone(), v.clone()));
                    }
                }
                ordered.sort_by(|a, b| {
                    if a.0 == "id" {
                        std::cmp::Ordering::Less
                    } else if b.0 == "id" {
                        std::cmp::Ordering::Greater
                    } else {
                        a.0.cmp(&b.0)
                    }
                });
                let rendered = ordered
                    .into_iter()
                    .map(|(k, v)| format!("{k}={}", projection_attr_value(&v)))
                    .collect::<Vec<_>>()
                    .join(" ");
                let inner = project_nodes(children);
                format!(
                    "{PROJECTION_TOKEN_START}{kind_name} {rendered}{PROJECTION_TOKEN_END}{inner}{PROJECTION_TOKEN_START}/{kind_name} id={}{PROJECTION_TOKEN_END}",
                    projection_attr_value(token_id)
                )
            } else {
                format!(
                    "{PROJECTION_TOKEN_START}revision-gap id={} kind={}{PROJECTION_TOKEN_END}",
                    projection_attr_value(token_id),
                    projection_attr_value(kind)
                )
            }
        }
    }
}

pub fn project_nodes(nodes: &[TypedNode]) -> String {
    nodes.iter().map(project_node).collect()
}

/// The `tokens` map `format.json` publishes: id -> the record, sorted by id.
///
/// The payload differs by kind, exactly as `_TokenTable.add` records it: a
/// range or revision carries `open`/`close`, everything else carries `raw`, and
/// the run-shaped kinds also carry `style_id`.
pub fn tokens_map(tokens: &[TokenRecord]) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for token in tokens {
        let mut record = serde_json::Map::new();
        record.insert("kind".to_string(), serde_json::json!(token.kind));
        record.insert(
            "attrs".to_string(),
            serde_json::Value::Object(
                token
                    .attrs
                    .iter()
                    .map(|(k, v)| (k.clone(), serde_json::json!(v)))
                    .collect(),
            ),
        );
        if matches!(token.kind.as_str(), "hyperlink" | "revision") {
            record.insert("open".to_string(), serde_json::json!(token.open));
            record.insert("close".to_string(), serde_json::json!(token.close));
        } else {
            record.insert("raw".to_string(), serde_json::json!(token.raw));
        }
        if matches!(
            token.kind.as_str(),
            "empty-run" | "rpr-change" | "unsupported-run"
        ) || KNOWN_INLINE.contains(&token.kind.as_str())
        {
            record.insert("style_id".to_string(), serde_json::json!(token.style_id));
        }
        map.insert(token.id.clone(), serde_json::Value::Object(record));
    }
    serde_json::Value::Object(map)
}

/// `source_track_enabled`: whether `word/settings.xml` turns track changes on.
pub fn source_track_enabled(package: &[u8]) -> bool {
    let reader = std::io::Cursor::new(package.to_vec());
    let Ok(mut archive) = zip::ZipArchive::new(reader) else {
        return false;
    };
    let Ok(mut file) = archive.by_name("word/settings.xml") else {
        return false;
    };
    let mut text = String::new();
    if std::io::Read::read_to_string(&mut file, &mut text).is_err() {
        return false;
    }
    // A `<w:trackChanges/>` with no explicit false value.
    let mut cursor = 0usize;
    while let Some(at) = text[cursor..].find("trackChanges") {
        let start = cursor + at;
        let end = text[start..]
            .find('>')
            .map(|n| start + n)
            .unwrap_or(text.len());
        let tag = &text[start..end];
        if !tag.contains("w:val=\"false\"") && !tag.contains("w:val=\"0\"") {
            return true;
        }
        cursor = end + 1;
    }
    false
}

/// `skeleton`: the paragraph's shape by node kind, which is what `format.json`
/// records instead of the text itself.
///
/// Mirrors `skeleton` exactly: a text node is `["text", style]`, an anchor
/// `["anchor", kind, attrs]`, an inline `["inline", kind, style, attrs]`, an
/// opaque `["opaque", kind, attrs]`, and a range or revision
/// `[<kind>, kind, attrs, children]` with its own children nested.
pub fn skeleton(nodes: &[TypedNode]) -> Vec<serde_json::Value> {
    nodes.iter().map(skeleton_node).collect()
}

fn attrs_pairs(attrs: &BTreeMap<String, String>) -> serde_json::Value {
    // `[[key, value], ...]` with the keys SORTED -- `sorted(node.attrs.items())`.
    serde_json::Value::Array(
        attrs
            .iter()
            .map(|(key, value)| serde_json::json!([key, value]))
            .collect(),
    )
}

fn skeleton_node(node: &TypedNode) -> serde_json::Value {
    match node {
        TypedNode::Text { style_id, .. } => serde_json::json!(["text", style_id]),
        TypedNode::Anchor { kind, attrs, .. } => {
            serde_json::json!(["anchor", kind, attrs_pairs(attrs)])
        }
        TypedNode::Inline {
            kind,
            style_id,
            attrs,
            ..
        } => serde_json::json!(["inline", kind, style_id, attrs_pairs(attrs)]),
        TypedNode::Opaque { kind, attrs, .. } => {
            serde_json::json!(["opaque", kind, attrs_pairs(attrs)])
        }
        TypedNode::Range {
            kind,
            attrs,
            children,
            ..
        } => serde_json::json!(["range", kind, attrs_pairs(attrs), skeleton(children)]),
        TypedNode::Revision {
            kind,
            attrs,
            children,
            ..
        } => serde_json::json!(["revision", kind, attrs_pairs(attrs), skeleton(children)]),
    }
}

/// `_token_ids`: `[[token_id, kind], ...]`, with a range's or revision's
/// children following it -- which `TypedNode::collect_token_ids` already does.
pub fn token_ids(nodes: &[TypedNode]) -> Vec<serde_json::Value> {
    let mut collected: Vec<(String, String)> = Vec::new();
    for node in nodes {
        node.collect_token_ids(&mut collected);
    }
    collected
        .into_iter()
        .map(|(id, kind)| serde_json::json!([id, kind]))
        .collect()
}

/// A paragraph's typed tree, plus the paragraph's own mark token.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParagraphNodes {
    pub part: String,
    pub paragraph_id: String,
    /// Byte offset of the paragraph inside its part.
    pub start: usize,
    /// The enclosing note's `w:id` for a footnote/endnote paragraph; "" else.
    pub part_entry_id: String,
    pub nodes: Vec<TypedNode>,
    /// `(kind, token_id, attrs)` when the paragraph mark carries a revision.
    pub mark: Option<(String, String, BTreeMap<String, String>)>,
    pub section_bearing: bool,
}

/// `merge_adjacent_text`: same style AND same vertical alignment merges, so a
/// run the scanner split does not become two nodes.
fn merge_adjacent_text(nodes: Vec<TypedNode>) -> Vec<TypedNode> {
    let mut merged: Vec<TypedNode> = Vec::new();
    for node in nodes {
        if let TypedNode::Text {
            style_id,
            text,
            vertical,
        } = &node
        {
            if let Some(TypedNode::Text {
                style_id: last_style,
                text: last_text,
                vertical: last_vertical,
            }) = merged.last_mut()
            {
                if last_style == style_id && last_vertical == vertical {
                    last_text.push_str(text);
                    continue;
                }
            }
        }
        merged.push(node);
    }
    merged
}

/// Python's `qname`: a known namespace becomes its prefix, an unknown one
/// becomes `{uri}local`.
///
/// The literal prefix is NOT the answer: `m:oMath` is declared
/// `xmlns:m=".../officeDocument/2006/math"`, which is not one of the five
/// namespaces `qname` knows, so the reference records
/// `{http://schemas.openxmlformats.org/officeDocument/2006/math}oMath`. Using
/// the literal prefix matched everything `w:`-prefixed and nothing else.
fn qname(literal: &str, namespaces: &BTreeMap<String, String>) -> String {
    let Some((prefix, local)) = literal.split_once(':') else {
        return literal.to_string();
    };
    match namespaces.get(prefix).map(String::as_str) {
        Some("http://schemas.openxmlformats.org/wordprocessingml/2006/main") => {
            format!("w:{local}")
        }
        Some("http://schemas.openxmlformats.org/officeDocument/2006/relationships") => {
            format!("r:{local}")
        }
        Some("http://schemas.microsoft.com/office/word/2012/wordml") => format!("w15:{local}"),
        Some("http://www.w3.org/XML/1998/namespace") => format!("xml:{local}"),
        Some("http://schemas.microsoft.com/office/word/2023/wordml/word16du") => {
            format!("w16du:{local}")
        }
        Some(uri) => format!("{{{uri}}}{local}"),
        None => literal.to_string(),
    }
}

/// Every `xmlns:PREFIX="URI"` declared anywhere in a part.
///
/// NOT just the root: `m:oMath` declares its namespace on the math element
/// itself, so a root-only map left it unresolved and the reference's
/// `{uri}oMath` came out as the literal `m:oMath`. Prefixes are unique within
/// a part in practice, so merging every declaration is both simpler and
/// correct for the case that mattered.
fn part_namespaces(bytes: &[u8], _nodes: &[ElementNode]) -> BTreeMap<String, String> {
    // A direct scan, because `canonical_attribute_pairs` deliberately SKIPS
    // xmlns declarations (ElementTree consumes them, so the canonical form
    // never sees them) -- reading them through it found nothing at all.
    let text = String::from_utf8_lossy(bytes);
    let mut out = BTreeMap::new();
    let mut cursor = 0usize;
    while let Some(at) = text[cursor..].find("xmlns:") {
        let start = cursor + at + "xmlns:".len();
        let Some(eq) = text[start..].find('=') else {
            break;
        };
        let prefix = text[start..start + eq].trim().to_string();
        let rest = &text[start + eq + 1..];
        let quote = rest.chars().next().unwrap_or('"');
        if quote == '"' || quote == '\'' {
            if let Some(end) = rest[1..].find(quote) {
                let uri = rest[1..1 + end].to_string();
                if !prefix.is_empty() && !prefix.contains([' ', '\t', '\n', '\r']) {
                    out.entry(prefix).or_insert(uri);
                }
            }
        }
        cursor = start + eq + 1;
    }
    out
}

/// `known_inline`: run children that are inlines rather than opaque interiors.
const KNOWN_INLINE: [&str; 14] = [
    "t",
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

/// `bookmarkStart` / `bookmarkEnd` / `commentRangeStart` / `commentRangeEnd`
/// and the kind each records.
fn anchor_kind(name: &str) -> Option<&'static str> {
    match name {
        "bookmarkStart" => Some("bookmark-start"),
        "bookmarkEnd" => Some("bookmark-end"),
        "commentRangeStart" => Some("comment-start"),
        "commentRangeEnd" => Some("comment-end"),
        _ => None,
    }
}

/// `ins` / `del` / `moveFrom` / `moveTo` and the kind each records.
fn revision_kind(name: &str) -> Option<&'static str> {
    match name {
        "ins" => Some("insert"),
        "del" => Some("delete"),
        "moveFrom" => Some("move_from"),
        "moveTo" => Some("move_to"),
        _ => None,
    }
}

struct Walker<'a> {
    bytes: &'a [u8],
    nodes: &'a [ElementNode],
    namespaces: BTreeMap<String, String>,
    /// ElementTree's `ns0`, `ns1`, ... numbering is PER PARSE, not per
    /// serialisation: `vml` becomes `ns1` in boxes.docx while math becomes
    /// `ns0` in revisions.docx. The counter therefore lives with the walk.
    unknown_prefixes: &'a mut BTreeMap<String, String>,
    part: String,
    paragraph_id: String,
    tokens: &'a mut Vec<TokenRecord>,
}

impl<'a> Walker<'a> {
    fn add(&mut self, kind: &str, attrs: BTreeMap<String, String>, node: &ElementNode) -> String {
        let id = format!("N{}", self.tokens.len());
        let raw = with_namespace(
            &String::from_utf8_lossy(&self.bytes[node.open_start..node.end]),
            &self.namespaces,
            self.unknown_prefixes,
        );
        // `open`/`close` are built by `element_start_xml`/`element_end_xml` from
        // the tag and its ATTRIBUTES, so they carry no namespace declaration --
        // unlike `raw`, which goes through `etree_xml`. Measured on N0, whose
        // reference `open` is `<w:ins w:id="1" ...>` with no xmlns.
        let open =
            String::from_utf8_lossy(&self.bytes[node.open_start..node.open_end]).into_owned();
        let close = if node.self_closing {
            String::new()
        } else {
            String::from_utf8_lossy(&self.bytes[node.close_start..node.end]).into_owned()
        };
        self.tokens.push(TokenRecord {
            id,
            kind: kind.to_string(),
            attrs,
            part: self.part.clone(),
            paragraph_id: self.paragraph_id.clone(),
            start: node.open_start,
            end: node.end,
            raw,
            open,
            close,
            style_id: String::new(),
        });
        self.tokens
            .last()
            .map(|token| token.id.clone())
            .unwrap_or_default()
    }

    /// The same, with the run's style recorded.
    fn add_styled(
        &mut self,
        kind: &str,
        attrs: BTreeMap<String, String>,
        node: &ElementNode,
        style_id: &str,
    ) -> String {
        let id = self.add(kind, attrs, node);
        if let Some(last) = self.tokens.last_mut() {
            last.style_id = style_id.to_string();
        }
        id
    }

    /// The literal tag name and attributes of one element, as the reference's
    /// `qname` / `_attrs` record them.
    fn tag(&self, node: &ElementNode) -> (String, BTreeMap<String, String>) {
        let raw = &self.bytes[node.open_start..node.open_end];
        let mut end = 1;
        while end < raw.len()
            && !raw[end].is_ascii_whitespace()
            && raw[end] != b'>'
            && raw[end] != b'/'
        {
            end += 1;
        }
        let literal = String::from_utf8_lossy(&raw[1..end]).into_owned();
        let name = qname(&literal, &self.namespaces);
        let mut attrs = BTreeMap::new();
        for (key, value) in
            crate::prose::canonical_attribute_pairs(&self.bytes[node.open_start..node.end])
        {
            // Attribute names go through `qname` too, so `w:val` stays `w:val`.
            attrs.insert(qname(&key, &self.namespaces), value);
        }
        (name, attrs)
    }

    /// `_parse_container`: walk one element list, allocating in ITS order.
    fn container(&mut self, children: &[usize]) {
        for &child in children {
            let node = self.nodes[child].clone();
            let name = node.name.clone();
            if name == "pPr" {
                continue;
            }
            if name == "proofErr" {
                let mut attrs = BTreeMap::new();
                attrs.insert("tag".to_string(), "w:proofErr".to_string());
                self.add("opaque", attrs, &node);
                continue;
            }
            if name == "r" {
                self.run(&node);
                continue;
            }
            if name == "hyperlink" {
                let (_, attrs) = self.tag(&node);
                self.add("hyperlink", attrs, &node);
                self.container(&node.children.clone());
                continue;
            }
            if let Some(kind) = revision_kind(&name) {
                let (_, attrs) = self.tag(&node);
                self.add("revision", attrs, &node);
                let _ = kind;
                self.container(&node.children.clone());
                continue;
            }
            if let Some(kind) = anchor_kind(&name) {
                let (_, attrs) = self.tag(&node);
                self.add(kind, attrs, &node);
                continue;
            }
            let (tag_name, _) = self.tag(&node);
            let mut attrs = BTreeMap::new();
            attrs.insert("tag".to_string(), tag_name);
            self.add("opaque", attrs, &node);
        }
    }

    /// `_parse_mark_revision`: a self-closing `w:ins`/`w:del` inside `pPr/rPr`
    /// is the paragraph mark's own revision, and it takes a token like any
    /// other -- allocated after the container has finished.
    fn paragraph_mark_token(
        &mut self,
        children: &[usize],
    ) -> Option<(String, String, BTreeMap<String, String>)> {
        for &child in children {
            let node = &self.nodes[child];
            if node.name != "pPr" {
                continue;
            }
            for &inner in &node.children.clone() {
                let inner_node = &self.nodes[inner];
                if inner_node.name != "rPr" {
                    continue;
                }
                for &deepest in &inner_node.children.clone() {
                    let mark = &self.nodes[deepest];
                    if mark.name == "ins" || mark.name == "del" {
                        let kind = if mark.name == "ins" {
                            "insert"
                        } else {
                            "delete"
                        };
                        let (_, attrs) = self.tag(mark);
                        let mark = mark.clone();
                        let token_id = self.add("paragraph-mark", attrs.clone(), &mark);
                        // The mark's `raw` is the regex MATCH from the pPr
                        // bytes, not an `etree_xml` re-serialisation, so it
                        // carries no namespace declaration -- measured on N2,
                        // whose reference raw is `<w:ins w:id="3" .../>`.
                        if let Some(last) = self.tokens.last_mut() {
                            last.raw =
                                String::from_utf8_lossy(&self.bytes[mark.open_start..mark.end])
                                    .into_owned();
                        }
                        return Some((kind.to_string(), token_id, attrs));
                    }
                }
            }
        }
        None
    }

    fn paragraph_mark(&mut self, children: &[usize]) {
        for &child in children {
            let node = &self.nodes[child];
            if node.name != "pPr" {
                continue;
            }
            for &inner in &node.children.clone() {
                let inner_node = &self.nodes[inner];
                if inner_node.name != "rPr" {
                    continue;
                }
                for &deepest in &inner_node.children.clone() {
                    let mark = &self.nodes[deepest];
                    if mark.name == "ins" || mark.name == "del" {
                        let (_, attrs) = self.tag(mark);
                        let mark = mark.clone();
                        self.add("paragraph-mark", attrs, &mark);
                        // Same rule as `paragraph_mark_token`: the mark's raw
                        // is the byte slice, not an ElementTree re-serialisation.
                        if let Some(last) = self.tokens.last_mut() {
                            last.raw =
                                String::from_utf8_lossy(&self.bytes[mark.open_start..mark.end])
                                    .into_owned();
                        }
                    }
                }
            }
        }
    }

    /// `_parse_container` as a TREE. Same allocation order as [`Walker::container`],
    /// so the ids agree; the difference is that a range or revision keeps its
    /// children instead of flattening them.
    fn container_nodes(&mut self, children: &[usize]) -> Vec<TypedNode> {
        let mut out: Vec<TypedNode> = Vec::new();
        for &child in children {
            let node = self.nodes[child].clone();
            let name = node.name.clone();
            if name == "pPr" {
                continue;
            }
            if name == "proofErr" {
                let mut attrs = BTreeMap::new();
                attrs.insert("tag".to_string(), "w:proofErr".to_string());
                let token_id = self.add("opaque", attrs.clone(), &node);
                out.push(TypedNode::Opaque {
                    token_id,
                    kind: "opaque".to_string(),
                    attrs,
                });
                continue;
            }
            if name == "r" {
                out.extend(self.run_nodes(&node));
                continue;
            }
            if name == "hyperlink" {
                let (_, attrs) = self.tag(&node);
                let token_id = self.add("hyperlink", attrs.clone(), &node);
                let inner = self.container_nodes(&node.children.clone());
                out.push(TypedNode::Range {
                    token_id,
                    kind: "hyperlink".to_string(),
                    attrs,
                    children: inner,
                });
                continue;
            }
            if let Some(kind) = revision_kind(&name) {
                let (_, attrs) = self.tag(&node);
                let token_id = self.add("revision", attrs.clone(), &node);
                let inner = self.container_nodes(&node.children.clone());
                out.push(TypedNode::Revision {
                    token_id,
                    kind: kind.to_string(),
                    attrs,
                    children: inner,
                });
                continue;
            }
            if let Some(kind) = anchor_kind(&name) {
                let (_, attrs) = self.tag(&node);
                let token_id = self.add(kind, attrs.clone(), &node);
                out.push(TypedNode::Anchor {
                    token_id,
                    kind: kind.to_string(),
                    attrs,
                });
                continue;
            }
            let (tag_name, _) = self.tag(&node);
            let mut attrs = BTreeMap::new();
            attrs.insert("tag".to_string(), tag_name);
            let token_id = self.add("opaque", attrs.clone(), &node);
            out.push(TypedNode::Opaque {
                token_id,
                kind: "opaque".to_string(),
                attrs,
            });
        }
        merge_adjacent_text(out)
    }

    /// `_parse_run` as a TREE.
    fn run_nodes(&mut self, run: &ElementNode) -> Vec<TypedNode> {
        let mut out: Vec<TypedNode> = Vec::new();
        let mut rpr_change: Option<ElementNode> = None;
        let mut style_id = crate::document_projection::default_style_id();
        for &child in &run.children {
            let node = self.nodes[child].clone();
            let name = node.name.clone();
            if name == "rPr" {
                style_id = self.style_of(&node);
                for &inner in &node.children {
                    if self.nodes[inner].name == "rPrChange" {
                        rpr_change = Some(self.nodes[inner].clone());
                    }
                }
                continue;
            }
            if name == "t" || name == "delText" {
                let text = self.text_of(&node);
                out.push(TypedNode::Text {
                    style_id: style_id.clone(),
                    text,
                    vertical: None,
                });
                continue;
            }
            if KNOWN_INLINE.contains(&name.as_str()) {
                let (_, attrs) = self.tag(&node);
                let token_id = self.add_styled(&name, attrs.clone(), &node, &style_id);
                out.push(TypedNode::Inline {
                    token_id,
                    kind: name.clone(),
                    style_id: style_id.clone(),
                    attrs,
                });
                continue;
            }
            // An unknown child makes the whole RUN opaque and stops the walk.
            let (tag_name, _) = self.tag(&node);
            let mut attrs = BTreeMap::new();
            attrs.insert("tag".to_string(), tag_name);
            let token_id = self.add_styled("unsupported-run", attrs.clone(), run, &style_id);
            return vec![TypedNode::Opaque {
                token_id,
                kind: "unsupported-run".to_string(),
                attrs,
            }];
        }
        if let Some(change) = rpr_change {
            let (_, attrs) = self.tag(&change);
            let token_id = self.add("rpr-change", attrs.clone(), &change);
            out.push(TypedNode::Inline {
                token_id,
                kind: "rpr-change".to_string(),
                style_id: style_id.clone(),
                attrs,
            });
        }
        if out.is_empty() {
            let (_, attrs) = self.tag(run);
            let token_id = self.add_styled("empty-run", attrs.clone(), run, &style_id);
            return vec![TypedNode::Inline {
                token_id,
                kind: "empty-run".to_string(),
                style_id,
                attrs,
            }];
        }
        merge_adjacent_text(out)
    }

    /// The style of a run's `w:rPr`, by the same canonical id scheme.
    fn style_of(&self, rpr: &ElementNode) -> String {
        let fragment = String::from_utf8_lossy(&self.bytes[rpr.open_start..rpr.end]).into_owned();
        crate::prose::style_id_for_rpr(&fragment)
            .unwrap_or_else(|_| crate::document_projection::default_style_id())
    }

    /// A `w:t`'s decoded text.
    fn text_of(&self, node: &ElementNode) -> String {
        let start = node.open_end;
        let end = if node.children.is_empty() {
            node.close_start
        } else {
            node.open_end
        };
        crate::prose::decode_text_segments(&self.bytes[start..end])
            .map(|(text, _)| text)
            .unwrap_or_default()
    }

    /// `_parse_run`: the run's own children, then `rPrChange`, then `empty-run`.
    fn run(&mut self, run: &ElementNode) {
        let mut rpr_change: Option<ElementNode> = None;
        let mut saw_content = false;
        let mut style = crate::document_projection::default_style_id();
        for &child in &run.children {
            let node = self.nodes[child].clone();
            let name = node.name.clone();
            if name == "rPr" {
                style = self.style_of(&node);
                for &inner in &node.children {
                    if self.nodes[inner].name == "rPrChange" {
                        rpr_change = Some(self.nodes[inner].clone());
                    }
                }
                continue;
            }
            if name == "t" || name == "delText" {
                saw_content = true;
                continue;
            }
            if KNOWN_INLINE.contains(&name.as_str()) {
                let (_, attrs) = self.tag(&node);
                // Styled, like `run_nodes`: an inline's token carries the run's
                // style, and format.json publishes it.
                self.add_styled(&name, attrs, &node, &style);
                saw_content = true;
                continue;
            }
            // An unknown child makes the whole RUN opaque, and the walk of
            // this run stops here -- the reference returns immediately.
            let (tag_name, _) = self.tag(&node);
            let mut attrs = BTreeMap::new();
            attrs.insert("tag".to_string(), tag_name);
            // `unsupported-run` carries the style too: it IS a run.
            self.add_styled("unsupported-run", attrs, run, &style);
            return;
        }
        if let Some(change) = rpr_change {
            let (_, attrs) = self.tag(&change);
            self.add("rpr-change", attrs, &change);
        }
        if !saw_content {
            let (_, attrs) = self.tag(run);
            self.add("empty-run", attrs, run);
        }
    }
}

/// The token table for one package, in the reference's allocation order.
pub fn token_table(package: &[u8]) -> Result<Vec<TokenRecord>, CoreError> {
    let part_xml = crate::document_projection::part_xml_members(package)?;
    let mut tokens: Vec<TokenRecord> = Vec::new();
    let mut unknown_prefixes: BTreeMap<String, String> = BTreeMap::new();

    // Rule 1: the BODY first, whatever order typed.md lists paragraphs in.
    let mut order: Vec<String> = vec!["document".to_string()];
    // The part map CONTAINS "document", so it must be excluded here or the body
    // is walked twice and every token after it is renumbered.
    let mut others: Vec<String> = part_xml
        .keys()
        .filter(|key| key.as_str() != "document")
        .cloned()
        .collect();
    others.sort_by_key(|key| {
        let rank = if key.starts_with("header") {
            0
        } else if key.starts_with("footer") {
            1
        } else if key == "footnotes" || key == "endnotes" {
            2
        } else {
            3
        };
        (rank, key.clone())
    });
    order.extend(others);

    for part_key in order {
        let Some(xml) = part_xml.get(&part_key) else {
            continue;
        };
        let nodes = build_tree(&scan_tags(xml))?;
        let located = if part_key == "document" {
            scan_document(xml)?
        } else {
            scan_part(xml, &part_key)?
        };
        // A fresh walker per part: the node arena is rebuilt each iteration,
        // and the counter lives in `tokens`, which outlives all of them.
        let namespaces = part_namespaces(xml, &nodes);
        let mut walker = Walker {
            bytes: xml,
            nodes: &nodes,
            namespaces,
            unknown_prefixes: &mut unknown_prefixes,
            part: part_key.clone(),
            paragraph_id: String::new(),
            tokens: &mut tokens,
        };
        for (paragraph_id, start, end) in located {
            let Some(root) = nodes.iter().find(|node| {
                node.open_start == start
                    && (node.end == end || (node.self_closing && node.open_start == end))
            }) else {
                continue;
            };
            walker.paragraph_id = paragraph_id.clone();
            walker.container(&root.children.clone());
            // Rule 2: the paragraph MARK is allocated AFTER the container
            // (`_parse_paragraph`), so its token lands last within the
            // paragraph however early the mark appears in the XML.
            walker.paragraph_mark(&root.children.clone());
        }
    }
    Ok(tokens)
}

/// Every paragraph's typed tree, in the SAME part and paragraph order the token
/// table uses -- the body first (rule 1), then the header/footer/note parts.
///
/// The trees and the flat table are two views of one walk, so their ids agree
/// by construction rather than by a second implementation.
pub fn paragraph_nodes(package: &[u8]) -> Result<Vec<ParagraphNodes>, CoreError> {
    let part_xml = crate::document_projection::part_xml_members(package)?;
    let mut out: Vec<ParagraphNodes> = Vec::new();
    let mut counter: Vec<TokenRecord> = Vec::new();
    let mut unknown_prefixes: BTreeMap<String, String> = BTreeMap::new();

    let mut order: Vec<String> = vec!["document".to_string()];
    let mut others: Vec<String> = part_xml
        .keys()
        .filter(|key| key.as_str() != "document")
        .cloned()
        .collect();
    others.sort_by_key(|key| {
        let rank = if key.starts_with("header") {
            0
        } else if key.starts_with("footer") {
            1
        } else if key == "footnotes" || key == "endnotes" {
            2
        } else {
            3
        };
        (rank, key.clone())
    });
    order.extend(others);

    for part_key in order {
        let Some(xml) = part_xml.get(&part_key) else {
            continue;
        };
        let nodes = build_tree(&scan_tags(xml))?;
        let located = if part_key == "document" {
            scan_document(xml)?
        } else {
            scan_part(xml, &part_key)?
        };
        let namespaces = part_namespaces(xml, &nodes);
        let mut walker = Walker {
            bytes: xml,
            nodes: &nodes,
            namespaces,
            unknown_prefixes: &mut unknown_prefixes,
            part: part_key.clone(),
            paragraph_id: String::new(),
            tokens: &mut counter,
        };
        for (paragraph_id, start, end) in located {
            let Some(root) = nodes.iter().find(|node| {
                node.open_start == start
                    && (node.end == end || (node.self_closing && node.open_start == end))
            }) else {
                continue;
            };
            walker.paragraph_id = paragraph_id.clone();
            let children = walker.container_nodes(&root.children.clone());
            // Rule 2: the mark is allocated after the container.
            let mark = walker.paragraph_mark_token(&root.children.clone());
            let section_bearing = root.children.iter().any(|child_idx| {
                let child = &nodes[*child_idx];
                child.name == "pPr"
                    && child
                        .children
                        .iter()
                        .any(|pchild_idx| nodes[*pchild_idx].name == "sectPr")
            });
            out.push(ParagraphNodes {
                part: part_key.clone(),
                paragraph_id,
                start,
                part_entry_id: part_entry_id(&part_key, start, xml),
                nodes: children,
                mark,
                section_bearing,
            });
        }
    }
    Ok(out)
}

/// `part_key` as `typed.md` spells it: the body is the empty string.
pub fn typed_part_key(part_key: &str) -> String {
    if part_key == "document" {
        String::new()
    } else {
        part_key.to_string()
    }
}

/// The `PART_KEYS_PATTERN` set, reused for the token walk's part order.
pub fn is_part_member(name: &str) -> bool {
    let _ = part_path(name);
    name.starts_with("word/")
}
