//! The token-table differential (#52): Rust's `N0, N1, ...` sequence must match
//! the reference's, in order and in kind.
//!
//! The ids are allocated by ONE counter over the whole parse, and `typed.md`
//! renders only some of the tokens -- `revisions.docx` uses N0, N1, N3..N7 and
//! skips N2. So this compares the FULL sequence, not the rendered subset: a
//! table that matches only what is printed would still be wrong.
//!
//! Run with `--ignored` to refresh; the comparison lives in the PR's notes.

use std::fs;
use std::path::PathBuf;

use docx2typed_core::token_table::token_table;
use serde_json::json;

fn corpus() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corpus/release")
}

#[test]
#[ignore]
fn emit_token_table_fixture() {
    let mut out = serde_json::Map::new();
    for name in [
        "revisions",
        "anchors",
        "parts",
        "boxes",
        "comments",
        "complex",
    ] {
        let path = corpus().join(format!("{name}.docx"));
        let Ok(bytes) = fs::read(&path) else { continue };
        match token_table(&bytes) {
            Ok(tokens) => {
                let rows: Vec<serde_json::Value> = tokens
                    .iter()
                    .map(|token| {
                        json!({
                            "id": token.id,
                            "kind": token.kind,
                            "part": token.part,
                            "paragraph_id": token.paragraph_id,
                        })
                    })
                    .collect();
                out.insert(name.to_string(), json!(rows));
            }
            Err(error) => {
                out.insert(name.to_string(), json!(format!("ERROR: {error}")));
            }
        }
    }
    let destination = PathBuf::from(
        std::env::var("TOKEN_FIXTURE").unwrap_or_else(|_| "C:/tmp/tokens.json".into()),
    );
    fs::write(&destination, serde_json::to_string_pretty(&out).unwrap()).unwrap();
    println!("wrote {}", destination.display());
}
