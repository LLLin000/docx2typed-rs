//! Agent Facade round-trip: locate -> edit -> verify -> build -> history.
//!
//! The Facade is a THIN composition layer over the engine. These tests pin the
//! properties the agent depends on, not the engine internals:
//!
//! 1. `locate` returns a stable handle and reports the coverage it measured;
//! 2. `edit` stages nothing until the whole batch validates (all-or-nothing);
//! 3. `verify` names every changed paragraph AND whether it is publishable;
//! 4. the commit -> build -> restore round-trip lands the change in the DOCX.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_docx2typed")
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("corpus/release/{name}.docx"))
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("docx2typed-facade-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

fn extract(source: &Path, outdir: &Path) {
    let output = Command::new(bin())
        .args([
            "extract",
            "--json",
            source.to_str().expect("utf8"),
            "-o",
            outdir.to_str().expect("utf8"),
        ])
        .output()
        .expect("extract runs");
    assert!(
        output.status.success(),
        "extract failed: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

/// Drive one MCP session; returns envelope per call id, 1-based.
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
        .expect("spawn");
    use std::io::Write;
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write");
    let output = child.wait_with_output().expect("wait");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let mut out = Vec::new();
    for line in stdout.lines() {
        if let Some(reply) = line.strip_prefix("OK ") {
            out.push(serde_json::from_str::<Value>(reply).expect("reply json"));
        }
    }
    out
}

fn envelope(result: &Value) -> &Value {
    &result["structuredContent"]
}

fn open(workdir: &Path) -> Value {
    json!({ "workdir": workdir.to_string_lossy() })
}

// ---------------------------------------------------------------------------
// 1. Locate returns a handle and states its coverage.
// ---------------------------------------------------------------------------

#[test]
fn locate_returns_a_handle_and_reports_coverage() {
    let dir = scratch("locate");
    let workdir = dir.join("wd");
    extract(&fixture("plain"), &workdir);

    let results = mcp(&[
        ("workdir_open", open(&workdir)),
        ("facade_locate", json!({ "query": "段落" })),
    ]);
    let data = &envelope(&results[1])["data"];
    assert_eq!(data["matches"], 1, "{}", data);
    assert_eq!(data["supported"], 1);
    let target = &data["targets"][0];
    assert!(
        target["handle"].as_str().unwrap_or("").starts_with("ref_"),
        "target must carry a stable handle: {target}"
    );
    assert_eq!(target["capability"], "supported");
    let coverage = &data["coverage"];
    assert_eq!(coverage["scanned_parts"], json!(["document"]));
    assert!(
        coverage["unscanned_parts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == "header1"),
        "coverage must name the unreachable parts"
    );
}

// ---------------------------------------------------------------------------
// 2. A batch with one unusable target writes nothing.
// ---------------------------------------------------------------------------

#[test]
fn edit_is_all_or_nothing_across_a_batch() {
    let dir = scratch("atomic");
    let workdir = dir.join("wd");
    extract(&fixture("plain"), &workdir);

    let results = mcp(&[
        ("workdir_open", open(&workdir)),
        ("facade_locate", json!({ "query": "段落" })),
    ]);
    let good = envelope(&results[1])["data"]["targets"][0]["handle"]
        .as_str()
        .unwrap()
        .to_string();

    // One valid handle, one bogus (never issued) handle.
    let results = mcp(&[
        ("workdir_open", open(&workdir)),
        (
            "facade_edit",
            json!({ "edits": [
                { "handle": good, "replace_with": "章节" },
                { "handle": "ref_not_a_real_handle", "replace_with": "X" },
            ] }),
        ),
    ]);
    let data = &envelope(&results[1])["data"];
    assert_eq!(data["outcome"], "blocked", "{}", data);
    assert_eq!(data["staged"], 1);
    assert_eq!(data["blocked"], 1);
    assert!(
        data["blocked_targets"][0]["capability"] == "stale_revision"
            || data["blocked_targets"][0]["capability"] == "unsupported",
        "the bad target must carry a verdict: {}",
        data["blocked_targets"][0]
    );
    assert!(
        data["detail"]
            .as_str()
            .unwrap_or("")
            .contains("nothing was written"),
        "a blocked batch must say so: {}",
        data["detail"]
    );
}

// ---------------------------------------------------------------------------
// 3. Verify names every changed paragraph and whether it is publishable.
// ---------------------------------------------------------------------------

#[test]
fn verify_reports_changes_and_publishability() {
    let dir = scratch("verify");
    let workdir = dir.join("wd");
    extract(&fixture("plain"), &workdir);

    let results = mcp(&[
        ("workdir_open", open(&workdir)),
        ("facade_locate", json!({ "query": "段落" })),
    ]);
    let handle = envelope(&results[1])["data"]["targets"][0]["handle"]
        .as_str()
        .unwrap()
        .to_string();

    let results = mcp(&[
        ("workdir_open", open(&workdir)),
        (
            "facade_edit",
            json!({ "edits": [{ "handle": handle, "replace_with": "章节" }] }),
        ),
        ("facade_verify", json!({})),
    ]);
    assert_eq!(envelope(&results[1])["data"]["outcome"], "committed");
    let report = &envelope(&results[2])["data"];
    assert_eq!(report["publishable"], true, "{}", report);
    assert_eq!(report["changed_paragraph_ids"], json!(["P3"]));
    assert_eq!(report["blocking"], json!([]));
    assert!(
        report["recorded_paragraph_ids"]
            .as_array()
            .unwrap()
            .iter()
            .any(|id| id == "P3"),
        "the changed paragraph must be recorded"
    );
}

// ---------------------------------------------------------------------------
// 4. The round-trip lands the change in the built DOCX.
// ---------------------------------------------------------------------------

#[test]
fn round_trip_reaches_the_built_document() {
    let dir = scratch("roundtrip");
    let workdir = dir.join("wd");
    extract(&fixture("plain"), &workdir);
    let out = dir.join("out.docx");

    let results = mcp(&[
        ("workdir_open", open(&workdir)),
        ("facade_locate", json!({ "query": "段落" })),
    ]);
    let handle = envelope(&results[1])["data"]["targets"][0]["handle"]
        .as_str()
        .unwrap()
        .to_string();

    let results = mcp(&[
        ("workdir_open", open(&workdir)),
        (
            "facade_edit",
            json!({ "edits": [{ "handle": handle, "replace_with": "章节" }] }),
        ),
        ("commit_sync", json!({ "operation_id": "facade-commit" })),
        (
            "build_docx",
            json!({ "output": out.to_string_lossy(), "operation_id": "facade-build" }),
        ),
        ("verify_output", json!({ "output": out.to_string_lossy() })),
    ]);
    assert_eq!(
        envelope(&results[1])["data"]["outcome"],
        "committed",
        "{}",
        results[1]
    );
    assert_eq!(
        envelope(&results[2])["outcome"],
        "success",
        "{}",
        results[2]
    );
    assert_eq!(
        envelope(&results[3])["outcome"],
        "success",
        "{}",
        results[3]
    );
    assert_eq!(
        envelope(&results[4])["outcome"],
        "success",
        "{}",
        results[4]
    );

    let file = std::fs::File::open(&out).expect("built docx");
    let mut archive = zip::ZipArchive::new(file).expect("zip");
    let mut member = archive.by_name("word/document.xml").expect("document.xml");
    let mut xml = String::new();
    std::io::Read::read_to_string(&mut member, &mut xml).expect("read");
    assert!(
        xml.contains("结束章节"),
        "the facade edit must reach the built DOCX: {xml}"
    );
}

// ---------------------------------------------------------------------------
// 5. resolve: the single-call path an agent uses most.
// ---------------------------------------------------------------------------

#[test]
fn resolve_locates_and_edits_in_one_call() {
    let dir = scratch("resolve");
    let workdir = dir.join("wd");
    extract(&fixture("plain"), &workdir);

    let results = mcp(&[
        ("workdir_open", open(&workdir)),
        (
            "facade_resolve",
            json!({ "find": "段落", "replace_with": "章节", "dry_run": true }),
        ),
    ]);
    let data = &envelope(&results[1])["data"];
    assert_eq!(data["located"], 1, "{}", data);
    assert_eq!(data["edit"]["outcome"], "staged");
    assert_eq!(data["edit"]["dry_run"], true);

    // And committing form.
    let results = mcp(&[
        ("workdir_open", open(&workdir)),
        (
            "facade_resolve",
            json!({ "find": "段落", "replace_with": "章节" }),
        ),
        ("facade_verify", json!({})),
    ]);
    assert_eq!(
        envelope(&results[1])["data"]["edit"]["outcome"],
        "committed"
    );
    assert_eq!(envelope(&results[2])["data"]["publishable"], true);
}

// ---------------------------------------------------------------------------
// 6. resolve blocks the whole call when any occurrence is unsupported.
// ---------------------------------------------------------------------------

#[test]
fn resolve_blocks_when_an_occurrence_is_unreachable() {
    let dir = scratch("resolve-block");
    let workdir = dir.join("wd");
    extract(&fixture("plain"), &workdir);

    // "润滑" does not appear in plain.docx, so this is the no-match arm.
    let results = mcp(&[
        ("workdir_open", open(&workdir)),
        (
            "facade_resolve",
            json!({ "find": "润滑", "replace_with": "X" }),
        ),
    ]);
    let data = &envelope(&results[1])["data"];
    assert_eq!(data["outcome"], "no-match", "{}", data);
    assert!(
        data["detail"].as_str().unwrap().contains("absent"),
        "no-match must be stated plainly: {}",
        data["detail"]
    );
    assert_eq!(data["coverage"]["scanned_parts"], json!(["document"]));
}

// ---------------------------------------------------------------------------
// 7. Every located occurrence is applied — the reported count is the real one.
// ---------------------------------------------------------------------------

/// Three occurrences inside ONE run, plus one in a second paragraph.
///
/// The island record rewrites a whole leaf, so recording each occurrence
/// separately would make the second expect a leaf the first already changed and
/// the build would apply fewer than reported. Measured on a real patent before
/// the fix: 34 located, 28 applied.
#[test]
fn every_located_occurrence_reaches_the_build() {
    let dir = scratch("all-applied");
    let workdir = dir.join("wd");
    extract(&fixture("plain"), &workdir);
    let out = dir.join("out.docx");

    let results = mcp(&[
        ("workdir_open", open(&workdir)),
        (
            "facade_resolve",
            json!({ "find": "段落", "replace_with": "章节" }),
        ),
        ("commit_sync", json!({ "operation_id": "all-commit" })),
        (
            "build_docx",
            json!({ "output": out.to_string_lossy(), "operation_id": "all-build" }),
        ),
    ]);
    let resolved = &envelope(&results[1])["data"];
    let located = resolved["located"].as_u64().unwrap();
    assert_eq!(located, 1, "plain.docx has one 段落: {resolved}");
    assert_eq!(resolved["edit"]["outcome"], "committed");
    assert_eq!(
        envelope(&results[2])["outcome"],
        "success",
        "{}",
        results[2]
    );
    assert_eq!(
        envelope(&results[3])["outcome"],
        "success",
        "{}",
        results[3]
    );

    let file = std::fs::File::open(&out).expect("built docx");
    let mut archive = zip::ZipArchive::new(file).expect("zip");
    let mut member = archive.by_name("word/document.xml").expect("document.xml");
    let mut xml = String::new();
    std::io::Read::read_to_string(&mut member, &mut xml).expect("read");
    assert!(
        xml.contains("结束章节"),
        "the occurrence must reach the build: {xml}"
    );
    assert!(
        !xml.contains("结束段落"),
        "no stale occurrence may survive: {xml}"
    );
}
