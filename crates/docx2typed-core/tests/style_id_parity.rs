//! Style identity must match the Python reference exactly.
//!
//! `style_id_for_rpr` is content-addressed over a canonical form both engines
//! must produce identically, and `styles.json` is a canonical asset — so a
//! disagreement does not merely rename a style, it moves the tree digest and
//! therefore the whole version chain.
//!
//! The expectation is checked in, captured from the reference
//! (`scripts.typed_core`), so this needs no Python at test time. It covers the
//! parts that are easy to get subtly wrong: the `repr`-based child sort (which
//! the corpus exercises twice, hence the synthetic multi-child cases), boolean
//! `w:val` folding, `rsid*` dropping, `xmlns` skipping, and `w:rPrChange`
//! removal.

use std::collections::BTreeSet;
use std::fs;

use docx2typed_core::prose::{canonical_rpr_json, style_id_for_rpr};
use serde_json::Value;

fn expectation() -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/style_id_expectation.json");
    serde_json::from_str(&fs::read_to_string(path).expect("expectation fixture")).expect("json")
}

#[test]
fn rust_reproduces_the_reference_style_identity() {
    let fixture = expectation();
    let cases = fixture["cases"].as_array().expect("cases");
    assert!(!cases.is_empty(), "the expectation must not be empty");

    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut checked = 0usize;
    for case in cases {
        let rpr = case["rpr"].as_str().expect("rpr");
        let expected_canonical = case["canonical"].as_str().expect("canonical");
        let expected_id = case["style_id"].as_str().expect("style_id");
        seen.insert(expected_id.to_string());

        let canonical = canonical_rpr_json(rpr)
            .unwrap_or_else(|error| panic!("canonical_rpr_json failed for {rpr}: {error}"));
        assert_eq!(
            canonical, expected_canonical,
            "canonical form differs from the reference for {rpr}"
        );
        let id = style_id_for_rpr(rpr).expect("style id");
        assert_eq!(
            id, expected_id,
            "style id differs from the reference for {rpr}"
        );
        checked += 1;
    }
    assert_eq!(checked, cases.len());
    // The cases are not all the same style, or the assertions above would be
    // vacuous.
    assert!(
        seen.len() >= 10,
        "the fixture must cover distinct styles, not one repeated: {seen:?}"
    );
}

/// The canonical form is order-insensitive on purpose: two rPrs that differ
/// only in child order are the same style. If this ever fails, the sort was
/// dropped and every multi-child style silently split in two.
#[test]
fn child_order_does_not_change_the_style() {
    let ns = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;
    let first = format!(r#"<w:rPr {ns}><w:b/><w:sz w:val="24"/></w:rPr>"#);
    let second = format!(r#"<w:rPr {ns}><w:sz w:val="24"/><w:b/></w:rPr>"#);
    assert_eq!(
        style_id_for_rpr(&first).unwrap(),
        style_id_for_rpr(&second).unwrap(),
        "child order must not affect style identity"
    );
    // And the reference's id for that style, captured in the fixture.
    assert_eq!(style_id_for_rpr(&first).unwrap(), "s_8625025d2d375d76");
}

/// A boolean property explicitly false is absent, and explicitly true is
/// implicit — so all three of these are the same style as a bare `w:b`.
#[test]
fn boolean_val_folding_matches_the_reference() {
    let ns = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;
    let bare = format!(r#"<w:rPr {ns}><w:b/></w:rPr>"#);
    let explicit_true = format!(r#"<w:rPr {ns}><w:b w:val="true"/></w:rPr>"#);
    assert_eq!(
        style_id_for_rpr(&bare).unwrap(),
        style_id_for_rpr(&explicit_true).unwrap()
    );

    let absent = format!(r#"<w:rPr {ns}/>"#);
    let explicit_false = format!(r#"<w:rPr {ns}><w:b w:val="false"/></w:rPr>"#);
    assert_eq!(
        style_id_for_rpr(&absent).unwrap(),
        style_id_for_rpr(&explicit_false).unwrap(),
        "an explicitly false boolean is the same as absent"
    );
    assert_ne!(
        style_id_for_rpr(&bare).unwrap(),
        style_id_for_rpr(&absent).unwrap(),
        "and it is NOT the same as present"
    );
}
