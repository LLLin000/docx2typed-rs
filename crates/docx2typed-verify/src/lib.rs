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

        // The recorded edits (the only mutation the output may implement).
        // Read independently from the workdir sidecars.
        let islands = load_islands(&request.workdir).unwrap_or_default();
        let structure = load_structure(&request.workdir).unwrap_or_default();

        if islands.is_empty() && structure.is_empty() {
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
    fn verify_edited(
        &self,
        template: &Path,
        output: &Path,
        output_parts: &BTreeMap<String, String>,
        islands: &[IslandEdit],
        structure: &StructureEdits,
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

        // Check 3: each edited part equals the template part with exactly
        // the recorded leaf replacements applied.
        let mut edited_exact = true;
        let mut edited_detail = String::new();
        let template_package = std::fs::read(template).unwrap_or_default();
        for part_path in &edited_part_paths {
            let Some(part_key) = part_key_from_path(part_path) else {
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
                        if expected != output_part {
                            edited_exact = false;
                            edited_detail = format!(
                                "part {part_path} differs from the independently recomputed edit"
                            );
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
                    "{} edited parts recomputed byte-exact",
                    edited_part_paths.len()
                )
            } else {
                edited_detail
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
            let Some(part_key) = part_key_from_path(part_path) else {
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
            let replay = baseline.and_then(|baseline| {
                check_protected_structure(&baseline, &output_part, &part_key, &targeted)
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
                check_non_target_preserved(&baseline, &output_part, &part_key, &targeted)
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
#[derive(Clone, Debug)]
struct NewParagraph {
    inherit: String,
    body: String,
    insert_before: Option<String>,
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
        structure.new_paragraphs.push(NewParagraph {
            inherit: entry.get("inherit")?.as_str()?.to_string(),
            body: entry.get("body")?.as_str()?.to_string(),
            insert_before: entry
                .get("insert_before")
                .and_then(|value| value.as_str())
                .map(str::to_string),
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
    let ppr = nodes[root]
        .children
        .iter()
        .find(|&&child| nodes[child].name == "pPr")
        .map(|&child| &inherit[nodes[child].open_start..nodes[child].end])
        .unwrap_or_default();
    let rpr = first_run_rpr(inherit, &nodes, root).unwrap_or_default();
    let mut out = strip_ephemeral_p_attrs(&opening);
    out.extend_from_slice(ppr);
    if !paragraph.body.is_empty() {
        out.extend_from_slice(b"<w:r>");
        out.extend_from_slice(rpr.as_bytes());
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
    for paragraph in &structure.new_paragraphs {
        let at = match &paragraph.insert_before {
            None => append_at,
            Some(anchor) => {
                let mut target = None;
                let mut index = index_of.get(anchor.as_str()).copied();
                while let Some(current) = index {
                    let (id, start, _) = &located[current];
                    if !deleted.contains(id.as_str()) {
                        target = Some(*start);
                        break;
                    }
                    index = Some(current + 1);
                }
                target.ok_or_else(|| format!("unknown insert_before paragraph: {anchor}"))?
            }
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
}

/// Every paragraph's shape, keyed by id.
fn paragraph_shapes(
    part: &[u8],
    part_key: &str,
) -> Result<std::collections::BTreeMap<String, ParagraphShape>, String> {
    let mut shapes = std::collections::BTreeMap::new();
    for (paragraph_id, start, end) in locate_paragraphs(part, part_key)? {
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
        shapes.insert(
            paragraph_id,
            ParagraphShape {
                text,
                run_properties,
            },
        );
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
/// The edited paragraphs are exempt on purpose: a tracked edit legitimately
/// re-wraps them, carrying each covered run's properties into the `w:del` and
/// the insertion, so their property MULTISET changes while the document's
/// meaning does not.
fn check_non_target_preserved(
    template_part: &[u8],
    output_part: &[u8],
    part_key: &str,
    targeted: &[String],
) -> Result<(), String> {
    let before = paragraph_shapes(template_part, part_key)?;
    let after = paragraph_shapes(output_part, part_key)?;
    let exempt = |id: &String| id.starts_with('N') || targeted.contains(id);
    for (id, shape) in &before {
        if exempt(id) {
            continue;
        }
        let Some(actual) = after.get(id) else {
            return Err(format!("{id} disappeared"));
        };
        if actual.text != shape.text {
            return Err(format!(
                "{id}: text outside the edit changed ({:?} -> {:?})",
                shape.text, actual.text
            ));
        }
        if actual.run_properties != shape.run_properties {
            return Err(format!(
                "{id}: run properties outside the edit changed ({} before, {} after)",
                shape.run_properties.len(),
                actual.run_properties.len()
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
#[derive(Clone, Debug, PartialEq, Eq)]
struct ProtectedNode {
    paragraph_id: String,
    /// Element name for an anchor, `opaque` for a locked interior.
    kind: String,
    /// The attributes that make this node that node (`w:id`, `w:name`, the
    /// hyperlink's `r:id`), or the bytes' digest for an opaque interior.
    identity: String,
}

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
    let bytes = &part[paragraph_start + node.open_end..paragraph_start + node.end];
    ProtectedNode {
        paragraph_id: paragraph_id.to_string(),
        kind: "opaque".to_string(),
        identity: format!("{}:{}", node.name, bytes_sha256(bytes)),
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
/// reordered — and it must survive RE-WRAPPING, which is what a tracked edit
/// does to everything around it.
///
/// `targeted` names the paragraphs the edit declared. In those, the template's
/// protected nodes must appear in the same relative order (an edit may move
/// them; it may not drop, reorder or invent them). Everywhere else the lists
/// must be equal outright. Synthesised paragraphs (`N`) have no baseline and are
/// out of scope.
fn check_protected_structure(
    template_part: &[u8],
    output_part: &[u8],
    part_key: &str,
    targeted: &[String],
) -> Result<(), String> {
    let before = protected_nodes(template_part, part_key)?;
    let after = protected_nodes(output_part, part_key)?;
    let is_targeted = |node: &ProtectedNode| {
        node.paragraph_id.starts_with('N') || targeted.contains(&node.paragraph_id)
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

    // Edited paragraphs: the template's nodes must still be there, in order.
    for paragraph_id in targeted {
        let expect: Vec<&ProtectedNode> = before
            .iter()
            .filter(|node| node.paragraph_id == *paragraph_id)
            .collect();
        if expect.is_empty() {
            continue;
        }
        let actual: Vec<&ProtectedNode> = after
            .iter()
            .filter(|node| node.paragraph_id == *paragraph_id)
            .collect();
        let mut cursor = 0usize;
        for node in &expect {
            match actual[cursor..]
                .iter()
                .position(|candidate| candidate == node)
            {
                Some(offset) => cursor += offset + 1,
                None => {
                    return Err(format!(
                        "{paragraph_id}: protected {} {} did not survive the edit \
                         (present after: {:?})",
                        node.kind,
                        node.identity,
                        actual
                            .iter()
                            .map(|candidate| format!("{}:{}", candidate.kind, candidate.identity))
                            .collect::<Vec<_>>()
                    ));
                }
            }
        }
    }
    Ok(())
}

/// One located leaf with its byte ranges (independent walker).
struct LocatedLeaf {
    editable: bool,
    text_start: usize,
    text_end: usize,
    open_end: usize,
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
    for node in nodes.iter() {
        if node.name != "r" {
            continue;
        }
        // a run is opaque when any child is unknown
        let run_opaque = node.children.iter().any(|&child| {
            let child_node = &nodes[child];
            !KNOWN_RUN_CHILDREN.contains(&child_node.name.as_str())
        });
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
struct LightNode {
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
    if part[..open_end]
        .windows(18)
        .any(|w| w == b"xml:space=\"preserve\"")
    {
        return None;
    }
    let tag_start = part[..open_end].iter().rposition(|&b| b == b'<')?;
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
    let mut found: Option<Vec<(usize, usize)>> = None;
    for start in edit.leaf_index..leaves.len() {
        let first = &leaves[start];
        let (_, first_chars) = decode_text(&part[first.text_start..first.text_end])?;
        let first_len = first_chars.len();
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
            let Some(byte_pos) = accumulated.find(&edit.old) else {
                continue;
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
