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
