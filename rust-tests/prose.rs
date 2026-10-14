//! Issue #58 binary-level tracer: recursive prose enumeration, island-local
//! text edits (CLI `edit text`), store generation commit, edited builds,
//! independent verification, and the fail-closed rejection paths (opaque
//! lock, missing/ambiguous old text, global invariant gate) — driven
//! through the installed-style `docx2typed` binary exactly as the
//! differential gate does.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_docx2typed")
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fixture(name: &str) -> PathBuf {
    repo_root().join(format!("corpus/release/{name}.docx"))
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("docx2typed-prose-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// Run the Rust binary with `--json` and parse the envelope.
fn rust_json(args: &[&str]) -> (i32, Value) {
    let output: Output = Command::new(bin())
        .args(args)
        .output()
        .expect("binary runs");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let value: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|error| panic!("binary JSON parse failed: {error}; output: {stdout}"));
    (output.status.code().unwrap_or(-1), value)
}

fn op_id() -> String {
    docx2typed_protocol::new_operation_id()
}

fn zip_part_hashes(path: &Path) -> BTreeMap<String, String> {
    let file = std::fs::File::open(path).expect("open docx");
    let mut archive = zip::ZipArchive::new(file).expect("zip");
    let mut hashes = BTreeMap::new();
    for index in 0..archive.len() {
        let mut member = archive.by_index(index).expect("member");
        let name = member.name().to_string();
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut member, &mut bytes).expect("read");
        use sha2::{Digest, Sha256};
        hashes.insert(name, hex::encode(Sha256::digest(&bytes)));
    }
    hashes
}

/// Frozen Python Reference typed.md paragraph id lists for the fixtures
/// (extract order, captured 2026-08-14).
fn expected_ids(name: &str) -> &'static [&'static str] {
    match name {
        "plain" => &["P0", "P1", "P2", "P3", "P4", "P5"],
        "table" => &[
            "P0",
            "T0.R0.C0.P0",
            "T0.R0.C1.P0",
            "T0.R0.C2.P0",
            "T0.R1.C0.P0",
            "T0.R1.C1.P0",
            "T1.R0.C0.P0",
            "T0.R1.C2.P0",
            "T0.R2.C0.P0",
            "T0.R2.C1.P0",
            "T0.R2.C2.P0",
            "T2.R0.C0.P0",
            "T2.R0.C1.P0",
            "T2.R0.C2.P0",
            "T2.R1.C0.P0",
            "T2.R1.C1.P0",
            "T2.R1.C2.P0",
            "T2.R2.C0.P0",
            "T2.R2.C1.P0",
            "T2.R2.C2.P0",
            "P1",
        ],
        "boxes" => &["P0", "B0.P0", "P1", "P2"],
        "parts" => &[
            "header1.P0",
            "P0",
            "P1",
            "P2",
            "footer1.P0",
            "endnotes.P0",
            "endnotes.P1",
            "endnotes.P2",
            "footnotes.P0",
            "footnotes.P1",
            "footnotes.P2",
        ],
        "complex" => &[
            "header1.P0",
            "header2.P0",
            "P0",
            "P1",
            "P2",
            "P3",
            "P4",
            "P5",
            "P6",
            "P7",
            "P8",
            "T0.R0.C0.P0",
            "T0.R0.C1.P0",
            "T0.R0.C2.P0",
            "T0.R0.C3.P0",
            "T0.R1.C0.P0",
            "T0.R1.C1.P0",
            "T0.R1.C2.P0",
            "T0.R1.C3.P0",
            "T0.R2.C0.P0",
            "T0.R2.C1.P0",
            "T0.R2.C2.P0",
            "T0.R2.C3.P0",
            "T0.R3.C0.P0",
            "T1.R0.C0.P0",
            "T1.R0.C1.P0",
            "T1.R1.C0.P0",
            "T1.R1.C1.P0",
            "T0.R3.C1.P0",
            "T0.R3.C2.P0",
            "T0.R3.C3.P0",
            "P9",
            "P10",
            "P11",
            "P12",
            "P13",
            "P14",
            "P15",
            "P16",
            "P17",
            "P18",
            "P19",
            "S0.P0",
            "footer1.P0",
            "endnotes.P0",
            "endnotes.P1",
            "endnotes.P2",
            "footnotes.P0",
            "footnotes.P1",
            "footnotes.P2",
            "comments.P0",
        ],
        other => panic!("unknown fixture {other}"),
    }
}

fn extract(fixture_path: &Path, outdir: &Path) {
    let (rc, envelope) = rust_json(&[
        "extract",
        "--json",
        fixture_path.to_str().expect("utf8"),
        "-o",
        outdir.to_str().expect("utf8"),
    ]);
    assert_eq!(rc, 0, "extract failed: {envelope}");
    assert_eq!(envelope["outcome"], "success");
}

fn edit_text(workdir: &Path, leaf: &str, old: &str, new: &str) -> (i32, Value) {
    rust_json(&[
        "edit",
        "text",
        "--json",
        workdir.to_str().expect("utf8"),
        leaf,
        old,
        new,
        "--operation-id",
        &op_id(),
    ])
}

// ---------------------------------------------------------------------------
// Bullet 1: enumeration parity with Python's typed paths
// ---------------------------------------------------------------------------

#[test]
fn enumerate_matches_python_typed_paths_on_all_fixtures() {
    for name in ["plain", "table", "boxes", "parts", "complex"] {
        let (rc, envelope) =
            rust_json(&["enumerate", "--json", fixture(name).to_str().expect("utf8")]);
        assert_eq!(rc, 0, "enumerate {name} failed: {envelope}");
        let data = envelope["data"].clone();
        let ids: Vec<String> = data["paragraphs"]
            .as_array()
            .expect("paragraphs")
            .iter()
            .map(|paragraph| paragraph["id"].as_str().expect("id").to_string())
            .collect();
        let expected: Vec<&str> = expected_ids(name).to_vec();
        assert_eq!(ids, expected, "typed paths differ for {name}");
    }
}

#[test]
fn enumerate_reports_editable_islands_and_opaque_blocks() {
    let (rc, envelope) = rust_json(&[
        "enumerate",
        "--json",
        fixture("complex").to_str().expect("utf8"),
    ]);
    assert_eq!(rc, 0);
    let data = envelope["data"].clone();
    let leaves = data["leaves"].as_array().expect("leaves");
    let editable: Vec<&Value> = leaves
        .iter()
        .filter(|leaf| leaf["editable"] == Value::Bool(true))
        .collect();
    assert!(!editable.is_empty(), "complex has editable leaves");
    let locked: Vec<&Value> = leaves
        .iter()
        .filter(|leaf| leaf["editable"] != Value::Bool(true))
        .collect();
    assert!(!locked.is_empty(), "opaque interiors produce locked leaves");
    // The FIELD-SLOT paragraph is locked (fldSimple); its leaf is locked.
    let field_leaf = leaves
        .iter()
        .find(|leaf| leaf["paragraph"] == "P12")
        .expect("P12 leaf");
    assert_eq!(field_leaf["editable"], Value::Bool(false));
    let opaques = data["opaques"].as_array().expect("opaques");
    assert!(!opaques.is_empty());
    let fld_simple = opaques
        .iter()
        .find(|opaque| opaque["paragraph"] == "P12")
        .expect("fldSimple opaque block");
    assert_eq!(fld_simple["tag"], "w:fldSimple");
}

// ---------------------------------------------------------------------------
// Bullet 2: island-local edits with byte replay; fail-closed rejections
// ---------------------------------------------------------------------------

#[test]
fn island_edit_build_verify_full_chain() {
    let dir = scratch("chain");
    let workdir = dir.join("wd");
    extract(&fixture("table"), &workdir);
    let (rc, envelope) = edit_text(&workdir, "T0.R1.C1.P0.0", "PVA", "PLBA");
    assert_eq!(rc, 0, "edit failed: {envelope}");
    assert_eq!(envelope["outcome"], "success");
    assert_eq!(envelope["data"]["changed"][0], "T0.R1.C1.P0.0");
    // The store committed a generation carrying the islands sidecar.
    assert!(workdir.join("islands.json").is_file());
    assert!(workdir.join(".docx2typed-store/generations").is_dir());

    let output = dir.join("out.docx");
    let (rc, build) = rust_json(&[
        "build",
        "--json",
        workdir.to_str().expect("utf8"),
        "-o",
        output.to_str().expect("utf8"),
        "--operation-id",
        &op_id(),
    ]);
    assert_eq!(rc, 0, "build failed: {build}");
    assert_eq!(build["outcome"], "success");

    // Bullet 2: the rest of the package replays byte-identically; only
    // word/document.xml changes, and only in the edited leaf.
    let source = zip_part_hashes(&fixture("table"));
    let built = zip_part_hashes(&output);
    let changed: Vec<String> = source
        .keys()
        .filter(|name| source.get(*name) != built.get(*name))
        .cloned()
        .collect();
    assert_eq!(changed, vec!["word/document.xml"]);
    assert_eq!(source.len(), built.len(), "no parts added or removed");

    // The independent verifier passes with the edited-build checks.
    let (rc, verify) = rust_json(&[
        "verify",
        "--json",
        workdir.to_str().expect("utf8"),
        output.to_str().expect("utf8"),
    ]);
    assert_eq!(rc, 0, "verify failed: {verify}");
    assert_eq!(verify["outcome"], "success");
    let verifier_checks = verify["evidence"][0]["payload"]["verifier_checks"]
        .as_array()
        .expect("verifier_checks");
    let names: Vec<String> = verifier_checks
        .iter()
        .map(|check| check["name"].as_str().unwrap().to_string())
        .collect();
    for required in [
        "package-openable",
        "parts-match-template-except-edited",
        "edited-part-exact",
        "protected-structure-preserved",
    ] {
        assert!(
            names.iter().any(|name| name == required),
            "missing {required}"
        );
    }
    assert!(
        verifier_checks
            .iter()
            .all(|check| check["status"] == "pass"),
        "all verifier checks pass: {verifier_checks:?}"
    );
}

#[test]
fn island_edit_on_header_part_replays_body() {
    let dir = scratch("header");
    let workdir = dir.join("wd");
    extract(&fixture("parts"), &workdir);
    let (rc, envelope) = edit_text(&workdir, "header1.P0.0", "Draft v1", "Confidential v1");
    assert_eq!(rc, 0, "header edit failed: {envelope}");
    let output = dir.join("out.docx");
    let (rc, build) = rust_json(&[
        "build",
        "--json",
        workdir.to_str().expect("utf8"),
        "-o",
        output.to_str().expect("utf8"),
        "--operation-id",
        &op_id(),
    ]);
    assert_eq!(rc, 0, "build failed: {build}");
    let source = zip_part_hashes(&fixture("parts"));
    let built = zip_part_hashes(&output);
    let changed: Vec<String> = source
        .keys()
        .filter(|name| source.get(*name) != built.get(*name))
        .cloned()
        .collect();
    assert_eq!(changed, vec!["word/header1.xml"]);
    let (rc, verify) = rust_json(&[
        "verify",
        "--json",
        workdir.to_str().expect("utf8"),
        output.to_str().expect("utf8"),
    ]);
    assert_eq!(rc, 0, "verify failed: {verify}");
    assert_eq!(verify["outcome"], "success");
}

#[test]
fn island_edit_rejects_locked_opaque_leaf() {
    let dir = scratch("locked");
    let workdir = dir.join("wd");
    extract(&fixture("complex"), &workdir);
    // P12 is the fldSimple paragraph: its leaf is inside locked structure.
    let (rc, envelope) = edit_text(&workdir, "P12.0", "FIELD", "XXX");
    assert_eq!(rc, 1, "locked edit must fail");
    assert_eq!(
        envelope["diagnostics"][0]["code"],
        "opaque-paragraph-mutated"
    );
    assert!(
        !workdir.join("islands.json").exists(),
        "no sidecar on rejection"
    );
}

#[test]
fn island_edit_rejects_missing_and_ambiguous_old() {
    let dir = scratch("missing");
    let workdir = dir.join("wd");
    extract(&fixture("plain"), &workdir);
    let (rc, envelope) = edit_text(&workdir, "P0.0", "nonexistent text", "x");
    assert_eq!(rc, 1);
    assert_eq!(envelope["diagnostics"][0]["code"], "invalid-edit");

    // P5 text is "重复句子内容 重复句子内容。": old occurs twice in the leaf.
    let (rc, envelope) = edit_text(&workdir, "P5.0", "重复句子内容", "x");
    assert_eq!(rc, 1);
    assert_eq!(envelope["diagnostics"][0]["code"], "prose-edit-ambiguous");
}

#[test]
fn island_edit_rejects_unknown_leaf_path() {
    let dir = scratch("badpath");
    let workdir = dir.join("wd");
    extract(&fixture("plain"), &workdir);
    let (rc, envelope) = edit_text(&workdir, "P99.0", "x", "y");
    assert_eq!(rc, 1);
    assert_eq!(envelope["diagnostics"][0]["code"], "invalid-edit");
}

// ---------------------------------------------------------------------------
// Bullet 3: global invariant gate rejects the whole build with no output
// ---------------------------------------------------------------------------

#[test]
fn global_invariant_gate_rejects_locked_island_with_no_output() {
    let dir = scratch("invariant");
    let workdir = dir.join("wd");
    extract(&fixture("complex"), &workdir);
    // Hand-write a sidecar that records an edit into the locked fldSimple
    // paragraph (what a tampered or stale workdir would carry).
    std::fs::write(
        workdir.join("islands.json"),
        r#"{
  "schema": "docx2typed-islands-1",
  "edits": [
    {"part": "document", "paragraph_id": "P12", "leaf_index": 0, "old": "FIELD", "new": "XXX"}
  ]
}
"#,
    )
    .unwrap();
    let output = dir.join("out.docx");
    let (rc, build) = rust_json(&[
        "build",
        "--json",
        workdir.to_str().expect("utf8"),
        "-o",
        output.to_str().expect("utf8"),
        "--operation-id",
        &op_id(),
    ]);
    assert_eq!(rc, 1, "build must reject the invariant violation");
    assert_eq!(build["diagnostics"][0]["code"], "opaque-paragraph-mutated");
    assert!(!output.exists(), "no output on invariant failure");
}

// ---------------------------------------------------------------------------
// Bullet 4 groundwork: store generation commit + operation-id replay
// ---------------------------------------------------------------------------

#[test]
fn island_edit_commits_generation_and_replays_operation_id() {
    let dir = scratch("gen");
    let workdir = dir.join("wd");
    extract(&fixture("plain"), &workdir);
    let op = op_id();
    let (rc, first) = rust_json(&[
        "edit",
        "text",
        "--json",
        workdir.to_str().expect("utf8"),
        "P0.0",
        "20 mg",
        "25 mg",
        "--operation-id",
        &op,
    ]);
    assert_eq!(rc, 0, "first edit failed: {first}");
    let gens_after_first = std::fs::read_dir(workdir.join(".docx2typed-store/generations"))
        .expect("generations")
        .count();
    let (rc2, replay) = rust_json(&[
        "edit",
        "text",
        "--json",
        workdir.to_str().expect("utf8"),
        "P0.0",
        "20 mg",
        "25 mg",
        "--operation-id",
        &op,
    ]);
    assert_eq!(rc2, 0, "replay failed: {replay}");
    assert_eq!(first, replay, "identical retry must replay the envelope");
    let gens_after_replay = std::fs::read_dir(workdir.join(".docx2typed-store/generations"))
        .expect("generations")
        .count();
    assert_eq!(gens_after_replay, gens_after_first, "no duplicate effect");
}

#[test]
fn two_island_edits_accumulate_and_build_applies_both() {
    let dir = scratch("two");
    let workdir = dir.join("wd");
    extract(&fixture("table"), &workdir);
    let (rc, first) = edit_text(&workdir, "T0.R1.C1.P0.0", "PVA", "PLBA");
    assert_eq!(rc, 0, "{first}");
    let (rc, second) = edit_text(&workdir, "T2.R2.C1.P0.0", "PVA", "XYZ");
    assert_eq!(rc, 0, "{second}");
    let islands = std::fs::read_to_string(workdir.join("islands.json")).unwrap();
    assert_eq!(islands.matches("\"paragraph_id\"").count(), 2);
    let output = dir.join("out.docx");
    let (rc, build) = rust_json(&[
        "build",
        "--json",
        workdir.to_str().expect("utf8"),
        "-o",
        output.to_str().expect("utf8"),
        "--operation-id",
        &op_id(),
    ]);
    assert_eq!(rc, 0, "build failed: {build}");
    let (rc, verify) = rust_json(&[
        "verify",
        "--json",
        workdir.to_str().expect("utf8"),
        output.to_str().expect("utf8"),
    ]);
    assert_eq!(rc, 0, "verify failed: {verify}");
    // Both cells changed; no PVA remains.
    let bytes = std::fs::read(&output).unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("PLBA"));
    assert!(text.contains("XYZ"));
    assert!(!text.contains("PVA"));
}
