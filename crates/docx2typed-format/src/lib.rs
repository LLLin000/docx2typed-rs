//! The format engine: the only place that interprets or plans character formatting.
//!
//! ## What this crate owns, and what it does not
//!
//! It owns three things, exclusively:
//!
//! 1. **The meaning of a formatting request** — which character ranges the caller
//!    meant, in which unit, and which style (a set of properties the document
//!    already carries) they name.
//! 2. **The plan from characters to runs** — a character-level request can change
//!    the NUMBER, the BOUNDARIES and the OWNERSHIP of runs. The authoritative
//!    representation is therefore not "what rPr should run *i* have" but
//!    `ExpectedFormatting`: paragraph + character range + the direct properties
//!    that range is expected to have. The run structure is DERIVED from that.
//! 3. **The expectation itself** — a `FormatPlan` that the build executes and the
//!    verifier checks. One derivation, consumed twice, instead of two modules
//!    each replaying the same edit records.
//!
//! It does NOT own text bytes, paragraph identity or anchors (the text core's
//! frozen contract), and it does not read or write a workdir: `plan` is a pure
//! function of a snapshot and a set of intents. Whoever has the transaction
//! builds the snapshot and applies the plan.
//!
//! ## Scope of v1
//!
//! **Direct formatting only.** A run's own `w:rPr` is what this crate reads,
//! plans and writes. Style inheritance — `docDefaults`, `styles.xml`, paragraph
//! styles, character styles, `basedOn`, the paragraph mark's own `rPr` — is NOT
//! computed here and must not be claimed: direct properties, computed properties
//! and what Word finally renders are three different things. A style the
//! registry holds is a set of direct properties; switching to it says nothing
//! about the document's inherited formatting.
//!
//! ## The unit, declared
//!
//! [`RangeUnit::UnicodeScalars`] — a `DraftRange` counts Unicode scalar values in
//! the paragraph's draft text, the same surface the projection publishes. A
//! range can therefore never split a character; combining sequences, grapheme
//! clusters and bidirectional text are addressed as their scalars, which is what
//! the projection shows a caller. Byte offsets are NOT a range unit here: they
//! appear only inside the materialiser, where the bytes are the crate's own.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub mod materialize;
pub mod xml;

/// Schema name for a serialised plan.
pub const PLAN_SCHEMA: &str = "docx2typed-format-plan-1";

/// The unit a [`DraftRange`] counts in. Declared in the plan, never implied.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RangeUnit {
    /// Unicode scalar values of the paragraph's draft text.
    UnicodeScalars,
}

/// A character range in a paragraph's draft text, in [`RangeUnit`]s.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DraftRange {
    pub start: usize,
    pub end: usize,
}

impl DraftRange {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(&self) -> bool {
        self.start >= self.end
    }

    pub fn overlaps(&self, other: &DraftRange) -> bool {
        self.start < other.end && other.start < self.end
    }

    pub fn touches(&self, other: &DraftRange) -> bool {
        self.start <= other.end && other.start <= self.end
    }

    /// The smallest range holding both.
    pub fn union(&self, other: &DraftRange) -> DraftRange {
        DraftRange::new(self.start.min(other.start), self.end.max(other.end))
    }
}

/// One run as the snapshot found it: its text and its raw `w:rPr` (empty when
/// the run carries none).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginalRun {
    pub index: usize,
    pub text: String,
    pub rpr: String,
}

/// One paragraph as the snapshot found it: the draft text a caller's offsets are
/// measured in, and the runs that text currently lives in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParagraphSnapshot {
    pub paragraph_id: String,
    /// The paragraph's draft text — the projection's surface, which is what a
    /// caller's offsets and this crate's ranges both address.
    pub draft_text: String,
    pub runs: Vec<OriginalRun>,
}

/// Everything a plan is derived from. The digest binds the baselines that
/// participate: the template package, the text islands, the structure sidecar
/// and the style registry. A plan carries it so a build can refuse a plan made
/// against a different document.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub generation_id: String,
    pub base_digest: String,
    /// `style_id -> raw w:rPr` — the properties a request may switch to. This is
    /// the registry the document carries; the engine never invents an entry.
    pub styles: BTreeMap<String, String>,
    pub paragraphs: Vec<ParagraphSnapshot>,
}

/// What the caller asked for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FormatIntent {
    /// The characters in `range` get the direct properties of `style_id`.
    SetStyle {
        paragraph_id: String,
        range: DraftRange,
        style_id: String,
    },
}

/// A character range and the direct properties it is expected to have.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpectedSegment {
    pub range: DraftRange,
    pub style_id: String,
}

/// One output run: which characters it holds, and where its properties come from.
///
/// `style` names the registry entry to use; `None` means "keep the properties of
/// `source_run`", which is how a character the caller did not touch keeps exactly
/// the bytes it had.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedRun {
    pub range: DraftRange,
    pub style: Option<String>,
    pub source_run: usize,
}

/// What one paragraph becomes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParagraphPlan {
    pub paragraph_id: String,
    pub segments: Vec<ExpectedSegment>,
    pub runs: Vec<PlannedRun>,
    /// The ranges this edit may not change: everything outside the segments.
    pub protected: Vec<DraftRange>,
}

/// The whole plan: what the build executes and the verifier checks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormatPlan {
    pub schema: String,
    pub generation_id: String,
    pub base_digest: String,
    pub unit: RangeUnit,
    pub paragraphs: Vec<ParagraphPlan>,
}

impl FormatPlan {
    pub fn paragraph(&self, paragraph_id: &str) -> Option<&ParagraphPlan> {
        self.paragraphs
            .iter()
            .find(|plan| plan.paragraph_id == paragraph_id)
    }
}

/// A refusal. `code` is the stable name a caller branches on; `detail` says what
/// to do about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormatError {
    pub code: &'static str,
    pub detail: String,
}

impl FormatError {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for FormatError {}

/// Derive the plan. Pure: same snapshot + same intents = same plan.
///
/// The rules, in the order they apply:
///
/// 1. A range must be inside its paragraph's draft text (`format-range-invalid`).
/// 2. A style must be one the registry holds (`format-style-unavailable`, with
///    the ones it does hold listed).
/// 3. Two intents that overlap and name DIFFERENT styles are refused
///    (`format-spans-overlap`): one character cannot be expected to have two
///    direct property sets, and choosing one silently is how a caller's second
///    request disappears. The same style overlapping itself merges.
/// 4. Everything else is boundaries: the paragraph's start and end, each
///    segment's edges, and each original run's edges. Between two boundaries is
///    one output run, taking the segment's style when it lies inside one, and
///    its source run's properties otherwise.
pub fn plan(snapshot: &Snapshot, intents: &[FormatIntent]) -> Result<FormatPlan, FormatError> {
    let mut paragraphs: Vec<ParagraphPlan> = Vec::new();

    for paragraph in &snapshot.paragraphs {
        let mut requested: Vec<(DraftRange, String)> = Vec::new();
        for intent in intents {
            let FormatIntent::SetStyle {
                paragraph_id,
                range,
                style_id,
            } = intent;
            if paragraph_id != &paragraph.paragraph_id {
                continue;
            }
            let length = paragraph.draft_text.chars().count();
            if range.is_empty() || range.end > length {
                return Err(FormatError::new(
                    "format-range-invalid",
                    format!(
                        "{} is not a range of {}: the paragraph's draft text is {length} \
                         character(s) long and a range must be non-empty and inside it",
                        describe_range(range),
                        paragraph.paragraph_id
                    ),
                ));
            }
            if !snapshot.styles.contains_key(style_id) {
                let mut listed: Vec<&str> = snapshot.styles.keys().map(String::as_str).collect();
                listed.sort();
                return Err(FormatError::new(
                    "format-style-unavailable",
                    format!(
                        "{style_id} is not a style this document carries; it holds [{}]. A \
                         format edit switches between the properties the document already has — \
                         it never invents one",
                        listed.join(", ")
                    ),
                ));
            }
            requested.push((*range, style_id.clone()));
        }

        // Overlap rule, applied before anything is planned: a character that two
        // different styles both claim is a question the engine must not answer
        // by picking one.
        requested.sort_by_key(|(range, _)| (range.start, range.end));
        let mut merged: Vec<(DraftRange, String)> = Vec::new();
        for (range, style) in requested {
            match merged.last_mut() {
                Some((existing, existing_style))
                    if existing.overlaps(&range) && *existing_style != style =>
                {
                    return Err(FormatError::new(
                        "format-spans-overlap",
                        format!(
                            "{} and {} overlap in {} and name different styles ({} and {}); one \
                             character cannot be expected to carry two sets of direct \
                             properties. Make them one range, or two that do not overlap",
                            describe_range(existing),
                            describe_range(&range),
                            paragraph.paragraph_id,
                            existing_style,
                            style
                        ),
                    ));
                }
                Some((existing, existing_style))
                    if existing.touches(&range) && *existing_style == style =>
                {
                    *existing = existing.union(&range);
                }
                _ => merged.push((range, style)),
            }
        }
        let segments: Vec<ExpectedSegment> = merged
            .iter()
            .map(|(range, style_id)| ExpectedSegment {
                range: *range,
                style_id: style_id.clone(),
            })
            .collect();

        // Boundaries: the paragraph, the segments, and the runs the text
        // currently lives in. This is what turns a character-level request into a
        // run structure — and what makes planning a second time a no-op, because
        // the boundaries it produces are the ones it already used.
        let mut boundaries: Vec<usize> = vec![0, paragraph.draft_text.chars().count()];
        for segment in &segments {
            boundaries.push(segment.range.start);
            boundaries.push(segment.range.end);
        }
        let mut cursor = 0usize;
        for run in &paragraph.runs {
            cursor += run.text.chars().count();
            boundaries.push(cursor);
        }
        boundaries.sort_unstable();
        boundaries.dedup();

        let mut runs: Vec<PlannedRun> = Vec::new();
        for window in boundaries.windows(2) {
            let (start, end) = (window[0], window[1]);
            if start >= end {
                continue;
            }
            let range = DraftRange::new(start, end);
            let style = segments
                .iter()
                .find(|segment| segment.range.start <= start && segment.range.end >= end)
                .map(|segment| segment.style_id.clone());
            let Some(source_run) = run_at(paragraph, start) else {
                return Err(FormatError::new(
                    "format-run-unmapped",
                    format!(
                        "{} has no run covering character {start}; the snapshot's runs and its \
                         draft text disagree",
                        paragraph.paragraph_id
                    ),
                ));
            };
            // Adjacent pieces that came from the same run and carry the same
            // style are one run: splitting for its own sake is not a plan.
            match runs.last_mut() {
                Some(previous)
                    if previous.style == style
                        && previous.source_run == source_run
                        && previous.range.end == start =>
                {
                    previous.range.end = end;
                }
                _ => runs.push(PlannedRun {
                    range,
                    style,
                    source_run,
                }),
            }
        }

        let mut protected: Vec<DraftRange> = Vec::new();
        let mut cursor = 0usize;
        for segment in &segments {
            if cursor < segment.range.start {
                protected.push(DraftRange::new(cursor, segment.range.start));
            }
            cursor = segment.range.end;
        }
        let length = paragraph.draft_text.chars().count();
        if cursor < length {
            protected.push(DraftRange::new(cursor, length));
        }

        paragraphs.push(ParagraphPlan {
            paragraph_id: paragraph.paragraph_id.clone(),
            segments,
            runs,
            protected,
        });
    }

    // An intent for a paragraph the snapshot does not hold is a refusal, not a
    // silently dropped request.
    for intent in intents {
        let FormatIntent::SetStyle { paragraph_id, .. } = intent;
        if !snapshot
            .paragraphs
            .iter()
            .any(|paragraph| &paragraph.paragraph_id == paragraph_id)
        {
            return Err(FormatError::new(
                "format-paragraph-unknown",
                format!("{paragraph_id} is not in this document's snapshot"),
            ));
        }
    }

    Ok(FormatPlan {
        schema: PLAN_SCHEMA.to_string(),
        generation_id: snapshot.generation_id.clone(),
        base_digest: snapshot.base_digest.clone(),
        unit: RangeUnit::UnicodeScalars,
        paragraphs,
    })
}

/// The index of the run holding the paragraph's `offset`-th character.
fn run_at(paragraph: &ParagraphSnapshot, offset: usize) -> Option<usize> {
    let mut cursor = 0usize;
    for run in &paragraph.runs {
        let length = run.text.chars().count();
        if offset >= cursor && offset < cursor + length {
            return Some(run.index);
        }
        cursor += length;
    }
    // An empty run at the end of the paragraph owns the boundary after the last
    // character; a plan that only touches the end must still name a source.
    paragraph.runs.last().map(|run| run.index)
}

fn describe_range(range: &DraftRange) -> String {
    format!("[{}, {})", range.start, range.end)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The acceptance case: `Fe3O4-x` is ONE run of seven characters, and the
    /// caller wants the two digits subscript. Planning it must produce five runs,
    /// not "one run with new properties".
    fn formula_snapshot() -> Snapshot {
        let mut styles = BTreeMap::new();
        styles.insert(
            "s_normal".to_string(),
            r#"<w:rPr><w:sz w:val="24"/></w:rPr>"#.to_string(),
        );
        styles.insert(
            "s_sub".to_string(),
            r#"<w:rPr><w:vertAlign w:val="subscript"/><w:sz w:val="24"/></w:rPr>"#.to_string(),
        );
        Snapshot {
            generation_id: "gen-1".to_string(),
            base_digest: "base-1".to_string(),
            styles,
            paragraphs: vec![ParagraphSnapshot {
                paragraph_id: "P9".to_string(),
                draft_text: "Fe3O4-x".to_string(),
                runs: vec![OriginalRun {
                    index: 0,
                    text: "Fe3O4-x".to_string(),
                    rpr: r#"<w:rPr><w:sz w:val="24"/></w:rPr>"#.to_string(),
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

    #[test]
    fn a_character_request_splits_one_run_into_five() {
        let plan = plan(&formula_snapshot(), &digits_subscript()).expect("plan");
        let paragraph = plan.paragraph("P9").expect("paragraph");
        assert_eq!(paragraph.segments.len(), 2, "two non-overlapping ranges");
        let runs: Vec<(usize, usize, Option<&str>)> = paragraph
            .runs
            .iter()
            .map(|run| (run.range.start, run.range.end, run.style.as_deref()))
            .collect();
        assert_eq!(
            runs,
            vec![
                (0, 2, None),
                (2, 3, Some("s_sub")),
                (3, 4, None),
                (4, 5, Some("s_sub")),
                (5, 7, None),
            ]
        );
        // Every piece knows which run it came from, so the properties it keeps
        // are the bytes that run had.
        assert!(paragraph.runs.iter().all(|run| run.source_run == 0));
    }

    #[test]
    fn non_target_characters_are_protected_and_keep_their_source() {
        let plan = plan(&formula_snapshot(), &digits_subscript()).expect("plan");
        let paragraph = plan.paragraph("P9").expect("paragraph");
        assert_eq!(
            paragraph.protected,
            vec![
                DraftRange::new(0, 2),
                DraftRange::new(3, 4),
                DraftRange::new(5, 7)
            ]
        );
        // The text is untouched: the ranges tile the paragraph exactly.
        let mut cursor = 0usize;
        for run in &paragraph.runs {
            assert_eq!(run.range.start, cursor);
            cursor = run.range.end;
        }
        assert_eq!(cursor, "Fe3O4-x".chars().count());
    }

    #[test]
    fn overlapping_ranges_with_different_styles_are_refused() {
        let mut styles = formula_snapshot().styles.clone();
        styles.insert("s_bold".to_string(), r#"<w:rPr><w:b/></w:rPr>"#.to_string());
        let mut snapshot = formula_snapshot();
        snapshot.styles = styles;
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
        assert!(error.detail.contains("different styles"));
    }

    #[test]
    fn overlapping_ranges_with_the_same_style_merge() {
        let intents = vec![
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
        ];
        let plan = plan(&formula_snapshot(), &intents).expect("plan");
        let paragraph = plan.paragraph("P9").expect("paragraph");
        assert_eq!(paragraph.segments.len(), 1);
        assert_eq!(paragraph.segments[0].range, DraftRange::new(2, 5));
    }

    #[test]
    fn an_unknown_style_is_refused_with_the_ones_available() {
        let intents = vec![FormatIntent::SetStyle {
            paragraph_id: "P9".to_string(),
            range: DraftRange::new(2, 3),
            style_id: "s_nope".to_string(),
        }];
        let error = plan(&formula_snapshot(), &intents).expect_err("unknown style");
        assert_eq!(error.code, "format-style-unavailable");
        assert!(error.detail.contains("s_normal"), "{}", error.detail);
        assert!(error.detail.contains("s_sub"), "{}", error.detail);
    }

    #[test]
    fn a_range_outside_the_paragraph_is_refused() {
        for range in [DraftRange::new(0, 0), DraftRange::new(3, 8)] {
            let intents = vec![FormatIntent::SetStyle {
                paragraph_id: "P9".to_string(),
                range,
                style_id: "s_sub".to_string(),
            }];
            let error = plan(&formula_snapshot(), &intents).expect_err("invalid range");
            assert_eq!(error.code, "format-range-invalid", "{range:?}");
        }
    }

    #[test]
    fn an_intent_for_a_missing_paragraph_is_refused() {
        let intents = vec![FormatIntent::SetStyle {
            paragraph_id: "P404".to_string(),
            range: DraftRange::new(0, 1),
            style_id: "s_sub".to_string(),
        }];
        let error = plan(&formula_snapshot(), &intents).expect_err("missing paragraph");
        assert_eq!(error.code, "format-paragraph-unknown");
    }

    #[test]
    fn a_plan_that_changes_nothing_still_tiles_the_paragraph() {
        let plan = plan(&formula_snapshot(), &[]).expect("plan");
        let paragraph = plan.paragraph("P9").expect("paragraph");
        assert_eq!(paragraph.segments.len(), 0);
        assert_eq!(paragraph.runs.len(), 1, "no request, no split");
        assert_eq!(paragraph.runs[0].range, DraftRange::new(0, 7));
        // With no request, every character is protected: nothing may change.
        assert_eq!(paragraph.protected, vec![DraftRange::new(0, 7)]);
    }
}
