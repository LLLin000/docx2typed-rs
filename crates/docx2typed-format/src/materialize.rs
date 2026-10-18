//! From a plan to bytes: the run structure a plan describes, written into a part.
//!
//! This is the serialisation half of the engine, deliberately separate from
//! planning: "what the formatting should be" is decided in [`crate::plan`], "how
//! that becomes runs" is decided here. A caller that reads a plan learns the
//! outcome without reading XML; a caller that applies one re-decides nothing.
//!
//! Three rules give the guarantees their teeth:
//!
//! - **A run the edit does not touch is copied byte for byte.** No rebuild, no
//!   re-encoding, no look inside. "Non-target runs are unchanged" is therefore a
//!   fact about bytes, not a promise about a rebuild.
//! - **A run of pure structure keeps its place.** A tab, a break, a drawing has
//!   no `w:t`, contributes no characters, and no plan can cover it: it is copied
//!   where it sits, between the runs the plan rebuilds. That is what makes a
//!   paragraph holding a tab editable at all.
//! - **A run the edit DOES rewrite must hold only its properties and its text.**
//!   Splitting or restyling a run that also carries a break would have to
//!   preserve more than properties, which v1 does not claim to do.
//!
//! It also refuses a snapshot that disagrees with the part, and a paragraph range
//! that is not inside the part.

use crate::xml::{decode_text, encode_text, needs_space_preservation, scan_tags};
use crate::{FormatError, FormatPlan, Snapshot};

/// One run as the part holds it.
#[derive(Clone, Debug)]
struct RunSpan {
    /// Byte range of the whole `<w:r>…</w:r>`.
    start: usize,
    end: usize,
    /// The open tag, copied onto every piece of this run.
    open_tag: Vec<u8>,
    /// Raw `w:rPr` bytes, tags included; empty when the run carries none.
    rpr: Vec<u8>,
    text: String,
    /// Whether the run holds a `w:t` at all.
    has_text: bool,
    /// A run that is not a direct child of `w:p` — inside a hyperlink, a `w:ins`,
    /// a smart tag. Its characters are part of the paragraph's coordinates, but
    /// its bytes belong to the element around it: this crate may not restyle it
    /// (that would mean rewriting a parent it does not own) and never rewrites it.
    nested: bool,
    /// A child besides the properties and one text node — a break, a tab, a
    /// drawing. Only a run the edit rewrites has to be clean.
    structured: Option<String>,
}

/// Apply one paragraph's plan to a part.
///
/// `paragraph` is the paragraph's byte range inside `part` — the caller owns
/// paragraph identity, this crate owns what happens inside.
pub fn materialize(
    part: &[u8],
    paragraph: (usize, usize),
    plan: &FormatPlan,
    snapshot: &Snapshot,
    paragraph_id: &str,
) -> Result<Vec<u8>, FormatError> {
    let Some(paragraph_plan) = plan.paragraph(paragraph_id) else {
        return Err(FormatError {
            code: "format-plan-missing-paragraph",
            detail: format!("the plan holds no entry for {paragraph_id}"),
        });
    };
    let Some(paragraph_snapshot) = snapshot
        .paragraphs
        .iter()
        .find(|entry| entry.paragraph_id == paragraph_id)
    else {
        return Err(FormatError {
            code: "format-paragraph-unknown",
            detail: format!("{paragraph_id} is not in this snapshot"),
        });
    };
    let (start, end) = paragraph;
    if start >= end || end > part.len() {
        return Err(FormatError {
            code: "format-paragraph-range-invalid",
            detail: format!(
                "the paragraph range [{start}, {end}) is not inside a part of {} byte(s)",
                part.len()
            ),
        });
    }
    let runs = runs_of(&part[start..end], start)?;

    // Only runs holding text take part in the coordinate comparison: a run of
    // pure structure contributes no characters and no snapshot entry.
    let text_runs: Vec<usize> = runs
        .iter()
        .enumerate()
        .filter(|(_, run)| run.has_text)
        .map(|(index, _)| index)
        .collect();
    let snapshot_text: String = paragraph_snapshot
        .runs
        .iter()
        .map(|run| run.text.as_str())
        .collect();
    let part_text: String = text_runs
        .iter()
        .map(|index| runs[*index].text.as_str())
        .collect();
    if text_runs.len() != paragraph_snapshot.runs.len() || snapshot_text != part_text {
        return Err(FormatError {
            code: "format-snapshot-stale",
            detail: format!(
                "{} holds {} text run(s) reading {:?}, the snapshot has {} reading {:?}; re-read \
                 the document and plan again",
                paragraph_snapshot.paragraph_id,
                text_runs.len(),
                part_text,
                paragraph_snapshot.runs.len(),
                snapshot_text
            ),
        });
    }

    // Where each text run's characters start, in the plan's coordinates.
    let mut source_ranges: Vec<(usize, usize)> = Vec::new();
    let mut cursor = 0usize;
    for index in &text_runs {
        let length = runs[*index].text.chars().count();
        source_ranges.push((cursor, cursor + length));
        cursor += length;
    }

    // Rebuild each TOP-LEVEL run in place, and leave every byte of everything else
    // alone: a run inside a hyperlink belongs to the hyperlink, a bookmark sits
    // between runs, and a `w:pPr` precedes them all. Splicing them back-to-front
    // is what keeps the paragraph's structure exactly where it was.
    let mut splices: Vec<(usize, usize, Vec<u8>)> = Vec::new();
    for (text_position, run_index) in text_runs.iter().enumerate() {
        let source = &runs[*run_index];
        let source_range = source_ranges[text_position];
        let mut pieces: Vec<&crate::PlannedRun> = paragraph_plan
            .runs
            .iter()
            .filter(|planned| planned.source_run == text_position)
            .collect();
        if pieces.is_empty() {
            return Err(FormatError {
                code: "format-plan-incomplete",
                detail: format!(
                    "the plan names no run for characters [{}, {}) of {}; a plan must tile the \
                     paragraph it plans",
                    source_range.0, source_range.1, paragraph_snapshot.paragraph_id
                ),
            });
        }
        pieces.sort_by_key(|planned| planned.range.start);
        if source.nested {
            // Its characters are in the plan, but its bytes are its parent's. An
            // untouched nested run needs nothing; a restyled one would mean
            // rewriting a hyperlink, which this crate does not own.
            let styled = pieces.iter().any(|planned| planned.style.is_some());
            if styled {
                return Err(FormatError {
                    code: "format-run-nested",
                    detail: format!(
                        "the characters [{}, {}) of {} sit in a run inside another element (a \
                         hyperlink, a revision wrapper); restyling them would mean rewriting that \
                         element, which the format engine does not own. Format text in a run that \
                         is a direct child of the paragraph",
                        source_range.0, source_range.1, paragraph_snapshot.paragraph_id
                    ),
                });
            }
            continue;
        }
        // The ordinary case, and the one that keeps untouched bytes untouched.
        let untouched = pieces.len() == 1
            && pieces[0].style.is_none()
            && pieces[0].range.start == source_range.0
            && pieces[0].range.end == source_range.1;
        if untouched {
            continue;
        }
        if let Some(child) = &source.structured {
            return Err(FormatError {
                code: "format-run-has-structure",
                detail: format!(
                    "the run holding characters [{}, {}) of {} carries <{}> besides its \
                     properties and text; splitting or restyling it would have to preserve more \
                     than properties, which v1 does not claim to do. Format a range inside a run \
                     that holds only text",
                    source_range.0, source_range.1, paragraph_snapshot.paragraph_id, child
                ),
            });
        }
        let characters: Vec<char> = source.text.chars().collect();
        let mut rebuilt: Vec<u8> = Vec::new();
        for planned in pieces {
            let piece: String = characters
                [planned.range.start - source_range.0..planned.range.end - source_range.0]
                .iter()
                .collect();
            let rpr: &[u8] = match &planned.style {
                Some(style_id) => snapshot
                    .styles
                    .get(style_id)
                    .ok_or_else(|| FormatError {
                        code: "format-style-unavailable",
                        detail: format!("{style_id} is not in this snapshot's registry"),
                    })?
                    .as_bytes(),
                // A piece of a run the edit splits keeps that run's properties.
                None => &source.rpr,
            };
            rebuilt.extend_from_slice(&source.open_tag);
            rebuilt.extend_from_slice(rpr);
            if needs_space_preservation(&piece) {
                rebuilt.extend_from_slice(b"<w:t xml:space=\"preserve\">");
            } else {
                rebuilt.extend_from_slice(b"<w:t>");
            }
            rebuilt.extend_from_slice(encode_text(&piece).as_bytes());
            rebuilt.extend_from_slice(b"</w:t></w:r>");
        }
        splices.push((source.start, source.end, rebuilt));
    }

    let mut out = part.to_vec();
    for (start, end, replacement) in splices.into_iter().rev() {
        out.splice(start..end, replacement);
    }
    Ok(out)
}

/// The paragraph's runs, in order, with what a rewrite needs to preserve.
///
/// A run that is not a direct child of `w:p` is recorded too — its characters are
/// part of the paragraph's coordinates — but marked `nested`, so the rewrite knows
/// its bytes belong to the element around it.
fn runs_of(paragraph: &[u8], base: usize) -> Result<Vec<RunSpan>, FormatError> {
    let tags = scan_tags(paragraph);
    let mut runs: Vec<RunSpan> = Vec::new();
    let mut index = 0usize;
    let mut depth = 0usize;
    while index < tags.len() {
        let tag = &tags[index];
        if tag.closing {
            depth = depth.saturating_sub(1);
            index += 1;
            continue;
        }
        let nested = depth > 1;
        if tag.name == "r" && !tag.self_closing {
            let run_depth = depth;
            let mut cursor = index + 1;
            let mut inner_depth = run_depth + 1;
            let mut rpr: Option<(usize, usize)> = None;
            let mut text: Option<(usize, usize)> = None;
            let mut structured: Option<String> = None;
            let mut run_end = tag.end;
            while cursor < tags.len() && inner_depth > run_depth {
                let inner = &tags[cursor];
                if !inner.closing && !inner.self_closing {
                    inner_depth += 1;
                    if inner_depth == run_depth + 2 {
                        match inner.name.as_str() {
                            // The WHOLE element, tags included.
                            "rPr" => rpr = Some((inner.start, usize::MAX)),
                            "t" => text = Some((inner.end, usize::MAX)),
                            other => structured = Some(other.to_string()),
                        }
                    }
                } else if inner.self_closing {
                    if inner_depth == run_depth + 1 {
                        structured = Some(inner.name.clone());
                    }
                } else {
                    if inner_depth == run_depth + 2 {
                        let name = inner.name.as_str();
                        if name == "rPr" {
                            if let Some(entry) = rpr.as_mut() {
                                entry.1 = inner.end;
                            }
                        }
                        if name == "t" {
                            if let Some(entry) = text.as_mut() {
                                entry.1 = inner.start;
                            }
                        }
                        if !matches!(name, "rPr" | "t") {
                            structured = Some(name.to_string());
                        }
                    }
                    inner_depth -= 1;
                    if inner_depth == run_depth {
                        run_end = inner.end;
                    }
                }
                cursor += 1;
            }
            let decoded = text
                .map(|(from, to)| decode_text(&paragraph[from..to]))
                .unwrap_or_default();
            runs.push(RunSpan {
                start: base + tag.start,
                end: base + run_end,
                open_tag: paragraph[tag.start..tag.end].to_vec(),
                rpr: rpr
                    .map(|(from, to)| paragraph[from..to].to_vec())
                    .unwrap_or_default(),
                text: decoded,
                has_text: text.is_some(),
                nested,
                structured,
            });
            index = cursor;
            continue;
        }
        if !tag.self_closing {
            depth += 1;
        }
        index += 1;
    }
    if runs.is_empty() {
        return Err(FormatError {
            code: "format-paragraph-has-no-runs",
            detail: "the paragraph holds no runs".to_string(),
        });
    }
    Ok(runs)
}

/// Serialise a plan for the sidecar (indent-2, trailing newline, like the rest).
pub fn to_json(plan: &FormatPlan) -> String {
    let mut json = serde_json::to_string_pretty(plan).expect("plan serialises");
    json.push('\n');
    json
}
