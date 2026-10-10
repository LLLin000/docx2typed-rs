//! The style-id differential: Rust's `style_id_for_rpr` must agree with the
//! Python reference's, because the id is content-addressed over a canonical
//! form that both engines must produce identically.
//!
//! Emits `{rpr_xml: style_id}` for every distinct run property in the corpus
//! plus a set of synthetic multi-child cases (the corpus only has two, and the
//! `repr`-based child sort is exactly what synthetic cases must cover).
//!
//! Run with `--ignored` to refresh; `scripts/check_style_ids.py` compares.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use serde_json::json;

fn corpus() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corpus/release")
}

#[test]
#[ignore]
fn emit_style_id_fixture() {
    let mut fragments: BTreeSet<String> = BTreeSet::new();
    if let Ok(entries) = fs::read_dir(corpus()) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("docx") {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else { continue };
            let Ok(inventory) = docx2typed_core::prose::enumerate_package_bytes(&bytes) else {
                continue;
            };
            for leaf in &inventory.leaves {
                if !leaf.rpr.is_empty() {
                    fragments.insert(leaf.rpr.clone());
                }
            }
        }
    }
    // Synthetic cases: the child sort is the risky part of the canonical form
    // and the corpus exercises it twice, so cover it deliberately. These are
    // the shapes a patent document actually produces (font + size + bold).
    for extra in [
        r#"<w:rPr xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:sz w:val="24"/><w:b/></w:rPr>"#,
        r#"<w:rPr xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:b/><w:sz w:val="24"/></w:rPr>"#,
        r#"<w:rPr xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:vertAlign w:val="superscript"/><w:lang w:val="en-US"/></w:rPr>"#,
        r#"<w:rPr xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:lang w:val="en-US"/><w:vertAlign w:val="superscript"/></w:rPr>"#,
        r#"<w:rPr xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:b w:val="false"/></w:rPr>"#,
        r#"<w:rPr xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:b w:val="true"/></w:rPr>"#,
        r#"<w:rPr xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:rFonts w:ascii="Times New Roman" w:hAnsi="Times New Roman" w:rsidR="00A"/><w:i/><w:sz w:val="28"/></w:rPr>"#,
        r#"<w:rPr xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:rPrChange w:id="1"><w:rPr><w:b/></w:rPr></w:rPrChange><w:i/></w:rPr>"#,
        r#"<w:rPr xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"/>"#,
        "",
    ] {
        fragments.insert(extra.to_string());
    }

    let mut out = serde_json::Map::new();
    for fragment in &fragments {
        match docx2typed_core::prose::style_id_for_rpr(fragment) {
            Ok(id) => {
                let canonical = docx2typed_core::prose::canonical_rpr_json(fragment)
                    .unwrap_or_else(|e| format!("ERROR: {e}"));
                out.insert(fragment.clone(), json!({"id": id, "canonical": canonical}));
            }
            Err(error) => {
                out.insert(fragment.clone(), json!(format!("ERROR: {error}")));
            }
        }
    }
    let destination = PathBuf::from(
        std::env::var("STYLE_ID_FIXTURE").unwrap_or_else(|_| "C:/tmp/styleids.json".into()),
    );
    if let Some(parent) = destination.parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::write(&destination, serde_json::to_string_pretty(&out).unwrap()).unwrap();
    println!("{} fragments -> {}", out.len(), destination.display());
}
