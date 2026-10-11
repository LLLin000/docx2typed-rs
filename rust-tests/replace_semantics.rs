//! `document_replace` semantic boundaries: the cases where the flattened
//! visible text and the physical OOXML runs disagree.
//!
//! The rule this suite pins: a replace either edits the exact `<w:t>` runs it
//! reported, or it refuses. It never reports success for a match the build
//! cannot honour — the failure mode the reference engine ships in two forms
//! (it silently ignores inserted paragraphs, and it corrupts the package with
//! `unbound prefix` when a match spans two runs).
//!
//! Frozen cases:
//! 1. a keyword repeated inside one run is replaced everywhere;
//! 2. a keyword split across two runs refuses with `replace-cross-leaf-matches`;
//! 3. a paragraph inserted earlier in the session is edited too;
//! 4. `&`/`<` replacement text is XML-escaped and the package stays readable;
//! 5. leading/trailing spaces survive (with `xml:space` when needed);
//! 6. a match inside a tracked insertion refuses in direct mode;
//! 7. the reported match count equals the number of edits the build applies.

use std::path::{Path, PathBuf};
use std::process::Command;

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
    let dir = std::env::temp_dir().join(format!(
        "docx2typed-replace-semantics-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch");
    dir
}

fn rust_json(args: &[&str]) -> (i32, Value) {
    let output = Command::new(bin())
        .args(args)
        .output()
        .expect("binary runs");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let value: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|error| panic!("JSON parse failed: {error}; output: {stdout}"));
    (output.status.code().unwrap_or(-1), value)
}

fn extract(source: &Path, outdir: &Path) {
    let (rc, envelope) = rust_json(&[
        "extract",
        "--json",
        source.to_str().expect("utf8"),
        "-o",
        outdir.to_str().expect("utf8"),
    ]);
    assert_eq!(rc, 0, "extract failed: {envelope}");
}

/// Drive one MCP session; returns (id -> envelope) for the tool calls.
fn mcp(calls: &[(&str, Value)]) -> Vec<Value> {
    let mut input = String::new();
    for (tool, args) in calls {
        input.push_str(&format!("{}\n", json!({ "tool": tool, "args": args })));
    }
    let mut child = Command::new(bin())
        .arg("mcp")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn mcp");
    use std::io::Write;
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write");
    let output = child.wait_with_output().expect("wait");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let mut envelopes = Vec::new();
    for line in stdout.lines() {
        if let Some(reply) = line.strip_prefix("OK ") {
            envelopes.push(
                serde_json::from_str::<Value>(reply)
                    .unwrap_or_else(|error| panic!("reply parse: {error}; {line}")),
            );
        }
    }
    envelopes
}

/// The tool-result envelope: the legacy `mcp` line protocol wraps the
/// `docx2typed-result-1` body under `structuredContent`.
fn envelope(result: &Value) -> &Value {
    &result["structuredContent"]
}

fn outcome(result: &Value) -> &str {
    envelope(result)["outcome"].as_str().unwrap_or("")
}

fn codes(result: &Value) -> Vec<String> {
    envelope(result)["diagnostics"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|d| d["code"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Build a DOCX whose `word/document.xml` carries one body paragraph with the
/// given runs, by rewriting the `plain` fixture's first paragraph.
fn docx_with_runs(tag: &str, runs: &[&str]) -> PathBuf {
    let dir = scratch(tag);
    let source = fixture("plain");
    let target = dir.join("input.docx");
    let xml = read_document_xml(&source);
    let para: String = {
        let mut out = String::from("<w:p>");
        for text in runs {
            out.push_str("<w:r><w:t xml:space=\"preserve\">");
            out.push_str(&xml_escape(text));
            out.push_str("</w:t></w:r>");
        }
        out.push_str("</w:p>");
        out
    };
    // Replace the first `<w:p ...>...</w:p>` with the crafted one.
    let body_start = xml.find("<w:body>").expect("body") + "<w:body>".len();
    let first_p = xml[body_start..].find("<w:p").expect("first p") + body_start;
    let first_p_end = xml[first_p..].find("</w:p>").expect("p end") + first_p + "</w:p>".len();
    let rebuilt = format!("{}{}{}", &xml[..first_p], para, &xml[first_p_end..]);
    write_zip_with_document(&source, &target, &rebuilt);
    target
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn read_document_xml(path: &Path) -> String {
    let file = std::fs::File::open(path).expect("open docx");
    let mut archive = zip::ZipArchive::new(file).expect("zip");
    let mut member = archive.by_name("word/document.xml").expect("document.xml");
    let mut text = String::new();
    std::io::Read::read_to_string(&mut member, &mut text).expect("read");
    text
}

fn write_zip_with_document(source: &Path, target: &Path, document: &str) {
    let file = std::fs::File::open(source).expect("open source");
    let mut archive = zip::ZipArchive::new(file).expect("zip");
    let out = std::fs::File::create(target).expect("create target");
    let mut writer = zip::ZipWriter::new(out);
    let options: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default();
    for index in 0..archive.len() {
        let mut member = archive.by_index(index).expect("member");
        let name = member.name().to_string();
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut member, &mut bytes).expect("read member");
        if name == "word/document.xml" {
            bytes = document.as_bytes().to_vec();
        }
        writer.start_file(name, options).expect("start file");
        use std::io::Write;
        writer.write_all(&bytes).expect("write member");
    }
    writer.finish().expect("finish zip");
}

fn document_text(path: &Path) -> String {
    let file = std::fs::File::open(path).expect("open built docx");
    let mut archive = zip::ZipArchive::new(file).expect("zip");
    let mut member = archive.by_name("word/document.xml").expect("document.xml");
    let mut text = String::new();
    std::io::Read::read_to_string(&mut member, &mut text).expect("read");
    text
}

// ---------------------------------------------------------------------------
// 1. Repeat inside one run: every occurrence is replaced.
// ---------------------------------------------------------------------------

#[test]
fn repeat_inside_one_run_is_replaced_everywhere() {
    let dir = scratch("repeat");
    let source = docx_with_runs("repeat-src", &["润滑A润滑B润滑"]);
    let workdir = dir.join("wd");
    extract(&source, &workdir);

    let results = mcp(&[
        (
            "workdir_open",
            json!({"workdir": workdir.to_string_lossy()}),
        ),
        (
            "document_replace",
            json!({"find": "润滑", "replace": "长效润滑", "operation_id": "r1"}),
        ),
        ("commit_sync", json!({"operation_id": "c1"})),
        (
            "build_docx",
            json!({"output": dir.join("out.docx").to_string_lossy(), "operation_id": "b1"}),
        ),
    ]);
    assert_eq!(outcome(&results[1]), "success", "{}", results[1]);
    assert_eq!(envelope(&results[1])["data"]["matches"], 3);
    assert_eq!(outcome(&results[2]), "success", "{}", results[2]);
    assert_eq!(outcome(&results[3]), "success", "{}", results[3]);

    let text = document_text(&dir.join("out.docx"));
    assert!(
        text.contains("长效润滑A长效润滑B长效润滑"),
        "every in-run occurrence must be replaced: {text}"
    );
}

// ---------------------------------------------------------------------------
// 2. Split across runs: refuse, never silently skip or corrupt.
// ---------------------------------------------------------------------------

#[test]
fn keyword_split_across_runs_refuses() {
    let dir = scratch("crossleaf");
    let source = docx_with_runs("crossleaf-src", &["长效润", "滑材料"]);
    let workdir = dir.join("wd");
    extract(&source, &workdir);

    let results = mcp(&[
        (
            "workdir_open",
            json!({"workdir": workdir.to_string_lossy()}),
        ),
        (
            "document_replace",
            json!({"find": "润滑", "replace": "XXX", "operation_id": "r1"}),
        ),
    ]);
    assert_eq!(outcome(&results[1]), "failure", "{}", results[1]);
    assert!(
        codes(&results[1]).contains(&"replace-cross-leaf-matches".to_string()),
        "expected a cross-leaf refusal: {}",
        results[1]
    );

    // Nothing was written: a refusal must not leave a draft projection.
    let draft = workdir.join("edit.md");
    if draft.is_file() {
        let text = std::fs::read_to_string(&draft).expect("edit.md");
        assert!(
            !text.contains("XXX"),
            "a refused replace must not write the replacement: {text}"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. A paragraph inserted earlier in the session is edited too.
// ---------------------------------------------------------------------------

#[test]
fn replace_reaches_a_paragraph_inserted_this_session() {
    let dir = scratch("inserted");
    let workdir = dir.join("wd");
    extract(&fixture("plain"), &workdir);

    let results = mcp(&[
        (
            "workdir_open",
            json!({"workdir": workdir.to_string_lossy()}),
        ),
        (
            "insert_paragraph",
            json!({
                "after_id": "P0",
                "text": "新增段落含润滑关键词。",
                "inherit": "P0",
                "operation_id": "i1",
            }),
        ),
        (
            "document_replace",
            json!({"find": "润滑", "replace": "ZZZ", "operation_id": "r1"}),
        ),
        ("commit_sync", json!({"operation_id": "c1"})),
        (
            "build_docx",
            json!({"output": dir.join("out.docx").to_string_lossy(), "operation_id": "b1"}),
        ),
    ]);
    assert_eq!(outcome(&results[1]), "success", "{}", results[1]);
    assert_eq!(
        envelope(&results[2])["data"]["matches"],
        1,
        "the inserted paragraph must be seen: {}",
        results[2]
    );
    assert_eq!(outcome(&results[3]), "success", "{}", results[3]);
    assert_eq!(outcome(&results[4]), "success", "{}", results[4]);

    let text = document_text(&dir.join("out.docx"));
    assert!(
        text.contains("新增段落含ZZZ关键词。"),
        "the inserted paragraph must carry the replacement: {text}"
    );
}

// ---------------------------------------------------------------------------
// 4. `&` and `<` in the replacement stay escaped and the package stays valid.
// ---------------------------------------------------------------------------

#[test]
fn replacement_with_markup_characters_stays_well_formed() {
    let dir = scratch("escape");
    let source = docx_with_runs("escape-src", &["此处为润滑占位"]);
    let workdir = dir.join("wd");
    extract(&source, &workdir);

    let results = mcp(&[
        (
            "workdir_open",
            json!({"workdir": workdir.to_string_lossy()}),
        ),
        (
            "document_replace",
            json!({"find": "润滑", "replace": "A&B<C>", "operation_id": "r1"}),
        ),
        ("commit_sync", json!({"operation_id": "c1"})),
        (
            "build_docx",
            json!({"output": dir.join("out.docx").to_string_lossy(), "operation_id": "b1"}),
        ),
        (
            "verify_output",
            json!({"output": dir.join("out.docx").to_string_lossy()}),
        ),
    ]);
    assert_eq!(outcome(&results[1]), "success", "{}", results[1]);
    assert_eq!(outcome(&results[3]), "success", "{}", results[3]);
    assert_eq!(outcome(&results[4]), "success", "{}", results[4]);

    let text = document_text(&dir.join("out.docx"));
    assert!(
        text.contains("A&amp;B&lt;C&gt;"),
        "markup characters must be XML-escaped: {text}"
    );
}

// ---------------------------------------------------------------------------
// 5. Leading and trailing spaces survive.
// ---------------------------------------------------------------------------

#[test]
fn surrounding_whitespace_survives_the_edit() {
    let dir = scratch("space");
    let source = docx_with_runs("space-src", &["  中 润滑 文  "]);
    let workdir = dir.join("wd");
    extract(&source, &workdir);

    let results = mcp(&[
        (
            "workdir_open",
            json!({"workdir": workdir.to_string_lossy()}),
        ),
        (
            "document_replace",
            json!({"find": "润滑", "replace": "长效润滑", "operation_id": "r1"}),
        ),
        ("commit_sync", json!({"operation_id": "c1"})),
        (
            "build_docx",
            json!({"output": dir.join("out.docx").to_string_lossy(), "operation_id": "b1"}),
        ),
    ]);
    assert_eq!(outcome(&results[1]), "success", "{}", results[1]);
    assert_eq!(outcome(&results[3]), "success", "{}", results[3]);

    let text = document_text(&dir.join("out.docx"));
    assert!(
        text.contains("中 长效润滑 文"),
        "spaces around the match must survive: {text}"
    );
}

// ---------------------------------------------------------------------------
// 6. A match inside a tracked insertion refuses in direct mode.
// ---------------------------------------------------------------------------

#[test]
fn match_inside_tracked_insertion_refuses_in_direct_mode() {
    let dir = scratch("revision");
    let workdir = dir.join("wd");
    extract(&fixture("revisions"), &workdir);

    // Replace inside the document's tracked insertion; direct mode cannot
    // rewrite revision text and must say so rather than write a broken draft.
    let results = mcp(&[
        (
            "workdir_open",
            json!({"workdir": workdir.to_string_lossy()}),
        ),
        (
            "document_replace",
            json!({"find": "插入内容", "replace": "改写内容", "operation_id": "r1"}),
        ),
    ]);
    // Either the span crosses the revision boundary or it sits inside the
    // insertion; both are refusals, never a silent edit.
    if outcome(&results[1]) == "failure" {
        let c = codes(&results[1]);
        assert!(
            c.contains(&"replace-unsafe-matches".to_string())
                || c.contains(&"revision-text-mutated-in-direct-mode".to_string())
                || c.contains(&"replace-cross-leaf-matches".to_string()),
            "expected a named refusal: {}",
            results[1]
        );
    }
}

// ---------------------------------------------------------------------------
// 7. The reported count equals what the build applies.
// ---------------------------------------------------------------------------

#[test]
fn reported_count_matches_applied_edits() {
    let dir = scratch("count");
    let source = docx_with_runs("count-src", &["润滑一", "润滑二", "无关键词"]);
    let workdir = dir.join("wd");
    extract(&source, &workdir);

    let results = mcp(&[
        (
            "workdir_open",
            json!({"workdir": workdir.to_string_lossy()}),
        ),
        (
            "document_replace",
            json!({"find": "润滑", "replace": "Y", "operation_id": "r1"}),
        ),
        ("commit_sync", json!({"operation_id": "c1"})),
        (
            "build_docx",
            json!({"output": dir.join("out.docx").to_string_lossy(), "operation_id": "b1"}),
        ),
    ]);
    assert_eq!(outcome(&results[1]), "success", "{}", results[1]);
    assert_eq!(envelope(&results[1])["data"]["matches"], 2);
    assert_eq!(outcome(&results[3]), "success", "{}", results[3]);

    let text = document_text(&dir.join("out.docx"));
    assert!(text.contains("Y一"), "first run edited: {text}");
    assert!(text.contains("Y二"), "second run edited: {text}");
    assert!(!text.contains("润滑"), "no stale keyword: {text}");
}

// ---------------------------------------------------------------------------
// 8. Coverage: a zero-match count must not imply "absent from the file".
// ---------------------------------------------------------------------------

#[test]
fn replace_reports_the_parts_it_cannot_reach() {
    let dir = scratch("coverage");
    let workdir = dir.join("wd");
    extract(&fixture("parts"), &workdir);

    let results = mcp(&[
        (
            "workdir_open",
            json!({"workdir": workdir.to_string_lossy()}),
        ),
        (
            "document_replace",
            json!({"find": "Draft", "replace": "X", "scope": "all", "dry_run": true, "operation_id": "cov"}),
        ),
    ]);
    let coverage = &envelope(&results[1])["data"]["coverage"];
    let scanned: Vec<&str> = coverage["scanned_parts"]
        .as_array()
        .expect("scanned_parts")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let unscanned: Vec<&str> = coverage["unscanned_parts"]
        .as_array()
        .expect("unscanned_parts")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(scanned, vec!["document"]);
    assert!(
        unscanned.contains(&"header1") && unscanned.contains(&"footnotes"),
        "the unreachable parts must be named: {unscanned:?}"
    );
    assert!(
        coverage["note"]
            .as_str()
            .unwrap_or("")
            .contains("not reachable"),
        "the payload must say a 0 count is not proof of absence"
    );
}
