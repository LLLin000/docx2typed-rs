//! PR 3 structural-closure tracer: a new or deleted paragraph must really
//! reach the built DOCX, driven through the installed-style `docx2typed`
//! binary exactly as the qualification gate does.
//!
//! The failure this tracer pins: `insert_paragraph` + `commit_sync` used to
//! succeed while `build_docx` refused with `edits-not-implemented` — a new
//! paragraph has no baseline byte range, so it cannot be expressed as an
//! island (leaf byte-replace) edit. The synthesis path renders
//! `p_open + pPr + run + </w:p>` from the inherit paragraph, splices it
//! before the next surviving baseline paragraph, and records the edit in
//! `structure.json`; deletion is "not rendered". Everything outside
//! `word/document.xml` must stay byte-identical, and unsupported structural
//! deletions (section properties, tracked revisions, anchors, tables) must
//! refuse instead of silently dropping content.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{json, Value};

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
    let dir = std::env::temp_dir().join(format!("docx2typed-pr3-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

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

fn extract(fixture_path: &Path, outdir: &Path) {
    let (rc, envelope) = rust_json(&[
        "extract",
        "--json",
        fixture_path.to_str().expect("utf8"),
        "-o",
        outdir.to_str().expect("utf8"),
    ]);
    assert_eq!(rc, 0, "extract failed: {envelope}");
    assert_eq!(envelope["outcome"], "success", "{envelope}");
}

/// Drive one MCP(stdout) session: each stdin line is
/// `{"tool","args"}`, each reply `OK <json>` / `ERR <msg>`.
fn mcp(requests: &[(&str, Value)]) -> Vec<Value> {
    let mut input = String::new();
    for (tool, args) in requests {
        input.push_str(&format!("{}\n", json!({ "tool": tool, "args": args })));
    }
    let mut child = Command::new(bin())
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mcp");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .expect("stdout")
        .read_to_string(&mut stdout)
        .expect("read stdout");
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("stderr")
        .read_to_string(&mut stderr)
        .expect("read stderr");
    let status = child.wait().expect("wait");
    assert!(status.success(), "mcp exited {status:?}: {stderr}");
    stdout
        .lines()
        .filter_map(|line| line.strip_prefix("OK "))
        .map(|reply| {
            serde_json::from_str(reply)
                .unwrap_or_else(|error| panic!("mcp reply parse failed: {error}; line: {reply}"))
        })
        .collect()
}

fn envelope(result: &Value) -> &Value {
    &result["structuredContent"]
}

/// Assert one tool succeeded and return its `data` payload.
fn ok(result: &Value) -> &Value {
    let envelope = envelope(result);
    assert_eq!(
        envelope["outcome"], "success",
        "{} failed: {envelope}",
        envelope["operation"]
    );
    &envelope["data"]
}

/// Assert one tool failed with `code` and return its diagnostic message.
fn refused(result: &Value, code: &str) -> String {
    let envelope = envelope(result);
    assert_eq!(
        envelope["outcome"], "failure",
        "{} should have been refused: {envelope}",
        envelope["operation"]
    );
    let diagnostic = &envelope["diagnostics"][0];
    assert_eq!(diagnostic["code"], code, "{envelope}");
    diagnostic["message"].as_str().unwrap_or("").to_string()
}

fn zip_parts(path: &Path) -> BTreeMap<String, Vec<u8>> {
    let file = std::fs::File::open(path).expect("open package");
    let mut archive = zip::ZipArchive::new(file).expect("zip package");
    let mut parts = BTreeMap::new();
    for index in 0..archive.len() {
        let mut member = archive.by_index(index).expect("member");
        let name = member.name().to_string();
        let mut bytes = Vec::new();
        member.read_to_end(&mut bytes).expect("member bytes");
        parts.insert(name, bytes);
    }
    parts
}

fn document_xml(path: &Path) -> String {
    let parts = zip_parts(path);
    String::from_utf8(parts["word/document.xml"].clone()).expect("document.xml is UTF-8")
}

/// Every part that differs from the template package.
fn changed_parts(template: &Path, output: &Path) -> Vec<String> {
    let before = zip_parts(template);
    let after = zip_parts(output);
    assert_eq!(
        before.keys().collect::<Vec<_>>(),
        after.keys().collect::<Vec<_>>(),
        "the member list must not change"
    );
    before
        .iter()
        .filter(|(name, bytes)| after[*name] != **bytes)
        .map(|(name, _)| name.clone())
        .collect()
}

fn body(document: &str) -> &str {
    let start = document.find("<w:body>").expect("body");
    let end = document.find("</w:body>").expect("body end");
    &document[start..end]
}

// ---------------------------------------------------------------------------
// Acceptance 1-3: insert -> commit -> build succeeds, the new w:p is really
// there, and verify_output passes.
// ---------------------------------------------------------------------------

#[test]
fn a_new_paragraph_really_reaches_the_built_package() {
    let workdir = scratch("new");
    extract(&fixture("plain"), &workdir);
    let wd = workdir.to_string_lossy().into_owned();
    let out = workdir.join("out.docx");
    let out_str = out.to_string_lossy().into_owned();
    let source_before = std::fs::read(fixture("plain")).expect("source bytes");
    let results = mcp(&[
        ("workdir_open", json!({ "workdir": wd })),
        (
            "insert_paragraph",
            json!({
                "after_id": "P0",
                "text": "新插入的段落 XYZ",
                "inherit": "P0",
                "operation_id": "op-insert",
            }),
        ),
        ("commit_sync", json!({ "operation_id": "op-commit" })),
        (
            "build_docx",
            json!({ "output": out_str, "operation_id": "op-build" }),
        ),
        ("verify_output", json!({ "output": out_str })),
    ]);
    let inserted = ok(&results[1]);
    assert_eq!(inserted["temp_id"], "N1");
    let committed = ok(&results[2]);
    assert_eq!(committed["changed_paragraph_ids"], json!(["N1"]));
    ok(&results[3]);
    // verify_output reports success only when every independent check passed.
    ok(&results[4]);

    // The new <w:p> really exists, spliced between P0 and P1 (not appended).
    let document = document_xml(&out);
    let new_paragraph = "<w:p><w:r><w:t>新插入的段落 XYZ</w:t></w:r></w:p>";
    let p0 = document
        .find("本发明涉及生物医用材料技术领域。剂量为 20 mg。")
        .expect("P0");
    let p1 = document.find("实施例1采用 250 mg 剂量。").expect("P1");
    let at = document.find(new_paragraph).expect("the new w:p");
    assert!(p0 < at && at < p1, "insertion order in {}", body(&document));

    // Acceptance 6: the original source DOCX is byte-unchanged.
    assert_eq!(
        std::fs::read(fixture("plain")).expect("source bytes"),
        source_before,
        "the source DOCX must never be written"
    );
}

#[test]
fn a_new_paragraph_copies_the_inherit_paragraph_formatting() {
    let workdir = scratch("format");
    extract(&fixture("styled"), &workdir);
    let wd = workdir.to_string_lossy().into_owned();
    let out = workdir.join("out.docx");
    let out_str = out.to_string_lossy().into_owned();
    let results = mcp(&[
        ("workdir_open", json!({ "workdir": wd })),
        (
            "insert_paragraph",
            json!({
                "after_id": "P2",
                "text": "新段落内容",
                "inherit": "P2",
                "operation_id": "op-insert",
            }),
        ),
        ("commit_sync", json!({ "operation_id": "op-commit" })),
        (
            "build_docx",
            json!({ "output": out_str, "operation_id": "op-build" }),
        ),
        ("verify_output", json!({ "output": out_str })),
    ]);
    ok(&results[1]);
    ok(&results[2]);
    ok(&results[3]);
    ok(&results[4]);
    // The inherit paragraph's first text run carries the 宋体 rFonts; the
    // synthesised run copies it (mirror of Python's inherited base style).
    let document = document_xml(&out);
    assert!(
        document.contains(
            "<w:p><w:r><w:rPr><w:rFonts w:ascii=\"宋体\" w:hAnsi=\"宋体\" \
             w:eastAsia=\"宋体\"/></w:rPr><w:t>新段落内容</w:t></w:r></w:p>"
        ),
        "inherited run properties: {}",
        body(&document)
    );
}

// ---------------------------------------------------------------------------
// Acceptance 4-5: the new paragraph can be edited again, and a deleted
// paragraph really disappears from the output.
// ---------------------------------------------------------------------------

#[test]
fn a_new_paragraph_can_be_edited_again_and_a_deleted_one_disappears() {
    let workdir = scratch("again");
    extract(&fixture("plain"), &workdir);
    let wd = workdir.to_string_lossy().into_owned();
    let first = workdir.join("first.docx");
    let second = workdir.join("second.docx");
    let first_str = first.to_string_lossy().into_owned();
    let second_str = second.to_string_lossy().into_owned();
    let results = mcp(&[
        ("workdir_open", json!({ "workdir": wd })),
        (
            "insert_paragraph",
            json!({
                "after_id": "P0",
                "text": "新插入的段落 XYZ",
                "inherit": "P0",
                "operation_id": "op-insert",
            }),
        ),
        ("commit_sync", json!({ "operation_id": "op-commit-1" })),
        (
            "replace_text",
            json!({
                "paragraph_id": "N1",
                "old": "XYZ",
                "new": "QQQ",
                "operation_id": "op-replace",
            }),
        ),
        ("commit_sync", json!({ "operation_id": "op-commit-2" })),
        (
            "build_docx",
            json!({ "output": first_str, "operation_id": "op-build-1" }),
        ),
        ("verify_output", json!({ "output": first_str })),
        (
            "delete_paragraph",
            json!({ "paragraph_id": "P3", "operation_id": "op-delete" }),
        ),
        ("commit_sync", json!({ "operation_id": "op-commit-3" })),
        (
            "build_docx",
            json!({ "output": second_str, "operation_id": "op-build-2" }),
        ),
        ("verify_output", json!({ "output": second_str })),
    ]);
    for index in [1, 2, 3, 4, 5, 6, 7, 8, 9, 10] {
        ok(&results[index]);
    }
    // Acceptance 4: the re-edited body builds.
    let edited = document_xml(&first);
    assert!(edited.contains("新插入的段落 QQQ"), "{edited}");
    assert!(!edited.contains("XYZ"), "{edited}");
    // Acceptance 5: P3 is genuinely gone; every other paragraph survives.
    let deleted = document_xml(&second);
    assert!(deleted.contains("新插入的段落 QQQ"), "{deleted}");
    assert!(!deleted.contains("结束段落。"), "{deleted}");
    assert!(deleted.contains("实施例1采用 250 mg 剂量。"), "{deleted}");
    assert!(deleted.contains("重复句子内容 重复句子内容。"), "{deleted}");
}

// ---------------------------------------------------------------------------
// Acceptance 7: nothing outside word/document.xml moves.
// ---------------------------------------------------------------------------

#[test]
fn unauthorised_parts_stay_byte_identical() {
    let workdir = scratch("parts");
    extract(&fixture("parts"), &workdir);
    let wd = workdir.to_string_lossy().into_owned();
    let out = workdir.join("out.docx");
    let out_str = out.to_string_lossy().into_owned();
    let results = mcp(&[
        ("workdir_open", json!({ "workdir": wd })),
        (
            "insert_paragraph",
            json!({
                "after_id": "P0",
                "text": "新增正文段落",
                "inherit": "P0",
                "operation_id": "op-insert",
            }),
        ),
        ("commit_sync", json!({ "operation_id": "op-commit" })),
        (
            "build_docx",
            json!({ "output": out_str, "operation_id": "op-build" }),
        ),
        ("verify_output", json!({ "output": out_str })),
    ]);
    ok(&results[1]);
    ok(&results[2]);
    ok(&results[3]);
    ok(&results[4]);
    assert!(document_xml(&out).contains("新增正文段落"));
    // Headers, footers, footnotes, endnotes, styles, relationships, and the
    // rest of the package replay verbatim; only document.xml is edited.
    assert_eq!(
        changed_parts(&workdir.join("_template.docx"), &out),
        vec!["word/document.xml".to_string()]
    );
}

// ---------------------------------------------------------------------------
// Acceptance 8: unsupported structural deletions refuse; nothing is silently
// dropped.
// ---------------------------------------------------------------------------

#[test]
fn unsupported_structural_deletions_refuse_instead_of_dropping_content() {
    let workdir = scratch("refuse");
    extract(&fixture("complex"), &workdir);
    let wd = workdir.to_string_lossy().into_owned();
    let out = workdir.join("out.docx");
    let out_str = out.to_string_lossy().into_owned();
    let results = mcp(&[
        ("workdir_open", json!({ "workdir": wd })),
        // P17 carries the section properties (w:pPr/w:sectPr).
        (
            "delete_paragraph",
            json!({ "paragraph_id": "P17", "operation_id": "op-delete-section" }),
        ),
        // P12 carries a field (opaque interior).
        (
            "delete_paragraph",
            json!({ "paragraph_id": "P12", "operation_id": "op-delete-field" }),
        ),
        // A table cell paragraph is a container: out of scope.
        (
            "delete_paragraph",
            json!({ "paragraph_id": "T0.R0.C0.P0", "operation_id": "op-delete-cell" }),
        ),
        ("commit_sync", json!({ "operation_id": "op-commit" })),
        (
            "build_docx",
            json!({ "output": out_str, "operation_id": "op-build" }),
        ),
    ]);
    assert!(refused(&results[1], "invalid-structure").contains("section properties"));
    assert!(refused(&results[2], "invalid-structure").contains("structural content"));
    refused(&results[3], "table-structure-immutable");
    ok(&results[4]);
    ok(&results[5]);
    // Every refused paragraph is still in the output.
    let document = document_xml(&out);
    assert!(document.contains("The section boundary and sectPr are part of the protected ba"));
    assert!(document.contains("FIELD-SLOT"));
    assert!(document.contains("<w:tbl>"));
}

#[test]
fn tracked_revisions_and_anchors_refuse_deletion() {
    let revisions = scratch("refuse-revisions");
    extract(&fixture("revisions"), &revisions);
    let revisions_out = revisions.join("out.docx");
    let results = mcp(&[
        (
            "workdir_open",
            json!({ "workdir": revisions.to_string_lossy() }),
        ),
        (
            "delete_paragraph",
            json!({ "paragraph_id": "P0", "operation_id": "op-delete-revision" }),
        ),
        ("commit_sync", json!({ "operation_id": "op-commit" })),
        (
            "build_docx",
            json!({
                "output": revisions_out.to_string_lossy(),
                "operation_id": "op-build",
            }),
        ),
    ]);
    assert!(refused(&results[1], "invalid-structure").contains("tracked revisions"));
    ok(&results[2]);
    ok(&results[3]);
    let document = document_xml(&revisions_out);
    assert!(
        document.contains("<w:t>修订前文</w:t>")
            && document.contains("<w:ins w:id=\"1\"")
            && document.contains("<w:t>修订后文</w:t>"),
        "the revision containers survive the refused deletion: {document}"
    );

    let anchors = scratch("refuse-anchors");
    extract(&fixture("anchors"), &anchors);
    let anchors_out = anchors.join("out.docx");
    let results = mcp(&[
        (
            "workdir_open",
            json!({ "workdir": anchors.to_string_lossy() }),
        ),
        (
            "delete_paragraph",
            json!({ "paragraph_id": "P1", "operation_id": "op-delete-anchor" }),
        ),
        ("commit_sync", json!({ "operation_id": "op-commit" })),
        (
            "build_docx",
            json!({
                "output": anchors_out.to_string_lossy(),
                "operation_id": "op-build",
            }),
        ),
    ]);
    assert!(refused(&results[1], "invalid-structure").contains("anchors"));
    ok(&results[2]);
    ok(&results[3]);
    let document = document_xml(&anchors_out);
    assert!(
        document.contains("<w:t>关键内容</w:t>")
            && document.contains("<w:bookmarkStart w:id=\"0\"")
            && document.contains("<w:commentRangeStart w:id=\"0\"/>"),
        "the anchors survive the refused deletion: {document}"
    );
}

#[test]
fn a_new_paragraph_cannot_inherit_protected_structure() {
    let workdir = scratch("refuse-inherit");
    extract(&fixture("complex"), &workdir);
    let results = mcp(&[
        (
            "workdir_open",
            json!({ "workdir": workdir.to_string_lossy() }),
        ),
        (
            "insert_paragraph",
            json!({
                "after_id": "P0",
                "text": "新段落",
                "inherit": "P12",
                "operation_id": "op-insert-field",
            }),
        ),
        (
            "insert_paragraph",
            json!({
                "after_id": "P0",
                "text": "新段落",
                "inherit": "P17",
                "operation_id": "op-insert-section",
            }),
        ),
    ]);
    assert!(refused(&results[1], "invalid-structure").contains("structural content"));
    assert!(refused(&results[2], "invalid-structure").contains("section properties"));
}
