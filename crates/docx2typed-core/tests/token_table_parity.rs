//! The token table must match the reference's, in order and in kind (#52).
//!
//! The ids come from ONE counter over the whole parse, and `typed.md` renders
//! only some of the tokens: `revisions.docx` uses N0, N1, N3..N7 and skips N2.
//! A table that matched only what is printed would still be wrong, so this
//! compares the FULL sequence.
//!
//! The expectation is checked in, captured from the reference, so no Python is
//! needed at test time.

use std::fs;
use std::path::PathBuf;

use docx2typed_core::token_table::token_table;
use serde_json::Value;

fn corpus() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../corpus/release")
}

fn expectation() -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/token_table_expectation.json");
    serde_json::from_str(&fs::read_to_string(path).expect("expectation fixture")).expect("json")
}

#[test]
fn rust_reproduces_the_reference_token_sequence() {
    let fixture = expectation();
    let documents = fixture["documents"].as_object().expect("documents");
    assert!(!documents.is_empty());

    for (name, expected) in documents {
        let bytes = fs::read(corpus().join(format!("{name}.docx")))
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let tokens = token_table(&bytes).unwrap_or_else(|error| panic!("{name}: {error}"));
        let expected = expected.as_array().expect("rows");
        assert_eq!(
            tokens.len(),
            expected.len(),
            "{name}: token count differs; got {:?}",
            tokens.iter().map(|t| (&t.id, &t.kind)).collect::<Vec<_>>()
        );
        for (index, row) in expected.iter().enumerate() {
            assert_eq!(
                tokens[index].id,
                row["id"].as_str().unwrap(),
                "{name} at {index}"
            );
            assert_eq!(
                tokens[index].kind,
                row["kind"].as_str().unwrap(),
                "{name} at {index} ({})",
                tokens[index].id
            );
        }
    }
}

/// The ids are a contiguous run from N0 with no gaps and no repeats. Gaps are
/// exactly what makes this table worth having: N2 in `revisions.docx` belongs to
/// a token `typed.md` never prints, and a table derived from the markup could
/// not know it exists.
#[test]
fn the_sequence_is_contiguous_and_never_renumbered() {
    let bytes = fs::read(corpus().join("revisions.docx")).expect("fixture");
    let tokens = token_table(&bytes).expect("token table");
    assert!(
        tokens.len() >= 8,
        "revisions.docx allocates at least 8 tokens"
    );
    for (index, token) in tokens.iter().enumerate() {
        assert_eq!(
            token.id,
            format!("N{index}"),
            "ids are the counter, in order"
        );
    }
    // The mark token is allocated AFTER the container, however early the mark
    // sits in the XML.
    assert_eq!(tokens[2].kind, "paragraph-mark", "{:?}", tokens);
    assert!(
        tokens.iter().any(|token| token.kind == "revision"),
        "and the container's revisions come first"
    );
}

/// The tree and the flat table are two views of ONE walk, so their ids must
/// agree. If they ever diverge, the markup and `format.json` would disagree
/// about which token is which.
#[test]
fn the_tree_and_the_table_agree_on_every_id() {
    use docx2typed_core::token_table::paragraph_nodes;
    let fixture = expectation();
    let documents = fixture["documents"].as_object().expect("documents");
    for (name, expected) in documents {
        let bytes = fs::read(corpus().join(format!("{name}.docx"))).expect("fixture");
        let table = token_table(&bytes).expect("table");
        let paragraphs = paragraph_nodes(&bytes).expect("tree");

        // The tree's tokens in document order. A mark is allocated LAST within
        // its paragraph (rule 2), so it is appended per paragraph rather than
        // collected with the container's nodes.
        let mut from_tree: Vec<(String, String)> = Vec::new();
        for paragraph in &paragraphs {
            let mut here: Vec<(String, String)> = Vec::new();
            for node in &paragraph.nodes {
                node.collect_token_ids(&mut here);
            }
            // A revision NODE says insert/delete/...; its TOKEN says
            // "revision". The reference stores them the same way, so the node
            // kind is mapped back rather than pretending they are one field.
            from_tree.extend(here.into_iter().map(|(id, kind)| {
                let kind = match kind.as_str() {
                    "insert" | "delete" | "move_from" | "move_to" => "revision".to_string(),
                    other => other.to_string(),
                };
                (id, kind)
            }));
            if let Some((_, token_id, _)) = &paragraph.mark {
                from_tree.push((token_id.clone(), "paragraph-mark".to_string()));
            }
        }

        let from_table: Vec<(String, String)> = table
            .iter()
            .map(|token| (token.id.clone(), token.kind.clone()))
            .collect();
        assert_eq!(
            from_table, from_tree,
            "{name}: the tree and the table disagree"
        );
        assert_eq!(
            from_table.len(),
            expected.as_array().unwrap().len(),
            "{name}"
        );
    }
}
