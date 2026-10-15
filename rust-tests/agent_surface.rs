//! The five-tool agent surface, end to end.
//!
//! `docs/specs/08_agent_surface.md` §6 says the acceptance is the WORKFLOW, not
//! the tool calls: a task passes only when the exported file reaches the expected
//! state. These tests therefore read the built DOCX back rather than trusting a
//! success flag, and they drive the surface the way an agent does — one call at a
//! time, with no knowledge of the engine underneath.
//!
//! The fault-injection tests cover the failure modes that are injectable from
//! outside: a batch that cannot be applied, and a retry. Two others are covered
//! by construction rather than by test, and are named in the design: the edit and
//! its sync run inside ONE store mutation, so a refusal anywhere discards the
//! generation it was building (there is no window in which the draft has moved
//! and the canonical document has not); and the store's ledger check runs before
//! that closure, so a replay never reaches a handle at all.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn bin() -> PathBuf {
    let name = if cfg!(windows) {
        "docx2typed.exe"
    } else {
        "docx2typed"
    };
    let release = repo_root().join("target").join("release").join(name);
    if release.is_file() {
        return release;
    }
    let debug = repo_root().join("target").join("debug").join(name);
    assert!(
        debug.is_file(),
        "build the binary first: {}",
        debug.display()
    );
    debug
}

fn fixture(name: &str) -> PathBuf {
    repo_root()
        .join("corpus")
        .join("release")
        .join(format!("{name}.docx"))
}

/// A scratch directory that removes itself, panics included.
struct ScratchGuard(PathBuf);

impl std::ops::Deref for ScratchGuard {
    type Target = Path;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for ScratchGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn scratch(tag: &str) -> ScratchGuard {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("agent-surface-{tag}-{}-{seq}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    ScratchGuard(dir)
}

/// Run the MCP server with a batch of calls on one connection.
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
        .map(|reply| serde_json::from_str(reply).expect("reply json"))
        .collect()
}

fn envelope(reply: &Value) -> &Value {
    reply.get("structuredContent").expect("envelope")
}

fn data(reply: &Value) -> &Value {
    envelope(reply).get("data").unwrap_or(&Value::Null)
}

fn outcome(reply: &Value) -> &str {
    envelope(reply)
        .get("outcome")
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn code(reply: &Value) -> String {
    envelope(reply)
        .get("diagnostics")
        .and_then(Value::as_array)
        .and_then(|diagnostics| diagnostics.first())
        .and_then(|diagnostic| diagnostic.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// The text of a built DOCX, read back from the file rather than from a report.
fn built_text(path: &Path) -> String {
    let bytes = std::fs::read(path).expect("built docx");
    let cursor = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor).expect("zip");
    let mut document = archive.by_name("word/document.xml").expect("document.xml");
    let mut xml = String::new();
    document.read_to_string(&mut xml).expect("read xml");
    let mut text = String::new();
    let mut rest = xml.as_str();
    while let Some(start) = rest.find("<w:t") {
        let after = &rest[start..];
        let Some(open_end) = after.find('>') else {
            break;
        };
        let body = &after[open_end + 1..];
        let Some(close) = body.find("</w:t>") else {
            break;
        };
        text.push_str(&body[..close]);
        rest = &body[close..];
    }
    text
}

/// A digest of everything a caller could observe about the document's content.
///
/// The question "was anything written" is about the CONTENT, not about whether a
/// generation was minted: a mutation that changes nothing may still publish a
/// generation, and that is harmless. Hashing the content files answers the
/// question a caller actually has.
fn content_digest(workdir: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    // The files that ARE the document. Evidence, ledgers and the store pointer
    // legitimately change on every call; including them would answer a different
    // question than the one this test asks.
    let names: Vec<String> = [
        "typed.md",
        "edit.md",
        "islands.json",
        "structure.json",
        "_template.docx",
        "edit.state.json",
    ]
    .iter()
    .map(|name| name.to_string())
    .collect();
    for name in names {
        let path = workdir.join(&name);
        if !path.is_file() {
            continue;
        }
        digest.update(name.as_bytes());
        if let Ok(bytes) = std::fs::read(&path) {
            digest.update(&bytes);
        }
    }
    hex::encode(digest.finalize())
}

/// Extract a fixture, then open the workdir — the state every test starts from.
fn opened(tag: &str) -> (ScratchGuard, String) {
    let dir = scratch(tag);
    let source = dir.join("source.docx");
    std::fs::copy(fixture("styled"), &source).expect("copy fixture");
    let workdir = dir.join("wd");
    let status = Command::new(bin())
        .args([
            "extract",
            source.to_str().expect("path"),
            "-o",
            workdir.to_str().expect("path"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("extract");
    assert!(status.success(), "extract failed");
    (dir, workdir.to_string_lossy().into_owned())
}

/// The first handle `document_find` offers for `query`, or a failure that says
/// what the search saw — an empty handle would make a later assert lie.
fn handle(workdir: &str, query: &str) -> String {
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        ("document_find", json!({ "query": query })),
    ]);
    let found = data(&replies[1]);
    let entry = found
        .get("matches")
        .and_then(Value::as_array)
        .and_then(|matches| matches.first())
        .unwrap_or_else(|| panic!("no match for {query:?}: {found}"));
    assert_eq!(
        entry["verdict"]["editable"], true,
        "the test needs an editable span: {entry}"
    );
    entry["handle"].as_str().expect("handle").to_string()
}

#[test]
fn the_whole_workflow_runs_through_the_five_tools() {
    let (dir, workdir) = opened("workflow");
    let target = handle(&workdir, "中文区域内容");

    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "edits": [{ "handle": target, "replace_with": "中文区块区域内容" }] }),
        ),
        (
            "document_save",
            json!({ "output": dir.join("out.docx").to_string_lossy() }),
        ),
        ("document_status", json!({})),
    ]);
    assert_eq!(outcome(&replies[1]), "success", "edit: {:?}", replies[1]);
    assert_eq!(outcome(&replies[2]), "success", "save: {:?}", replies[2]);
    assert_eq!(outcome(&replies[3]), "success", "status: {:?}", replies[3]);

    // The acceptance is the FILE, not the success flag.
    let text = built_text(&dir.join("out.docx"));
    assert!(
        text.contains("中文区块区域内容"),
        "the built document does not carry the edit: {text}"
    );
    // Nothing is pending once the edit is settled and saved.
    assert_eq!(
        data(&replies[3])
            .get("next")
            .cloned()
            .unwrap_or(Value::Null),
        Value::Null,
        "a settled document has no next step: {:?}",
        data(&replies[3])
    );
}

#[test]
fn a_blocked_batch_writes_nothing() {
    let (_dir, workdir) = opened("blocked");
    let good = handle(&workdir, "中文区域内容");
    // Open first, THEN measure. `document_open` materialises the projection, so
    // a digest taken before it would report the open's work as the blocked
    // batch's — the measurement has to isolate the call under test.
    let _ = mcp(&[("document_open", json!({ "source": workdir }))]);
    let before = content_digest(Path::new(&workdir));

    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({
                "edits": [
                    { "handle": good, "replace_with": "X" },
                    { "handle": "ref_bogus", "replace_with": "Y" }
                ]
            }),
        ),
    ]);
    assert_eq!(
        outcome(&replies[1]),
        "failure",
        "a blocked batch is not success"
    );
    assert_eq!(code(&replies[1]), "edit-blocked");
    assert_eq!(
        content_digest(Path::new(&workdir)),
        before,
        "a blocked batch changed the document's content"
    );
}

#[test]
fn a_retry_with_the_same_operation_id_replays_and_does_not_apply_twice() {
    let (dir, workdir) = opened("replay");
    let target = handle(&workdir, "中文区域内容");
    let edits = json!({
        "operation_id": "surface-replay-1",
        "edits": [{ "handle": target, "replace_with": "中文区块区域内容" }]
    });

    let first = mcp(&[
        ("document_open", json!({ "source": workdir })),
        ("document_edit", edits.clone()),
    ]);
    assert_eq!(outcome(&first[1]), "success", "{:?}", first[1]);
    let after_first = content_digest(Path::new(&workdir));

    // The handles are stale now — a naive re-run would refuse. The ledger check
    // runs before the closure, so a replay never touches them.
    let replay = mcp(&[
        ("document_open", json!({ "source": workdir })),
        ("document_edit", edits),
    ]);
    assert_eq!(outcome(&replay[1]), "success", "replay: {:?}", replay[1]);
    assert_eq!(
        data(&replay[1]).get("operation_id").and_then(Value::as_str),
        Some("surface-replay-1")
    );
    assert_eq!(
        content_digest(Path::new(&workdir)),
        after_first,
        "a replay changed the content, so it applied the edit a second time"
    );

    // And the document carries the edit once, not twice.
    let out = dir.join("replay.docx");
    let saved = mcp(&[
        ("document_open", json!({ "source": workdir })),
        ("document_save", json!({ "output": out.to_string_lossy() })),
    ]);
    assert_eq!(outcome(&saved[1]), "success", "{:?}", saved[1]);
    let text = built_text(&out);
    assert_eq!(
        text.matches("中文区块区域内容").count(),
        1,
        "the edit was applied more than once: {text}"
    );
}

#[test]
fn the_same_operation_id_with_different_edits_is_refused() {
    let (_dir, workdir) = opened("reused-id");
    let target = handle(&workdir, "中文区域内容");
    let first = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({
                "operation_id": "surface-reused-1",
                "edits": [{ "handle": target, "replace_with": "中文区块区域内容" }]
            }),
        ),
    ]);
    assert_eq!(outcome(&first[1]), "success", "{:?}", first[1]);

    let second = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({
                "operation_id": "surface-reused-1",
                "edits": [{ "handle": target, "replace_with": "完全不同的文本" }]
            }),
        ),
    ]);
    assert_eq!(
        outcome(&second[1]),
        "failure",
        "a reused id with different edits must not be treated as a retry: {:?}",
        second[1]
    );
    assert_eq!(code(&second[1]), "operation-id-reused");
}

#[test]
fn re_opening_the_same_document_does_not_rebuild_the_baseline() {
    let (dir, _) = opened("reopen-docx");
    let source = dir.join("source.docx");
    let workdir = dir.join("source.docx2typed");
    let baseline = |path: &Path| -> Option<u64> {
        let text = std::fs::read_to_string(path.join("baseline.json")).ok()?;
        serde_json::from_str::<Value>(&text)
            .ok()?
            .get("epoch")?
            .as_u64()
    };

    // The first open extracts; the second must FIND that workdir. Extracting
    // again rebuilds the baseline, and once the workdir carries committed edits
    // that rebuild is refused — the observable failure being "the document
    // opened, I edited it, and now it will not open again".
    let first = mcp(&[(
        "document_open",
        json!({ "source": source.to_string_lossy() }),
    )]);
    assert_eq!(outcome(&first[0]), "success", "{:?}", first[0]);
    let after_first = baseline(&workdir);
    assert!(after_first.is_some(), "no baseline after the first open");

    let second = mcp(&[(
        "document_open",
        json!({ "source": source.to_string_lossy() }),
    )]);
    assert_eq!(outcome(&second[0]), "success", "{:?}", second[0]);
    assert_eq!(
        baseline(&workdir),
        after_first,
        "re-opening the same DOCX rebuilt the baseline"
    );

    // The case that actually bit: edit, then re-open the DOCX.
    let target = handle(&workdir.to_string_lossy(), "中文区域内容");
    let edited = mcp(&[
        (
            "document_open",
            json!({ "source": workdir.to_string_lossy() }),
        ),
        (
            "document_edit",
            json!({ "edits": [{ "handle": target, "replace_with": "中文区块区域内容" }] }),
        ),
    ]);
    assert_eq!(outcome(&edited[1]), "success", "{:?}", edited[1]);
    let reopened = mcp(&[(
        "document_open",
        json!({ "source": source.to_string_lossy() }),
    )]);
    assert_eq!(
        outcome(&reopened[0]),
        "success",
        "a document that opened once must open again after an edit: {:?}",
        reopened[0]
    );
    assert_eq!(
        baseline(&workdir),
        after_first,
        "re-opening after an edit rebuilt the baseline"
    );
}

#[test]
fn find_reports_its_coverage_rather_than_implying_it_saw_everything() {
    let (_dir, workdir) = opened("coverage");
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        ("document_find", json!({ "query": "中" })),
    ]);
    let counts = data(&replies[1]).get("counts").expect("counts");
    for field in [
        "total_matches",
        "total_blocks",
        "returned_blocks",
        "complete",
    ] {
        assert!(
            counts.get(field).is_some(),
            "counts must carry {field}: {counts}"
        );
    }
    // `complete` is the field a caller acts on: without it, the first page of a
    // long result reads exactly like the whole of a short one.
    assert!(
        counts["complete"].is_boolean(),
        "complete must be a boolean: {counts}"
    );
}
