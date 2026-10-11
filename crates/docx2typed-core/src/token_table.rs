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
