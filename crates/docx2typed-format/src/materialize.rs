//! From a plan to bytes: the run structure a plan describes, written into a part.
//!
//! This is the serialisation half of the engine, and it is deliberately separate
//! from planning: "what the formatting should be" is decided in [`crate::plan`],
//! "how that becomes runs" is decided here. A caller that reads a plan learns
//! everything about the outcome without reading XML; a caller that applies one
//! does not re-decide anything.
//!
//! What it refuses, rather than guessing:
//!
//! - a paragraph whose runs are not contiguous, top-level children of `w:p`
//!   (runs inside a hyperlink, a `w:ins`, a smart tag — replacing them wholesale
//!   would move nodes this edit was never about);
//! - a run carrying anything besides its properties and one text node (a break, a
//!   drawing, a field — the properties are not the only thing a rewrite would
//!   have to preserve, and v1 does not claim to);
//! - a snapshot whose runs do not match the paragraph's (the plan would be
//!   applied to a different document than the one it was made from).

use crate::xml::{decode_text, encode_text, needs_space_preservation, scan_tags};
use crate::{FormatError, FormatPlan, Snapshot};

/// One run as the part holds it.
#[derive(Clone, Debug)]
struct RunSpan {
    /// Byte range of the whole `<w:r>…</w:r>`.
    start: usize,
    end: usize,
    /// The open tag's attributes, copied onto every piece of this run.
    open_tag: Vec<u8>,
    /// Raw `w:rPr` bytes, empty when the run carries none.
    rpr: Vec<u8>,
    text: String,
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

    // The snapshot and the part must agree, or the plan is being applied to a
    // document it was not made from. Cheap to check, and the failure it prevents
    // is a silent rewrite of the wrong characters.
    let snapshot_text: String = paragraph_snapshot
        .runs
        .iter()
        .map(|run| run.text.as_str())
        .collect();
    let part_text: String = runs.iter().map(|run| run.text.as_str()).collect();
    if runs.len() != paragraph_snapshot.runs.len() || snapshot_text != part_text {
        return Err(FormatError {
            code: "format-snapshot-stale",
            detail: format!(
                "{} holds {} run(s) reading {:?}, the snapshot has {} reading {:?}; re-read the \
                 document and plan again",
                paragraph_snapshot.paragraph_id,
                runs.len(),
                part_text,
                paragraph_snapshot.runs.len(),
                snapshot_text
            ),
        });
    }

    let text: Vec<char> = part_text.chars().collect();
    let mut rebuilt: Vec<u8> = Vec::new();
    for planned in &paragraph_plan.runs {
        let piece: String = text[planned.range.start..planned.range.end]
            .iter()
            .collect();
        let source = runs
            .iter()
            .zip(paragraph_snapshot.runs.iter())
            .find(|(_, run)| run.index == planned.source_run)
            .map(|(span, _)| span)
            .ok_or_else(|| FormatError {
                code: "format-run-unmapped",
                detail: format!(
                    "the plan names run {} in {}, which the part does not hold",
                    planned.source_run, paragraph_snapshot.paragraph_id
                ),
            })?;
        let rpr: &[u8] = match &planned.style {
            Some(style_id) => snapshot
                .styles
                .get(style_id)
                .ok_or_else(|| FormatError {
                    code: "format-style-unavailable",
                    detail: format!("{style_id} is not in this snapshot's registry"),
                })?
                .as_bytes(),
            // Untouched characters keep the bytes their run had — the whole
            // promise of "non-target properties are unchanged".
            None => &source.rpr,
        };
        rebuilt.extend_from_slice(&source.open_tag);
        rebuilt.extend_from_slice(rpr);
        let body = encode_text(&piece);
        if needs_space_preservation(&piece) {
            rebuilt.extend_from_slice(b"<w:t xml:space=\"preserve\">");
        } else {
            rebuilt.extend_from_slice(b"<w:t>");
        }
        rebuilt.extend_from_slice(body.as_bytes());
        rebuilt.extend_from_slice(b"</w:t></w:r>");
    }

    // Replace exactly the run region: from the first run's open tag to the last
    // run's close tag. Everything the paragraph holds besides its runs — the
    // `w:pPr`, bookmarks, anything after — is untouched.
    let region_start = runs.first().map(|run| run.start).unwrap_or(start);
    let region_end = runs.last().map(|run| run.end).unwrap_or(end);
    if runs.is_empty() {
        return Err(FormatError {
            code: "format-paragraph-has-no-runs",
            detail: format!(
                "{} holds no runs to rewrite",
                paragraph_snapshot.paragraph_id
            ),
        });
    }
    let mut out = Vec::with_capacity(part.len() + rebuilt.len());
    out.extend_from_slice(&part[..region_start]);
    out.extend_from_slice(&rebuilt);
    out.extend_from_slice(&part[region_end..]);
    Ok(out)
}

/// The paragraph's runs, in order, with what a rewrite needs to preserve.
fn runs_of(paragraph: &[u8], base: usize) -> Result<Vec<RunSpan>, FormatError> {
    let tags = scan_tags(paragraph);
    let mut runs: Vec<RunSpan> = Vec::new();
    let mut index = 0usize;
    while index < tags.len() {
        let tag = &tags[index];
        if tag.name != "r" || tag.closing || tag.self_closing {
            index += 1;
            continue;
        }
        // The matching close tag, at the same depth.
        let mut depth = 1usize;
        let mut cursor = index + 1;
        let mut rpr: Option<(usize, usize)> = None;
        let mut text: Option<(usize, usize)> = None;
        let mut children = 0usize;
        while cursor < tags.len() && depth > 0 {
            let inner = &tags[cursor];
            if !inner.closing && !inner.self_closing {
                depth += 1;
                if depth == 2 {
                    children += 1;
                    match inner.name.as_str() {
                        // The WHOLE element, tags included: a run's properties
                        // are `<w:rPr>…</w:rPr>`, and copying only its children
                        // produces a run whose properties are bare children of
                        // `w:r` — which the next pass then refuses as structure.
                        "rPr" => rpr = Some((inner.start, usize::MAX)),
                        "t" => text = Some((inner.end, usize::MAX)),
                        _ => {
                            return Err(FormatError {
                                code: "format-run-has-structure",
                                detail: format!(
                                    "a run holds <{}> besides its properties and text; rewriting \
                                     it would have to preserve more than properties, which v1 \
                                     does not claim to do. Split the paragraph, or format a run \
                                     that carries only text",
                                    inner.name
                                ),
                            })
                        }
                    }
                }
            } else if inner.self_closing && depth == 1 {
                // A self-closing child of the run: a break, a tab, a drawing.
                // It is a child like any other, and `w:rPr` and `w:t` are never
                // self-closing — so anything here is structure this rewrite would
                // have to preserve and does not.
                return Err(FormatError {
                    code: "format-run-has-structure",
                    detail: format!(
                        "a run holds <{}> besides its properties and text; rewriting it would \
                         have to preserve more than properties, which v1 does not claim to do",
                        inner.name
                    ),
                });
            } else if inner.closing {
                if depth == 2 {
                    let name = inner.name.as_str();
                    // The properties are the whole element, closing tag included;
                    // the text is what lies BETWEEN its tags. Copying the wrong
                    // one of those produces a run whose `</w:rPr>` is missing,
                    // which is not a formatting bug but malformed XML.
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
                        return Err(FormatError {
                            code: "format-run-has-structure",
                            detail: format!(
                                "a run holds </{name}> besides its properties and text"
                            ),
                        });
                    }
                }
                depth -= 1;
                if depth == 0 {
                    let run_end = inner.end;
                    let text_range = text.ok_or_else(|| FormatError {
                        code: "format-run-has-no-text",
                        detail: "a run holds no text node".to_string(),
                    })?;
                    let decoded = decode_text(&paragraph[text_range.0..text_range.1]);
                    runs.push(RunSpan {
                        start: base + tag.start,
                        end: base + run_end,
                        open_tag: paragraph[tag.start..tag.end].to_vec(),
                        rpr: rpr
                            .map(|(from, to)| paragraph[from..to].to_vec())
                            .unwrap_or_default(),
                        text: decoded,
                    });
                    index = cursor;
                    break;
                }
            }
            cursor += 1;
        }
        if children == 0 && depth == 1 {
            // A self-closing or empty run: nothing to rewrite.
            return Err(FormatError {
                code: "format-run-has-no-text",
                detail: "a run holds no text node".to_string(),
            });
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
