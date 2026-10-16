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
fn a_dry_run_checks_a_structural_edit_instead_of_calling_it_empty() {
    let (_dir, workdir) = opened("dry-structural");
    let target = handle(&workdir, "中文区域内容");
    let _ = mcp(&[("document_open", json!({ "source": workdir }))]);
    let before = content_digest(Path::new(&workdir));

    // `dry_run` is the honest way to try a call whose handle you are unsure of,
    // so it is the first thing an agent reaches for — and it used to drop every
    // structural edit, hand the facade an empty replacement list, and report
    // `edit-empty`, "at least one edit is required", for a call that gave one.
    // A blind agent tried 20 shapes that way and concluded that inserting a
    // paragraph was impossible, which is what this test exists to prevent.
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({
                "dry_run": true,
                "edits": [
                    { "kind": "insert_after", "handle": target, "text": "新增的一段。" }
                ]
            }),
        ),
    ]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    let structural = data(&replies[1])
        .get("structural")
        .and_then(Value::as_array)
        .unwrap_or_else(|| {
            panic!(
                "a dry run reports the structure it checked: {:?}",
                replies[1]
            )
        });
    assert_eq!(structural.len(), 1, "{structural:?}");
    assert_eq!(structural[0]["kind"], "insert_after");
    assert_eq!(structural[0]["capability"], "supported");
    assert!(
        structural[0]
            .get("anchor")
            .is_some_and(|anchor| !anchor.is_null()),
        "the preview names the paragraph it resolved: {structural:?}"
    );
    assert_eq!(
        content_digest(Path::new(&workdir)),
        before,
        "a dry run wrote to the document"
    );

    // The same edit, written for real. Its reply carries the engine's own
    // vocabulary one level down (`result.next`), and the engine's step names
    // — `commit_sync`, `diff_preview` — are not mounted on this surface: the
    // commit happened inside this call. A blind agent quoted that advice back
    // as a defect, so what the agent reads names a step it has.
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "edits": [
                { "kind": "insert_after", "handle": target, "text": "新增的一段。" }
            ] }),
        ),
    ]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    let written = serde_json::to_string(data(&replies[1])).expect("reply as text");
    for absent in ["commit_sync", "diff_preview", "workdir_open", "facade_edit"] {
        assert!(
            !written.contains(absent),
            "the reply tells the agent to call `{absent}`, which is not mounted: {written}"
        );
    }

    // The replace path relays the engine's own report one level down
    // (`edit.engine.next`), and it carried the same advice.
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "edits": [
                { "handle": handle(&workdir, "中文区域内容"), "replace_with": "中文区块区域内容" }
            ] }),
        ),
    ]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    let replaced = serde_json::to_string(data(&replies[1])).expect("reply as text");
    for absent in ["commit_sync", "diff_preview", "workdir_open", "facade_edit"] {
        assert!(
            !replaced.contains(absent),
            "the replace reply tells the agent to call `{absent}`: {replaced}"
        );
    }
}

#[test]
fn a_second_edit_in_a_leaf_the_first_rewrote_merges_into_one_island() {
    let (dir, workdir) = opened("leaf-composition");
    let first = handle(&workdir, "中文区域内容");
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "edits": [{ "handle": first, "replace_with": "中文区块区域内容A" }] }),
        ),
    ]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);

    // The second edit shrinks a span inside the very leaf the first one rewrote.
    // Its offsets are the caller's, and the leaf already carries an edit, so the
    // two spaces no longer agree — which is the state that used to record a
    // SECOND island over the same bytes and build a `word/document.xml` with an
    // unclosed `w:t`. Measured on a scratch copy of a real patent: 6 saves
    // reported `output-not-verified` and had each already written the malformed
    // file to the path the caller asked for.
    let second = handle(&workdir, "中文区块区域内容A");
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "edits": [{ "handle": second, "replace_with": "中文区域内容A" }] }),
        ),
    ]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);

    let islands =
        std::fs::read_to_string(Path::new(&workdir).join("islands.json")).expect("islands.json");
    let parsed: Value = serde_json::from_str(&islands).expect("islands json");
    let edits = parsed["edits"].as_array().expect("edits");
    assert!(!edits.is_empty(), "no island recorded: {islands}");
    let mut leaves: Vec<(String, u64)> = edits
        .iter()
        .map(|edit| {
            (
                edit["paragraph_id"].as_str().unwrap_or("").to_string(),
                edit["leaf_index"].as_u64().unwrap_or(0),
            )
        })
        .collect();
    leaves.sort();
    let mut unique = leaves.clone();
    unique.dedup();
    assert_eq!(
        unique.len(),
        leaves.len(),
        "two islands address one leaf, and the build splices both over the same bytes: {islands}"
    );

    // And the file it publishes is the document the workdir describes.
    let out = dir.join("composed.docx");
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        ("document_save", json!({ "output": out.to_string_lossy() })),
    ]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    let text = built_text(&out);
    assert!(
        text.contains("中文区域内容A"),
        "both edits reached the file: {text}"
    );
}

#[test]
fn a_save_publishes_only_what_it_verified() {
    let (dir, workdir) = opened("save-publish");
    let target = handle(&workdir, "中文区域内容");
    let out = dir.join("published.docx");
    std::fs::write(&out, b"a file that was here first").expect("placeholder");

    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "edits": [{ "handle": target, "replace_with": "中文区块区域内容" }] }),
        ),
        ("document_save", json!({ "output": out.to_string_lossy() })),
    ]);
    assert_eq!(outcome(&replies[2]), "success", "{:?}", replies[2]);
    assert!(
        built_text(&out).contains("中文区块区域内容"),
        "the publish replaced the old file with the built package"
    );
    // The build happens out of the way: nothing of it is left beside the file,
    // and the evidence the build and the verification wrote travels with the
    // document to the name it was published under.
    let mut staged: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&*dir).expect("scratch dir") {
        let name = entry
            .expect("entry")
            .file_name()
            .to_string_lossy()
            .into_owned();
        if name.contains(".building-") {
            staged.push(name);
        }
    }
    assert!(
        staged.is_empty(),
        "a staged build was left behind: {staged:?}"
    );
    assert!(
        out.with_file_name("published.docx.evidence.json").is_file(),
        "the build's evidence did not follow the document to its published name"
    );

    // A save that cannot write where it was told must not leave anything there:
    // the workdir itself is a directory, so the publish has nowhere to land.
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        ("document_save", json!({ "output": workdir })),
    ]);
    assert_ne!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    assert!(
        Path::new(&workdir).is_dir(),
        "a refused save damaged the workdir it was pointed at"
    );
    let mut leftovers: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&*dir).expect("scratch dir") {
        let name = entry
            .expect("entry")
            .file_name()
            .to_string_lossy()
            .into_owned();
        if name.contains(".building-") {
            leftovers.push(name);
        }
    }
    assert!(
        leftovers.is_empty(),
        "a refused save left a staged build behind: {leftovers:?}"
    );
}

#[test]
fn an_edit_sequence_across_calls_keeps_one_island_per_leaf_and_one_version() {
    let (dir, workdir) = opened("sequence");
    let out = dir.join("sequenced.docx");
    let wd = Path::new(&workdir);

    // 1-3: two edits in the same leaf in two calls (the leaf is rewritten twice),
    // then a dry run that must change nothing at all.
    let first = handle(&workdir, "中文区域内容");
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "edits": [{ "handle": first, "replace_with": "中文区块区域内容" }] }),
        ),
    ]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    let after_first = content_digest(wd);
    assert_islands_cover_distinct_leaves(wd);

    let second = handle(&workdir, "中文区块区域内容");
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "edits": [{ "handle": second, "replace_with": "中文区域内容A" }] }),
        ),
    ]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    assert_islands_cover_distinct_leaves(wd);

    let third = handle(&workdir, "中文区域内容A");
    let before_dry = content_digest(wd);
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "dry_run": true, "edits": [
                { "kind": "insert_after", "handle": third.clone(), "text": "不会写入的一段。" }
            ] }),
        ),
    ]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    assert_eq!(
        content_digest(wd),
        before_dry,
        "a dry run changed the workdir"
    );
    assert_ne!(after_first, before_dry, "the second edit did not land");

    // 4-5: a save that has to publish the composed edits as one version.
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        ("document_save", json!({ "output": out.to_string_lossy() })),
    ]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    let built = built_text(&out);
    assert!(built.contains("中文区域内容A"), "{built}");
    assert!(!built.contains("不会写入的一段。"), "{built}");

    // 6-7: keep editing after the save, then save again over the same path.
    let fourth = handle(&workdir, "中文区域内容A");
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "edits": [{ "handle": fourth, "replace_with": "中文区域内容" }] }),
        ),
        ("document_save", json!({ "output": out.to_string_lossy() })),
    ]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    assert_eq!(outcome(&replies[2]), "success", "{:?}", replies[2]);
    let rebuilt = built_text(&out);
    assert!(rebuilt.contains("中文区域内容"), "{rebuilt}");
    assert!(!rebuilt.contains("中文区域内容A"), "{rebuilt}");
    assert_islands_cover_distinct_leaves(wd);

    // 8-9: a batch that cannot be applied, then continuing as if it never was.
    // The refusal must leave the workdir exactly as it found it.
    let before_refusal = content_digest(wd);
    let good = handle(&workdir, "中文区域内容");
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "edits": [
                { "handle": good.clone(), "replace_with": "X" },
                { "handle": "ref_bogus", "replace_with": "Y" },
            ] }),
        ),
    ]);
    assert_eq!(outcome(&replies[1]), "failure", "{:?}", replies[1]);
    assert_eq!(
        content_digest(wd),
        before_refusal,
        "a refused batch changed the workdir"
    );

    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "edits": [{ "handle": good, "replace_with": "中文区块区域内容" }] }),
        ),
        ("document_save", json!({ "output": out.to_string_lossy() })),
    ]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    assert_eq!(outcome(&replies[2]), "success", "{:?}", replies[2]);
    assert!(built_text(&out).contains("中文区块区域内容"));

    // And the published set is one version: the document says so about itself.
    let replies = mcp(&[("document_open", json!({ "source": out.to_string_lossy() }))]);
    assert_eq!(outcome(&replies[0]), "success", "{:?}", replies[0]);
    assert_eq!(
        data(&replies[0])["sidecars"]["state"],
        "consistent",
        "the sequence published a document whose evidence is not its own: {:?}",
        data(&replies[0])["sidecars"]
    );
}

/// One island per leaf, across every call the sequence made.
fn assert_islands_cover_distinct_leaves(workdir: &Path) {
    let islands = std::fs::read_to_string(workdir.join("islands.json")).expect("islands.json");
    let parsed: Value = serde_json::from_str(&islands).expect("islands json");
    let edits = parsed["edits"].as_array().expect("edits");
    let mut leaves: Vec<(String, u64)> = edits
        .iter()
        .map(|edit| {
            (
                edit["paragraph_id"].as_str().unwrap_or("").to_string(),
                edit["leaf_index"].as_u64().unwrap_or(0),
            )
        })
        .collect();
    leaves.sort();
    let mut unique = leaves.clone();
    unique.dedup();
    assert_eq!(
        unique.len(),
        leaves.len(),
        "two islands address one leaf, which the build cannot splice: {islands}"
    );
}

#[test]
fn a_session_can_leave_a_workdir_without_losing_its_work() {
    let (dir, first) = opened("release-first");
    let second_source = dir.join("second.docx");
    std::fs::copy(fixture("styled"), &second_source).expect("copy fixture");
    let second_workdir = dir.join("second.docx2typed");
    let target = handle(&first, "中文区域内容");

    // ONE connection throughout: the binding belongs to the session, and a new
    // process has none — which is also why this is a surface question and not an
    // engine one.
    let replies = mcp(&[
        ("document_open", json!({ "source": first })),
        (
            "document_edit",
            json!({ "edits": [{ "handle": target, "replace_with": "中文区块区域内容" }] }),
        ),
        // A second document on the same connection: refused, and the refusal
        // names the call that works.
        (
            "document_open",
            json!({ "source": second_source.to_string_lossy() }),
        ),
        // Saying so leaves the first and opens the second.
        (
            "document_open",
            json!({ "source": second_source.to_string_lossy(), "release_current": true }),
        ),
        // And going back finds the edit where it was: releasing a binding costs
        // nothing, because the workdir keeps its own state.
        (
            "document_open",
            json!({ "source": first, "release_current": true }),
        ),
        ("document_find", json!({ "query": "中文区块区域内容" })),
    ]);

    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    assert_eq!(
        outcome(&replies[2]),
        "failure",
        "a second document was opened without releasing the first: {:?}",
        replies[2]
    );
    assert_eq!(code(&replies[2]), "workdir-already-open");
    let why = envelope(&replies[2])
        .get("diagnostics")
        .and_then(Value::as_array)
        .and_then(|diagnostics| diagnostics.first())
        .and_then(|diagnostic| diagnostic.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    assert!(
        why.contains("release_current"),
        "the refusal does not name the call that would work: {why}"
    );

    assert_eq!(outcome(&replies[3]), "success", "{:?}", replies[3]);
    assert_eq!(
        data(&replies[3])["released"]["workdir"].as_str(),
        Some(first.as_str()),
        "the reply does not say which workdir it left: {:?}",
        data(&replies[3])["released"]
    );
    assert!(
        second_workdir.is_dir(),
        "the second document was not opened at all"
    );

    assert_eq!(outcome(&replies[4]), "success", "{:?}", replies[4]);
    assert_eq!(
        data(&replies[5])["counts"]["occurrences"].as_u64(),
        Some(1),
        "leaving the workdir lost the edit that was in it: {:?}",
        data(&replies[5])
    );
}

#[test]
fn an_opened_document_reports_whether_its_evidence_is_its_own() {
    let (dir, workdir) = opened("sidecar-set");
    let target = handle(&workdir, "中文区域内容");
    let out = dir.join("reported.docx");
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "edits": [{ "handle": target, "replace_with": "中文区块区域内容" }] }),
        ),
        ("document_save", json!({ "output": out.to_string_lossy() })),
    ]);
    assert_eq!(outcome(&replies[2]), "success", "{:?}", replies[2]);

    // The file, opened by the tool that wrote it: one version, and it says so.
    let replies = mcp(&[("document_open", json!({ "source": out.to_string_lossy() }))]);
    assert_eq!(outcome(&replies[0]), "success", "{:?}", replies[0]);
    assert_eq!(
        data(&replies[0])["sidecars"]["state"],
        "consistent",
        "a document just published does not describe itself as one version: {:?}",
        data(&replies[0])["sidecars"]
    );

    // Now the evidence beside it belongs to a different build — which is what an
    // interrupted publication leaves behind, and which nothing but the hashes can
    // tell apart. It must be reported, not trusted.
    let evidence = dir.join("reported.docx.evidence.json");
    let mut recorded: Value =
        serde_json::from_str(&std::fs::read_to_string(&evidence).expect("evidence"))
            .expect("evidence json");
    recorded["payload"]["outputs"]["docx"]["sha256"] = json!("0000000000000000");
    std::fs::write(&evidence, serde_json::to_vec(&recorded).expect("json")).expect("write");

    let replies = mcp(&[("document_open", json!({ "source": out.to_string_lossy() }))]);
    assert_eq!(outcome(&replies[0]), "success", "{:?}", replies[0]);
    assert_eq!(
        data(&replies[0])["sidecars"]["state"],
        "mixed",
        "evidence from another build was accepted as this document's: {:?}",
        data(&replies[0])["sidecars"]
    );
}

#[test]
fn find_counts_agree_with_the_verdicts_beside_them() {
    let (_dir, workdir) = opened("counts");
    // Two edits in one leaf first: the second one lands in a paragraph whose
    // offsets the draft has moved, which is what makes some occurrences of the
    // query uneditable while the paragraph's first occurrence is not.
    let first = handle(&workdir, "中文区域内容");
    let _ = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "edits": [{ "handle": first, "replace_with": "中文区块区域内容A" }] }),
        ),
    ]);
    let second = handle(&workdir, "中文区块区域内容A");
    let _ = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "edits": [{ "handle": second, "replace_with": "中文区域内容A中文区域内容" }] }),
        ),
    ]);

    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_find",
            json!({ "query": "中文区域内容", "limit": 40 }),
        ),
    ]);
    let found = data(&replies[1]);
    let matches = found["matches"].as_array().expect("matches");
    let counts = found.get("counts").expect("counts");

    let mut occurrences = 0u64;
    let mut editable = 0u64;
    for entry in matches {
        for occurrence in entry["occurrences"].as_array().unwrap_or(&Vec::new()) {
            occurrences += 1;
            if occurrence["verdict"]["editable"] == json!(true) {
                editable += 1;
            }
        }
    }
    assert!(occurrences > 0, "the query matched nothing: {found}");
    assert_eq!(
        counts["occurrences"].as_u64(),
        Some(occurrences),
        "the page count is not the list beside it: {found}"
    );
    assert_eq!(
        counts["editable"].as_u64(),
        Some(editable),
        "the editable count is not the list beside it: {found}"
    );
    assert_eq!(
        counts["not_editable"].as_u64(),
        Some(occurrences - editable),
        "the not-editable count is not the list beside it: {found}"
    );
    assert_eq!(
        counts["blocks"].as_u64(),
        Some(matches.len() as u64),
        "the block count is not the page's shape: {found}"
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
