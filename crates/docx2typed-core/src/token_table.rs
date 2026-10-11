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

/// A paragraph's typed tree, plus the paragraph's own mark token.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParagraphNodes {
    pub part: String,
    pub paragraph_id: String,
    pub nodes: Vec<TypedNode>,
    /// `(kind, token_id, attrs)` when the paragraph mark carries a revision.
    pub mark: Option<(String, String, BTreeMap<String, String>)>,
}

/// `merge_adjacent_text`: same style AND same vertical alignment merges, so a
/// run the scanner split does not become two nodes.
fn merge_adjacent_text(nodes: Vec<TypedNode>) -> Vec<TypedNode> {
    let mut merged: Vec<TypedNode> = Vec::new();
    for node in nodes {
        if let TypedNode::Text { style_id, text } = &node {
            if let Some(TypedNode::Text {
                style_id: last_style,
                text: last_text,
            }) = merged.last_mut()
            {
                if last_style == style_id {
                    last_text.push_str(text);
                    continue;
                }
            }
        }
        merged.push(node);
    }
    merged
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
    part: String,
    paragraph_id: String,
    tokens: &'a mut Vec<TokenRecord>,
}

impl<'a> Walker<'a> {
    fn add(&mut self, kind: &str, attrs: BTreeMap<String, String>, node: &ElementNode) -> String {
        let id = format!("N{}", self.tokens.len());
        self.tokens.push(TokenRecord {
            id,
            kind: kind.to_string(),
            attrs,
            part: self.part.clone(),
            paragraph_id: self.paragraph_id.clone(),
            start: node.open_start,
            end: node.end,
        });
        self.tokens
            .last()
            .map(|token| token.id.clone())
            .unwrap_or_default()
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
        let name = String::from_utf8_lossy(&raw[1..end]).into_owned();
        let mut attrs = BTreeMap::new();
        for (key, value) in
            crate::prose::canonical_attribute_pairs(&self.bytes[node.open_start..node.end])
        {
            attrs.insert(key, value);
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
                let (_, attrs) = self.tag(&node);
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
                let (_, attrs) = self.tag(&node);
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
                });
                continue;
            }
            if KNOWN_INLINE.contains(&name.as_str()) {
                let (_, attrs) = self.tag(&node);
                let token_id = self.add(&name, attrs.clone(), &node);
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
            let token_id = self.add("unsupported-run", attrs.clone(), run);
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
            let token_id = self.add("empty-run", attrs.clone(), run);
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
        for &child in &run.children {
            let node = self.nodes[child].clone();
            let name = node.name.clone();
            if name == "rPr" {
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
                self.add(&name, attrs, &node);
                saw_content = true;
                continue;
            }
            // An unknown child makes the whole RUN opaque, and the walk of
            // this run stops here -- the reference returns immediately.
            let (tag_name, _) = self.tag(&node);
            let mut attrs = BTreeMap::new();
            attrs.insert("tag".to_string(), tag_name);
            self.add("unsupported-run", attrs, run);
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
        let mut walker = Walker {
            bytes: xml,
            nodes: &nodes,
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
        let mut walker = Walker {
            bytes: xml,
            nodes: &nodes,
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
            out.push(ParagraphNodes {
                part: part_key.clone(),
                paragraph_id,
                nodes: children,
                mark,
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
