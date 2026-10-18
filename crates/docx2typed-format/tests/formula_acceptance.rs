//! The first vertical acceptance for the format engine: `Fe3O4-x`.
//!
//! The case is a real one, measured on a Chinese patent application: a run of
//! eight characters whose digits must be subscript, where the naive "style the
//! run the span falls in" widens the request to the whole run. Every row of the
//! acceptance table below is a test here, and the last one is the mixed
//! transaction: a text edit and a format edit in one generation.
//!
//! The table:
//!
//! | check | expectation |
//! |---|---|
//! | original | one run, seven characters |
//! | intent | digits `3` and `4` subscript |
//! | plan | two non-overlapping character ranges |
//! | build | five runs |
//! | non-target text (`Fe`, `O`, `-x`) | properties unchanged |
//! | text content | `Fe3O4-x` exactly |
//! | second run of the same plan | no further splits |
//! | overlapping format operations | the declared rule, up front |
//! | verification | target ranges and every non-target range |

use docx2typed_format::materialize::materialize;
use docx2typed_format::xml::scan_tags;
use docx2typed_format::{
    plan, DraftRange, FormatIntent, OriginalRun, ParagraphSnapshot, RangeUnit, Snapshot,
};

const NORMAL: &str = r#"<w:rPr><w:rFonts w:ascii="Times New Roman"/><w:sz w:val="24"/></w:rPr>"#;
const SUBSCRIPT: &str = r#"<w:rPr><w:vertAlign w:val="subscript"/><w:rFonts w:ascii="Times New Roman"/><w:sz w:val="24"/></w:rPr>"#;

/// One run of seven characters, exactly as the patent holds it after a text edit
/// collapsed `Fe3O4-x` into a single run.
fn formula_part() -> Vec<u8> {
    format!(
        r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:pPr><w:spacing w:line="360"/></w:pPr><w:r w:rsidR="00A1">{NORMAL}<w:t>Fe3O4-x</w:t></w:r></w:p></w:body></w:document>"#
    )
    .into_bytes()
}

fn formula_snapshot() -> Snapshot {
    let mut styles = std::collections::BTreeMap::new();
    styles.insert("s_normal".to_string(), NORMAL.to_string());
    styles.insert("s_sub".to_string(), SUBSCRIPT.to_string());
    Snapshot {
        generation_id: "gen-1".to_string(),
        base_digest: "template+islands+structure+styles".to_string(),
        styles,
        paragraphs: vec![ParagraphSnapshot {
            paragraph_id: "P9".to_string(),
            draft_text: "Fe3O4-x".to_string(),
            runs: vec![OriginalRun {
                index: 0,
                text: "Fe3O4-x".to_string(),
                rpr: NORMAL.to_string(),
            }],
        }],
    }
}

fn digits_subscript() -> Vec<FormatIntent> {
    vec![
        FormatIntent::SetStyle {
            paragraph_id: "P9".to_string(),
            range: DraftRange::new(2, 3),
            style_id: "s_sub".to_string(),
        },
        FormatIntent::SetStyle {
            paragraph_id: "P9".to_string(),
            range: DraftRange::new(4, 5),
            style_id: "s_sub".to_string(),
        },
    ]
}

/// The paragraph's byte range inside the part, found the way a caller with a
/// paragraph locator would hand it over.
fn paragraph_range(part: &[u8]) -> (usize, usize) {
    let tags = scan_tags(part);
    let start = tags
        .iter()
        .find(|tag| tag.name == "p" && !tag.closing)
        .expect("paragraph open")
        .start;
    let end = tags
        .iter()
        .rev()
        .find(|tag| tag.name == "p" && tag.closing)
        .expect("paragraph close")
        .end;
    (start, end)
}

/// Every run of the materialised part, as `(text, has-subscript)`.
fn runs(part: &[u8]) -> Vec<(String, bool)> {
    use docx2typed_format::xml::decode_text;
    let tags = scan_tags(part);
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut index = 0usize;
    while index < tags.len() {
        let tag = &tags[index];
        if tag.name != "r" || tag.closing {
            index += 1;
            continue;
        }
        let mut depth = 1usize;
        let mut cursor = index + 1;
        let mut text = String::new();
        let mut subscript = false;
        let mut text_start = 0usize;
        while cursor < tags.len() && depth > 0 {
            let inner = &tags[cursor];
            if !inner.closing && !inner.self_closing {
                depth += 1;
                if depth == 2 && inner.name == "t" {
                    text_start = inner.end;
                }
            } else if inner.self_closing {
                // `w:vertAlign` is self-closing; a helper that only looked at
                // container tags would report every styled run as plain. It sits
                // inside `w:rPr`, which is the run's direct child — so at the
                // moment it is seen, the walk is one level deeper.
                if inner.name == "vertAlign" && depth == 2 {
                    subscript = true;
                }
            } else if inner.closing {
                if depth == 2 && inner.name == "t" {
                    text = decode_text(&part[text_start..inner.start]);
                }
                depth -= 1;
                if depth == 0 {
                    out.push((text.clone(), subscript));
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

#[test]
fn the_plan_is_two_ranges_and_five_runs() {
    let snapshot = formula_snapshot();
    let planned = plan(&snapshot, &digits_subscript()).expect("plan");
    assert_eq!(planned.unit, RangeUnit::UnicodeScalars);
    assert_eq!(planned.base_digest, "template+islands+structure+styles");
    let paragraph = planned.paragraph("P9").expect("paragraph");
    assert_eq!(
        paragraph.segments,
        vec![
            docx2typed_format::ExpectedSegment {
                range: DraftRange::new(2, 3),
                style_id: "s_sub".to_string()
            },
            docx2typed_format::ExpectedSegment {
                range: DraftRange::new(4, 5),
                style_id: "s_sub".to_string()
            },
        ],
        "the two ranges the caller asked for, non-overlapping"
    );
    assert_eq!(paragraph.runs.len(), 5, "one run becomes five");
}

#[test]
fn the_build_splits_the_run_and_leaves_everything_else_alone() {
    let part = formula_part();
    let snapshot = formula_snapshot();
    let planned = plan(&snapshot, &digits_subscript()).expect("plan");
    let range = paragraph_range(&part);
    let built = materialize(&part, range, &planned, &snapshot, "P9").expect("materialise");

    let built_runs = runs(&built);
    assert_eq!(
        built_runs,
        vec![
            ("Fe".to_string(), false),
            ("3".to_string(), true),
            ("O".to_string(), false),
            ("4".to_string(), true),
            ("-x".to_string(), false),
        ],
        "five runs, the digits subscript, the rest as they were; built: {}",
        String::from_utf8_lossy(&built)
    );

    // The text is exactly what it was.
    let text: String = built_runs.iter().map(|(text, _)| text.as_str()).collect();
    assert_eq!(text, "Fe3O4-x");

    // Non-target runs keep their properties byte for byte: `Fe` and `O` carry the
    // source run's own rPr, not a copy of the style.
    let built_text = String::from_utf8_lossy(&built);
    assert!(
        built_text.contains(NORMAL),
        "the source properties survive; built: {built_text}"
    );
    assert!(
        built_text.contains(SUBSCRIPT),
        "the requested style landed; built: {built_text}"
    );

    // Protected structure: the paragraph's pPr and the run's own attributes.
    assert!(
        built_text.contains(r#"<w:spacing w:line="360"/>"#),
        "pPr untouched"
    );
    assert!(
        built_text.contains(r#"w:rsidR="00A1""#),
        "run attributes survive"
    );

    // Everything outside the paragraph is byte-identical.
    let (start, end) = range;
    assert_eq!(&built[..start], &part[..start]);
    assert_eq!(&built[end + (built.len() - part.len())..], &part[end..]);
}

#[test]
fn planning_the_built_part_again_splits_nothing_further() {
    let part = formula_part();
    let snapshot = formula_snapshot();
    let planned = plan(&snapshot, &digits_subscript()).expect("plan");
    let range = paragraph_range(&part);
    let built = materialize(&part, range, &planned, &snapshot, "P9").expect("materialise");

    // The second generation sees the split runs as its original runs.
    let mut second = formula_snapshot();
    second.generation_id = "gen-2".to_string();
    second.paragraphs[0].runs = runs(&built)
        .into_iter()
        .enumerate()
        .map(|(index, (text, subscript))| OriginalRun {
            index,
            text,
            rpr: if subscript { SUBSCRIPT } else { NORMAL }.to_string(),
        })
        .collect();
    let second_plan = plan(&second, &digits_subscript()).expect("plan again");
    let built_again = materialize(&built, paragraph_range(&built), &second_plan, &second, "P9")
        .expect("materialise");

    assert_eq!(
        built_again, built,
        "re-planning an already-planned paragraph must not add splits"
    );
}

#[test]
fn overlapping_requests_are_settled_before_anything_is_built() {
    // Different styles over one character: refused, and the refusal names both.
    let mut snapshot = formula_snapshot();
    snapshot
        .styles
        .insert("s_bold".to_string(), r#"<w:rPr><w:b/></w:rPr>"#.to_string());
    let intents = vec![
        FormatIntent::SetStyle {
            paragraph_id: "P9".to_string(),
            range: DraftRange::new(2, 4),
            style_id: "s_sub".to_string(),
        },
        FormatIntent::SetStyle {
            paragraph_id: "P9".to_string(),
            range: DraftRange::new(3, 5),
            style_id: "s_bold".to_string(),
        },
    ];
    let error = plan(&snapshot, &intents).expect_err("overlap");
    assert_eq!(error.code, "format-spans-overlap");

    // The same style twice: one segment, not two overlapping runs.
    let planned = plan(
        &formula_snapshot(),
        &[
            FormatIntent::SetStyle {
                paragraph_id: "P9".to_string(),
                range: DraftRange::new(2, 4),
                style_id: "s_sub".to_string(),
            },
            FormatIntent::SetStyle {
                paragraph_id: "P9".to_string(),
                range: DraftRange::new(3, 5),
                style_id: "s_sub".to_string(),
            },
        ],
    )
    .expect("plan");
    assert_eq!(
        planned.paragraph("P9").expect("paragraph").segments.len(),
        1
    );
}

#[test]
fn a_run_holding_more_than_properties_and_text_is_refused() {
    // A break inside the run: rewriting it would have to preserve more than
    // properties, and v1 does not claim to.
    let part = String::from_utf8(formula_part())
        .expect("utf8")
        .replace("<w:t>Fe3O4-x</w:t>", "<w:br/><w:t>Fe3O4-x</w:t>");
    let part = part.into_bytes();
    let snapshot = formula_snapshot();
    let planned = plan(&snapshot, &digits_subscript()).expect("plan");
    let error = materialize(&part, paragraph_range(&part), &planned, &snapshot, "P9")
        .expect_err("structured run");
    assert_eq!(error.code, "format-run-has-structure");
    assert!(error.detail.contains("br"), "{}", error.detail);
}

#[test]
fn a_snapshot_that_disagrees_with_the_part_is_refused() {
    let part = formula_part();
    let mut snapshot = formula_snapshot();
    snapshot.paragraphs[0].runs[0].text = "Fe3O4-y".to_string();
    let planned = plan(&snapshot, &digits_subscript()).expect("plan");
    let error = materialize(&part, paragraph_range(&part), &planned, &snapshot, "P9")
        .expect_err("stale snapshot");
    assert_eq!(error.code, "format-snapshot-stale");
}

/// The mixed transaction: a text edit and a format edit in one generation.
///
/// The text lane rewrote the run (`Fe3O4` → `Fe3O4-x`, which collapsed it to one
/// run and lost the subscripts); the format lane then styles the digits in the
/// text the text lane produced. Both addresses are measured in the DRAFT, so the
/// format plan resolves against what the text edit left, not against the
/// template — the coordinate fact that cost a debugging round when the verifier
/// resolved it the other way.
#[test]
fn a_text_edit_and_a_format_edit_share_one_generation() {
    // The template run, before the text edit.
    let template_part = String::from_utf8(formula_part())
        .expect("utf8")
        .replace("Fe3O4-x", "Fe3O4")
        .into_bytes();
    let template_run = OriginalRun {
        index: 0,
        text: "Fe3O4".to_string(),
        rpr: NORMAL.to_string(),
    };

    // The text lane's edit, applied the way the core applies it: the new text
    // takes the first covered run's place.
    let after_text_edit = String::from_utf8(template_part)
        .expect("utf8")
        .replace("<w:t>Fe3O4</w:t>", "<w:t>Fe3O4-x</w:t>")
        .into_bytes();

    // The snapshot the format engine plans against: the DRAFT — template runs
    // with the text islands applied.
    let snapshot = Snapshot {
        generation_id: "gen-mixed".to_string(),
        base_digest: "template+islands(P9)+structure+styles".to_string(),
        styles: formula_snapshot().styles,
        paragraphs: vec![ParagraphSnapshot {
            paragraph_id: "P9".to_string(),
            draft_text: "Fe3O4-x".to_string(),
            runs: vec![OriginalRun {
                index: 0,
                text: "Fe3O4-x".to_string(),
                rpr: template_run.rpr.clone(),
            }],
        }],
    };
    let planned = plan(&snapshot, &digits_subscript()).expect("plan");
    let built = materialize(
        &after_text_edit,
        paragraph_range(&after_text_edit),
        &planned,
        &snapshot,
        "P9",
    )
    .expect("materialise");

    let built_runs = runs(&built);
    assert_eq!(
        built_runs
            .iter()
            .map(|(text, _)| text.as_str())
            .collect::<String>(),
        "Fe3O4-x",
        "the text edit's result is the text that survives"
    );
    assert_eq!(
        built_runs
            .iter()
            .filter(|(_, subscript)| *subscript)
            .map(|(text, _)| text.as_str())
            .collect::<Vec<&str>>(),
        vec!["3", "4"],
        "and the format edit lands on the digits of the NEW text"
    );
    // The plan binds the generation and the baseline it was derived from, so a
    // build against another one is refusable rather than silently applied.
    assert_eq!(planned.generation_id, "gen-mixed");
    assert!(planned.base_digest.contains("islands(P9)"));
}
