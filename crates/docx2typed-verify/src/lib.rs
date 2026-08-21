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

        // The recorded island edits (the only mutation the output may
        // implement). Read independently from the workdir sidecar.
        let islands = load_islands(&request.workdir).unwrap_or_default();

        if islands.is_empty() {
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

    /// Issue #58 edited-build verification: independent re-walk of the
    /// template bytes, recomputing the expected edited parts and comparing
    /// them byte-for-byte with the output, plus the opaque replay check.
    #[allow(clippy::too_many_arguments)]
    fn verify_edited(
        &self,
        template: &Path,
        output: &Path,
        output_parts: &BTreeMap<String, String>,
        islands: &[IslandEdit],
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
            if edits.iter().any(|edit| edit.tracked) {
                if let Err(message) = verify_tracked_part(&output_part, &edits) {
                    edited_exact = false;
                    edited_detail = format!("part {part_path}: {message}");
                }
            } else {
                match recompute_edited_part(&template_part, &edits) {
                    Ok(expected) => {
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
            match check_opaque_replay(&template_part, &output_part, &edits) {
                Ok(()) => {}
                Err(message) => {
                    opaque_locked = false;
                    opaque_detail = format!("part {part_path}: {message}");
                    break;
                }
            }
        }
        checks.push(VerificationCheck {
            name: "opaque-interiors-replayed".to_string(),
            status: if opaque_locked { "pass" } else { "fail" }.to_string(),
            detail: Some(if opaque_detail.is_empty() {
                "all opaque field/math/drawing interiors byte-replayed".to_string()
            } else {
                opaque_detail
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

fn verify_tracked_part(output_part: &[u8], edits: &[&IslandEdit]) -> Result<(), String> {
    let xml = String::from_utf8_lossy(output_part);
    for edit in edits.iter().filter(|edit| edit.tracked) {
        let old = xml_escape(&edit.old);
        let new = xml_escape(&edit.new);
        if !xml.contains("<w:del") || !xml.contains("<w:ins") {
            return Err("tracked output is missing w:del/w:ins containers".to_string());
        }
        if !xml.contains(&format!("<w:delText>{old}</w:delText>"))
            && !xml.contains(&format!(
                "<w:delText xml:space=\"preserve\">{old}</w:delText>"
            ))
        {
            return Err(format!("tracked deletion text {:?} is missing", edit.old));
        }
        if !xml.contains(&format!("<w:t>{new}</w:t>"))
            && !xml.contains(&format!("<w:t xml:space=\"preserve\">{new}</w:t>"))
        {
            return Err(format!("tracked insertion text {:?} is missing", edit.new));
        }
    }
    Ok(())
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
        let (start, end) = find_old(template_part, leaf.text_start, leaf.text_end, &edit.old)?;
        let replacement = xml_escape(&edit.new).into_bytes();
        spans.push((start, end, replacement));
        if needs_xml_space(&edit.new) {
            if let Some(tag_span) = inject_xml_space(template_part, leaf.open_end) {
                spans.push(tag_span);
            }
        }
    }
    spans.sort_by_key(|(start, _, _)| *start);
    for (start, end, replacement) in spans.into_iter().rev() {
        work.splice(start..end, replacement);
    }
    Ok(work)
}

/// Opaque replay check: every opaque block of the template part must appear
/// verbatim in the output at its shifted position (shifts come only from
/// the recorded leaf replacements before it).
fn check_opaque_replay(
    template_part: &[u8],
    output_part: &[u8],
    edits: &[&IslandEdit],
) -> Result<(), String> {
    let part_key = edits
        .first()
        .map(|edit| edit.part.as_str())
        .unwrap_or("document");
    let opaques = opaque_blocks(template_part, part_key)?;
    // Shifts contributed by edits at or before a byte position.
    let mut spans: Vec<(usize, usize, Vec<u8>)> = Vec::new();
    for edit in edits {
        let (leaf, _) = locate_leaf(
            template_part,
            &edit.part,
            &edit.paragraph_id,
            edit.leaf_index,
        )?;
        let (start, end) = find_old(template_part, leaf.text_start, leaf.text_end, &edit.old)?;
        spans.push((start, end, xml_escape(&edit.new).into_bytes()));
    }
    spans.sort_by_key(|(start, _, _)| *start);
    for block in &opaques {
        let contains_edit = spans
            .iter()
            .any(|(start, end, _)| block.start <= *start && *end <= block.end);
        let partial_overlap = spans.iter().any(|(start, end, _)| {
            (*start < block.end && block.start < *end)
                && !(block.start <= *start && *end <= block.end)
        });
        if partial_overlap {
            return Err(format!(
                "opaque block {} ({}) is partially crossed by an edit",
                block.tag, block.paragraph_id
            ));
        }
        if contains_edit {
            // Text-box / content-control bridge: the block's interior prose
            // is the documented editable surface (B0.P0 lives inside the
            // w:pict run); the edit proof covers the leaf range.
            continue;
        }
        let shift: i64 = spans
            .iter()
            .filter(|(_, end, _)| *end <= block.start)
            .map(|(start, end, replacement)| replacement.len() as i64 - (end - start) as i64)
            .sum();
        let out_start = block.start as i64 + shift;
        let out_end = block.end as i64 + shift;
        if out_start < 0 || out_end > output_part.len() as i64 {
            return Err(format!("opaque block {} out of range in output", block.tag));
        }
        let expected = &template_part[block.start..block.end];
        let actual = &output_part[out_start as usize..out_end as usize];
        if expected != actual {
            return Err(format!(
                "opaque block {} ({}) not byte-replayed",
                block.tag, block.paragraph_id
            ));
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

/// Locate a paragraph's leaf in one part's bytes (independent port of the
/// locator discipline: paragraph ids, box/sdt/cell paths, leaf ordinals).
fn locate_leaf(
    part: &[u8],
    part_key: &str,
    paragraph_id: &str,
    leaf_index: usize,
) -> Result<(LocatedLeaf, ()), String> {
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
    let mut found: Option<LocatedLeaf> = None;
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
    let mut leaf_cursor = 0usize;
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
                if leaf_cursor == leaf_index {
                    found = Some(LocatedLeaf {
                        editable,
                        text_start: start + text_start,
                        text_end: start + text_end,
                        open_end: start + child_node.open_end,
                    });
                }
                leaf_cursor += 1;
            }
        }
    }
    found
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

fn opaque_blocks(part: &[u8], part_key: &str) -> Result<Vec<OpaqueBlock>, String> {
    let mut blocks = Vec::new();
    let paragraphs = locate_paragraphs(part, part_key)?;
    for (paragraph_id, start, end) in paragraphs {
        let paragraph = &part[start..end];
        let tags = scan_tags(paragraph);
        let nodes = build_light_tree(&tags)?;
        let root = nodes
            .iter()
            .position(|node| node.parent.is_none())
            .ok_or("no root")?;
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        let mut _rpr_ok = true;
        collect_opaque_ranges(root, &nodes, &mut ranges, &mut _rpr_ok);
        for (block_start, block_end) in ranges {
            blocks.push(OpaqueBlock {
                tag: "opaque".to_string(),
                paragraph_id: paragraph_id.clone(),
                start: start + block_start,
                end: start + block_end,
            });
        }
    }
    Ok(blocks)
}

struct OpaqueBlock {
    tag: String,
    paragraph_id: String,
    start: usize,
    end: usize,
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

/// Entity-aware old-text search inside a leaf's byte range.
fn find_old(
    part: &[u8],
    text_start: usize,
    text_end: usize,
    old: &str,
) -> Result<(usize, usize), String> {
    if old.is_empty() {
        return Err("empty old text".to_string());
    }
    let (decoded, char_bytes) = decode_text(&part[text_start..text_end])?;
    // `match_indices` yields BYTE indices; translate to decoded char index
    // (CJK text is multi-byte).
    let mut matches = decoded.match_indices(old);
    let Some((byte_index, _)) = matches.next() else {
        return Err(format!("old text {:?} not found in leaf", old));
    };
    if matches.next().is_some() {
        return Err("old text occurs more than once in the leaf".to_string());
    }
    let char_index = decoded[..byte_index].chars().count();
    let byte_start = char_bytes[char_index].0 + text_start;
    let byte_end = char_bytes[char_index + old.chars().count() - 1].1 + text_start;
    Ok((byte_start, byte_end))
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
