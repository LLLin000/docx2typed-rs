//! Independent verifier (issue #55 slice, extended for issue #58 island
//! edits): read-only, verifies a built output package against the
//! workdir's immutable template package. The request carries only
//! immutable byte sources (workdir + output paths) plus a profile; the
//! verifier never receives Core AST, build plans, or mutation state, and
//! never writes. It implements its own package walking, hashing, and leaf
//! re-walk (zip + sha2 + its own byte scanner) rather than calling Core —
//! per issue #36.
//!
//! No-op verification (no `islands.json` sidecar): every output part must
//! be byte-identical (SHA-256) to the template part, and the whole output
//! file must equal the template file (copy-if-unchanged contract).
//!
//! Edited verification (islands.json present): every part must be
//! byte-identical to the template EXCEPT the edited parts; each edited part
//! must equal the template part with exactly the recorded leaf text
//! replacements applied (independently recomputed from the template bytes);
//! the edited leaves must be editable (never inside an opaque
//! field/math/drawing interior); and every opaque block's bytes must be
//! replayed at their shifted positions.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use docx2typed_protocol::{bytes_sha256, file_sha256};
use zip::ZipArchive;

#[derive(Clone, Debug)]
pub struct VerificationRequest {
    /// Immutable input: the typed workdir whose `_template.docx` is the
    /// authoritative package baseline.
    pub workdir: PathBuf,
    /// Immutable input: the built output package under verification.
    pub output: PathBuf,
    /// Check profile name (S / L / X); the slice records it in evidence.
    pub profile: String,
}

#[derive(Clone, Debug)]
pub struct VerificationCheck {
    pub name: String,
    pub status: String, // pass | fail | not-applicable
    #[allow(dead_code)]
    pub detail: Option<String>,
}

/// Canonical evidence payload produced by the verifier. The App engine wraps
/// this into a `docx2typed-run-evidence-1` record.
#[derive(Clone, Debug)]
pub struct VerificationEvidence {
    pub verdict: String, // pass | fail
    pub checks: Vec<VerificationCheck>,
    pub output_sha256: String,
    pub template_sha256: String,
    pub parts_identical: bool,
    pub profile: String,
}

pub struct IndependentVerifier;

impl Default for IndependentVerifier {
    fn default() -> Self {
        Self::new()
    }
}

impl IndependentVerifier {
    pub fn new() -> Self {
        IndependentVerifier
    }

    pub fn verify(&self, request: &VerificationRequest) -> VerificationEvidence {
        let mut checks: Vec<VerificationCheck> = Vec::new();
        let template = request.workdir.join("_template.docx");
        let template_sha256 = file_sha256(&template).unwrap_or_default();
        let output_sha256 = file_sha256(&request.output).unwrap_or_default();

        // Check 1: output package opens as a zip.
        let output_parts = match package_parts(&request.output) {
            Some(parts) => {
                checks.push(VerificationCheck {
                    name: "package-openable".to_string(),
                    status: "pass".to_string(),
                    detail: Some(format!("{} parts", parts.len())),
                });
                parts
            }
            None => {
                checks.push(VerificationCheck {
                    name: "package-openable".to_string(),
                    status: "fail".to_string(),
                    detail: Some("output docx unreadable".to_string()),
                });
                BTreeMap::new()
            }
        };
        // XML gate (issue #70): strict well-formedness of every XML-bearing
        // output member BEFORE any text/fidelity acceptance. The verifier's
        // byte walker is lenient by design, so a duplicated attribute (e.g. a
        // second `xml:space` from a direct edit) is invisible to the text
        // checks yet fatal per XML 1.0 §3.1. Runs in both paths; a fail here
        // fails the verdict via the all-pass rule below.
        match check_output_xml_wellformed(&request.output) {
            Ok(detail) => checks.push(VerificationCheck {
                name: "output-xml-wellformed".to_string(),
                status: "pass".to_string(),
                detail: Some(detail),
            }),
            Err(detail) => checks.push(VerificationCheck {
                name: "output-xml-wellformed".to_string(),
                status: "fail".to_string(),
                detail: Some(detail),
            }),
        }
        // The recorded edits (the only mutation the output may implement).
        // Read independently from the workdir sidecars.
        let islands = load_islands(&request.workdir).unwrap_or_default();
        let structure = load_structure(&request.workdir).unwrap_or_default();
        // Format edits are a third mutation the output may implement. Read here
        // too, or a restyled document fails verification for a change it was
        // told to make.
        let planned = load_planned_runs(&request.workdir);
        let styles = load_style_rpr_bytes(&request.workdir);

        if islands.is_empty() && structure.is_empty() && planned.is_empty() {
            self.verify_noop(
                &template,
                &output_parts,
                &mut checks,
                &template_sha256,
                &output_sha256,
            )
        } else {
            self.verify_edited(
                &template,
                &request.output,
                &output_parts,
                &islands,
                &structure,
                &planned,
                &styles,
                &mut checks,
                &template_sha256,
                &output_sha256,
            )
        }
    }

    /// The frozen no-op contract (issue #55).
    fn verify_noop(
        &self,
        template: &Path,
        output_parts: &BTreeMap<String, String>,
        checks: &mut Vec<VerificationCheck>,
        template_sha256: &str,
        output_sha256: &str,
    ) -> VerificationEvidence {
        // Check 2: output parts byte-identical to the template parts.
        let template_parts = package_parts(template).unwrap_or_default();
        let (parts_identical, detail) = parts_diff(&template_parts, output_parts);
        checks.push(VerificationCheck {
            name: "parts-match-template".to_string(),
            status: if parts_identical { "pass" } else { "fail" }.to_string(),
            detail: Some(detail),
        });

        // Check 3: whole-file byte identity (copy-if-unchanged).
        let whole_identical = !output_sha256.is_empty() && output_sha256 == template_sha256;
        checks.push(VerificationCheck {
            name: "output-identical-to-template".to_string(),
            status: if whole_identical { "pass" } else { "fail" }.to_string(),
            detail: Some(format!(
                "output={} template={}",
                short(output_sha256),
                short(template_sha256)
            )),
        });

        let verdict = if checks.iter().all(|check| check.status == "pass") {
            "pass"
        } else {
            "fail"
        };
        VerificationEvidence {
            verdict: verdict.to_string(),
            checks: checks.clone(),
            output_sha256: output_sha256.to_string(),
            template_sha256: template_sha256.to_string(),
            parts_identical: parts_identical && whole_identical,
            profile: String::new(),
        }
    }

    /// Issue #58 / PR 3 edited-build verification: independent re-walk of
    /// the template bytes, recomputing the expected edited parts (leaf
    /// surgery + structural synthesis) and comparing them byte-for-byte
    /// with the output, plus the opaque replay check.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    fn verify_edited(
        &self,
        template: &Path,
        output: &Path,
        output_parts: &BTreeMap<String, String>,
        islands: &[IslandEdit],
        structure: &StructureEdits,
        planned: &[PlannedRunCheck],
        styles: &BTreeMap<String, String>,
        checks: &mut Vec<VerificationCheck>,
        template_sha256: &str,
        output_sha256: &str,
    ) -> VerificationEvidence {
        // Check 2: every part byte-identical to the template except the
        // edited parts.
        let template_parts = package_parts(template).unwrap_or_default();
        let mut edited_part_paths: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::new();
        for edit in islands {
            edited_part_paths.insert(part_path(&edit.part));
        }
        if !planned.is_empty() {
            // A plan's runs live in the document part.
            edited_part_paths.insert(part_path("document"));
        }
        if !structure.is_empty() {
            edited_part_paths.insert(part_path("document"));
        }
        let mut changed: Vec<String> = Vec::new();
        for (name, hash) in &template_parts {
            match output_parts.get(name) {
                Some(output_hash) if output_hash == hash => {}
                Some(_) if edited_part_paths.contains(name) => changed.push(name.clone()),
                Some(_) => changed.push(name.clone()),
                None => changed.push(name.clone()),
            }
        }
        for name in output_parts.keys() {
            if !template_parts.contains_key(name) {
                changed.push(name.clone());
            }
        }
        let untouched_identical = changed.iter().all(|name| edited_part_paths.contains(name));
        let detail = format!(
            "changed={} edited_parts={}",
            changed.len(),
            edited_part_paths.len()
        );
        checks.push(VerificationCheck {
            name: "parts-match-template-except-edited".to_string(),
            status: if untouched_identical { "pass" } else { "fail" }.to_string(),
            detail: Some(detail),
        });

        // Check 3: direct edits recompute the part byte-exact from the
        // template plus the recorded leaf replacements; tracked edits instead
        // validate the output's own revision markup (del/ins texts plus the
        // THIS-revision author/date agreement) while the protected-structure
        // gate below pins every opaque byte independently. The check name stays
        // `edited-part-exact` for evidence stability, but the success detail
        // must say which proof actually ran — never claim a byte recomputation
        // the tracked branch did not perform.
        let mut edited_exact = true;
        let mut edited_detail = String::new();
        let mut tracked_validated = 0usize;
        let mut recomputed = 0usize;
        let template_package = std::fs::read(template).unwrap_or_default();
        for part_path in &edited_part_paths {
            let this_part: &str = part_path.as_str();
            let Some(part_key) = part_key_from_path(this_part) else {
                edited_exact = false;
                edited_detail = format!("unknown edited part {part_path}");
                break;
            };
            let edits: Vec<&IslandEdit> = islands
                .iter()
                .filter(|edit| edit.part == part_key)
                .collect();
            let template_part = read_member_bytes(&template_package, part_path).unwrap_or_default();
            let output_part =
                read_member_bytes(&std::fs::read(output).unwrap_or_default(), part_path)
                    .unwrap_or_default();
            let structural = part_key == "document" && !structure.is_empty();
            if edits.iter().any(|edit| edit.tracked) && !structural {
                if let Err(message) = verify_tracked_part(&output_part, &edits) {
                    edited_exact = false;
                    edited_detail = format!("part {part_path}: {message}");
                } else {
                    tracked_validated += 1;
                }
            } else if edits.iter().any(|edit| edit.tracked) {
                // PR 3 scope: tracked leaf revisions and paragraph-level
                // structural edits are not combined; fail closed.
                edited_exact = false;
                edited_detail = format!(
                    "part {part_path}: tracked edits cannot be combined with structural edits"
                );
            } else {
                match recompute_edited_part(&template_part, &edits) {
                    Ok(mut expected) => {
                        if structural {
                            match recompute_structure_part(&expected, structure) {
                                Ok(spliced) => expected = spliced,
                                Err(message) => {
                                    edited_exact = false;
                                    edited_detail = format!("part {part_path}: {message}");
                                    continue;
                                }
                            }
                        }
                        // A part a plan touches is compared modulo the runs that
                        // plan names: the plan decides their structure and their
                        // properties, and `format-plan-applied` checks those
                        // against the output on its own. What this comparison
                        // keeps proving is that nothing ELSE moved.
                        let part_planned: Vec<&PlannedRunCheck> =
                            if this_part == verify_part_path("document") {
                                planned.iter().collect()
                            } else {
                                Vec::new()
                            };
                        let (expected_cmp, output_cmp) = if part_planned.is_empty() {
                            (expected.clone(), output_part.clone())
                        } else {
                            match (
                                strip_planned_runs(&expected, part_key.as_str(), &part_planned),
                                strip_planned_runs(&output_part, part_key.as_str(), &part_planned),
                            ) {
                                (Ok(left), Ok(right)) => (left, right),
                                (Err(message), _) | (_, Err(message)) => {
                                    edited_exact = false;
                                    edited_detail = format!("part {part_path}: {message}");
                                    continue;
                                }
                            }
                        };
                        if expected_cmp != output_cmp {
                            edited_exact = false;
                            edited_detail = format!(
                                "part {part_path} differs from the independently recomputed edit"
                            );
                        } else {
                            recomputed += 1;
                        }
                    }
                    Err(message) => {
                        edited_exact = false;
                        edited_detail = format!("part {part_path}: {message}");
                    }
                }
            }
        }
        checks.push(VerificationCheck {
            name: "edited-part-exact".to_string(),
            status: if edited_exact { "pass" } else { "fail" }.to_string(),
            detail: Some(if edited_detail.is_empty() {
                format!(
                    "{} of {} edited parts validated ({recomputed} recomputed byte-exact, \
                     {tracked_validated} tracked revision-markup validated, \
                     opaque bytes pinned by protected-structure-preserved)",
                    recomputed + tracked_validated,
                    edited_part_paths.len()
                )
            } else {
                edited_detail
            }),
        });

        // Check 3b: the plan, checked against the output — contract conformance
        // only. The plan's rules are not re-derived here (ADR 0050); what is
        // checked is that the artifact is what the approved plan describes, and
        // that a run the plan does not restyle kept the properties of the run it
        // came from.
        let mut restyle_detail = String::new();
        let mut restyle_ok = true;
        for edit in planned {
            let path = verify_part_path("document");
            let Some(part_key) = part_key_from_path(&path) else {
                restyle_ok = false;
                restyle_detail = format!("unknown part {path}");
                break;
            };
            let expected_rpr = match &edit.style {
                Some(style_id) => match styles.get(style_id) {
                    Some(rpr) => rpr.clone(),
                    None => {
                        restyle_ok = false;
                        restyle_detail = format!(
                            "{} names {} which styles.json does not hold",
                            path, style_id
                        );
                        break;
                    }
                },
                None => String::new(),
            };
            let template_package = std::fs::read(template).unwrap_or_default();
            let output_package = std::fs::read(output).unwrap_or_default();
            let template_part = read_member_bytes(&template_package, &path).unwrap_or_default();
            let output_part = read_member_bytes(&output_package, &path).unwrap_or_default();
            // The span was recorded against the DRAFT — the template with the
            // text islands applied — so that is the baseline its offsets address.
            // Measured: a text edit followed by a restyle in the same paragraph
            // made this check resolve the span against the template and refuse a
            // correct output.
            let part_islands: Vec<&IslandEdit> = islands
                .iter()
                .filter(|island| island.part == part_key)
                .collect();
            let baseline = if part_islands.is_empty() {
                Ok(template_part.clone())
            } else {
                recompute_edited_part(&template_part, &part_islands)
            };
            let baseline = match baseline {
                Ok(baseline) => baseline,
                Err(message) => {
                    restyle_ok = false;
                    restyle_detail = format!("part {path}: {message}");
                    break;
                }
            };
            if let Err(message) = verify_planned_run(
                &baseline,
                &output_part,
                part_key.as_str(),
                edit,
                &expected_rpr,
            ) {
                restyle_ok = false;
                restyle_detail = format!("part {path}: {message}");
                break;
            }
        }
        checks.push(VerificationCheck {
            name: "format-plan-applied".to_string(),
            status: if restyle_ok { "pass" } else { "fail" }.to_string(),
            detail: Some(if restyle_detail.is_empty() {
                format!("{} planned run(s) matched the plan", planned.len())
            } else {
                restyle_detail
            }),
        });

        // Check 4: opaque interiors replayed — every opaque block of the
        // template part appears verbatim in the output at its shifted
        // position (the edit never touches locked bytes).
        let mut opaque_locked = true;
        let mut opaque_detail = String::new();
        let mut untouched_preserved = true;
        let mut untouched_detail = String::new();
        for part_path in &edited_part_paths {
            let this_part: &str = part_path.as_str();
            let Some(part_key) = part_key_from_path(this_part) else {
                continue;
            };
            let edits: Vec<&IslandEdit> = islands
                .iter()
                .filter(|edit| edit.part == part_key)
                .collect();
            let template_part = read_member_bytes(&template_package, part_path).unwrap_or_default();
            let output_part =
                read_member_bytes(&std::fs::read(output).unwrap_or_default(), part_path)
                    .unwrap_or_default();
            // The protected structure must survive, compared by IDENTITY rather
            // than by position. A tracked edit re-wraps every run it touches, so
            // a byte offset after it moves by an amount that depends on the
            // WRAPPING; estimating that from the replacement's length is wrong by
            // construction, which is what this check used to do and why it
            // rejected correct output.
            let mut targeted: Vec<String> =
                edits.iter().map(|edit| edit.paragraph_id.clone()).collect();
            // Synthesised paragraphs have no baseline to compare against, and
            // the engine marks them with an `N` id; deleted ones legitimately
            // take their protected nodes with them.
            if part_key == "document" {
                targeted.extend(structure.deleted.iter().cloned());
            }
            // With structural edits the baseline for the comparison is the part
            // with BOTH the island and the structural edits applied, not the raw
            // template. Inserting a paragraph renumbers the ones after it —
            // measured: the same protected node reads as `P1` before and `P2`
            // after — so comparing against the template would call every shifted
            // paragraph changed. This is the base the byte-range check used, kept
            // for the same reason.
            let baseline = if part_key == "document" && !structure.is_empty() {
                recompute_edited_part(&template_part, &edits).and_then(|applied| {
                    let spans = structure_spans(&applied, structure)?;
                    apply_spans(&applied, &spans)
                })
            } else {
                Ok(template_part.clone())
            };
            // The edits' byte spans, in the baseline's coordinates: they decide
            // which opaque interiors are containers of an edit rather than
            // locked bytes beside it.
            let spans: Vec<(usize, usize)> = {
                let mut collected = Vec::new();
                for edit in &edits {
                    if let Ok(ranges) = edit_run_ranges(&template_part, &part_key, edit) {
                        collected.extend(ranges);
                    }
                }
                // A restyled run is a target too: its properties are the point
                // of the edit, so a check that compares "everything outside the
                // edit" has to know where the edit is. Measured: without this the
                // verifier refused its own restyle as `non-target-preserved`.
                if this_part == verify_part_path("document") {
                    for edit in planned {
                        if let Ok(ranges) = planned_run_ranges(&template_part, &part_key, edit) {
                            collected
                                .extend(ranges.into_iter().map(|(start, end, _)| (start, end)));
                        }
                    }
                }
                collected
            };
            let replay = baseline.and_then(|baseline| {
                check_protected_structure(&baseline, &output_part, &part_key, &targeted, &spans)
            });
            match replay {
                Ok(()) => {}
                Err(message) => {
                    opaque_locked = false;
                    opaque_detail = format!("part {part_path}: {message}");
                    break;
                }
            }
            // Nothing outside the edit's own paragraphs may change. Same
            // structural baseline: a structural edit renumbers the paragraphs
            // after it, and this compares by id.
            let untouched = if part_key == "document" && !structure.is_empty() {
                recompute_edited_part(&template_part, &edits).and_then(|applied| {
                    let spans = structure_spans(&applied, structure)?;
                    apply_spans(&applied, &spans)
                })
            } else {
                Ok(template_part.clone())
            };
            let untouched = untouched.and_then(|baseline| {
                check_non_target_preserved(&baseline, &output_part, &part_key, &spans)
            });
            match untouched {
                Ok(()) => {}
                Err(message) => {
                    untouched_preserved = false;
                    untouched_detail = format!("part {part_path}: {message}");
                    break;
                }
            }
        }
        checks.push(VerificationCheck {
            name: "protected-structure-preserved".to_string(),
            status: if opaque_locked { "pass" } else { "fail" }.to_string(),
            detail: Some(if opaque_detail.is_empty() {
                "anchors, hyperlink targets and locked interiors all survived the edit".to_string()
            } else {
                opaque_detail
            }),
        });

        checks.push(VerificationCheck {
            name: "non-target-preserved".to_string(),
            status: if untouched_preserved { "pass" } else { "fail" }.to_string(),
            detail: Some(if untouched_detail.is_empty() {
                "text and run properties outside the edited paragraphs are unchanged".to_string()
            } else {
                untouched_detail
            }),
        });

        // Check 5: edited builds are never whole-file identical to the
        // template (their parts differ by design).
        checks.push(VerificationCheck {
            name: "edited-package-distinct".to_string(),
            status: if output_sha256 != template_sha256 {
                "pass"
            } else {
                "fail"
            }
            .to_string(),
            detail: Some(format!(
                "output={} template={}",
                short(output_sha256),
                short(template_sha256)
            )),
        });

        let verdict = if checks.iter().all(|check| check.status == "pass") {
            "pass"
        } else {
            "fail"
        };
        VerificationEvidence {
            verdict: verdict.to_string(),
            checks: checks.clone(),
            output_sha256: output_sha256.to_string(),
            template_sha256: template_sha256.to_string(),
            parts_identical: untouched_identical && edited_exact && opaque_locked,
            profile: String::new(),
        }
    }
}

fn parts_diff(
    template: &BTreeMap<String, String>,
    output: &BTreeMap<String, String>,
) -> (bool, String) {
    let mut changed: Vec<String> = Vec::new();
    let mut added: Vec<String> = Vec::new();
    let mut removed: Vec<String> = Vec::new();
    for (name, hash) in template {
        match output.get(name) {
            Some(output_hash) if output_hash == hash => {}
            Some(_) => changed.push(name.clone()),
            None => removed.push(name.clone()),
        }
    }
    for name in output.keys() {
        if !template.contains_key(name) {
            added.push(name.clone());
        }
    }
    (
        changed.is_empty() && added.is_empty() && removed.is_empty(),
        format!(
            "changed={} added={} removed={}",
            changed.len(),
            added.len(),
            removed.len()
        ),
    )
}

fn package_parts(path: &Path) -> Option<BTreeMap<String, String>> {
    let file = File::open(path).ok()?;
    let mut archive = ZipArchive::new(file).ok()?;
    let mut parts = BTreeMap::new();
    for index in 0..archive.len() {
        let mut member = archive.by_index(index).ok()?;
        let name = member.name().to_string();
        let mut buf = Vec::new();
        member.read_to_end(&mut buf).ok()?;
        parts.insert(name, bytes_sha256(&buf));
    }
    Some(parts)
}

fn read_member_bytes(package: &[u8], member: &str) -> Option<Vec<u8>> {
    let file = std::io::Cursor::new(package.to_vec());
    let mut archive = ZipArchive::new(file).ok()?;
    for index in 0..archive.len() {
        let mut part = archive.by_index(index).ok()?;
        if part.name() == member {
            let mut bytes = Vec::new();
            part.read_to_end(&mut bytes).ok()?;
            return Some(bytes);
        }
    }
    None
}
/// Strict XML well-formedness gate (issue #70): every XML-bearing output
/// member must parse as XML BEFORE any text/fidelity check runs. The
/// verifier's own byte walker is lenient by design, so a duplicated
/// attribute (e.g. a second `xml:space` injected by a direct edit onto a
/// `w:t` that already carries one) is invisible to the text gates yet fatal
/// per XML 1.0 §3.1 — and silently shipping it corrupts the published
/// output. `roxmltree` (already in the tree via core) is the strict parser:
/// duplicate attributes, unclosed tags and bad entities all fail the parse.
/// `Err` names the offending member deterministically.
fn check_output_xml_wellformed(output: &Path) -> Result<String, String> {
    let file = File::open(output).map_err(|_| "output docx unreadable".to_string())?;
    let mut archive = ZipArchive::new(file).map_err(|_| "output docx unreadable".to_string())?;
    let mut checked = 0usize;
    for index in 0..archive.len() {
        let mut member = archive
            .by_index(index)
            .map_err(|_| "output docx unreadable".to_string())?;
        let name = member.name().to_string();
        if !(name.ends_with(".xml") || name.ends_with(".rels")) {
            continue;
        }
        let mut bytes = Vec::new();
        member
            .read_to_end(&mut bytes)
            .map_err(|_| format!("member {name}: unreadable"))?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| format!("member {name}: part XML is not UTF-8"))?;
        roxmltree::Document::parse(text)
            .map_err(|error| format!("member {name}: not well-formed XML ({error})"))?;
        checked += 1;
    }
    Ok(format!("{checked} xml members well-formed"))
}

fn short(hash: &str) -> String {
    if hash.len() >= 12 {
        hash[..12].to_string()
    } else {
        hash.to_string()
    }
}

// ---------------------------------------------------------------------------
// Independent island re-walk (issue #58): the verifier's own byte scanner,
// leaf locator, and edit recomputation — never calls Core.
// ---------------------------------------------------------------------------

/// One recorded island edit (independent sidecar parse).
#[derive(Clone, Debug)]
pub struct IslandEdit {
    pub part: String,
    pub paragraph_id: String,
    pub leaf_index: usize,
    /// The paragraph offset `old` begins at, when the edit came from a handle.
    /// Part of the edit's claim like everything else here, so the verifier has
    /// to see it — and read it, because a merged island's text can occur twice
    /// in its leaf and a text-only search would refuse it.
    pub char_start: Option<usize>,
    pub old: String,
    pub new: String,
    pub tracked: bool,
    /// Who the revision says made it, and when. Part of the edit's claim, so the
    /// verifier has to see them to check them.
    pub author: String,
    pub date: String,
}

const ISLANDS_SCHEMA: &str = "docx2typed-islands-1";

fn load_islands(workdir: &Path) -> Option<Vec<IslandEdit>> {
    let path = workdir.join("islands.json");
    if !path.is_file() {
        return Some(Vec::new());
    }
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    if value.get("schema").and_then(|v| v.as_str()) != Some(ISLANDS_SCHEMA) {
        return None;
    }
    let mut out = Vec::new();
    for edit in value.get("edits")?.as_array()? {
        let part = edit.get("part")?.as_str()?.to_string();
        let paragraph_id = edit.get("paragraph_id")?.as_str()?.to_string();
        let leaf_index = edit.get("leaf_index")?.as_u64()? as usize;
        let char_start = edit
            .get("char_start")
            .and_then(serde_json::Value::as_u64)
            .map(|v| v as usize);
        let old = edit.get("old")?.as_str()?.to_string();
        let new = edit.get("new")?.as_str()?.to_string();
        let tracked = edit
            .get("tracked")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        if !old.is_empty() {
            out.push(IslandEdit {
                part,
                paragraph_id,
                leaf_index,
                char_start,
                old,
                new,
                tracked,
                author: edit
                    .get("author")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                date: edit
                    .get("date")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            });
        }
    }
    Some(out)
}

fn part_path(part_key: &str) -> String {
    if part_key == "document" {
        "word/document.xml".to_string()
    } else {
        format!("word/{part_key}.xml")
    }
}

// ---------------------------------------------------------------------------
// Independent structural re-walk (PR 3): new / deleted paragraphs
// ---------------------------------------------------------------------------

/// The structural-edit sidecar, parsed independently (never via Core).
#[derive(Clone, Debug, Default)]
struct StructureEdits {
    new_paragraphs: Vec<NewParagraph>,
    deleted: Vec<String>,
}

impl StructureEdits {
    fn is_empty(&self) -> bool {
        self.new_paragraphs.is_empty() && self.deleted.is_empty()
    }
}

/// One recorded synthesised paragraph.
#[derive(Clone, Debug, Default)]
pub struct NewParagraph {
    /// The id the structure gave it — the same id an `insert_before` anchor may
    /// name, which is how two paragraphs sent after the same one chain.
    pub id: String,
    pub inherit: String,
    pub body: String,
    pub insert_before: Option<String>,
    pub format_policy: FormatPolicy,
}

/// How a synthesised paragraph takes formatting from its anchor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FormatPolicy {
    #[default]
    SafeInherit,
    None,
}

const STRUCTURE_SCHEMA: &str = "docx2typed-structure-1";

fn load_structure(workdir: &Path) -> Option<StructureEdits> {
    let path = workdir.join("structure.json");
    if !path.is_file() {
        return Some(StructureEdits::default());
    }
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    if value.get("schema").and_then(|v| v.as_str()) != Some(STRUCTURE_SCHEMA) {
        return None;
    }
    let mut structure = StructureEdits::default();
    for entry in value.get("new_paragraphs")?.as_array()? {
        if entry.get("part").and_then(|v| v.as_str()) != Some("document") {
            return None;
        }
        let format_policy = entry
            .get("format_policy")
            .and_then(|value| value.as_str())
            .map(|value| match value {
                "none" => FormatPolicy::None,
                _ => FormatPolicy::SafeInherit,
            })
            .unwrap_or_default();
        structure.new_paragraphs.push(NewParagraph {
            id: entry.get("paragraph_id")?.as_str()?.to_string(),
            inherit: entry.get("inherit")?.as_str()?.to_string(),
            body: entry.get("body")?.as_str()?.to_string(),
            insert_before: entry
                .get("insert_before")
                .and_then(|value| value.as_str())
                .map(str::to_string),
            format_policy,
        });
    }
    for entry in value.get("deleted_paragraphs")?.as_array()? {
        if entry.get("part").and_then(|v| v.as_str()) != Some("document") {
            return None;
        }
        structure
            .deleted
            .push(entry.get("paragraph_id")?.as_str()?.to_string());
    }
    Some(structure)
}

/// The byte offset a new paragraph with no following baseline paragraph is
/// appended at: the body-level `w:sectPr` start, else before `</w:body>`.
fn body_append_offset(part: &[u8]) -> Result<usize, String> {
    fn find(nodes: &[LightNode], index: usize, name: &str) -> Option<usize> {
        if nodes[index].name == name {
            return Some(index);
        }
        nodes[index]
            .children
            .iter()
            .find_map(|&child| find(nodes, child, name))
    }
    let nodes = build_light_tree(&scan_tags(part))?;
    let body = (0..nodes.len())
        .find_map(|root| find(&nodes, root, "body"))
        .ok_or("no body element")?;
    for &child in &nodes[body].children {
        if nodes[child].name == "sectPr" {
            return Ok(nodes[child].open_start);
        }
    }
    Ok(nodes[body].close_start)
}

/// The run properties of the inherit paragraph's first text run (the run
/// the synthesised paragraph's own run copies).
fn first_run_rpr(paragraph: &[u8], nodes: &[LightNode], root: usize) -> Option<String> {
    fn walk(paragraph: &[u8], nodes: &[LightNode], index: usize) -> Option<String> {
        let node = &nodes[index];
        if node.name == "r" {
            let has_text = node
                .children
                .iter()
                .any(|&child| matches!(nodes[child].name.as_str(), "t" | "delText"));
            if !has_text {
                return None;
            }
            let rpr = node
                .children
                .iter()
                .find(|&&child| nodes[child].name == "rPr")
                .map(|&child| &paragraph[nodes[child].open_start..nodes[child].end])
                .unwrap_or_default();
            return Some(String::from_utf8_lossy(rpr).into_owned());
        }
        node.children
            .iter()
            .find_map(|&child| walk(paragraph, nodes, child))
    }
    walk(paragraph, nodes, root)
}

/// The `w:pPr` children a synthesised paragraph may inherit (mirror of Core).
const SAFE_PPR_CHILDREN: [&str; 27] = [
    "pStyle",
    "keepNext",
    "keepLines",
    "pageBreakBefore",
    "widowControl",
    "numPr",
    "suppressLineNumbers",
    "shd",
    "tabs",
    "suppressAutoHyphens",
    "kinsoku",
    "wordWrap",
    "overflowPunct",
    "topLinePunct",
    "autoSpaceDE",
    "autoSpaceDN",
    "bidi",
    "adjustRightInd",
    "snapToGrid",
    "spacing",
    "ind",
    "contextualSpacing",
    "mirrorIndents",
    "jc",
    "textAlignment",
    "outlineLvl",
    "rPr",
];

const REFUSED_PPR_CHILDREN: [&str; 5] = ["sectPr", "pPrChange", "rPrChange", "divId", "cnfStyle"];

const SAFE_RPR_CHILDREN: [&str; 36] = [
    "rStyle",
    "rFonts",
    "b",
    "bCs",
    "i",
    "iCs",
    "caps",
    "smallCaps",
    "strike",
    "dstrike",
    "outline",
    "shadow",
    "emboss",
    "imprint",
    "noProof",
    "snapToGrid",
    "vanish",
    "webHidden",
    "color",
    "spacing",
    "w",
    "kern",
    "position",
    "sz",
    "szCs",
    "highlight",
    "u",
    "effect",
    "bdr",
    "shd",
    "fitText",
    "vertAlign",
    "rtl",
    "cs",
    "em",
    "lang",
];

const REFUSED_RPR_CHILDREN: [&str; 7] = [
    "rPrChange",
    "ins",
    "del",
    "moveFrom",
    "moveTo",
    "eastAsianLayout",
    "specVanish",
];

pub fn safe_property_block(
    xml: &[u8],
    nodes: &[LightNode],
    node: &LightNode,
    allow: &[&str],
    refuse: &[&str],
    what: &str,
) -> Result<Vec<u8>, String> {
    let mut out = xml[node.open_start..node.open_end].to_vec();
    for &child in &node.children {
        let child_node = &nodes[child];
        let name = child_node.name.as_str();
        if refuse.contains(&name) {
            continue;
        }
        if name == "rPr" {
            let inner_xml = &xml[child_node.open_start..child_node.end];
            let inner_nodes = build_light_tree(&scan_tags(inner_xml))?;
            if let Some(inner_root) = inner_nodes.first() {
                let filtered = safe_property_block(
                    inner_xml,
                    &inner_nodes,
                    inner_root,
                    &SAFE_RPR_CHILDREN,
                    &REFUSED_RPR_CHILDREN,
                    what,
                )?;
                out.extend_from_slice(&filtered);
            }
            continue;
        }
        if name == "numPr" {
            let mut inner = xml[child_node.open_start..child_node.open_end].to_vec();
            for &grandchild in &child_node.children {
                let g = &nodes[grandchild];
                match g.name.as_str() {
                    "ilvl" | "numId" => inner.extend_from_slice(&xml[g.open_start..g.end]),
                    "ins" | "del" | "moveFrom" | "moveTo" | "numPrChange" => {}
                    other => {
                        return Err(format!(
                            "invalid-structure: {what} carries `numPr/{other}`, which this build does not know how \
                             to inherit safely; nothing was written"
                        ))
                    }
                }
            }
            if child_node.end != child_node.open_end {
                inner.extend_from_slice(&xml[child_node.close_start..child_node.end]);
            }
            out.extend_from_slice(&inner);
            continue;
        }
        if !allow.contains(&name) {
            return Err(format!(
                "invalid-structure: {what} carries `{name}`, which this build does not know how to inherit safely; \
                 nothing was written"
            ));
        }
        out.extend_from_slice(&xml[child_node.open_start..child_node.end]);
    }
    if node.end != node.open_end {
        out.extend_from_slice(&xml[node.close_start..node.end]);
    }
    Ok(out)
}

/// Render one synthesised `w:p` from the inherit paragraph (independent port
/// of the writer: opening tag with ephemeral attributes stripped, `pPr`
/// verbatim, one run carrying the inherited run properties).
fn render_new_paragraph(
    part: &[u8],
    located: &[(String, usize, usize)],
    paragraph: &NewParagraph,
) -> Result<Vec<u8>, String> {
    let (start, end) = located
        .iter()
        .find(|(id, _, _)| id == &paragraph.inherit)
        .map(|(_, start, end)| (*start, *end))
        .ok_or_else(|| format!("inherit paragraph not found: {}", paragraph.inherit))?;
    let inherit = &part[start..end];
    let nodes = build_light_tree(&scan_tags(inherit))?;
    let root = nodes
        .iter()
        .position(|node| node.parent.is_none())
        .ok_or("no paragraph root")?;
    let mut opening = inherit[..nodes[root].open_end].to_vec();
    if nodes[root].end == nodes[root].open_end {
        // `<w:p .../>` cannot be reopened: drop the `/`.
        opening.pop();
    }
    let (ppr, rpr) = if paragraph.format_policy == FormatPolicy::None {
        (Vec::new(), Vec::new())
    } else {
        let what = format!("paragraph {}'s properties", paragraph.inherit);
        let ppr = match nodes[root]
            .children
            .iter()
            .find(|&&child| nodes[child].name == "pPr")
        {
            Some(&child) => safe_property_block(
                inherit,
                &nodes,
                &nodes[child],
                &SAFE_PPR_CHILDREN,
                &REFUSED_PPR_CHILDREN,
                &what,
            )?,
            None => Vec::new(),
        };
        let rpr_raw = first_run_rpr(inherit, &nodes, root).unwrap_or_default();
        let rpr = if rpr_raw.is_empty() {
            Vec::new()
        } else {
            let rnodes = build_light_tree(&scan_tags(rpr_raw.as_bytes()))?;
            match rnodes.first() {
                Some(node) => safe_property_block(
                    rpr_raw.as_bytes(),
                    &rnodes,
                    node,
                    &SAFE_RPR_CHILDREN,
                    &REFUSED_RPR_CHILDREN,
                    &what,
                )?,
                None => Vec::new(),
            }
        };
        (ppr, rpr)
    };
    let mut out = strip_ephemeral_p_attrs(&opening);
    out.extend_from_slice(&ppr);
    if !paragraph.body.is_empty() {
        out.extend_from_slice(b"<w:r>");
        out.extend_from_slice(&rpr);
        let space = if needs_xml_space(&paragraph.body) {
            " xml:space=\"preserve\""
        } else {
            ""
        };
        out.extend_from_slice(format!("<w:t{space}>").as_bytes());
        out.extend_from_slice(xml_escape(&paragraph.body).as_bytes());
        out.extend_from_slice(b"</w:t></w:r>");
    }
    out.extend_from_slice(b"</w:p>");
    Ok(out)
}

fn attr_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-')
}

/// Match one `\s+name\s*=\s*"value"` attribute at `start` (unquoted values
/// do not match).
fn match_p_attr(bytes: &[u8], start: usize) -> Option<(usize, String)> {
    let mut index = start;
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    if index == start {
        return None;
    }
    let name_start = index;
    if !matches!(bytes.get(index), Some(byte) if byte.is_ascii_alphabetic() || *byte == b'_') {
        return None;
    }
    index += 1;
    while index < bytes.len() && attr_name_byte(bytes[index]) {
        index += 1;
    }
    if bytes.get(index) == Some(&b':') {
        index += 1;
        if !matches!(bytes.get(index), Some(byte) if byte.is_ascii_alphabetic() || *byte == b'_') {
            return None;
        }
        index += 1;
        while index < bytes.len() && attr_name_byte(bytes[index]) {
            index += 1;
        }
    }
    let name = String::from_utf8_lossy(&bytes[name_start..index]).into_owned();
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    if bytes.get(index) != Some(&b'=') {
        return None;
    }
    index += 1;
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    let quote = *bytes.get(index)?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    index += 1;
    while index < bytes.len() && bytes[index] != quote {
        index += 1;
    }
    if bytes.get(index) != Some(&quote) {
        return None;
    }
    index += 1;
    Some((index, name))
}

/// Ephemeral paragraph opening attributes (never inherited).
const EPHEMERAL_P_ATTRS: [&str; 7] = [
    "paraId",
    "textId",
    "rsidP",
    "rsidR",
    "rsidRDefault",
    "rsidDel",
    "rsidRPr",
];

/// Copy one paragraph opening tag, dropping the ephemeral attributes.
fn strip_ephemeral_p_attrs(opening: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(opening.len());
    let mut index = 0usize;
    while index < opening.len() {
        if opening[index].is_ascii_whitespace() {
            if let Some((end, name)) = match_p_attr(opening, index) {
                let local = name.rsplit(':').next().unwrap_or(&name);
                if EPHEMERAL_P_ATTRS.contains(&local) {
                    index = end;
                    continue;
                }
                out.extend_from_slice(&opening[index..end]);
                index = end;
                continue;
            }
        }
        out.push(opening[index]);
        index += 1;
    }
    out
}

/// The byte spans one structural edit set produces in `part`: deletions
/// (start, end, empty) and synthesised insertions (at, at, rendered).
fn structure_spans(
    part: &[u8],
    structure: &StructureEdits,
) -> Result<Vec<(usize, usize, Vec<u8>)>, String> {
    let located = locate_paragraphs(part, "document")?;
    let index_of: BTreeMap<&str, usize> = located
        .iter()
        .enumerate()
        .map(|(index, (id, _, _))| (id.as_str(), index))
        .collect();
    let deleted: std::collections::BTreeSet<&str> =
        structure.deleted.iter().map(String::as_str).collect();
    let append_at = body_append_offset(part)?;
    let mut spans: Vec<(usize, usize, Vec<u8>)> = Vec::new();
    for id in &structure.deleted {
        let index = *index_of
            .get(id.as_str())
            .ok_or_else(|| format!("deleted paragraph not found: {id}"))?;
        let (_, start, end) = &located[index];
        spans.push((*start, *end, Vec::new()));
    }
    // Where each synthesised paragraph goes. The anchor may be a paragraph of the
    // template, one a delete is removing (fall forward), or one this structure is
    // about to insert itself — two paragraphs sent "after the same one" chain,
    // and the chain resolves to where the anchor goes. A chain that leads back to
    // itself is refused: resolving it is a hang, not an answer.
    let anchor_offset = |anchor: &str| -> Result<usize, String> {
        let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut current = anchor.to_string();
        loop {
            if !seen.insert(current.clone()) {
                return Err(format!(
                    "insert_before paragraphs form a cycle at {current}"
                ));
            }
            if let Some(mut index) = index_of.get(current.as_str()).copied() {
                loop {
                    let (id, start, _) = &located[index];
                    if !deleted.contains(id.as_str()) {
                        return Ok(*start);
                    }
                    index += 1;
                    if index >= located.len() {
                        return Ok(append_at);
                    }
                }
            }
            match structure
                .new_paragraphs
                .iter()
                .find(|paragraph| paragraph.id == current)
            {
                Some(pending) => match &pending.insert_before {
                    Some(next) => current = next.clone(),
                    None => return Ok(append_at),
                },
                None => return Err(format!("unknown insert_before paragraph: {current}")),
            }
        }
    };
    // Reversed: the sort below is stable, and paragraphs landing at the same
    // offset belong in reverse order of recording — the later one was sent "after
    // the same paragraph" and the draft already showed the earlier one there.
    for paragraph in structure.new_paragraphs.iter().rev() {
        let at = match &paragraph.insert_before {
            None => append_at,
            Some(anchor) => anchor_offset(anchor)?,
        };
        spans.push((at, at, render_new_paragraph(part, &located, paragraph)?));
    }
    spans.sort_by_key(|(start, end, _)| (*start, *end));
    Ok(spans)
}

/// Apply a sorted, non-overlapping span list to one part.
fn apply_spans(part: &[u8], spans: &[(usize, usize, Vec<u8>)]) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(part.len());
    let mut cursor = 0usize;
    for (start, end, replacement) in spans {
        if *start < cursor || end < start {
            return Err("overlapping paragraph splices".to_string());
        }
        out.extend_from_slice(&part[cursor..*start]);
        out.extend_from_slice(replacement);
        cursor = *end;
    }
    out.extend_from_slice(&part[cursor..]);
    Ok(out)
}

/// Independently recompute the structurally edited document part.
fn recompute_structure_part(part: &[u8], structure: &StructureEdits) -> Result<Vec<u8>, String> {
    apply_spans(part, &structure_spans(part, structure)?)
}

fn verify_tracked_part(output_part: &[u8], edits: &[&IslandEdit]) -> Result<(), String> {
    let xml = String::from_utf8_lossy(output_part);
    for edit in edits.iter().filter(|edit| edit.tracked) {
        let old = xml_escape(&edit.old);
        let new = xml_escape(&edit.new);
        if !xml.contains("<w:del") || !xml.contains("<w:ins") {
            return Err("tracked output is missing w:del/w:ins containers".to_string());
        }
        if !deletion_covers(&xml, &old) {
            return Err(format!("tracked deletion text {:?} is missing", edit.old));
        }
        // The revision's IDENTITY is part of the claim, and it is checked on the
        // revision THIS edit produced — the `del` carrying its old text and the
        // `ins` carrying its new one. A document-wide search for the author
        // would accept a tampered revision whenever any other revision still
        // carried the value: measured, changing one `w:author` passed.
        let revision = revision_for(&xml, &old, &new)?;
        if !edit.author.is_empty() && revision.author != xml_escape(&edit.author) {
            return Err(format!(
                "tracked revision says author {:?}, the edit recorded {:?}",
                revision.author, edit.author
            ));
        }
        if !edit.date.is_empty() && revision.date != xml_escape(&edit.date) {
            return Err(format!(
                "tracked revision says date {:?}, the edit recorded {:?}",
                revision.date, edit.date
            ));
        }
        if !xml.contains(&format!("<w:t>{new}</w:t>"))
            && !xml.contains(&format!("<w:t xml:space=\"preserve\">{new}</w:t>"))
        {
            return Err(format!("tracked insertion text {:?} is missing", edit.new));
        }
    }
    Ok(())
}

/// The revision an edit produced, and the identity it claims.
struct ProducedRevision {
    author: String,
    date: String,
}

/// Read the identity off the revision THIS edit produced.
///
/// A deletion that crosses runs is one `w:del` PER RUN — wrapping each run is
/// what keeps whatever sits between runs intact — so the deletion is a run of
/// elements whose `delText` bodies join to `old`, not one element. All of them
/// belong to the same edit and must agree about who made it.
fn revision_for(xml: &str, old: &str, new: &str) -> Result<ProducedRevision, String> {
    let deleted = xml_escape(old);
    let mut joined = String::new();
    let mut identity: Option<ProducedRevision> = None;
    let mut rest = xml;
    while let Some(start) = rest.find("<w:del ") {
        let Some(tag_end) = rest[start..].find('>') else {
            break;
        };
        let tag = &rest[start..start + tag_end + 1];
        let Some(body_end) = rest[start..].find("</w:del>") else {
            break;
        };
        let body = &rest[start + tag_end + 1..start + body_end];
        let text = inner_text(body, "w:delText");
        let found = ProducedRevision {
            author: attribute(tag, "w:author").unwrap_or_default(),
            date: attribute(tag, "w:date").unwrap_or_default(),
        };
        match &identity {
            Some(seen)
                if (seen.author.as_str(), seen.date.as_str())
                    != (found.author.as_str(), found.date.as_str()) =>
            {
                return Err("the runs of one deletion disagree about who made it".to_string())
            }
            _ => identity = Some(found),
        }
        joined.push_str(&text);
        if joined == deleted {
            break;
        }
        rest = &rest[start + body_end + "</w:del>".len()..];
    }
    let deleted_identity =
        identity.ok_or_else(|| format!("no w:del carries the deleted text {old:?}"))?;
    if joined != deleted {
        return Err(format!(
            "the deletion markup reads {joined:?}, the edit recorded {old:?}"
        ));
    }

    // The insertion is one element: the new text is written once, in the run
    // that carries the match's style.
    let inserted = xml_escape(new);
    let mut rest = xml;
    while let Some(start) = rest.find("<w:ins ") {
        let Some(tag_end) = rest[start..].find('>') else {
            break;
        };
        let tag = &rest[start..start + tag_end + 1];
        let Some(body_end) = rest[start..].find("</w:ins>") else {
            break;
        };
        let body = &rest[start + tag_end + 1..start + body_end];
        if inner_text(body, "w:t") == inserted {
            let ins = ProducedRevision {
                author: attribute(tag, "w:author").unwrap_or_default(),
                date: attribute(tag, "w:date").unwrap_or_default(),
            };
            if (
                deleted_identity.author.as_str(),
                deleted_identity.date.as_str(),
            ) != (ins.author.as_str(), ins.date.as_str())
            {
                return Err(format!(
                    "the deletion and the insertion disagree about who made the edit \
                     ({:?}/{:?} vs {:?}/{:?})",
                    deleted_identity.author, deleted_identity.date, ins.author, ins.date
                ));
            }
            return Ok(deleted_identity);
        }
        rest = &rest[start + body_end + "</w:ins>".len()..];
    }
    Err(format!("no w:ins carries the inserted text {new:?}"))
}

/// The text bodies of one tag inside a fragment, joined.
fn inner_text(fragment: &str, tag: &str) -> String {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut collected = String::new();
    let mut cursor = fragment;
    while let Some(inner) = cursor.find(&open) {
        let Some(gt) = cursor[inner..].find('>') else {
            break;
        };
        let from = inner + gt + 1;
        let Some(end) = cursor[from..].find(&close) else {
            break;
        };
        collected.push_str(&cursor[from..from + end]);
        cursor = &cursor[from + end + close.len()..];
    }
    collected
}

/// One attribute's value from an element's opening tag.
fn attribute(tag: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=\"");
    let start = tag.find(&needle)? + needle.len();
    let rest = &tag[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// One paragraph as the untouched zone sees it: its text and the run properties
/// it carries, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ParagraphShape {
    text: String,
    run_properties: Vec<String>,
    start: usize,
    end: usize,
}

/// Every paragraph's shape, keyed by id.
fn paragraph_shapes(part: &[u8], part_key: &str) -> Result<Vec<ParagraphShape>, String> {
    let mut shapes = Vec::new();
    for (_paragraph_id, start, end) in locate_paragraphs(part, part_key)? {
        let paragraph = &part[start..end];
        let tags = scan_tags(paragraph);
        let tree = build_light_tree(&tags)?;
        let root = tree
            .iter()
            .position(|node| node.parent.is_none())
            .ok_or("no root")?;
        let mut text = String::new();
        let mut run_properties = Vec::new();
        collect_shape(part, start, root, &tree, &mut text, &mut run_properties);
        shapes.push(ParagraphShape {
            text,
            run_properties,
            start,
            end,
        });
    }
    Ok(shapes)
}

fn collect_shape(
    part: &[u8],
    paragraph_start: usize,
    node: usize,
    tree: &[LightNode],
    text: &mut String,
    run_properties: &mut Vec<String>,
) {
    let current = &tree[node];
    for &child in &current.children {
        let child_node = &tree[child];
        match child_node.name.as_str() {
            // The text a reader sees: a deletion revision's `delText` is not it.
            "t" => {
                text.push_str(&String::from_utf8_lossy(
                    &part[paragraph_start + child_node.open_end..paragraph_start + child_node.end],
                ));
            }
            "rPr" => {
                run_properties.push(
                    String::from_utf8_lossy(
                        &part[paragraph_start + child_node.open_start
                            ..paragraph_start + child_node.end],
                    )
                    .into_owned(),
                );
            }
            _ => collect_shape(part, paragraph_start, child, tree, text, run_properties),
        }
    }
}

/// Nothing outside the edit's own paragraphs may change: not the text a reader
/// sees, not the run properties a run carries.
///
/// This is the invariant no other check covered. `parts-match-template-except-
/// edited` compares the OTHER parts, and the edited part is the whole of
/// `document.xml`, so a run property changed three paragraphs away was invisible
/// — measured: adding `<w:u w:val="double"/>` to an untouched run verified clean.
///
/// Beyond text and run properties, the RAW XML bytes of a paragraph outside the
/// edit must be identical: moving protected nodes within the same untouched
/// paragraph keeps every identity, count and order — and the visible text —
/// while changing what the bytes mean positionally. Measured on a genuine
/// tracked build: relocating both drawings ahead of the paragraph's text
/// verified `pass` on every check before this.
///
/// The edited paragraphs are exempt on purpose: a tracked edit legitimately
/// re-wraps them, carrying each covered run's properties into the `w:del` and
/// the insertion, so their property MULTISET changes while the document's
/// meaning does not.
fn check_non_target_preserved(
    baseline: &[u8],
    output_part: &[u8],
    part_key: &str,
    spans: &[(usize, usize)],
) -> Result<(), String> {
    let before = paragraph_shapes(baseline, part_key)?;
    let after = paragraph_shapes(output_part, part_key)?;
    // The baseline here is the part with every edit already applied, so the two
    // sides walk the same paragraphs in the same order. Pairing by ORDER is
    // deliberate: the ids this walker assigns are its own (`P1` for a text box
    // the engine calls `B0.P0`), so an id-keyed comparison exempts the wrong
    // paragraph and reports the edit itself as a change — measured on
    // `boxes.docx`.
    if before.len() != after.len() {
        return Err(format!(
            "paragraph count changed outside the edit ({} before, {} after)",
            before.len(),
            after.len()
        ));
    }
    for (index, shape) in before.iter().enumerate() {
        // An edit inside this paragraph — containment, which is what decides it,
        // not the id.
        if spans
            .iter()
            .any(|(start, end)| shape.start <= *start && *end <= shape.end)
        {
            continue;
        }
        let actual = &after[index];
        if actual.text != shape.text {
            return Err(format!(
                "paragraph {index}: text outside the edit changed ({:?} -> {:?})",
                shape.text, actual.text
            ));
        }
        if actual.run_properties != shape.run_properties {
            return Err(format!(
                "paragraph {index}: run properties outside the edit changed ({} before, {} after)",
                shape.run_properties.len(),
                actual.run_properties.len()
            ));
        }
        // Source-position contract: same identities in the same order can still
        // sit at different byte positions, which no identity list can see. The
        // spans pair the two sides by ORDER (same paragraphs, same sequence),
        // so a slice comparison is a position proof, not an offset estimate.
        if baseline[shape.start..shape.end] != output_part[actual.start..actual.end] {
            return Err(format!(
                "paragraph {index}: XML outside the edit changed                  (same text and properties, different source bytes)"
            ));
        }
    }
    Ok(())
}

/// true when the deletion markup carries `escaped_old`.
///
/// A deletion that crosses runs is emitted as one `w:del` per run — wrapping
/// each run is what keeps whatever sits BETWEEN runs (bookmarks, comment
/// anchors) intact — so the recorded text is the JOIN of their `delText`, not
/// one element. Reading them in document order and joining is the same
/// assertion the single-element form made.
fn deletion_covers(xml: &str, escaped_old: &str) -> bool {
    if xml.contains(&format!("<w:delText>{escaped_old}</w:delText>"))
        || xml.contains(&format!(
            "<w:delText xml:space=\"preserve\">{escaped_old}</w:delText>"
        ))
    {
        return true;
    }
    let mut joined = String::new();
    let mut rest = xml;
    while let Some(open) = rest.find("<w:delText") {
        let after = &rest[open..];
        let Some(gt) = after.find('>') else { break };
        let body_start = open + gt + 1;
        let Some(close) = rest[body_start..].find("</w:delText>") else {
            break;
        };
        joined.push_str(&rest[body_start..body_start + close]);
        rest = &rest[body_start + close + "</w:delText>".len()..];
    }
    joined.contains(escaped_old)
}

fn part_key_from_path(path: &str) -> Option<String> {
    let name = path.strip_prefix("word/")?.strip_suffix(".xml")?;
    let editable = name == "document"
        || name.starts_with("header")
        || name.starts_with("footer")
        || matches!(name, "footnotes" | "endnotes" | "comments");
    editable.then(|| name.to_string())
}

/// Independently recompute the edited part bytes from the template part
/// bytes and the recorded edits; any unprovable edit is a verification
/// failure.
fn recompute_edited_part(template_part: &[u8], edits: &[&IslandEdit]) -> Result<Vec<u8>, String> {
    let mut work = template_part.to_vec();
    // Each edit is resolved fresh against the ORIGINAL template (positions
    // are stable across edits), then applied right-to-left.
    let mut spans: Vec<(usize, usize, Vec<u8>)> = Vec::new();
    for edit in edits {
        let (leaf, _) = locate_leaf(
            template_part,
            &edit.part,
            &edit.paragraph_id,
            edit.leaf_index,
        )?;
        if !leaf.editable {
            return Err(format!(
                "leaf {}.{} is not editable (opaque interior)",
                edit.paragraph_id, edit.leaf_index
            ));
        }
        // The new text takes the first covered run's place; every later covered
        // run is emptied. One run covered is the ordinary case, and produces the
        // single span this always produced.
        let new_bytes = xml_escape(&edit.new).into_bytes();
        let mut covered = edit_run_ranges(template_part, &edit.part, edit)?;
        covered.sort_by_key(|(start, _)| *start);
        for (position, (start, end)) in covered.into_iter().enumerate() {
            if position == 0 {
                spans.push((start, end, new_bytes.clone()));
                if needs_xml_space(&edit.new) {
                    if let Some(tag_span) = inject_xml_space(template_part, leaf.open_end) {
                        spans.push(tag_span);
                    }
                }
            } else {
                spans.push((start, end, Vec::new()));
            }
        }
    }
    spans.sort_by_key(|(start, _, _)| *start);
    for (start, end, replacement) in spans.into_iter().rev() {
        work.splice(start..end, replacement);
    }
    Ok(work)
}
/// One protected node, identified by what it IS rather than where it sits.
///
/// Comparing positions is what broke the old check: a tracked edit rewrites a
/// run into `w:del` + `w:ins`, so every byte after it moves by an amount that
/// depends on the WRAPPING, and estimating that from `xml_escape(new)` is wrong
/// by construction. Identity survives re-wrapping; position does not.
#[derive(Clone, Debug)]
struct ProtectedNode {
    paragraph_id: String,
    /// Element name for an anchor, `opaque` for a locked interior.
    kind: String,
    /// The attributes that make this node that node (`w:id`, `w:name`, the
    /// hyperlink's `r:id`), or the bytes' digest for an opaque interior.
    identity: String,
    /// Byte range in the part it was read from. Used only to ask whether an
    /// edit lies INSIDE this node — a containment test, not a shift estimate.
    /// Deliberately NOT part of equality: the range moves when the edit re-wraps
    /// the text around this node, and comparing it would report every node as
    /// having changed. Measured: it made the check reject its own clean output.
    start: usize,
    end: usize,
}

/// Two protected nodes are the same node when they ARE the same node — same
/// paragraph, same kind, same identifying attributes. Where they sit is not
/// part of that.
impl PartialEq for ProtectedNode {
    fn eq(&self, other: &Self) -> bool {
        self.paragraph_id == other.paragraph_id
            && self.kind == other.kind
            && self.identity == other.identity
    }
}

impl Eq for ProtectedNode {}

/// Every protected node of one part, in document order.
///
/// The set is the one the byte-range check used — anchors, hyperlink ranges,
/// opaque interiors — read as identities. Anchors are recognised by their
/// attributes rather than their bytes, so a rewrite that moves them keeps them.
fn protected_nodes(part: &[u8], part_key: &str) -> Result<Vec<ProtectedNode>, String> {
    let mut nodes = Vec::new();
    let paragraphs = locate_paragraphs(part, part_key)?;
    for (paragraph_id, start, end) in paragraphs {
        let paragraph = &part[start..end];
        let tags = scan_tags(paragraph);
        let tree = build_light_tree(&tags)?;
        let root = tree
            .iter()
            .position(|node| node.parent.is_none())
            .ok_or("no root")?;
        collect_protected(part, start, root, &tree, &paragraph_id, &mut nodes)?;
    }
    Ok(nodes)
}

fn collect_protected(
    part: &[u8],
    paragraph_start: usize,
    node: usize,
    tree: &[LightNode],
    paragraph_id: &str,
    out: &mut Vec<ProtectedNode>,
) -> Result<(), String> {
    let current = &tree[node];
    for &child in &current.children {
        let child_node = &tree[child];
        let name = child_node.name.as_str();
        match name {
            "pPr" => {}
            // The immutable anchors: same id, same pairing, same relative order.
            // Their POSITION may move — a legal edit re-wraps the text around
            // them — so only identity is compared here.
            "bookmarkStart" | "bookmarkEnd" | "commentRangeStart" | "commentRangeEnd" => {
                out.push(ProtectedNode {
                    paragraph_id: paragraph_id.to_string(),
                    kind: name.to_string(),
                    identity: identity_attributes(
                        &part[paragraph_start + child_node.open_start
                            ..paragraph_start + child_node.open_end],
                        name,
                    ),
                    start: paragraph_start + child_node.open_start,
                    end: paragraph_start + child_node.end,
                });
            }
            // A hyperlink is a range whose target is a relationship: the r:id
            // must survive, or the link points somewhere else.
            "hyperlink" => {
                out.push(ProtectedNode {
                    paragraph_id: paragraph_id.to_string(),
                    kind: name.to_string(),
                    identity: identity_attributes(
                        &part[paragraph_start + child_node.open_start
                            ..paragraph_start + child_node.open_end],
                        name,
                    ),
                    start: paragraph_start + child_node.open_start,
                    end: paragraph_start + child_node.end,
                });
                collect_protected(part, paragraph_start, child, tree, paragraph_id, out)?;
            }
            // Revision wrappers are the one thing an edit is allowed to ADD, so
            // they are walked through rather than counted.
            "ins" | "del" | "moveFrom" | "moveTo" => {
                collect_protected(part, paragraph_start, child, tree, paragraph_id, out)?;
            }
            // Proofing marks are Word's, not the document's: they come and go.
            "proofErr" => {}
            "r" => {
                let opaque = child_node
                    .children
                    .iter()
                    .any(|&grand| !KNOWN_RUN_CHILDREN.contains(&tree[grand].name.as_str()));
                if opaque {
                    out.push(opaque_identity(
                        part,
                        paragraph_start,
                        child_node,
                        paragraph_id,
                    ));
                }
            }
            _ => out.push(opaque_identity(
                part,
                paragraph_start,
                child_node,
                paragraph_id,
            )),
        }
    }
    Ok(())
}

fn opaque_identity(
    part: &[u8],
    paragraph_start: usize,
    node: &LightNode,
    paragraph_id: &str,
) -> ProtectedNode {
    // The identity pins the COMPLETE opaque XML (`open_start..end`): interior
    // bytes, closing tag AND the node's own opening-tag attributes. Hashing
    // from `open_end` left an outer-attribute rewrite (e.g. a smuggled
    // `w:rsidR` on the run) invisible to the protected check; the tracked path
    // has no byte-exact recomputation to catch it instead. Measured on a
    // genuine tracked build: the tampered output verified `pass` before this.
    let bytes = &part[paragraph_start + node.open_start..paragraph_start + node.end];
    ProtectedNode {
        paragraph_id: paragraph_id.to_string(),
        kind: "opaque".to_string(),
        identity: format!("{}:{}", node.name, bytes_sha256(bytes)),
        start: paragraph_start + node.open_start,
        end: paragraph_start + node.end,
    }
}

/// The attributes that identify one anchor, in a stable order.
///
/// Only the identity-carrying attributes are read, so an edit that re-writes the
/// surrounding XML but leaves the anchor alone compares equal.
fn identity_attributes(tag: &[u8], name: &str) -> String {
    let text = String::from_utf8_lossy(tag);
    let wanted: &[&str] = match name {
        "bookmarkStart" => &["w:id", "w:name"],
        "bookmarkEnd" => &["w:id"],
        "commentRangeStart" | "commentRangeEnd" => &["w:id"],
        "hyperlink" => &["r:id", "w:anchor"],
        _ => &[],
    };
    let mut parts = Vec::new();
    for key in wanted {
        let needle = format!("{key}=\"");
        if let Some(start) = text.find(&needle) {
            let rest = &text[start + needle.len()..];
            if let Some(end) = rest.find('"') {
                parts.push(format!("{key}={}", &rest[..end]));
            }
        }
    }
    parts.join(" ")
}

/// The protected structure of a part must survive an edit: nothing lost, nothing
/// reordered, nothing invented — and it must survive RE-WRAPPING, which is what
/// a tracked edit does to everything around it.
///
/// `targeted` names the paragraphs the edit declared. In those, the template's
/// protected-node list must equal the output's list EXACTLY (same identities,
/// same count, same order): the old subsequence walk let an invented or
/// duplicated node pass, and the tracked path has no byte-exact recomputation
/// to catch it instead. Revision wrappers (`ins`/`del`/`moveFrom`/`moveTo`)
/// stay transparent — an edit may legitimately add those — and proof marks
/// stay ignored. Everywhere else the lists must be equal outright.
/// Synthesised paragraphs (`N`) have no baseline and are out of scope.
fn check_protected_structure(
    template_part: &[u8],
    output_part: &[u8],
    part_key: &str,
    targeted: &[String],
    spans: &[(usize, usize)],
) -> Result<(), String> {
    let before = protected_nodes(template_part, part_key)?;
    let after = protected_nodes(output_part, part_key)?;
    // An edit INSIDE an opaque interior rewrites that interior's bytes: a text
    // box's paragraph (`B0.P0`) lives inside the `w:pict` run of the paragraph
    // that owns it, so editing the box legitimately changes the drawing. The
    // proof for such an edit is the leaf range, not byte identity of the
    // container — this is a containment test, not the shift estimate the rest of
    // this check exists to avoid.
    let containers_of_edits: Vec<&ProtectedNode> = before
        .iter()
        .filter(|node| {
            node.kind == "opaque"
                && spans
                    .iter()
                    .any(|(start, end)| node.start <= *start && *end <= node.end)
        })
        .collect();
    let is_targeted = |node: &ProtectedNode| {
        node.paragraph_id.starts_with('N')
            || targeted.contains(&node.paragraph_id)
            || containers_of_edits
                .iter()
                .any(|container| container.paragraph_id == node.paragraph_id)
    };

    // Untouched paragraphs, compared ONE PARAGRAPH AT A TIME.
    //
    // Not as one list over the whole part: a structural edit inserts or removes
    // a paragraph and shifts everything after it, so a global comparison reads
    // the same nodes in a different order and calls it a change. Measured: an
    // insert reported the identical identities as both "missing" and "added".
    // Order WITHIN a paragraph is protected; order BETWEEN them is not.
    let per_paragraph = |nodes: &[ProtectedNode]| {
        let mut map: std::collections::BTreeMap<String, Vec<String>> =
            std::collections::BTreeMap::new();
        for node in nodes.iter().filter(|node| !is_targeted(node)) {
            map.entry(node.paragraph_id.clone())
                .or_default()
                .push(format!("{}:{}", node.kind, node.identity));
        }
        map
    };
    let untouched_before = per_paragraph(&before);
    let untouched_after = per_paragraph(&after);
    if untouched_before != untouched_after {
        let changed: Vec<_> = untouched_before
            .iter()
            .filter(|(id, nodes)| untouched_after.get(*id) != Some(nodes))
            .map(|(id, nodes)| {
                format!(
                    "{id} was {nodes:?}, is {:?}",
                    untouched_after.get(id).cloned().unwrap_or_default()
                )
            })
            .take(3)
            .collect();
        return Err(format!(
            "protected structure changed outside the edited paragraphs: {changed:?}"
        ));
    }

    // Edited paragraphs: the template's protected-node list must equal the
    // output's list exactly -- same identities, same count, same relative
    // order. Re-wrapping around the nodes (what a tracked edit does) keeps the
    // list; dropping, reordering or inventing a node changes it.
    for paragraph_id in targeted {
        // No empty-baseline skip: a targeted paragraph that starts with NO
        // protected nodes and gains one is an invention, not a no-op. (`N`
        // synthesised ids never reach this loop — edits and deletions name
        // baseline paragraphs — so this cannot misfire on new paragraphs.)
        // Compared as node references: `PartialEq` pins paragraph, kind and
        // identity while ignoring the shifted byte ranges, so no per-node
        // string is allocated on the success path.
        let expect: Vec<&ProtectedNode> = before
            .iter()
            .filter(|node| node.paragraph_id == *paragraph_id)
            .collect();
        let actual: Vec<&ProtectedNode> = after
            .iter()
            .filter(|node| node.paragraph_id == *paragraph_id)
            .collect();
        if expect != actual {
            let show = |nodes: &[&ProtectedNode]| {
                nodes
                    .iter()
                    .map(|node| format!("{}:{}", node.kind, node.identity))
                    .collect::<Vec<_>>()
            };
            return Err(format!(
                "{paragraph_id}: protected structure changed by the edit \
                 (expected {:?}, present after: {:?})",
                show(&expect),
                show(&actual)
            ));
        }
    }
    Ok(())
}

/// One located leaf with its byte ranges (independent walker).
struct LocatedLeaf {
    editable: bool,
    /// false when the projection drops this leaf's text as a revision gap — a
    /// `w:delText`, or a run inside `w:del` / `w:moveFrom`. The caller's offsets
    /// are measured in that projection, so the applier must read an island's
    /// recorded offset the same way or it resolves a different run than the one
    /// the edit was applied to.
    projected: bool,
    text_start: usize,
    text_end: usize,
    open_end: usize,
}

/// Is this element inside a tracked deletion (`w:del` / `w:moveFrom`)?
///
/// The projection renders those subtrees as a zero-width revision gap, so their
/// text is not part of the offsets an island records.
fn inside_deletion(nodes: &[LightNode], mut index: usize) -> bool {
    while let Some(parent) = nodes[index].parent {
        if matches!(nodes[parent].name.as_str(), "del" | "moveFrom") {
            return true;
        }
        index = parent;
    }
    false
}

/// Locate EVERY leaf of one paragraph, in order (independent port of the
/// locator discipline: paragraph ids, box/sdt/cell paths, leaf ordinals).
///
/// `locate_leaf` picks one by ordinal; an edit that crosses runs needs the whole
/// list, because which runs it covers is exactly what has to be resolved.
fn locate_paragraph_leaves(
    part: &[u8],
    part_key: &str,
    paragraph_id: &str,
) -> Result<Vec<LocatedLeaf>, String> {
    let paragraphs = locate_paragraphs(part, part_key)?;
    let Some((start, end)) = paragraphs
        .iter()
        .find(|(id, _, _)| id == paragraph_id)
        .map(|(_, start, end)| (*start, *end))
    else {
        return Err(format!("paragraph not found: {paragraph_id}"));
    };
    let paragraph = &part[start..end];
    let tags = scan_tags(paragraph);
    let _stack: Vec<&str> = Vec::new();
    let _run_open_end = 0usize;
    let _in_run = false;
    let mut rpr_ok = true;
    let _leaves: Vec<LocatedLeaf> = Vec::new();
    let mut leaves: Vec<LocatedLeaf> = Vec::new();
    let _index = 0usize;
    let opaque_children = false;
    // Reuse the tree walk: build a light element list for the paragraph.
    let nodes = build_light_tree(&tags)?;
    let root = nodes
        .iter()
        .position(|node| node.parent.is_none())
        .ok_or("no root")?;
    let mut opaque_ranges: Vec<(usize, usize)> = Vec::new();
    collect_opaque_ranges(root, &nodes, &mut opaque_ranges, &mut rpr_ok);
    // walk runs
    for (node_index, node) in nodes.iter().enumerate() {
        if node.name != "r" {
            continue;
        }
        // a run is opaque when any child is unknown
        let run_opaque = node.children.iter().any(|&child| {
            let child_node = &nodes[child];
            !KNOWN_RUN_CHILDREN.contains(&child_node.name.as_str())
        });
        let in_deletion = inside_deletion(&nodes, node_index);
        for &child in &node.children {
            let child_node = &nodes[child];
            if child_node.name == "t" || child_node.name == "delText" {
                let text_start = child_node.open_end;
                let text_end = if child_node.children.is_empty() {
                    child_node.close_start
                } else {
                    child_node.open_end
                };
                let editable = !run_opaque
                    && !opaque_children
                    && child_node.name == "t"
                    && !opaque_ranges.iter().any(|(start, end)| {
                        child_node.open_start >= *start && child_node.end <= *end
                    });
                leaves.push(LocatedLeaf {
                    editable,
                    projected: !in_deletion && child_node.name == "t",
                    text_start: start + text_start,
                    text_end: start + text_end,
                    open_end: start + child_node.open_end,
                });
            }
        }
    }
    Ok(leaves)
}

/// Locate one paragraph's leaf by ordinal.
/// The placeholder a restyled run's `w:rPr` is replaced with before a part is
/// compared. The bytes do not matter — only that both sides carry the same ones,
/// so everything EXCEPT the restyled properties has to match.
/// The package member path of a part key ("document" -> "word/document.xml").
/// Named apart from the verifier's own `part_path`, which a caller's local of the
/// same name shadows.
fn verify_part_path(part_key: &str) -> String {
    if part_key == "document" {
        "word/document.xml".to_string()
    } else {
        format!("word/{part_key}.xml")
    }
}

/// One run a plan says the output must carry: the characters it holds, and the
/// properties it must have (a style the registry holds, or `None` for "the run
/// it came from").
#[derive(Clone, Debug)]
struct PlannedRunCheck {
    paragraph_id: String,
    start: usize,
    end: usize,
    style: Option<String>,
}

/// The format engine's plan, read independently of Core the same way islands and
/// structure are. The verifier CHECKS the plan against the artifact; it does not
/// re-derive the plan's rules (ADR 0050).
fn load_planned_runs(workdir: &Path) -> Vec<PlannedRunCheck> {
    let path = workdir.join("format_plan.json");
    let Ok(bytes) = std::fs::read(&path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let Some(paragraphs) = value.get("paragraphs").and_then(|rows| rows.as_array()) else {
        return out;
    };
    for paragraph in paragraphs {
        let Some(paragraph_id) = paragraph.get("paragraph_id").and_then(|id| id.as_str()) else {
            continue;
        };
        let Some(runs) = paragraph.get("runs").and_then(|rows| rows.as_array()) else {
            continue;
        };
        for run in runs {
            let range = run.get("range");
            let start = range
                .and_then(|range| range.get("start"))
                .and_then(|value| value.as_u64())
                .unwrap_or(0) as usize;
            let end = range
                .and_then(|range| range.get("end"))
                .and_then(|value| value.as_u64())
                .unwrap_or(0) as usize;
            out.push(PlannedRunCheck {
                paragraph_id: paragraph_id.to_string(),
                start,
                end,
                style: run
                    .get("style")
                    .and_then(|style| style.as_str())
                    .map(str::to_string),
            });
        }
    }
    out
}

/// `style_id -> w:rPr` from `styles.json`, the registry the build splices from.
fn load_style_rpr_bytes(workdir: &Path) -> BTreeMap<String, String> {
    let path = workdir.join("styles.json");
    let Ok(bytes) = std::fs::read(&path) else {
        return BTreeMap::new();
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return BTreeMap::new();
    };
    let mut out = BTreeMap::new();
    if let Some(styles) = value.get("styles").and_then(|styles| styles.as_object()) {
        for (style_id, entry) in styles {
            if let Some(rpr) = entry.get("rPr").and_then(|rpr| rpr.as_str()) {
                out.insert(style_id.clone(), rpr.to_string());
            }
        }
    }
    out
}

/// The byte range of the whole `<w:r>…</w:r>` that owns `text_start`.
///
/// A plan may SPLIT a run, so a check that compares bytes has to set the entire
/// run aside rather than its properties: five runs where the baseline has one is
/// not a formatting difference, and masking only the `w:rPr` regions made this
/// verifier refuse a correct build.
fn run_element_range(part: &[u8], text_start: usize) -> Option<(usize, usize)> {
    let before = &part[..text_start.min(part.len())];
    let mut run_start = None;
    for index in 0..before.len().saturating_sub(3) {
        if &before[index..index + 4] == b"<w:r"
            && before
                .get(index + 4)
                .is_some_and(|byte| *byte == b' ' || *byte == b'>')
        {
            run_start = Some(index);
        }
    }
    let run_start = run_start?;
    let end = part[run_start..]
        .windows(6)
        .position(|window| window == b"</w:r>")
        .map(|offset| run_start + offset + 6)?;
    Some((run_start, end))
}

/// The byte range of the `w:rPr` of the run that owns `text_start`, or the point
/// where one belongs when the run has none: `(start, end, run_open_end)`.
fn run_rpr_range(part: &[u8], text_start: usize) -> Option<(usize, usize, usize)> {
    let before = &part[..text_start.min(part.len())];
    let mut run_start = None;
    for index in 0..before.len().saturating_sub(3) {
        if &before[index..index + 4] == b"<w:r"
            && before
                .get(index + 4)
                .is_some_and(|byte| *byte == b' ' || *byte == b'>')
        {
            run_start = Some(index);
        }
    }
    let run_start = run_start?;
    let run_open_end = part[run_start..]
        .iter()
        .position(|byte| *byte == b'>')
        .map(|offset| run_start + offset + 1)?;
    let tail = &part[run_open_end..];
    let rpr_start = tail
        .windows(6)
        .position(|window| window == b"<w:rPr")
        .map(|offset| run_open_end + offset);
    match rpr_start {
        Some(start) => {
            let end = part[start..]
                .windows(8)
                .position(|window| window == b"</w:rPr>")
                .map(|offset| start + offset + 8)?;
            Some((start, end, run_open_end))
        }
        None => Some((run_open_end, run_open_end, run_open_end)),
    }
}

/// The runs a restyle names, located by the paragraph's visible-text offsets —
/// this crate's own walker, not Core's.
fn planned_run_ranges(
    part: &[u8],
    part_key: &str,
    edit: &PlannedRunCheck,
) -> Result<Vec<(usize, usize, usize)>, String> {
    let leaves = locate_paragraph_leaves(part, part_key, &edit.paragraph_id)?;
    let mut cursor = 0usize;
    let mut ranges = Vec::new();
    for leaf in &leaves {
        if leaf.text_start == leaf.text_end {
            continue;
        }
        let length = String::from_utf8_lossy(&part[leaf.text_start..leaf.text_end])
            .chars()
            .count();
        let (start, end) = (cursor, cursor + length);
        cursor = end;
        if start < edit.end && end > edit.start {
            let Some(range) = run_rpr_range(part, leaf.text_start) else {
                return Err(format!(
                    "{}: run for offset {} not found",
                    edit.paragraph_id, leaf.text_start
                ));
            };
            ranges.push(range);
        }
    }
    if ranges.is_empty() {
        return Err(format!(
            "{}: no run covers [{}, {})",
            edit.paragraph_id, edit.start, edit.end
        ));
    }
    Ok(ranges)
}

/// Replace every named run's `w:rPr` with `mask`, so two parts can be compared
/// with the restyled properties set aside. Insertion where a run has none.
fn strip_planned_runs(
    part: &[u8],
    part_key: &str,
    edits: &[&PlannedRunCheck],
) -> Result<Vec<u8>, String> {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for edit in edits {
        let leaves = locate_paragraph_leaves(part, part_key, &edit.paragraph_id)?;
        let mut cursor = 0usize;
        for leaf in &leaves {
            if leaf.text_start == leaf.text_end {
                continue;
            }
            let length = String::from_utf8_lossy(&part[leaf.text_start..leaf.text_end])
                .chars()
                .count();
            let (start, end) = (cursor, cursor + length);
            cursor = end;
            if start < edit.end && end > edit.start {
                if let Some(range) = run_element_range(part, leaf.text_start) {
                    ranges.push(range);
                }
            }
        }
    }
    ranges.sort();
    ranges.dedup();
    let mut out = part.to_vec();
    for (start, end) in ranges.into_iter().rev() {
        out.splice(start..end, std::iter::empty());
    }
    Ok(out)
}

/// One planned run, checked against the output: it exists, it holds exactly the
/// characters the plan gives it, and it carries the properties the plan says —
/// the style's bytes when the plan names one, and otherwise the properties of the
/// run it came from, which is the baseline's.
fn verify_planned_run(
    baseline_part: &[u8],
    output_part: &[u8],
    part_key: &str,
    edit: &PlannedRunCheck,
    style_rpr: &str,
) -> Result<(), String> {
    let output_ranges = planned_run_ranges(output_part, part_key, edit)?;
    if output_ranges.len() != 1 {
        return Err(format!(
            "{}: the output has {} run(s) where the plan names one for [{}, {})",
            edit.paragraph_id,
            output_ranges.len(),
            edit.start,
            edit.end
        ));
    }
    let (start, end, _) = output_ranges[0];
    let actual = String::from_utf8_lossy(&output_part[start..end]).to_string();
    let expected = match &edit.style {
        Some(_) => style_rpr.to_string(),
        // Untouched: the properties must be the ones the run had. The baseline is
        // the template with the text islands applied — what the plan was derived
        // from — so a run the plan leaves alone must read identically there.
        None => {
            let baseline_ranges = planned_run_ranges(baseline_part, part_key, edit)?;
            if baseline_ranges.len() != 1 {
                return Err(format!(
                    "{}: the baseline has {} run(s) where the plan names one for [{}, {})",
                    edit.paragraph_id,
                    baseline_ranges.len(),
                    edit.start,
                    edit.end
                ));
            }
            let (start, end, _) = baseline_ranges[0];
            String::from_utf8_lossy(&baseline_part[start..end]).to_string()
        }
    };
    if actual != expected {
        return Err(format!(
            "{}: the run holding [{}, {}) carries {:?}, not {:?}",
            edit.paragraph_id, edit.start, edit.end, actual, expected
        ));
    }
    Ok(())
}

fn locate_leaf(
    part: &[u8],
    part_key: &str,
    paragraph_id: &str,
    leaf_index: usize,
) -> Result<(LocatedLeaf, ()), String> {
    locate_paragraph_leaves(part, part_key, paragraph_id)?
        .into_iter()
        .nth(leaf_index)
        .map(|leaf| (leaf, ()))
        .ok_or_else(|| format!("leaf {leaf_index} not found in {paragraph_id}"))
}

/// Minimal element tree over a tag list (independent of Core).
pub struct LightNode {
    name: String,
    open_start: usize,
    open_end: usize,
    close_start: usize,
    end: usize,
    parent: Option<usize>,
    children: Vec<usize>,
}

fn build_light_tree(tags: &[Tag]) -> Result<Vec<LightNode>, String> {
    let mut nodes: Vec<LightNode> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    for tag in tags {
        if tag.closing {
            let Some(open) = stack.pop() else {
                return Err("malformed XML nesting".to_string());
            };
            if nodes[open].name != tag.name {
                return Err("malformed XML nesting".to_string());
            }
            nodes[open].close_start = tag.start;
            nodes[open].end = tag.end;
            continue;
        }
        let index = nodes.len();
        nodes.push(LightNode {
            name: tag.name.clone(),
            open_start: tag.start,
            open_end: tag.end,
            close_start: tag.end,
            end: tag.end,
            parent: stack.last().copied(),
            children: Vec::new(),
        });
        if let Some(&parent) = stack.last() {
            nodes[parent].children.push(index);
        }
        if !tag.self_closing {
            stack.push(index);
        }
    }
    if !stack.is_empty() {
        return Err("unclosed elements".to_string());
    }
    Ok(nodes)
}

const KNOWN_RUN_CHILDREN: [&str; 16] = [
    "rPr",
    "t",
    "delText",
    "tab",
    "br",
    "cr",
    "noBreakHyphen",
    "softHyphen",
    "sym",
    "commentReference",
    "footnoteRef",
    "endnoteRef",
    "annotationRef",
    "separator",
    "continuationSeparator",
    "lastRenderedPageBreak",
];

/// Collect opaque element ranges inside a paragraph (independent walk).
fn collect_opaque_ranges(
    node: usize,
    nodes: &[LightNode],
    out: &mut Vec<(usize, usize)>,
    _rpr_ok: &mut bool,
) {
    let node_ref = &nodes[node];
    for &child in &node_ref.children {
        let child_node = &nodes[child];
        let name = child_node.name.as_str();
        match name {
            "pPr" => {}
            "proofErr" => out.push((child_node.open_end, child_node.end)),
            "r" => {
                for &grand in &child_node.children {
                    let grand_node = &nodes[grand];
                    if !KNOWN_RUN_CHILDREN.contains(&grand_node.name.as_str()) {
                        out.push((child_node.open_end, child_node.end));
                        break;
                    }
                }
            }
            "hyperlink" | "ins" | "del" | "moveFrom" | "moveTo" => {
                collect_opaque_ranges(child, nodes, out, _rpr_ok)
            }
            "bookmarkStart" | "bookmarkEnd" | "commentRangeStart" | "commentRangeEnd" => {}
            _ => out.push((child_node.open_end, child_node.end)),
        }
    }
}
/// Scan one part into paragraphs (independent port of the full recursive
/// locator: direct body paragraphs, table-cell paragraphs with nested
/// tables, text-box paragraphs, content-control paragraphs, and
/// header/footer/note part paragraphs).
fn locate_paragraphs(part: &[u8], part_key: &str) -> Result<Vec<(String, usize, usize)>, String> {
    let tags = scan_tags(part);
    if part_key == "document" {
        locate_document_paragraphs(&tags)
    } else {
        locate_part_paragraphs(&tags, part_key)
    }
}

enum PathKind {
    Direct,
    Box {
        box_index: i64,
        p_ordinal: u32,
    },
    Sdt {
        sdt_index: i64,
        p_ordinal: u32,
    },
    Cell {
        table_index: i64,
        chain: Vec<(String, u32)>,
    },
}

fn locate_document_paragraphs(tags: &[Tag]) -> Result<Vec<(String, usize, usize)>, String> {
    let mut stack: Vec<(String, usize, u32)> = Vec::new();
    let mut body_depth: Option<usize> = None;
    let mut starts: Vec<(usize, usize, PathKind)> = Vec::new();
    let mut direct = 0u32;
    let mut table_ordinal: i64 = -1;
    let mut open_tables: Vec<(i64, usize)> = Vec::new();
    let mut box_ordinal: i64 = -1;
    let mut open_boxes: Vec<(i64, usize)> = Vec::new();
    let mut sdt_ordinal: i64 = -1;
    let mut open_sdts: Vec<(i64, usize)> = Vec::new();
    let mut ordinals: std::collections::HashMap<(usize, usize, String), u32> =
        std::collections::HashMap::new();
    for tag in tags {
        if tag.closing {
            let depth = stack.len();
            let Some((top_name, top_start, top_ordinal)) = stack.last() else {
                return Err("malformed document XML".to_string());
            };
            if top_name != &tag.name {
                return Err("malformed document XML".to_string());
            }
            let Some(body_depth) = body_depth else {
                return Err("document has no body".to_string());
            };
            if tag.name == "p" && depth == body_depth + 2 {
                starts.push((*top_start, tag.end, PathKind::Direct));
            } else if tag.name == "p"
                && depth > body_depth + 1
                && (!open_tables.is_empty() || !open_boxes.is_empty() || !open_sdts.is_empty())
            {
                let chain: Vec<&str> = stack[body_depth + 1..]
                    .iter()
                    .map(|(name, _, _)| name.as_str())
                    .collect();
                if is_box_stack(&chain) {
                    starts.push((
                        *top_start,
                        tag.end,
                        PathKind::Box {
                            box_index: open_boxes.last().expect("box").0,
                            p_ordinal: *top_ordinal,
                        },
                    ));
                } else if is_sdt_stack(&chain) {
                    starts.push((
                        *top_start,
                        tag.end,
                        PathKind::Sdt {
                            sdt_index: open_sdts.last().expect("sdt").0,
                            p_ordinal: *top_ordinal,
                        },
                    ));
                } else if is_cell_stack(&chain) {
                    let table_index = open_tables.last().expect("table").0;
                    let path: Vec<(String, u32)> = stack[body_depth + 1..]
                        .iter()
                        .map(|(name, _, ordinal)| (name.clone(), *ordinal))
                        .collect();
                    starts.push((
                        *top_start,
                        tag.end,
                        PathKind::Cell {
                            table_index,
                            chain: path,
                        },
                    ));
                }
            }
            if tag.name == "tbl"
                && open_tables
                    .last()
                    .is_some_and(|(_, start)| *start == *top_start)
            {
                open_tables.pop();
            }
            if tag.name == "txbxContent"
                && open_boxes
                    .last()
                    .is_some_and(|(_, start)| *start == *top_start)
            {
                open_boxes.pop();
            }
            if tag.name == "sdtContent"
                && open_sdts
                    .last()
                    .is_some_and(|(_, start)| *start == *top_start)
            {
                open_sdts.pop();
            }
            stack.pop();
            continue;
        }
        let depth = stack.len();
        if tag.name == "body" && body_depth.is_none() {
            body_depth = Some(depth);
        }
        let key = (
            depth.saturating_sub(1),
            stack
                .last()
                .map(|(_, start, _)| *start)
                .unwrap_or(usize::MAX),
            tag.name.clone(),
        );
        let ordinal = ordinals.get(&key).copied().unwrap_or(0);
        ordinals.insert(key, ordinal + 1);
        if let Some(body_depth) = body_depth {
            if tag.name == "p" && depth == body_depth + 1 {
                if tag.self_closing {
                    starts.push((tag.start, tag.end, PathKind::Direct));
                } else {
                    stack.push((tag.name.clone(), tag.start, ordinal));
                }
                continue;
            }
            if tag.name == "sdtContent"
                && depth > body_depth + 1
                && stack[body_depth + 1..]
                    .first()
                    .is_some_and(|(name, _, _)| name == "sdt")
                && stack[body_depth + 2..]
                    .iter()
                    .all(|(name, _, _)| name == "sdtPr" || name == "sdtEndPr")
            {
                sdt_ordinal += 1;
                open_sdts.push((sdt_ordinal, tag.start));
            }
            if tag.name == "txbxContent"
                && depth > body_depth + 1
                && stack
                    .get(body_depth + 1)
                    .is_some_and(|(name, _, _)| name == "p")
            {
                box_ordinal += 1;
                open_boxes.push((box_ordinal, tag.start));
            }
            if tag.name == "tbl" && depth == body_depth + 1 {
                table_ordinal += 1;
                open_tables.push((table_ordinal, tag.start));
                stack.push((tag.name.clone(), tag.start, ordinal));
                continue;
            }
            if tag.name == "tbl"
                && depth > body_depth + 1
                && stack.last().is_some_and(|(name, _, _)| name == "tc")
            {
                table_ordinal += 1;
                open_tables.push((table_ordinal, tag.start));
            }
        }
        if !tag.self_closing {
            stack.push((tag.name.clone(), tag.start, ordinal));
        }
    }
    let mut paragraphs = Vec::with_capacity(starts.len());
    for (start, end, path) in starts {
        let id = match path {
            PathKind::Direct => {
                let id = format!("P{direct}");
                direct += 1;
                id
            }
            PathKind::Box {
                box_index,
                p_ordinal,
            } => format!("B{box_index}.P{p_ordinal}"),
            PathKind::Sdt {
                sdt_index,
                p_ordinal,
            } => format!("S{sdt_index}.P{p_ordinal}"),
            PathKind::Cell { table_index, chain } => {
                let mut values: std::collections::BTreeMap<&str, u32> =
                    std::collections::BTreeMap::new();
                for (name, ordinal) in &chain {
                    values.insert(name.as_str(), *ordinal);
                }
                format!(
                    "T{table_index}.R{}.C{}.P{}",
                    values.get("tr").copied().unwrap_or(0),
                    values.get("tc").copied().unwrap_or(0),
                    values.get("p").copied().unwrap_or(0)
                )
            }
        };
        paragraphs.push((id, start, end));
    }
    Ok(paragraphs)
}

fn is_box_stack(chain: &[&str]) -> bool {
    chain.last() == Some(&"p")
        && chain.contains(&"txbxContent")
        && !chain
            .iter()
            .any(|name| matches!(*name, "tbl" | "tr" | "tc"))
}

fn is_sdt_stack(chain: &[&str]) -> bool {
    chain.last() == Some(&"p") && chain.len() == 3 && chain[0] == "sdt" && chain[1] == "sdtContent"
}

fn is_cell_stack(chain: &[&str]) -> bool {
    chain.last() == Some(&"p") && chain.len() >= 4 && {
        let mut ok = true;
        for (level, name) in chain[..chain.len() - 1].iter().rev().enumerate() {
            let expected = match level % 3 {
                0 => "tc",
                1 => "tr",
                _ => "tbl",
            };
            ok = ok && *name == expected;
        }
        ok
    }
}

fn locate_part_paragraphs(
    tags: &[Tag],
    part_key: &str,
) -> Result<Vec<(String, usize, usize)>, String> {
    let mut stack: Vec<(String, usize)> = Vec::new();
    let mut paragraphs: Vec<(String, usize, usize)> = Vec::new();
    let mut index = 0usize;
    let root_hdr = part_key.starts_with("header") || part_key.starts_with("footer");
    for tag in tags {
        if tag.closing {
            let depth = stack.len();
            let Some((name, start)) = stack.last() else {
                return Err("malformed part XML".to_string());
            };
            if name != &tag.name {
                return Err("malformed part XML".to_string());
            }
            if tag.name == "p" && ((root_hdr && depth == 2) || (!root_hdr && depth == 3)) {
                paragraphs.push((format!("{part_key}.P{index}"), *start, tag.end));
                index += 1;
            }
            stack.pop();
            continue;
        }
        if !tag.self_closing {
            stack.push((tag.name.clone(), tag.start));
        }
    }
    Ok(paragraphs)
}

/// One structural token (independent port of the byte discipline).
struct Tag {
    name: String,
    closing: bool,
    self_closing: bool,
    start: usize,
    end: usize,
}

fn scan_tags(xml: &[u8]) -> Vec<Tag> {
    let mut tags = Vec::new();
    let mut index = 0usize;
    while index < xml.len() {
        let Some(lt) = find_bytes(xml, b"<", index) else {
            break;
        };
        let rest = &xml[lt..];
        let end = if rest.starts_with(b"<!--") {
            find_bytes(xml, b"-->", lt + 4).map(|e| e + 3)
        } else if rest.starts_with(b"<![CDATA[") {
            find_bytes(xml, b"]]>", lt + 9).map(|e| e + 3)
        } else if rest.starts_with(b"<?") {
            find_bytes(xml, b"?>", lt + 2).map(|e| e + 2)
        } else {
            scan_angle(xml, lt)
        };
        let Some(end) = end else { break };
        let token = &xml[lt..end];
        if token.starts_with(b"<!--") || token.starts_with(b"<?") || token.starts_with(b"<!") {
            index = end;
            continue;
        }
        let mut pos = 1;
        while pos < token.len() && token[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if pos < token.len() && token[pos] == b'/' {
            // closing
            pos += 1;
            while pos < token.len() && token[pos].is_ascii_whitespace() {
                pos += 1;
            }
            let name_start = pos;
            while pos < token.len() && is_name_byte(token[pos]) {
                pos += 1;
            }
            if pos == name_start {
                index = end;
                continue;
            }
            let name = std::str::from_utf8(&token[name_start..pos])
                .map(|s| s.rsplit(':').next().unwrap_or(s).to_string())
                .unwrap_or_default();
            tags.push(Tag {
                name,
                closing: true,
                self_closing: false,
                start: lt,
                end,
            });
            index = end;
            continue;
        }
        let name_start = pos;
        while pos < token.len() && is_name_byte(token[pos]) {
            pos += 1;
        }
        if pos == name_start {
            index = end;
            continue;
        }
        let name = std::str::from_utf8(&token[name_start..pos])
            .map(|s| s.rsplit(':').next().unwrap_or(s).to_string())
            .unwrap_or_default();
        let self_closing = token[token.len().saturating_sub(2)..] == *b"/>";
        tags.push(Tag {
            name,
            closing: false,
            self_closing,
            start: lt,
            end,
        });
        index = end;
    }
    tags
}

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b':' | b'-')
}

fn scan_angle(xml: &[u8], start: usize) -> Option<usize> {
    let mut index = start + 1;
    while index < xml.len() {
        match xml[index] {
            b'"' => {
                index += 1;
                while index < xml.len() && xml[index] != b'"' {
                    index += 1;
                }
                index += 1;
            }
            b'\'' => {
                index += 1;
                while index < xml.len() && xml[index] != b'\'' {
                    index += 1;
                }
                index += 1;
            }
            b'>' => return Some(index + 1),
            _ => index += 1,
        }
    }
    None
}

fn find_bytes(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from > haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|index| from + index)
}

fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    out
}

fn needs_xml_space(text: &str) -> bool {
    text.starts_with(' ') || text.ends_with(' ')
}

fn inject_xml_space(part: &[u8], open_end: usize) -> Option<(usize, usize, Vec<u8>)> {
    let tag_start = part[..open_end].iter().rposition(|&b| b == b'<')?;
    for attribute in [b"xml:space=\"preserve\"", b"xml:space='preserve'"] {
        if part[tag_start..open_end]
            .windows(attribute.len())
            .any(|window| window == attribute)
        {
            return None;
        }
    }
    let insert_at = part[tag_start..open_end]
        .iter()
        .rposition(|&b| b == b'>')
        .map(|pos| tag_start + pos)
        .unwrap_or(open_end);
    let mut out = Vec::new();
    out.extend_from_slice(&part[tag_start..insert_at]);
    out.extend_from_slice(b" xml:space=\"preserve\"");
    out.extend_from_slice(&part[insert_at..open_end]);
    Some((tag_start, open_end, out))
}

/// The byte range of every run one recorded edit covers, in order.
///
/// `old` may start in one run and end in another: a Word paragraph splits its
/// text at every formatting change, so a phrase a human typed in Word routinely
/// crosses runs, and the recorded `leaf_index` is the run it starts in. The
/// covered texts concatenated equal `old`.
///
/// This is the verifier's own walk — it must reach the same runs Core's applier
/// did, or the byte-exact check compares against a different edit than the one
/// that was applied.
fn edit_run_ranges(
    part: &[u8],
    part_key: &str,
    edit: &IslandEdit,
) -> Result<Vec<(usize, usize)>, String> {
    if edit.old.is_empty() {
        return Err("empty old text".to_string());
    }
    let leaves = locate_paragraph_leaves(part, part_key, &edit.paragraph_id)?;
    if edit.leaf_index >= leaves.len() {
        return Err(format!(
            "leaf {} not found in {}",
            edit.leaf_index, edit.paragraph_id
        ));
    }
    // Paragraph-level offset of each run's first char, in the projection the
    // island's offset was recorded in — not in the runs, where a revision gap
    // still has width. This crate stays independent of the applier on purpose —
    // that is what makes it a check — but it must read the same input: one
    // island carries every replacement in its leaf, and its merged text can
    // occur twice in that leaf, which a text-only search refuses. It must also
    // measure that offset the same way, or a paragraph with a tracked deletion
    // resolves to a different run than the applier wrote.
    let mut projected_prefix: Vec<usize> = Vec::with_capacity(leaves.len());
    let mut running = 0usize;
    for leaf in &leaves {
        projected_prefix.push(running);
        if leaf.projected {
            let (text, _) = decode_text(&part[leaf.text_start..leaf.text_end])?;
            running += text.chars().count();
        }
    }

    let mut found: Option<Vec<(usize, usize)>> = None;
    for start in edit.leaf_index..leaves.len() {
        let first = &leaves[start];
        let (_, first_chars) = decode_text(&part[first.text_start..first.text_end])?;
        let first_len = first_chars.len();
        // The recorded offset starts inside this run, or this start is not its
        // business.
        let anchored_head = match edit.char_start {
            Some(want)
                if first.projected
                    && want >= projected_prefix[start]
                    && want < projected_prefix[start] + first_len =>
            {
                Some(want - projected_prefix[start])
            }
            Some(_) => continue,
            None => None,
        };
        let mut accumulated = String::new();
        let mut before_last = 0usize;
        for end in start..leaves.len() {
            if end > start {
                let previous = &leaves[end - 1];
                let (text, _) = decode_text(&part[previous.text_start..previous.text_end])?;
                before_last += text.chars().count();
            }
            let (text, _) = decode_text(&part[leaves[end].text_start..leaves[end].text_end])?;
            accumulated.push_str(&text);
            let byte_pos = match anchored_head {
                Some(head) => match accumulated.char_indices().nth(head) {
                    Some((byte_pos, _)) if accumulated[byte_pos..].starts_with(&edit.old) => {
                        byte_pos
                    }
                    _ => continue,
                },
                None => match accumulated.find(&edit.old) {
                    Some(byte_pos) => byte_pos,
                    None => continue,
                },
            };
            let head = accumulated[..byte_pos].chars().count();
            let covered = edit.old.chars().count();
            // The match must start inside the FIRST run and finish inside the
            // LAST one; anything else belongs to a later start.
            if head >= first_len || head + covered <= before_last {
                break;
            }
            if found.is_some() {
                return Err(
                    "old text occurs more than once across the runs of this paragraph".to_string(),
                );
            }
            let tail = head + covered - before_last;
            let mut ranges = Vec::new();
            for (offset, leaf) in leaves[start..=end].iter().enumerate() {
                let is_first = offset == 0;
                let is_last = offset == end - start;
                let (text, char_bytes) = decode_text(&part[leaf.text_start..leaf.text_end])?;
                let len = text.chars().count();
                let from = if is_first { head } else { 0 };
                let to = if is_last { tail } else { len };
                if to <= from {
                    continue;
                }
                let byte_start = char_bytes[from].0 + leaf.text_start;
                let byte_end = char_bytes[to - 1].1 + leaf.text_start;
                ranges.push((byte_start, byte_end));
            }
            found = Some(ranges);
            break;
        }
    }
    found.ok_or_else(|| {
        format!(
            "old text {:?} not found in paragraph {}",
            edit.old, edit.paragraph_id
        )
    })
}

fn decode_text(raw: &[u8]) -> Result<(String, Vec<(usize, usize)>), String> {
    let mut decoded = String::new();
    let mut char_bytes: Vec<(usize, usize)> = Vec::new();
    let mut index = 0usize;
    while index < raw.len() {
        let byte = raw[index];
        if byte == b'&' {
            let Some(semi) = raw[index..].iter().position(|&b| b == b';') else {
                return Err("unterminated entity".to_string());
            };
            let entity = &raw[index..index + semi + 1];
            let entity_text = std::str::from_utf8(entity).map_err(|_| "non-ASCII entity")?;
            let ch = decode_entity(entity_text).ok_or("unrecognized entity")?;
            decoded.push(ch);
            char_bytes.push((index, index + entity.len()));
            index += entity.len();
            continue;
        }
        let len = match byte {
            0x00..=0x7F => 1,
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            _ => 4,
        };
        if index + len > raw.len() {
            return Err("truncated UTF-8".to_string());
        }
        let slice = &raw[index..index + len];
        let ch = std::str::from_utf8(slice)
            .map_err(|_| "bad UTF-8")?
            .chars()
            .next()
            .ok_or("empty utf8")?;
        char_bytes.push((index, index + len));
        decoded.push(ch);
        index += len;
    }
    Ok((decoded, char_bytes))
}

fn decode_entity(entity: &str) -> Option<char> {
    match entity {
        "&amp;" => Some('&'),
        "&lt;" => Some('<'),
        "&gt;" => Some('>'),
        "&quot;" => Some('"'),
        "&apos;" => Some('\''),
        _ => {
            let hex = entity
                .strip_prefix("&#x")
                .or_else(|| entity.strip_prefix("&#X"));
            if let Some(digits) = hex {
                let code = digits.strip_suffix(';')?;
                return char::from_u32(u32::from_str_radix(code, 16).ok()?);
            }
            let digits = entity.strip_prefix("&#")?;
            let code = digits.strip_suffix(';')?;
            char::from_u32(code.parse().ok()?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_synthesis_filters_refused_properties() {
        let xml = b"<w:pPr><w:sectPr/><w:pPrChange w:id=\"1\"/><w:jc w:val=\"center\"/><w:spacing w:after=\"120\"/></w:pPr>";
        let nodes = build_light_tree(&scan_tags(xml)).unwrap();
        let root = &nodes[0];
        let out = safe_property_block(
            xml,
            &nodes,
            root,
            &SAFE_PPR_CHILDREN,
            &REFUSED_PPR_CHILDREN,
            "p",
        )
        .unwrap();
        let s = String::from_utf8_lossy(&out);
        assert!(!s.contains("sectPr"));
        assert!(!s.contains("pPrChange"));
        assert!(s.contains("jc"));
        assert!(s.contains("spacing"));

        let rxml = b"<w:rPr><w:rPrChange w:id=\"2\"/><w:b/></w:rPr>";
        let rnodes = build_light_tree(&scan_tags(rxml)).unwrap();
        let rout = safe_property_block(
            rxml,
            &rnodes,
            &rnodes[0],
            &SAFE_RPR_CHILDREN,
            &REFUSED_RPR_CHILDREN,
            "r",
        )
        .unwrap();
        let rs = String::from_utf8_lossy(&rout);
        assert!(!rs.contains("rPrChange"));
        assert!(rs.contains("<w:b"));
    }

    #[test]
    fn format_policy_none_inherits_nothing() {
        let part = b"<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:body><w:p><w:pPr><w:jc w:val=\"center\"/><w:spacing w:after=\"120\"/><w:sectPr/><w:pPrChange w:id=\"1\"/></w:pPr><w:r><w:rPr><w:rPrChange w:id=\"2\"/><w:b/></w:rPr><w:t>x</w:t></w:r></w:p></w:body></w:document>";
        let located = locate_paragraphs(part, "document").unwrap();
        let paragraph = NewParagraph {
            id: "N1".to_string(),
            inherit: located[0].0.clone(),
            body: "hello".to_string(),
            insert_before: None,
            format_policy: FormatPolicy::None,
        };
        let out = render_new_paragraph(part, &located, &paragraph).unwrap();
        let s = String::from_utf8_lossy(&out);
        assert!(!s.contains("jc"));
        assert!(!s.contains("spacing"));
        assert!(!s.contains("sectPr"));
        assert!(!s.contains("pPrChange"));
        assert!(!s.contains("rPrChange"));
        assert!(!s.contains("<w:b"));
    }
    #[test]
    fn safe_synthesis_unknown_child_errors() {
        let xml = b"<w:pPr><w:foo/></w:pPr>";
        let nodes = build_light_tree(&scan_tags(xml)).unwrap();
        let root = &nodes[0];
        let err = safe_property_block(
            xml,
            &nodes,
            root,
            &SAFE_PPR_CHILDREN,
            &REFUSED_PPR_CHILDREN,
            "p",
        )
        .unwrap_err();
        assert!(err.contains("foo"));
    }

    /// The verifier resolves an island at the offset the island recorded, and
    /// that offset is a paragraph offset in the PROJECTION — where a tracked
    /// deletion is a zero-width gap. Counting the deleted characters instead put
    /// the anchor two places late, so the verifier looked for the edit's text in
    /// the wrong run and reported a template mismatch on a paragraph the applier
    /// had written correctly.
    #[test]
    fn an_anchor_after_a_tracked_deletion_resolves_the_run_the_applier_wrote() {
        let part = b"<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
                     <w:body><w:p><w:r><w:t>AB</w:t></w:r>\
                     <w:del w:id=\"1\" w:author=\"jinxq\"><w:r><w:delText>XY</w:delText></w:r></w:del>\
                     <w:r><w:t>CD</w:t></w:r></w:p></w:body></w:document>";
        let edit = |char_start| IslandEdit {
            part: "document".to_string(),
            paragraph_id: "P0".to_string(),
            leaf_index: 2,
            char_start: Some(char_start),
            old: "CD".to_string(),
            new: "CE".to_string(),
            tracked: false,
            author: String::new(),
            date: String::new(),
        };
        let ranges = edit_run_ranges(part, "document", &edit(2)).expect("the run the offset names");
        let text = String::from_utf8_lossy(&part[ranges[0].0..ranges[0].1]);
        assert_eq!(text, "CD");
        assert!(
            edit_run_ranges(part, "document", &edit(4)).is_err(),
            "an offset counted over the runs is a different address, not a second chance"
        );
    }
    /// Opaque-safety audit (blind-agent finding: `readings_disagree` is only an
    /// auxiliary Word-view comparison that ignores opaque markers, so it MUST
    /// NOT carry protection; see draft.rs `readings_disagree`). These tests call
    /// the REAL verifier entrypoint (`IndependentVerifier::verify`) on a legal
    /// edited output and then on tampered outputs, and name the exact failing
    /// gate. Threads: direct (non-tracked) island edit of P0 `world`->`WORLD`.
    ///
    /// Existing strong gates cited below (do NOT weaken them):
    /// - `edited-part-exact` (verify_edited, recompute_edited_part): the whole
    ///   edited part must equal the template part with exactly the recorded leaf
    ///   replacements applied, compared byte-for-byte. This is the gate that
    ///   protects opaque OPENING attributes, invented/duplicated nodes and any
    ///   other byte change inside a TARGETED paragraph.
    /// - `protected-structure-preserved` / `check_protected_structure`: EVERY
    ///   paragraph compared as an exact kind+identity/count/order list —
    ///   untouched and targeted alike. Revision wrappers stay transparent;
    ///   proof marks stay ignored; textbox-container edits stay exempt by
    ///   containment.
    /// - `opaque_identity`: pins the COMPLETE opaque XML (`open_start..end`):
    ///   interior bytes, closing tag AND the node's own opening-tag attributes.
    /// - `check_non_target_preserved`: text/run-properties outside edited
    ///   paragraphs must be unchanged; the edited paragraph itself is exempt.
    static OPAQUE_AUDIT_COUNTER: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);

    /// Reachability contract (core `resolve_leaf` / `opaque-paragraph-mutated`):
    /// a paragraph containing ANY opaque node is fully locked, so the targeted
    /// paragraph P0 carries plain text + bookmarks only (editable), and the two
    /// distinct drawings live in the NON-target paragraph P1. Genuine tracked
    /// P0 `world`->`WORLD` builds therefore succeed; tampering targets P1 for
    /// the untouched-paragraph exact gate and P0 for the targeted exact-list
    /// gate.
    const OPAQUE_AUDIT_TEMPLATE: &str = concat!(
        "<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\" xmlns:wp=\"http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing\">",
        "<w:body>",
        "<w:p><w:r><w:t>Hello </w:t></w:r><w:r><w:t>world</w:t></w:r>",
        "<w:bookmarkStart w:id=\"7\" w:name=\"markA\"/>",
        "<w:bookmarkEnd w:id=\"7\"/>",
        "</w:p>",
        "<w:p><w:r><w:t>Second paragraph</w:t></w:r>",
        "<w:r><w:drawing><wp:inline>SHAPE-A</wp:inline></w:drawing></w:r>",
        "<w:r><w:drawing><wp:inline>SHAPE-B</wp:inline></w:drawing></w:r>",
        "<w:bookmarkStart w:id=\"8\" w:name=\"markB\"/>",
        "<w:bookmarkEnd w:id=\"8\"/>",
        "</w:p>",
        "</w:body></w:document>",
    );

    const OPAQUE_AUDIT_DRAWING_A: &str =
        "<w:r><w:drawing><wp:inline>SHAPE-A</wp:inline></w:drawing></w:r>";
    const OPAQUE_AUDIT_DRAWING_B: &str =
        "<w:r><w:drawing><wp:inline>SHAPE-B</wp:inline></w:drawing></w:r>";

    fn opaque_audit_package(document_xml: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut zip_bytes = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut zip_bytes));
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            writer.start_file("[Content_Types].xml", options).unwrap();
            writer.write_all(br#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="xml" ContentType="application/xml"/></Types>"#).unwrap();
            writer.start_file("word/document.xml", options).unwrap();
            writer.write_all(document_xml).unwrap();
            writer.start_file("word/styles.xml", options).unwrap();
            writer.write_all(br#"<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"/>"#).unwrap();
            writer.finish().unwrap();
        }
        zip_bytes
    }

    fn opaque_audit_part(package: &[u8], member: &str) -> Vec<u8> {
        use std::io::Read;
        let mut archive =
            zip::ZipArchive::new(std::io::Cursor::new(package.to_vec())).expect("zip");
        for index in 0..archive.len() {
            let mut part = archive.by_index(index).expect("member");
            if part.name() == member {
                let mut buf = Vec::new();
                part.read_to_end(&mut buf).expect("read");
                return buf;
            }
        }
        panic!("missing member {member}");
    }

    fn opaque_audit_repack(template_pkg: &[u8], member: &str, bytes: &[u8]) -> Vec<u8> {
        use std::io::{Read, Write};
        let mut members: Vec<(String, Vec<u8>)> = Vec::new();
        {
            let mut archive = zip::ZipArchive::new(std::io::Cursor::new(template_pkg.to_vec()))
                .expect("template zip");
            for index in 0..archive.len() {
                let mut part = archive.by_index(index).expect("member");
                let name = part.name().to_string();
                let mut buf = Vec::new();
                part.read_to_end(&mut buf).expect("read");
                members.push((name, buf));
            }
        }
        let mut out = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut out));
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            for (name, data) in &members {
                writer.start_file(name.as_str(), options).unwrap();
                if name == member {
                    writer.write_all(bytes).unwrap();
                } else {
                    writer.write_all(data).unwrap();
                }
            }
            writer.finish().unwrap();
        }
        out
    }

    fn opaque_audit_setup(document_xml: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let n = OPAQUE_AUDIT_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "docx2typed-verify-opaque-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("scratch workdir");
        let package = opaque_audit_package(document_xml.as_bytes());
        std::fs::write(dir.join("_template.docx"), &package).expect("template");
        let islands = "{\"schema\":\"docx2typed-islands-1\",\"edits\":[{\"part\":\"document\",\"paragraph_id\":\"P0\",\"leaf_index\":1,\"old\":\"world\",\"new\":\"WORLD\",\"tracked\":false,\"author\":\"\",\"date\":\"\"}]}";
        std::fs::write(dir.join("islands.json"), islands).expect("islands");
        let output = dir.join(format!("output-{n}.docx"));
        std::fs::write(&output, &package).expect("output placeholder");
        (dir, output)
    }

    fn opaque_audit_legal_xml() -> String {
        OPAQUE_AUDIT_TEMPLATE.replace("<w:t>world</w:t>", "<w:t>WORLD</w:t>")
    }

    fn opaque_audit_verify(
        workdir: &std::path::Path,
        output: &std::path::Path,
    ) -> VerificationEvidence {
        IndependentVerifier::new().verify(&VerificationRequest {
            workdir: workdir.to_path_buf(),
            output: output.to_path_buf(),
            profile: "S".to_string(),
        })
    }

    fn opaque_audit_check(evidence: &VerificationEvidence, name: &str) -> String {
        evidence
            .checks
            .iter()
            .find(|check| check.name == name)
            .map(|check| check.status.clone())
            .unwrap_or_else(|| "missing".to_string())
    }

    fn opaque_audit_tampered_evidence(
        mutate: impl FnOnce(String) -> String,
    ) -> VerificationEvidence {
        let (workdir, output) = opaque_audit_setup(OPAQUE_AUDIT_TEMPLATE);
        let template_pkg = std::fs::read(workdir.join("_template.docx")).expect("template");
        let tampered_xml = mutate(opaque_audit_legal_xml());
        let out_pkg =
            opaque_audit_repack(&template_pkg, "word/document.xml", tampered_xml.as_bytes());
        std::fs::write(&output, &out_pkg).expect("output");
        opaque_audit_verify(&workdir, &output)
    }

    #[test]
    fn legal_edit_passes_and_leaves_non_target_protected_bytes_identical() {
        // Baseline: the recorded P0 text edit verifies clean, and the untouched
        // paragraph's protected XML is byte-identical (strong gate:
        // `edited-part-exact` byte equality + per-paragraph protected lists).
        let (workdir, output) = opaque_audit_setup(OPAQUE_AUDIT_TEMPLATE);
        let template_pkg = std::fs::read(workdir.join("_template.docx")).expect("template");
        let legal_xml = opaque_audit_legal_xml();
        let out_pkg = opaque_audit_repack(&template_pkg, "word/document.xml", legal_xml.as_bytes());
        std::fs::write(&output, &out_pkg).expect("output");
        let evidence = opaque_audit_verify(&workdir, &output);
        assert_eq!(
            evidence.verdict, "pass",
            "legal edit must verify clean: {:?}",
            evidence.checks
        );
        for name in [
            "parts-match-template-except-edited",
            "edited-part-exact",
            "protected-structure-preserved",
            "non-target-preserved",
        ] {
            assert_eq!(opaque_audit_check(&evidence, name), "pass", "{name}");
        }
        let template_part = opaque_audit_part(&template_pkg, "word/document.xml");
        let output_part = opaque_audit_part(&out_pkg, "word/document.xml");
        let template_ps = locate_paragraphs(&template_part, "document").expect("template ps");
        let output_ps = locate_paragraphs(&output_part, "document").expect("output ps");
        let template_p1 = template_ps
            .iter()
            .find(|(id, _, _)| id == "P1")
            .expect("template P1");
        let output_p1 = output_ps
            .iter()
            .find(|(id, _, _)| id == "P1")
            .expect("output P1");
        assert_eq!(
            &template_part[template_p1.1..template_p1.2],
            &output_part[output_p1.1..output_p1.2],
            "non-target paragraph P1 bytes are untouched by the P0 edit"
        );
        let before = protected_nodes(&template_part, "document").expect("before");
        let after = protected_nodes(&output_part, "document").expect("after");
        let shape = |nodes: &[ProtectedNode], id: &str| {
            nodes
                .iter()
                .filter(|node| node.paragraph_id == id)
                .map(|node| format!("{}:{}", node.kind, node.identity))
                .collect::<Vec<_>>()
        };
        assert_eq!(shape(&before, "P1"), shape(&after, "P1"), "P1 identities");
        assert_eq!(
            shape(&before, "P0"),
            shape(&after, "P0"),
            "P0 protected identities survive a legal text edit"
        );
    }

    #[test]
    fn removing_an_opaque_node_from_the_untouched_paragraph_is_refused() {
        // Drawings now live in non-target P1: removal trips BOTH the exact
        // recomputation (P1 bytes changed outside any edit) and the
        // per-paragraph-exact protected comparison for P1.
        let evidence =
            opaque_audit_tampered_evidence(|xml| xml.replace(OPAQUE_AUDIT_DRAWING_A, ""));
        assert_eq!(evidence.verdict, "fail", "deletion must not verify");
        assert_eq!(opaque_audit_check(&evidence, "edited-part-exact"), "fail");
        assert_eq!(
            opaque_audit_check(&evidence, "protected-structure-preserved"),
            "fail"
        );
    }

    #[test]
    fn swapping_two_distinct_opaque_nodes_is_refused() {
        // Count-preserving swap inside non-target P1: multiset identical, so
        // only RELATIVE ORDER protects it — the per-paragraph-exact comparison
        // fails alongside `edited-part-exact`.
        let evidence = opaque_audit_tampered_evidence(|xml| {
            let staged = xml.replace(OPAQUE_AUDIT_DRAWING_A, "@@OPAQUE-A@@");
            let staged = staged.replace(OPAQUE_AUDIT_DRAWING_B, OPAQUE_AUDIT_DRAWING_A);
            staged.replace("@@OPAQUE-A@@", OPAQUE_AUDIT_DRAWING_B)
        });
        assert_eq!(evidence.verdict, "fail", "reorder must not verify");
        assert_eq!(opaque_audit_check(&evidence, "edited-part-exact"), "fail");
        assert_eq!(
            opaque_audit_check(&evidence, "protected-structure-preserved"),
            "fail"
        );
    }

    #[test]
    fn replacing_a_protected_identity_while_keeping_text_is_refused() {
        // Attribute-only identity change on the P1 anchor (`markB` -> `markX`):
        // visible text unchanged, anchor identity rewritten. Exact and
        // protected gates both fail. (P0's `markA` variant is covered by the
        // tracked suite below.)
        let evidence = opaque_audit_tampered_evidence(|xml| {
            xml.replace("w:name=\"markB\"", "w:name=\"markX\"")
        });
        assert_eq!(evidence.verdict, "fail", "identity rewrite must not verify");
        assert_eq!(opaque_audit_check(&evidence, "edited-part-exact"), "fail");
        assert_eq!(
            opaque_audit_check(&evidence, "protected-structure-preserved"),
            "fail"
        );
    }

    #[test]
    fn outer_opening_attribute_change_is_refused_by_both_gates() {
        // `opaque_identity` pins the complete opaque XML (`open_start..end`),
        // so a `w:rsidR` smuggled onto the drawing run's opening tag changes
        // the identity: `protected-structure-preserved` fails alongside
        // `edited-part-exact` (measured: protected passed before the fix).
        let evidence = opaque_audit_tampered_evidence(|xml| {
            xml.replacen(
                "<w:r><w:drawing>",
                "<w:r w:rsidR=\"00FF0000\"><w:drawing>",
                1,
            )
        });
        assert_eq!(
            evidence.verdict, "fail",
            "attribute smuggling must not verify"
        );
        assert_eq!(opaque_audit_check(&evidence, "edited-part-exact"), "fail");
        assert_eq!(
            opaque_audit_check(&evidence, "protected-structure-preserved"),
            "fail"
        );
    }

    #[test]
    fn duplicating_a_protected_node_in_the_untouched_paragraph_is_refused() {
        // Duplication inside non-target P1: the per-paragraph-exact comparison
        // has a full count bound there, so protected fails alongside exact.
        let evidence = opaque_audit_tampered_evidence(|xml| {
            xml.replace(
                OPAQUE_AUDIT_DRAWING_A,
                &format!("{OPAQUE_AUDIT_DRAWING_A}{OPAQUE_AUDIT_DRAWING_A}"),
            )
        });
        assert_eq!(evidence.verdict, "fail", "duplication must not verify");
        assert_eq!(opaque_audit_check(&evidence, "edited-part-exact"), "fail");
        assert_eq!(
            opaque_audit_check(&evidence, "protected-structure-preserved"),
            "fail"
        );
    }

    #[test]
    fn inventing_a_protected_node_inside_the_edited_paragraph_is_refused() {
        // The targeted check is an exact list comparison (same identities,
        // same count, same order), so an invented drawing inside targeted P0
        // fails `protected-structure-preserved` alongside `edited-part-exact`
        // (measured: protected passed before the fix).
        let evidence = opaque_audit_tampered_evidence(|xml| {
            xml.replace(
                "<w:bookmarkEnd w:id=\"7\"/>",
                &format!("<w:bookmarkEnd w:id=\"7\"/>{OPAQUE_AUDIT_DRAWING_A}"),
            )
        });
        assert_eq!(evidence.verdict, "fail", "invention must not verify");
        assert_eq!(opaque_audit_check(&evidence, "edited-part-exact"), "fail");
        assert_eq!(
            opaque_audit_check(&evidence, "protected-structure-preserved"),
            "fail"
        );
    }

    #[test]
    fn duplicating_a_protected_node_inside_the_edited_paragraph_is_refused() {
        // Count guard on the direct path: doubling the P1 drawing (edited here
        // via the direct recomputation's P1 bytes) fails both gates; the
        // targeted-P0 count case is pinned by the tracked suite below.
        let evidence = opaque_audit_tampered_evidence(|xml| {
            xml.replace(
                OPAQUE_AUDIT_DRAWING_B,
                &format!("{OPAQUE_AUDIT_DRAWING_B}{OPAQUE_AUDIT_DRAWING_B}"),
            )
        });
        assert_eq!(evidence.verdict, "fail", "duplication must not verify");
        assert_eq!(opaque_audit_check(&evidence, "edited-part-exact"), "fail");
        assert_eq!(
            opaque_audit_check(&evidence, "protected-structure-preserved"),
            "fail"
        );
    }

    #[test]
    fn moving_a_protected_node_across_paragraphs_is_refused() {
        // Paragraph/source containment: identities carry `paragraph_id`, and
        // untouched paragraphs are compared per-paragraph exactly, so moving
        // SHAPE-A from P1 into targeted P0 breaks both sides at once.
        let evidence = opaque_audit_tampered_evidence(|xml| {
            let without = xml.replace(OPAQUE_AUDIT_DRAWING_A, "");
            without.replace(
                "<w:bookmarkEnd w:id=\"7\"/>",
                &format!("<w:bookmarkEnd w:id=\"7\"/>{OPAQUE_AUDIT_DRAWING_A}"),
            )
        });
        assert_eq!(
            evidence.verdict, "fail",
            "cross-paragraph move must not verify"
        );
        assert_eq!(opaque_audit_check(&evidence, "edited-part-exact"), "fail");
        assert_eq!(
            opaque_audit_check(&evidence, "protected-structure-preserved"),
            "fail"
        );
    }

    #[test]
    fn text_edit_crossing_a_protected_node_is_refused() {
        // A text edit that absorbs the bookmark pair it should have preserved:
        // the anchors are gone AND the edited text is longer than recorded, so
        // both the recomputation and the protected exact-list comparison refuse it.
        let evidence = opaque_audit_tampered_evidence(|xml| {
            let crossed = xml.replace(
                "<w:bookmarkStart w:id=\"7\" w:name=\"markA\"/><w:bookmarkEnd w:id=\"7\"/>",
                "",
            );
            crossed.replace("<w:t>WORLD</w:t>", "<w:t>WORLDX</w:t>")
        });
        assert_eq!(evidence.verdict, "fail", "cross-node edit must not verify");
        assert_eq!(opaque_audit_check(&evidence, "edited-part-exact"), "fail");
        assert_eq!(
            opaque_audit_check(&evidence, "protected-structure-preserved"),
            "fail"
        );
    }

    /// Tracked-path opaque-safety audit: same tamper classes, but the output
    /// under test is a GENUINE `apply_build` tracked result (`tracked: true`,
    /// author `audit-author`, date `2026-10-18`), not a hand-edited string. The
    /// tracked branch (`verify_edited`) calls `verify_tracked_part` on the
    /// OUTPUT part instead of `recompute_edited_part`, so `edited-part-exact`
    /// never runs there; the revision check proves the deletion/insertion texts
    /// plus the THIS-revision author/date agreement (`revision_for`).
    ///
    /// Strong gates on the tracked path:
    /// - `parts-match-template-except-edited`: only the recorded edited parts
    ///   may differ at all.
    /// - `protected-structure-preserved` / `check_protected_structure`: EVERY
    ///   paragraph compared as an exact identity/count/order list — untouched
    ///   and targeted alike. Revision wrappers stay transparent; proof marks
    ///   stay ignored; textbox-container edits stay exempt by containment.
    /// - `non-target-preserved` / `check_non_target_preserved`: text/run-props
    ///   outside edited spans unchanged (edited paragraph exempt by design).
    /// - `verify_tracked_part` / `revision_for` / `deletion_covers`: the output
    ///   must carry w:del+w:ins, the joined delText must equal `old`, one w:ins
    ///   must carry `new`, and every del of the run plus that ins must agree on
    ///   author/date with the sidecar.
    fn tracked_audit_sidecar_for(
        author: &str,
        date: &str,
        leaf_index: usize,
        old: &str,
        new: &str,
    ) -> String {
        format!(
            "{{\"schema\":\"docx2typed-islands-1\",\"edits\":[{{\
               \"part\":\"document\",\"paragraph_id\":\"P0\",\"leaf_index\":{leaf_index},\
               \"old\":\"{old}\",\"new\":\"{new}\",\"tracked\":true,\
               \"author\":\"{author}\",\"date\":\"{date}\"}}]}}"
        )
    }

    fn tracked_audit_sidecar(author: &str, date: &str) -> String {
        tracked_audit_sidecar_for(author, date, 1, "world", "WORLD")
    }

    const TRACKED_AUDIT_AUTHOR: &str = "audit-author";
    const TRACKED_AUDIT_DATE: &str = "2026-10-18";

    /// P0 carries no protected nodes at all — the empty-baseline case the old
    /// `if expect.is_empty() { continue; }` skip used to wave through.
    const TRACKED_AUDIT_PLAIN_TEMPLATE: &str = concat!(
        "<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">",
        "<w:body>",
        "<w:p><w:r><w:t>Just plain text here</w:t></w:r></w:p>",
        "</w:body></w:document>",
    );

    fn tracked_audit_setup_for(
        template_xml: &str,
        sidecar: &str,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let n = OPAQUE_AUDIT_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "docx2typed-verify-tracked-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("scratch workdir");
        let package = opaque_audit_package(template_xml.as_bytes());
        std::fs::write(dir.join("_template.docx"), &package).expect("template");
        std::fs::write(dir.join("islands.json"), sidecar).expect("islands");
        (dir.clone(), dir.join(format!("tracked-output-{n}.docx")))
    }

    fn tracked_audit_setup() -> (std::path::PathBuf, std::path::PathBuf) {
        tracked_audit_setup_for(
            OPAQUE_AUDIT_TEMPLATE,
            &tracked_audit_sidecar(TRACKED_AUDIT_AUTHOR, TRACKED_AUDIT_DATE),
        )
    }

    fn tracked_audit_edit_for(
        leaf_index: usize,
        old: &str,
        new: &str,
    ) -> docx2typed_core::prose::IslandEdit {
        docx2typed_core::prose::IslandEdit {
            part: "document".to_string(),
            paragraph_id: "P0".to_string(),
            leaf_index,
            old: old.to_string(),
            char_start: None,
            new: new.to_string(),
            tracked: true,
            author: TRACKED_AUDIT_AUTHOR.to_string(),
            date: TRACKED_AUDIT_DATE.to_string(),
        }
    }

    fn tracked_audit_edit() -> docx2typed_core::prose::IslandEdit {
        tracked_audit_edit_for(1, "world", "WORLD")
    }

    fn tracked_audit_genuine_output_for(
        workdir: &std::path::Path,
        edit: docx2typed_core::prose::IslandEdit,
    ) -> Vec<u8> {
        let template = workdir.join("_template.docx");
        let out = docx2typed_core::prose::apply_build(
            &template,
            &[edit],
            &docx2typed_core::prose::StructureEdits::default(),
        )
        .expect("genuine tracked build")
        .package;
        assert!(
            out.windows(b"<w:del ".len()).any(|w| w == b"<w:del "),
            "genuine output must carry a tracked deletion"
        );
        assert!(
            out.windows(b"<w:ins ".len()).any(|w| w == b"<w:ins "),
            "genuine output must carry a tracked insertion"
        );
        out
    }

    fn tracked_audit_genuine_output(workdir: &std::path::Path) -> Vec<u8> {
        tracked_audit_genuine_output_for(workdir, tracked_audit_edit())
    }

    fn tracked_audit_write(
        workdir: &std::path::Path,
        output: &std::path::Path,
        package: &[u8],
    ) -> VerificationEvidence {
        std::fs::write(output, package).expect("output");
        opaque_audit_verify(workdir, output)
    }

    fn tracked_audit_mutate(
        legal: &[u8],
        part: &str,
        mutate: impl FnOnce(String) -> String,
    ) -> Vec<u8> {
        let part_xml = String::from_utf8(opaque_audit_part(legal, part)).expect("utf8");
        opaque_audit_repack(legal, part, mutate(part_xml).as_bytes())
    }

    #[test]
    fn tracked_legal_output_passes_verification() {
        // Baseline: the genuine tracked build (w:del carrying `world`, w:ins
        // carrying `WORLD`, both stamped audit-author/2026-10-18; P0 bookmarks
        // are anchors, not opaque interiors, so core accepts the edit) verifies
        // clean, and non-target P1 (drawings + markB) is byte-identical to the
        // template — the drawings survived the tracked re-wrap untouched.
        let (workdir, output) = tracked_audit_setup();
        let legal = tracked_audit_genuine_output(&workdir);
        let evidence = tracked_audit_write(&workdir, &output, &legal);
        assert_eq!(
            evidence.verdict, "pass",
            "legal tracked output must verify clean: {:?}",
            evidence.checks
        );
        for name in [
            "parts-match-template-except-edited",
            "protected-structure-preserved",
            "non-target-preserved",
        ] {
            assert_eq!(opaque_audit_check(&evidence, name), "pass", "{name}");
        }
        // The tracked branch skips byte-exact recomputation by construction:
        // `edited-part-exact` is recorded as a pass with no exact comparison.
        let exact = evidence
            .checks
            .iter()
            .find(|check| check.name == "edited-part-exact")
            .expect("edited-part-exact present");
        assert_eq!(exact.status, "pass");
        let template_pkg = std::fs::read(workdir.join("_template.docx")).expect("template");
        let template_part = opaque_audit_part(&template_pkg, "word/document.xml");
        let output_part = opaque_audit_part(&legal, "word/document.xml");
        let template_ps = locate_paragraphs(&template_part, "document").expect("template ps");
        let output_ps = locate_paragraphs(&output_part, "document").expect("output ps");
        let template_p1 = template_ps
            .iter()
            .find(|(id, _, _)| id == "P1")
            .expect("template P1");
        let output_p1 = output_ps
            .iter()
            .find(|(id, _, _)| id == "P1")
            .expect("output P1");
        assert_eq!(
            &template_part[template_p1.1..template_p1.2],
            &output_part[output_p1.1..output_p1.2],
            "non-target paragraph P1 bytes are untouched by the tracked P0 edit"
        );
    }

    #[test]
    fn tracked_removing_an_opaque_node_from_the_untouched_paragraph() {
        // Drop SHAPE-A from P1 of the genuine tracked output: P1 is compared
        // per-paragraph exactly, so the protected gate must refuse it. The
        // revision gate still sees its del/ins texts and stays silent.
        let (workdir, output) = tracked_audit_setup();
        let legal = tracked_audit_genuine_output(&workdir);
        assert!(
            String::from_utf8_lossy(&opaque_audit_part(&legal, "word/document.xml"))
                .contains(OPAQUE_AUDIT_DRAWING_A)
        );
        let tampered = tracked_audit_mutate(&legal, "word/document.xml", |xml| {
            xml.replace(OPAQUE_AUDIT_DRAWING_A, "")
        });
        let evidence = tracked_audit_write(&workdir, &output, &tampered);
        let protected = opaque_audit_check(&evidence, "protected-structure-preserved");
        assert_eq!(
            protected, "fail",
            "dropping a protected node must be refused by the protected gate: {:?}",
            evidence.checks
        );
    }

    #[test]
    fn tracked_swapping_two_distinct_opaque_nodes_in_p1() {
        // Count-preserving A<->B swap inside non-target P1 of the genuine
        // tracked output: per-paragraph-exact comparison must refuse it.
        let (workdir, output) = tracked_audit_setup();
        let legal = tracked_audit_genuine_output(&workdir);
        let tampered = tracked_audit_mutate(&legal, "word/document.xml", |xml| {
            let staged = xml.replace(OPAQUE_AUDIT_DRAWING_A, "@@OPAQUE-A@@");
            let staged = staged.replace(OPAQUE_AUDIT_DRAWING_B, OPAQUE_AUDIT_DRAWING_A);
            staged.replace("@@OPAQUE-A@@", OPAQUE_AUDIT_DRAWING_B)
        });
        let evidence = tracked_audit_write(&workdir, &output, &tampered);
        let protected = opaque_audit_check(&evidence, "protected-structure-preserved");
        assert_eq!(
            protected, "fail",
            "reordering protected nodes must be refused by the protected gate: {:?}",
            evidence.checks
        );
    }

    #[test]
    fn tracked_relocating_opaque_group_within_untouched_p1() {
        // Same-paragraph relocation: BOTH drawings move as an ordered group to
        // BEFORE P1's text. Protected identities, count and relative order
        // (A,B,markB) are unchanged, and text/run-properties are untouched —
        // only the non-target XML bytes moved. The per-paragraph byte-exact
        // comparison must still refuse it: position inside the untouched
        // paragraph is protection-relevant, not just node identity.
        let (workdir, output) = tracked_audit_setup();
        let legal = tracked_audit_genuine_output(&workdir);
        let tampered = tracked_audit_mutate(&legal, "word/document.xml", |xml| {
            let text_run = "<w:r><w:t>Second paragraph</w:t></w:r>";
            assert!(
                xml.contains(text_run)
                    && xml.contains(OPAQUE_AUDIT_DRAWING_A)
                    && xml.contains(OPAQUE_AUDIT_DRAWING_B),
                "fixture shape changed"
            );
            let without_a = xml.replace(OPAQUE_AUDIT_DRAWING_A, "");
            let without_both = without_a.replace(OPAQUE_AUDIT_DRAWING_B, "");
            without_both.replacen(
                text_run,
                &format!("{OPAQUE_AUDIT_DRAWING_A}{OPAQUE_AUDIT_DRAWING_B}{text_run}"),
                1,
            )
        });
        let evidence = tracked_audit_write(&workdir, &output, &tampered);
        // Identity/order are legitimately unchanged here — the relocation keeps
        // the same node vector — so the protected gate is NOT the responsible
        // one; the non-target raw-XML gate carries this proof.
        assert_eq!(
            opaque_audit_check(&evidence, "protected-structure-preserved"),
            "pass",
            "relocation keeps identities/order: {:?}",
            evidence.checks
        );
        assert_eq!(
            opaque_audit_check(&evidence, "non-target-preserved"),
            "fail",
            "same-paragraph relocation must be refused by the non-target gate: {:?}",
            evidence.checks
        );
        assert_eq!(
            evidence.verdict, "fail",
            "tracked P1 relocation must not verify: {:?}",
            evidence.checks
        );
    }

    #[test]
    fn tracked_replacing_a_protected_identity_while_keeping_text() {
        // w:name markA (targeted P0) -> markX in the genuine tracked output:
        // visible text unchanged, anchor identity rewritten. The targeted
        // exact list finds no such node -> protected gate must refuse it.
        let (workdir, output) = tracked_audit_setup();
        let legal = tracked_audit_genuine_output(&workdir);
        let tampered = tracked_audit_mutate(&legal, "word/document.xml", |xml| {
            xml.replace("w:name=\"markA\"", "w:name=\"markX\"")
        });
        let evidence = tracked_audit_write(&workdir, &output, &tampered);
        let protected = opaque_audit_check(&evidence, "protected-structure-preserved");
        assert_eq!(
            protected, "fail",
            "identity rewrite must be refused by the protected gate: {:?}",
            evidence.checks
        );
    }

    #[test]
    fn tracked_outer_opening_attribute_change_on_opaque_run_in_p1() {
        // `opaque_identity` pins the complete opaque XML (`open_start..end`),
        // so a `w:rsidR` smuggled onto P1's drawing run opening tag changes
        // the identity and the protected gate fails (measured: the tampered
        // output verified `pass` before the fix).
        let (workdir, output) = tracked_audit_setup();
        let legal = tracked_audit_genuine_output(&workdir);
        let tampered = tracked_audit_mutate(&legal, "word/document.xml", |xml| {
            xml.replacen(
                "<w:r><w:drawing>",
                "<w:r w:rsidR=\"00FF0000\"><w:drawing>",
                1,
            )
        });
        let evidence = tracked_audit_write(&workdir, &output, &tampered);
        let protected = opaque_audit_check(&evidence, "protected-structure-preserved");
        assert_eq!(
            protected, "fail",
            "outer-attribute rewrite must be refused by the protected gate: {:?}",
            evidence.checks
        );
        assert_eq!(
            evidence.verdict, "fail",
            "tracked P1 outer-attribute tamper must not verify: {:?}",
            evidence.checks
        );
    }

    #[test]
    fn tracked_duplicating_a_protected_node_in_untouched_p1() {
        // Duplication inside non-target P1: the per-paragraph-exact comparison
        // has a full count bound there, so the protected gate must refuse it.
        let (workdir, output) = tracked_audit_setup();
        let legal = tracked_audit_genuine_output(&workdir);
        let tampered = tracked_audit_mutate(&legal, "word/document.xml", |xml| {
            xml.replace(
                OPAQUE_AUDIT_DRAWING_A,
                &format!("{OPAQUE_AUDIT_DRAWING_A}{OPAQUE_AUDIT_DRAWING_A}"),
            )
        });
        let evidence = tracked_audit_write(&workdir, &output, &tampered);
        let protected = opaque_audit_check(&evidence, "protected-structure-preserved");
        assert_eq!(
            protected, "fail",
            "duplication in an untouched paragraph must be refused: {:?}",
            evidence.checks
        );
    }

    #[test]
    fn tracked_moving_a_protected_node_from_p1_into_targeted_p0() {
        // SHAPE-A from untouched P1 into targeted P0 of the genuine tracked
        // output: P1's per-paragraph-exact list loses a node (and P0's gains
        // one), so the protected gate must refuse it.
        let (workdir, output) = tracked_audit_setup();
        let legal = tracked_audit_genuine_output(&workdir);
        let tampered = tracked_audit_mutate(&legal, "word/document.xml", |xml| {
            let without = xml.replace(OPAQUE_AUDIT_DRAWING_A, "");
            without.replace(
                "<w:bookmarkEnd w:id=\"7\"/>",
                &format!("<w:bookmarkEnd w:id=\"7\"/>{OPAQUE_AUDIT_DRAWING_A}"),
            )
        });
        let evidence = tracked_audit_write(&workdir, &output, &tampered);
        let protected = opaque_audit_check(&evidence, "protected-structure-preserved");
        assert_eq!(
            protected, "fail",
            "cross-paragraph move must be refused by the protected gate: {:?}",
            evidence.checks
        );
    }

    #[test]
    fn tracked_inventing_a_protected_node_inside_targeted_p0() {
        // EXTRA opaque smuggled into targeted P0 of the genuine tracked
        // output: the targeted list comparison is exact (count bound included),
        // so the protected gate fails (measured: the tampered output verified
        // `pass` before the fix).
        let (workdir, output) = tracked_audit_setup();
        let legal = tracked_audit_genuine_output(&workdir);
        let tampered = tracked_audit_mutate(&legal, "word/document.xml", |xml| {
            xml.replace(
                "<w:bookmarkEnd w:id=\"7\"/>",
                &format!("<w:bookmarkEnd w:id=\"7\"/>{OPAQUE_AUDIT_DRAWING_A}"),
            )
        });
        let evidence = tracked_audit_write(&workdir, &output, &tampered);
        let protected = opaque_audit_check(&evidence, "protected-structure-preserved");
        assert_eq!(
            protected, "fail",
            "invented node must be refused by the protected gate: {:?}",
            evidence.checks
        );
        assert_eq!(
            evidence.verdict, "fail",
            "tracked P0 invention tamper must not verify: {:?}",
            evidence.checks
        );
    }

    #[test]
    fn tracked_duplicating_a_bookmark_inside_targeted_p0() {
        // Count guard inside the targeted paragraph: doubling P0's markA
        // bookmark pair changes the targeted list's count, so the protected
        // gate fails even though the visible text is unchanged.
        let (workdir, output) = tracked_audit_setup();
        let legal = tracked_audit_genuine_output(&workdir);
        let tampered = tracked_audit_mutate(&legal, "word/document.xml", |xml| {
            xml.replace(
                "<w:bookmarkStart w:id=\"7\" w:name=\"markA\"/><w:bookmarkEnd w:id=\"7\"/>",
                "<w:bookmarkStart w:id=\"7\" w:name=\"markA\"/><w:bookmarkEnd w:id=\"7\"/>\
                     <w:bookmarkStart w:id=\"7\" w:name=\"markA\"/><w:bookmarkEnd w:id=\"7\"/>",
            )
        });
        let evidence = tracked_audit_write(&workdir, &output, &tampered);
        let protected = opaque_audit_check(&evidence, "protected-structure-preserved");
        assert_eq!(
            protected, "fail",
            "duplicated bookmark must be refused by the protected gate: {:?}",
            evidence.checks
        );
        assert_eq!(
            evidence.verdict, "fail",
            "tracked P0 duplication tamper must not verify: {:?}",
            evidence.checks
        );
    }

    #[test]
    fn tracked_inventing_a_protected_node_in_empty_baseline_p0() {
        // Same reproduced class on the EMPTY baseline the old skip hid: P0
        // starts with no protected nodes, so a smuggled drawing must fail the
        // exact list comparison (expect [] vs actual [opaque]). Genuine tracked
        // `plain`->`PLAIN` build, then the drawing is injected into P0.
        let (workdir, output) = tracked_audit_setup_for(
            TRACKED_AUDIT_PLAIN_TEMPLATE,
            &tracked_audit_sidecar_for(
                TRACKED_AUDIT_AUTHOR,
                TRACKED_AUDIT_DATE,
                0,
                "plain",
                "PLAIN",
            ),
        );
        let legal =
            tracked_audit_genuine_output_for(&workdir, tracked_audit_edit_for(0, "plain", "PLAIN"));
        let tampered = tracked_audit_mutate(&legal, "word/document.xml", |xml| {
            xml.replace(
                "<w:t>PLAIN</w:t>",
                &format!("<w:t>PLAIN</w:t>{OPAQUE_AUDIT_DRAWING_A}"),
            )
        });
        let evidence = tracked_audit_write(&workdir, &output, &tampered);
        let protected = opaque_audit_check(&evidence, "protected-structure-preserved");
        assert_eq!(
            protected, "fail",
            "invented node on an empty baseline must be refused: {:?}",
            evidence.checks
        );
        assert_eq!(
            evidence.verdict, "fail",
            "tracked empty-baseline invention must not verify: {:?}",
            evidence.checks
        );
    }

    #[test]
    fn duplicate_xml_space_in_the_edited_target_fails_the_xml_gate() {
        // Issue #70: a direct edit onto a `w:t` that already carries
        // `xml:space="preserve"` duplicated the attribute. The lenient byte
        // walker still matched the text, so every fidelity gate passed on
        // malformed XML. The XML gate must refuse it — and name the member.
        let evidence = opaque_audit_tampered_evidence(|xml| {
            xml.replace(
                "<w:t>WORLD</w:t>",
                "<w:t xml:space=\"preserve\" xml:space=\"preserve\">WORLD</w:t>",
            )
        });
        assert_eq!(evidence.verdict, "fail", "malformed XML must not verify");
        assert_eq!(
            opaque_audit_check(&evidence, "output-xml-wellformed"),
            "fail"
        );
        let detail = evidence
            .checks
            .iter()
            .find(|check| check.name == "output-xml-wellformed")
            .and_then(|check| check.detail.clone())
            .unwrap_or_default();
        assert!(
            detail.contains("word/document.xml"),
            "gate must name the offending member, got: {detail}"
        );
    }

    #[test]
    fn duplicate_attribute_in_an_untouched_member_fails_the_xml_gate() {
        // Same malformation in a member no edit touched (`word/styles.xml`):
        // still malformed, still refused — the gate covers the whole output,
        // not just the recorded target.
        let (workdir, output) = opaque_audit_setup(OPAQUE_AUDIT_TEMPLATE);
        let template_pkg = std::fs::read(workdir.join("_template.docx")).expect("template");
        let legal_xml = opaque_audit_legal_xml();
        let mut out_pkg =
            opaque_audit_repack(&template_pkg, "word/document.xml", legal_xml.as_bytes());
        let styles = opaque_audit_part(&template_pkg, "word/styles.xml");
        let mut tampered = String::from_utf8(styles).expect("styles utf8");
        tampered = tampered.replace(
            "<w:styles",
            "<w:styles xml:space=\"preserve\" xml:space=\"preserve\"",
        );
        out_pkg = opaque_audit_repack(&out_pkg, "word/styles.xml", tampered.as_bytes());
        std::fs::write(&output, &out_pkg).expect("output");
        let evidence = opaque_audit_verify(&workdir, &output);
        assert_eq!(evidence.verdict, "fail", "malformed XML must not verify");
        assert_eq!(
            opaque_audit_check(&evidence, "output-xml-wellformed"),
            "fail"
        );
    }

    #[test]
    fn valid_namespaced_xml_with_declarations_passes_the_xml_gate() {
        // The gate is strict, not trigger-happy: the legal edited output —
        // namespaces, preserved declarations, `xml:space` where the text
        // needs it — verifies clean.
        let (workdir, output) = opaque_audit_setup(OPAQUE_AUDIT_TEMPLATE);
        let template_pkg = std::fs::read(workdir.join("_template.docx")).expect("template");
        let legal_xml = opaque_audit_legal_xml();
        let spaced = legal_xml.replace(
            "<w:t>Hello </w:t>",
            "<w:t xml:space=\"preserve\">Hello </w:t>",
        );
        let out_pkg = opaque_audit_repack(&template_pkg, "word/document.xml", spaced.as_bytes());
        // A single `xml:space` is legal — but it is NOT the recorded edit, so
        // point the sidecar at the spaced text to keep the exact gate honest:
        // this test owns the XML gate, not the recomputation.
        std::fs::write(
            workdir.join("islands.json"),
            "{\"schema\":\"docx2typed-islands-1\",\"edits\":[{\"part\":\"document\",\"paragraph_id\":\"P0\",\"leaf_index\":0,\"old\":\"Hello \",\"new\":\"Hello \",\"tracked\":false,\"author\":\"\",\"date\":\"\"},{\"part\":\"document\",\"paragraph_id\":\"P0\",\"leaf_index\":1,\"old\":\"world\",\"new\":\"WORLD\",\"tracked\":false,\"author\":\"\",\"date\":\"\"}]}",
        )
        .expect("islands");
        std::fs::write(&output, &out_pkg).expect("output");
        let evidence = opaque_audit_verify(&workdir, &output);
        assert_eq!(
            opaque_audit_check(&evidence, "output-xml-wellformed"),
            "pass",
            "valid namespaced XML must pass the gate: {:?}",
            evidence.checks
        );
    }
    #[test]
    fn core_refuses_a_text_edit_inside_an_opaque_paragraph() {
        // Reachability-side proof, NOT a verifier case: a paragraph containing
        // an opaque drawing locks every leaf (core `resolve_leaf` ->
        // `opaque-paragraph-mutated`), so no genuine tracked output of that
        // shape exists. Anchors (bookmarks) do NOT lock: a text edit beside
        // them builds fine, which the legal-output tests above already prove.
        let dir = std::env::temp_dir().join(format!(
            "docx2typed-verify-cross-{}-{}",
            std::process::id(),
            OPAQUE_AUDIT_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).expect("scratch workdir");
        let package = opaque_audit_package(OPAQUE_AUDIT_TEMPLATE.as_bytes());
        let template = dir.join("_template.docx");
        std::fs::write(&template, &package).expect("template");
        let before = std::fs::read(&template).expect("template bytes");
        let err = docx2typed_core::prose::apply_edits(
            &template,
            &[docx2typed_core::prose::IslandEdit {
                part: "document".to_string(),
                paragraph_id: "P1".to_string(),
                leaf_index: 0,
                old: "Second paragraph".to_string(),
                char_start: None,
                new: "Second paragraph edited".to_string(),
                tracked: true,
                author: TRACKED_AUDIT_AUTHOR.to_string(),
                date: TRACKED_AUDIT_DATE.to_string(),
            }],
        )
        .expect_err("editing a paragraph with an opaque drawing must be refused");
        assert!(
            err.to_string().contains("opaque-paragraph-mutated"),
            "refusal must name the locked interior, got: {err}"
        );
        assert_eq!(
            std::fs::read(&template).expect("template bytes"),
            before,
            "a refused edit leaves the template untouched"
        );
    }
}
