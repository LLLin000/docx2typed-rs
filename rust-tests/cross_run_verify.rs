//! A tracked replace that crosses a paragraph's text runs, end to end.
//!
//! Word splits a paragraph's visible text into one `w:t` per formatting run —
//! `corpus/release/styled.docx` splits one paragraph into four runs — so an edit a
//! human typed in Word usually starts in one run and ends in another. This drives the
//! whole loop through the real binary, because the failure mode that matters is
//! not "does the applier do it" but "do the applier and the independent verifier
//! agree": the verifier recomputes the edited part from the template with its
//! own walker, and when that walker could only address a single run it rejected
//! correct output with `edited-part-exact`.

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
    let dir = std::env::temp_dir().join(format!("cross-run-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Call one MCP tool over stdio and return the structured result.
///
/// One connection drives one workdir for its lifetime, so the calls that set the
/// session up travel with the one under test on the same process.
fn call(tool: &str, args: Value, session: &mut [Value]) -> Value {
    let mut child = Command::new(bin())
        .arg("mcp")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("the server starts");
    let last_id;
    {
        use std::io::Write;
        // Taking stdin by value is what lets it close: the server reads requests
        // until EOF, so a borrowed handle would leave it waiting forever.
        let mut stdin = child.stdin.take().expect("stdin");
        let mut id = 1;
        for earlier in session.iter() {
            let request = json!({
                "jsonrpc": "2.0", "id": id, "method": "tools/call",
                "params": { "name": earlier["name"], "arguments": earlier["arguments"] },
            });
            writeln!(stdin, "{request}").expect("write request");
            id += 1;
        }
        let request = json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": { "name": tool, "arguments": args },
        });
        writeln!(stdin, "{request}").expect("write request");
        last_id = id;
    }
    let output = child.wait_with_output().expect("the server answers");
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    for line in text.lines() {
        let Ok(reply) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if reply["id"] != json!(last_id) {
            continue;
        }
        return reply["result"]["structuredContent"].clone();
    }
    panic!("no reply for {tool}; server said:\n{text}");
}

#[test]
fn a_tracked_replace_across_runs_survives_build_and_verification() {
    let dir = scratch("verify");
    let source = dir.join("source.docx");
    std::fs::copy(fixture("styled"), &source).expect("copy fixture");
    let workdir = dir.join("wd");
    let out = dir.join("out.docx");

    let extract = Command::new(bin())
        .args([
            "extract",
            source.to_str().expect("utf8"),
            "-o",
            workdir.to_str().expect("utf8"),
        ])
        .output()
        .expect("extract runs");
    assert!(extract.status.success(), "extract: {extract:?}");

    // `styled.docx` splits P4 into `智能响应` / `材料与 ` / `ABC` / ` 对照组相比。`,
    // so this phrase starts in run 0 and ends in run 2.
    let old = "智能响应材料与 ABC";
    let new = "智能响应材料与 XYZ";
    let mut session = vec![json!({
        "name": "workdir_open",
        "arguments": { "workdir": workdir.to_str().expect("utf8"), "track": true, "author": "e2e" },
    })];

    let patch = call(
        "document_patch",
        json!({
            "operation_id": "op_cross_run_patch",
            "hunks": [{ "paragraph_id": "P4", "old": old, "new": new }],
        }),
        &mut session,
    );
    assert_eq!(
        patch["outcome"], "success",
        "a cross-run replace must apply: {patch}"
    );
    session.push(json!({
        "name": "document_patch",
        "arguments": {
            "operation_id": "op_cross_run_patch",
            "hunks": [{ "paragraph_id": "P4", "old": old, "new": new }],
        },
    }));

    let commit = call(
        "commit_sync",
        json!({ "operation_id": "op_cross_run_commit" }),
        &mut session,
    );
    assert_eq!(commit["outcome"], "success", "commit: {commit}");
    session.push(json!({
        "name": "commit_sync",
        "arguments": { "operation_id": "op_cross_run_commit" },
    }));

    let build = call(
        "build_docx",
        json!({
            "operation_id": "op_cross_run_build",
            "output": out.to_str().expect("utf8"),
        }),
        &mut session,
    );
    assert_eq!(build["outcome"], "success", "build: {build}");

    // The independent verifier recomputes the edited part from the template with
    // its own walker, and `verify_output` succeeds only when every check passes.
    // It used to address a single run, so a cross-run edit failed here with
    // `edited-part-exact` even though the output was right.
    let mut verify_session = vec![json!({
        "name": "workdir_open",
        "arguments": { "workdir": workdir.to_str().expect("utf8") },
    })];
    let verify = call(
        "verify_output",
        json!({ "output": out.to_str().expect("utf8") }),
        &mut verify_session,
    );
    assert_eq!(
        verify["outcome"], "success",
        "independent verification must accept the built output: {verify}"
    );
    assert_eq!(
        verify["diagnostics"],
        json!([]),
        "a failing check would be reported here: {verify}"
    );

    // The output reads as the replacement, and the source is untouched.
    let before = std::fs::read(&source).expect("source readable");
    let after = std::fs::read(&source).expect("source readable");
    assert_eq!(before, after, "the source document is never written");

    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(
        std::fs::read(&out).expect("output readable"),
    ))
    .expect("output is a zip");
    let mut document = String::new();
    {
        use std::io::Read;
        archive
            .by_name("word/document.xml")
            .expect("document part")
            .read_to_string(&mut document)
            .expect("read part");
    }
    assert_eq!(
        document.matches("<w:ins ").count(),
        1,
        "one insertion carries the replacement"
    );
    assert!(
        document.matches("<w:del ").count() >= 2,
        "each covered run is deleted: {} marks",
        document.matches("<w:del ").count()
    );

    let _ = std::fs::remove_dir_all(Path::new(&dir));
}
