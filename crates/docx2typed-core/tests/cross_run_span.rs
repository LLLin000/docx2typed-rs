//! A text edit that starts in one of a paragraph's text runs and ends in another.
//!
//! Word splits a paragraph's visible text into one `w:t` per formatting run, so
//! text a human typed in Word routinely crosses runs — `corpus/release/
//! complex.docx` carries a 20-run paragraph for exactly that reason. The engine
//! used to refuse every such edit with `cross-leaf-edit`, which left the inside
//! of any Word-touched paragraph uneditable: one real patent paragraph carried
//! **65** runs, and a session that needed to edit it gave up on the engine and
//! rewrote the OOXML by hand.
//!
//! These tests pin the behaviour that replaced the refusal. The record shape for
//! an edit that stays inside one run is deliberately unchanged — see
//! `document_patch_applies_replace_insert_delete_in_one_batch` in the review
//! crate for that side.

use std::fs;
use std::path::PathBuf;

use docx2typed_core::prose::{
    apply_edits, enumerate_package, enumerate_package_bytes, match_leaf_span, IslandEdit,
    LeafSpanMatch,
};

/// The phrase under test. It starts in run 1 (`bold`) and ends in run 3
/// (` underline`), so no single run carries it.
const OLD: &str = "bold italic underline";
const NEW: &str = "BOLD ITALIC UNDERLINE";

struct Fixture {
    path: PathBuf,
    part: String,
    paragraph_id: String,
    span: LeafSpanMatch,
}

/// A per-test copy of the run-split paragraph's template.
fn fixture(tag: &str) -> Fixture {
    let source =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../corpus/release/complex.docx");
    let dir = std::env::temp_dir().join(format!("cross-run-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("scratch dir");
    let path = dir.join("_template.docx");
    fs::copy(&source, &path).expect("copy fixture");

    let inventory = enumerate_package(&path).expect("enumerate");
    let entry = inventory
        .paragraphs
        .iter()
        .find(|paragraph| paragraph.visible_text.starts_with("EDIT-ME:"))
        .expect("the run-split paragraph");
    assert!(
        entry.leaf_count >= 4,
        "the fixture no longer carries a run-split paragraph (leaf_count {})",
        entry.leaf_count
    );
    let span = match_leaf_span(&inventory, &entry.part_key, &entry.paragraph_id, 0, OLD)
        .expect("the search itself does not fail")
        .expect("the phrase is present");
    assert!(
        span.crosses_runs(),
        "the fixture must force a match across runs, else this file proves nothing"
    );
    Fixture {
        path,
        part: entry.part_key.clone(),
        paragraph_id: entry.paragraph_id.clone(),
        span,
    }
}

/// The recorded anchor is the run the match STARTS in; the applier re-derives
/// the rest of the range from `(leaf_index, old)`.
fn edit(fixture: &Fixture, tracked: bool, new: &str) -> IslandEdit {
    IslandEdit {
        part: fixture.part.clone(),
        paragraph_id: fixture.paragraph_id.clone(),
        leaf_index: fixture.span.first_leaf,
        old: OLD.to_string(),
        new: new.to_string(),
        tracked,
        author: if tracked {
            "tester".into()
        } else {
            String::new()
        },
        date: if tracked {
            "2026-10-13T00:00:00Z".into()
        } else {
            String::new()
        },
    }
}

/// The paragraph's text a reader sees: the editable leaves only. `visible_text`
/// on the inventory is the text as STORED and includes deletion revisions, so a
/// tracked delete would still read there.
fn live(package: &[u8], part: &str, paragraph_id: &str) -> String {
    enumerate_package_bytes(package)
        .expect("enumerate the edited bytes")
        .leaves
        .iter()
        .filter(|leaf| leaf.part_key == part && leaf.paragraph_id == paragraph_id && leaf.editable)
        .map(|leaf| leaf.text.clone())
        .collect()
}

fn document_xml(package: &[u8]) -> String {
    use std::io::Read;
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(package.to_vec())).expect("the output is a zip");
    let mut file = archive
        .by_name("word/document.xml")
        .expect("the document part");
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).expect("read the part");
    String::from_utf8_lossy(&bytes).into_owned()
}

#[test]
fn the_matcher_names_every_run_a_crossing_match_covers() {
    let fixture = fixture("match");
    assert_eq!(
        fixture.span.matched(),
        OLD,
        "the covered runs spell the phrase"
    );
    assert_eq!(
        fixture.span.covers.len(),
        fixture.span.last_leaf - fixture.span.first_leaf + 1,
        "one cover per run in the range"
    );
    assert!(
        fixture
            .span
            .covers
            .iter()
            .all(|cover| !cover.covered.is_empty()),
        "the phrase leaves no empty run behind"
    );
}

#[test]
fn a_direct_replace_that_crosses_runs_lands_and_leaves_its_neighbours_alone() {
    let fixture = fixture("direct");
    let out = apply_edits(&fixture.path, &[edit(&fixture, false, NEW)]).expect("applies");

    let after = live(&out, &fixture.part, &fixture.paragraph_id);
    assert!(
        after.contains(NEW),
        "the replacement is in the text: {after}"
    );
    assert!(!after.contains(OLD), "the old phrase is gone: {after}");
    // The runs before and after the match keep their text: a cross-run edit must
    // not absorb a neighbour the way one wide byte span over the whole run range
    // would.
    assert!(after.contains("EDIT-ME: "), "leading run intact: {after}");
    assert!(after.contains(" strike"), "following run intact: {after}");
    assert!(
        after.contains(" external hyperlink"),
        "the last run is untouched: {after}"
    );
}

#[test]
fn a_tracked_replace_that_crosses_runs_deletes_each_run_and_inserts_once() {
    let fixture = fixture("tracked");
    let out = apply_edits(&fixture.path, &[edit(&fixture, true, NEW)]).expect("applies");

    // What a reader sees carries the replacement and no longer the old phrase.
    let live_text = live(&out, &fixture.part, &fixture.paragraph_id);
    assert!(
        live_text.contains(NEW),
        "the insertion carries the new text: {live_text}"
    );
    assert!(
        !live_text.contains(OLD),
        "the deletion takes the old text off the live surface: {live_text}"
    );
    // …and the paragraph still reads as one sentence, neighbours included.
    assert!(
        live_text.contains("EDIT-ME: "),
        "leading run intact: {live_text}"
    );
    assert!(
        live_text.contains(" strike"),
        "following run intact: {live_text}"
    );

    let xml = document_xml(&out);
    assert_eq!(
        authored(&xml, "<w:ins "),
        1,
        "one insertion carries the replacement, not one per run"
    );
    assert_eq!(
        authored(&xml, "<w:del "),
        fixture.span.covers.len(),
        "every covered run is marked deleted exactly once"
    );
    assert!(
        xml.contains("w:author=\"tester\""),
        "the author is recorded"
    );
    assert!(
        xml.contains("w:date=\"2026-10-13T00:00:00Z\""),
        "the date is recorded"
    );
    // The old text survives in the markup as a deletion, so Word can accept or
    // reject the change rather than losing what was there.
    assert!(
        xml.contains("<w:delText>bold</w:delText>")
            || xml.contains("<w:delText xml:space=\"preserve\"> italic</w:delText>"),
        "the covered text is preserved as delText"
    );
}

/// How many `tag` wrappers this edit authored. The fixture carries revisions of
/// its own, so counting every `<w:del ` in the part would count those too.
fn authored(xml: &str, tag: &str) -> usize {
    xml.match_indices(tag)
        .filter(|(index, _)| {
            let tail = &xml[*index..(*index + 200).min(xml.len())];
            tail.contains("w:author=\"tester\"")
        })
        .count()
}
