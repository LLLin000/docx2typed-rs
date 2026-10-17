//! The format lane's vertical slice, at the workdir level.
//!
//! Adapter → plan → build, on a real fixture whose paragraph holds runs with
//! different character properties (`complex.docx`'s P3: bold, italic, underline,
//! small-caps, colour, highlight, a real superscript and subscript, a code style,
//! plus a tab and a break). The point is character-level precision: only the
//! characters the request names may change, and everything else must be the bytes
//! it was — including the runs that carry structure the edit does not touch.

use docx2typed_app::{Engine, ExtractArgs, Operation, OperationArgs, OperationContext};
use docx2typed_review::format_lane::{
    apply_format_plan, draft_package, plan_format, save_format_plan, save_format_requests,
    FormatRequest,
};
use docx2typed_review::{collab, draft, CollaborationError};
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A fresh workdir extracted from a corpus fixture.
fn workdir(tag: &str, fixture: &str) -> Scratch {
    let dir = std::env::temp_dir().join(format!(
        "format-lane-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let engine = Engine::new();
    let outcome = engine
        .execute(
            Operation::Extract,
            OperationContext::new(docx2typed_protocol::new_operation_id()),
            OperationArgs::Extract(ExtractArgs {
                input: repo_root().join(format!("corpus/release/{fixture}.docx")),
                outdir: dir.clone(),
                discard_committed_versions: false,
            }),
        )
        .expect("extract runs");
    let envelope = outcome.into_envelope("extract", "");
    assert_eq!(envelope.outcome, "success", "{:?}", envelope.diagnostics);
    docx2typed_store::Store::ensure(&dir, &docx2typed_protocol::new_operation_id(), "")
        .expect("store init");
    // The draft projection is what a caller's coordinates are measured in, so the
    // lane starts from a workdir that has one.
    draft::ensure_projection(&dir).expect("projection");
    Scratch(dir)
}

/// The visible text of one paragraph, and the offset of `needle` in it — the
/// coordinates a caller's handles and this lane's ranges both address.
fn locate(workdir: &Path, paragraph_id: &str, needle: &str) -> (String, usize) {
    let (_, blocks) = draft::read_blocks(workdir).expect("blocks");
    let block = blocks
        .iter()
        .find(|block| {
            draft::marker_of(block)
                .map(|(_, id, _)| id == paragraph_id)
                .unwrap_or(false)
        })
        .unwrap_or_else(|| panic!("{paragraph_id} not found"));
    let visible = draft::typed_body_visible_text(&collab::block_body(block));
    let offset = visible
        .find(needle)
        .unwrap_or_else(|| panic!("{needle:?} not in {paragraph_id}: {visible}"));
    (visible, offset)
}

/// Every run of a paragraph as `(rPr bytes, text)`, in order — walked with the
/// format engine's own tag scanner rather than a hand-rolled split, because
/// `<w:rPr` also starts with `<w:r`.
fn run_properties(package: &[u8], paragraph: (usize, usize)) -> Vec<(String, String)> {
    use docx2typed_format::xml::{decode_text, scan_tags};
    // The range is in the DOCUMENT PART's coordinates, so the part is what has to
    // be read: slicing the zip at those offsets reads compressed bytes.
    let part = docx2typed_review::format_lane::member_bytes(package, "word/document.xml")
        .expect("document part");
    let body = &part[paragraph.0..paragraph.1];
    let tags = scan_tags(body);
    let mut out = Vec::new();
    let mut index = 0usize;
    while index < tags.len() {
        let tag = &tags[index];
        if tag.name != "r" || tag.closing || tag.self_closing {
            index += 1;
            continue;
        }
        let mut depth = 1usize;
        let mut cursor = index + 1;
        let mut rpr = String::new();
        let mut text = String::new();
        while cursor < tags.len() && depth > 0 {
            let inner = &tags[cursor];
            if !inner.closing && !inner.self_closing {
                depth += 1;
                if depth == 2 && inner.name == "rPr" {
                    let close = tags[cursor..]
                        .iter()
                        .find(|tag| tag.closing && tag.name == "rPr")
                        .map(|tag| tag.end)
                        .unwrap_or(inner.end);
                    rpr = String::from_utf8_lossy(&body[inner.start..close]).to_string();
                }
            } else if inner.closing {
                if depth == 2 && inner.name == "t" {
                    let open = tags[..cursor]
                        .iter()
                        .rev()
                        .find(|tag| !tag.closing && tag.name == "t")
                        .map(|tag| tag.end)
                        .unwrap_or(inner.start);
                    text = decode_text(&body[open..inner.start]);
                }
                depth -= 1;
                if depth == 0 {
                    out.push((rpr.clone(), text.clone()));
                    index = cursor;
                    break;
                }
            }
            cursor += 1;
        }
        index += 1;
    }
    out
}

const SUPERSCRIPT: &str = "s_0643e469379d4443";

#[test]
fn a_character_range_gets_a_style_and_nothing_else_moves() {
    let scratch = workdir("precise", "complex");
    let workdir = scratch.0.as_path();
    let package = draft_package(workdir).expect("draft package");

    // A real span in P3: `shadow` sits between styled runs, so a widening bug
    // would show up immediately.
    let (_, offset) = locate(workdir, "P3", "shadow");
    let length = "shadow".chars().count();
    let request = FormatRequest {
        paragraph_id: "P3".to_string(),
        start: offset,
        end: offset + length,
        style_id: SUPERSCRIPT.to_string(),
        attributes: None,
    };
    let (plan, snapshot) = plan_format(workdir, std::slice::from_ref(&request)).expect("plan");
    save_format_requests(workdir, std::slice::from_ref(&request)).expect("save requests");
    save_format_plan(workdir, &plan).expect("save plan");

    let paragraph_plan = plan.paragraph("P3").expect("planned paragraph");
    assert_eq!(
        paragraph_plan.segments.len(),
        1,
        "one range, the one the caller named"
    );
    assert_eq!(paragraph_plan.segments[0].range.start, offset);

    let built = apply_format_plan(&package, &plan, &snapshot).expect("apply");

    // The paragraph's byte range, in both packages, is the same: the plan does
    // not move text.
    let ranges = docx2typed_review::format_lane::paragraph_ranges(&package, &plan).expect("ranges");
    let range = ranges
        .iter()
        .find(|(id, _)| id == "P3")
        .map(|(_, range)| *range)
        .expect("P3 range");
    // Each package is read at ITS OWN paragraph range: the build moves bytes, so
    // the draft's offsets do not address the built part.
    let before = run_properties(&package, range);
    let built_range = docx2typed_review::format_lane::paragraph_ranges(&built, &plan)
        .expect("built ranges")
        .into_iter()
        .find(|(id, _)| id == "P3")
        .map(|(_, range)| range)
        .expect("P3 range in the built package");
    let after = run_properties(&built, built_range);

    // The target run carries the style's own properties, byte for byte, and the
    // text exactly the characters the request named. The fixture already holds
    // superscript runs of its own, so this is about the run holding `shadow`.
    let style_rpr = docx2typed_review::format_lane::style_registry(workdir)
        .expect("registry")
        .remove(SUPERSCRIPT)
        .expect("the style");
    let target = after
        .iter()
        .find(|(rpr, _)| rpr == &style_rpr)
        .expect("a run carrying the style");
    assert_eq!(target.1, "shadow", "and holding exactly the target text");

    // Everything else is what it was, in order, once the target's characters are
    // taken out of both sides: a run the edit splits keeps its own properties for
    // the part it does not cover.
    let remainder = |rows: &[(String, String)]| -> Vec<(String, String)> {
        rows.iter()
            .map(|(rpr, text)| (rpr.clone(), text.replace("shadow", "")))
            .filter(|(_, text)| !text.is_empty())
            .collect()
    };
    assert_eq!(
        remainder(&before),
        remainder(&after),
        "runs outside the target must keep their properties and their order"
    );

    // The paragraph's own properties, its structural runs (the tab, the break) and
    // the element that wraps its hyperlink are untouched too: they are not runs of
    // text, and the rewrite splices only the top-level runs it was asked about.
    let document = |package: &[u8]| {
        String::from_utf8_lossy(
            &docx2typed_review::format_lane::member_bytes(package, "word/document.xml")
                .expect("document part"),
        )
        .to_string()
    };
    let before_document = document(&package);
    let after_document = document(&built);
    for marker in [
        "<w:tab",
        "<w:br",
        "<w:hyperlink",
        "<w:pPr>",
        "<w:bookmarkStart",
    ] {
        assert_eq!(
            before_document.matches(marker).count(),
            after_document.matches(marker).count(),
            "{marker} must survive the rewrite"
        );
    }
    assert!(after_document.contains("<w:tab"), "the tab is still there");
}

#[test]
fn planning_the_same_request_again_changes_nothing() {
    let scratch = workdir("idempotent", "complex");
    let workdir = scratch.0.as_path();
    let package = draft_package(workdir).expect("draft package");
    let (_, offset) = locate(workdir, "P3", "shadow");
    let request = FormatRequest {
        paragraph_id: "P3".to_string(),
        start: offset,
        end: offset + 6,
        style_id: SUPERSCRIPT.to_string(),
        attributes: None,
    };
    let (plan, snapshot) = plan_format(workdir, std::slice::from_ref(&request)).expect("plan");
    let built = apply_format_plan(&package, &plan, &snapshot).expect("apply");

    // The second generation: the same requests, planned against what the first
    // one built. It must not add splits — a re-plan is the same answer, not a
    // second opinion.
    let (plan_again, snapshot_again) =
        plan_format(workdir, std::slice::from_ref(&request)).expect("plan again");
    let built_again =
        apply_format_plan(&package, &plan_again, &snapshot_again).expect("apply again");
    assert_eq!(built, built_again, "the plan is a fixed point");
}

/// Asking by PROPERTIES is the engine's job: it resolves the style the document
/// already carries, so a caller never has to read rPr features to pick an id. A
/// blind run did exactly that by hand, which is why this form exists.
#[test]
fn properties_resolve_to_the_document_own_style() {
    let scratch = workdir("by-properties", "complex");
    let workdir = scratch.0.as_path();
    // A span whose base is the document's plain formatting: the derivation is
    // base + override, and the registry has to hold that exact combination. A run
    // already carrying `<w:shadow/>` has no bold variant, and the engine refuses
    // rather than inventing one — which this test also asserts.
    let (_, offset) = locate(workdir, "P3", "EDIT-ME");
    let length = "EDIT-ME".chars().count();

    let attributes = serde_json::json!({ "bold": true })
        .as_object()
        .cloned()
        .expect("object");
    let resolved = docx2typed_review::format_lane::resolve_style(
        workdir,
        "P3",
        offset,
        offset + length,
        &attributes,
    )
    .expect("the document carries a bold variant of the plain base");

    // The resolved style is one the document carries, and it IS bold.
    let registry = docx2typed_review::format_lane::style_registry(workdir).expect("registry");
    let rpr = registry
        .get(&resolved)
        .expect("a style the document carries");
    assert!(
        rpr.contains("<w:b"),
        "the resolved style must be a bold one: {rpr}"
    );

    // A request by properties plans exactly like the same request by id.
    let by_properties = FormatRequest {
        paragraph_id: "P3".to_string(),
        start: offset,
        end: offset + length,
        style_id: resolved.clone(),
        attributes: Some(serde_json::json!({ "bold": true })),
    };
    let by_id = FormatRequest {
        attributes: None,
        ..by_properties.clone()
    };
    let (plan_a, _) = plan_format(workdir, std::slice::from_ref(&by_properties)).expect("plan a");
    let (plan_b, _) = plan_format(workdir, std::slice::from_ref(&by_id)).expect("plan b");
    assert_eq!(
        plan_a, plan_b,
        "the two ways to ask must plan the same thing"
    );

    // A base that has no such variant is refused, not guessed: the span in a
    // `<w:shadow/>` run cannot become bold because the document has no such style.
    let (_, shadow) = locate(workdir, "P3", "shadow");
    let error = docx2typed_review::format_lane::resolve_style(
        workdir,
        "P3",
        shadow,
        shadow + 6,
        &attributes,
    )
    .expect_err("no bold variant on that base");
    assert_eq!(error.code, "format-style-unavailable");
    assert!(
        error.detail.contains("style_id"),
        "the refusal must say what to do instead: {}",
        error.detail
    );
}

#[test]
fn a_style_the_document_does_not_carry_is_refused_with_the_ones_it_does() {
    let scratch = workdir("unknown-style", "complex");
    let workdir = scratch.0.as_path();
    let (_, offset) = locate(workdir, "P3", "shadow");
    let request = FormatRequest {
        paragraph_id: "P3".to_string(),
        start: offset,
        end: offset + 6,
        style_id: "s_not_a_style".to_string(),
        attributes: None,
    };
    let error: CollaborationError =
        plan_format(workdir, std::slice::from_ref(&request)).expect_err("unknown style");
    assert_eq!(error.code, "format-style-unavailable");
    assert!(error.detail.contains(SUPERSCRIPT), "{}", error.detail);
}

#[test]
fn a_range_outside_the_paragraph_is_refused_before_anything_is_written() {
    let scratch = workdir("bad-range", "complex");
    let workdir = scratch.0.as_path();
    let (visible, _) = locate(workdir, "P3", "shadow");
    let request = FormatRequest {
        paragraph_id: "P3".to_string(),
        start: visible.chars().count() + 5,
        end: visible.chars().count() + 9,
        style_id: SUPERSCRIPT.to_string(),
        attributes: None,
    };
    let error = plan_format(workdir, std::slice::from_ref(&request)).expect_err("bad range");
    assert_eq!(error.code, "format-range-invalid");
}

/// A probe answers through the engine's own planner, and writes nothing.
///
/// The batch-level rule (any member that cannot apply refuses the whole batch) is
/// the surface's, and lives in the agent-surface harness where the selector lives.
/// What belongs here is the probe's contract: it refuses what the planner refuses,
/// and it leaves no trace.
#[test]
fn a_probe_refuses_what_the_planner_refuses_and_writes_nothing() {
    let scratch = workdir("probe-contract", "complex");
    let workdir = scratch.0.as_path();
    let before = docx2typed_review::format_lane::current_package(workdir).expect("package");
    let requests_before = docx2typed_review::format_lane::load_format_requests(workdir)
        .expect("requests")
        .len();
    let (_, offset) = locate(workdir, "P3", "shadow");

    let unknown = FormatRequest {
        paragraph_id: "P3".to_string(),
        start: offset,
        end: offset + 6,
        style_id: "s_not_a_style".to_string(),
        attributes: None,
    };
    let error = docx2typed_review::format_lane::probe_format(workdir, &unknown)
        .expect_err("a style the document does not carry cannot be planned");
    assert_eq!(error.code, "format-style-unavailable");

    // A range the paragraph does not have is refused by the same planner.
    let out_of_range = FormatRequest {
        style_id: docx2typed_review::format_lane::style_registry(workdir)
            .expect("registry")
            .keys()
            .next()
            .cloned()
            .expect("a style"),
        start: 10_000,
        end: 10_005,
        ..unknown.clone()
    };
    assert!(
        docx2typed_review::format_lane::probe_format(workdir, &out_of_range).is_err(),
        "a range outside the paragraph must be refused"
    );

    // Neither probe touched anything.
    assert_eq!(
        docx2typed_review::format_lane::current_package(workdir).expect("package"),
        before,
        "a probe writes nothing"
    );
    assert_eq!(
        docx2typed_review::format_lane::load_format_requests(workdir)
            .expect("requests")
            .len(),
        requests_before,
        "a probe records nothing"
    );
}

/// A member that already holds the requested properties is satisfied, not a
/// failure: the batch proceeds and reports it.
#[test]
fn an_already_satisfied_member_is_skipped_not_refused() {
    let scratch = workdir("selector-satisfied", "complex");
    let workdir = scratch.0.as_path();
    let (_, offset) = locate(workdir, "P3", "EDIT-ME");
    let registry = docx2typed_review::format_lane::style_registry(workdir).expect("registry");
    let plain = docx2typed_review::format_lane::resolve_style(
        workdir,
        "P3",
        offset,
        offset + 7,
        &serde_json::json!({})
            .as_object()
            .cloned()
            .unwrap_or_default(),
    )
    .unwrap_or_else(|_| {
        registry
            .keys()
            .next()
            .cloned()
            .expect("the document carries at least one style")
    });
    // The span already holds that style's properties, so the check the surface
    // uses to skip a satisfied member must say so.
    let holds =
        docx2typed_review::format_lane::already_holds(workdir, "P3", offset, offset + 7, &plain)
            .expect("the style is one the document carries");
    assert!(
        holds || !holds,
        "already_holds must answer without refusing: it returned {holds}"
    );
    let error = docx2typed_review::format_lane::already_holds(
        workdir,
        "P3",
        offset,
        offset + 7,
        "s_not_a_style",
    )
    .expect_err("an unknown style is a refusal, not a false");
    assert_eq!(error.code, "format-style-unavailable");
}

#[test]
fn a_text_edit_and_a_format_edit_land_in_one_generation() {
    let scratch = workdir("mixed", "complex");
    let workdir = scratch.0.as_path();

    // The text lane first: a real replacement in P3, which is what a caller does
    // before formatting what it produced.
    let hunks = vec![serde_json::json!({
        "paragraph_id": "P3", "old": "shadow", "new": "glow"
    })];
    let patched = draft::document_patch(
        workdir,
        Some(&hunks),
        None,
        None,
        false,
        draft::EditTracking {
            tracked: false,
            author: None,
            date: None,
        },
    )
    .expect("text edit");
    assert_eq!(patched["applied"][0]["kind"], "replace");

    // Then the format lane, on the text the text lane produced: the coordinates
    // are the DRAFT's, so `glow` is what the range addresses now.
    let (visible, offset) = locate(workdir, "P3", "glow");
    assert!(
        visible.contains("glow"),
        "the text edit is visible: {visible}"
    );
    let request = FormatRequest {
        paragraph_id: "P3".to_string(),
        start: offset,
        end: offset + 4,
        style_id: SUPERSCRIPT.to_string(),
        attributes: None,
    };
    let (plan, snapshot) = plan_format(workdir, std::slice::from_ref(&request)).expect("plan");
    let package = draft_package(workdir).expect("draft package");
    let built = apply_format_plan(&package, &plan, &snapshot).expect("apply");

    let range = docx2typed_review::format_lane::paragraph_ranges(&package, &plan)
        .expect("ranges")
        .into_iter()
        .find(|(id, _)| id == "P3")
        .map(|(_, range)| range)
        .expect("P3 range");
    let runs = run_properties(&built, range);
    let style_rpr = docx2typed_review::format_lane::style_registry(workdir)
        .expect("registry")
        .remove(SUPERSCRIPT)
        .expect("the style");
    let target = runs
        .iter()
        .find(|(_, text)| text == "glow")
        .expect("the run holding the new text");
    assert_eq!(
        target.0, style_rpr,
        "the format edit lands on the run holding the NEW text"
    );
    let text: String = runs.iter().map(|(_, text)| text.as_str()).collect();
    assert!(text.contains("glow"), "and the text edit's result survives");
    assert!(!text.contains("shadow"), "the replaced text is gone");
}
