//! Syncing a changed source into a workdir, and the guard that protects it.
//!
//! A workdir is its document's permanent editing home (ADR 0039). Following a
//! change made outside the tool is `extract` over that same workdir, which
//! rewrites the derived assets — so it is the one place a pending draft can be
//! destroyed silently, and nothing checked before. A caller that asks for it
//! anyway must be refused and told to commit, not quietly obeyed.

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
    let dir = std::env::temp_dir().join(format!("sync-guard-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// `docx2typed extract <source> -o <workdir>`, as the binary runs it.
/// `discard` passes the explicit declaration that the incoming file becomes the
/// current state.
fn extract_with(source: &Path, workdir: &Path, discard: bool) -> Value {
    let mut args = vec![
        "extract".to_string(),
        source.to_str().expect("utf8").to_string(),
        "-o".to_string(),
        workdir.to_str().expect("utf8").to_string(),
        "--json".to_string(),
    ];
    if discard {
        args.push("--discard-committed-versions".to_string());
    }
    let out = Command::new(bin())
        .args(&args)
        .output()
        .expect("extract runs");
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    serde_json::from_str(text.trim()).unwrap_or_else(|_| panic!("extract output: {text}"))
}

fn extract(source: &Path, workdir: &Path) -> Value {
    extract_with(source, workdir, false)
}

/// Call one MCP tool over stdio, replaying `session` first so the connection is
/// in the state the call expects.
fn call(tool: &str, args: Value, session: &[Value]) -> Value {
    let mut child = Command::new(bin())
        .arg("mcp")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("the server starts");
    let last_id;
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().expect("stdin");
        let mut id = 1;
        for earlier in session {
            writeln!(
                stdin,
                "{}",
                json!({
                    "jsonrpc": "2.0", "id": id, "method": "tools/call",
                    "params": { "name": earlier["name"], "arguments": earlier["arguments"] },
                })
            )
            .expect("write request");
            id += 1;
        }
        writeln!(
            stdin,
            "{}",
            json!({
                "jsonrpc": "2.0", "id": id, "method": "tools/call",
                "params": { "name": tool, "arguments": args },
            })
        )
        .expect("write request");
        last_id = id;
    }
    let output = child.wait_with_output().expect("the server answers");
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Ok(reply) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if reply["id"] == json!(last_id) {
            return reply["result"]["structuredContent"].clone();
        }
    }
    panic!("no reply for {tool}");
}

fn codes(result: &Value) -> Vec<String> {
    result["diagnostics"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|entry| entry["code"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn sync_into_a_workdir_with_a_pending_draft_is_refused_until_it_is_committed() {
    let dir = scratch("guard");
    let source = dir.join("source.docx");
    std::fs::copy(fixture("styled"), &source).expect("copy fixture");
    let workdir = dir.join("wd");

    let first = extract(&source, &workdir);
    assert_eq!(first["outcome"], "success", "the first extract: {first}");

    // Re-syncing a clean workdir is the supported way to follow a change.
    let clean = extract(&source, &workdir);
    assert_eq!(
        clean["outcome"], "success",
        "syncing a clean workdir must be allowed: {clean}"
    );

    // Leave a draft behind, the way a real edit does.
    let mut session = vec![json!({
        "name": "workdir_open",
        "arguments": { "workdir": workdir.to_str().expect("utf8"), "track": true, "author": "guard" },
    })];
    let patch = call(
        "document_patch",
        json!({
            "operation_id": "op_guard_patch",
            "hunks": [{ "paragraph_id": "P4", "old": "智能响应材料与 ABC", "new": "智能响应材料与 XYZ" }],
        }),
        &session,
    );
    assert_eq!(patch["outcome"], "success", "the draft: {patch}");
    assert!(
        workdir.join("edit.md").is_file(),
        "the draft projection exists"
    );

    // Now the sync would discard it. Refuse, and say what to do instead.
    let refused = extract(&source, &workdir);
    assert_eq!(
        refused["outcome"], "failure",
        "a pending draft must block the sync: {refused}"
    );
    assert!(
        codes(&refused)
            .iter()
            .any(|code| code == "workdir-has-pending-edit"),
        "the refusal names the condition: {refused}"
    );
    let message = refused["diagnostics"][0]["message"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        message.contains("commit_sync"),
        "the refusal names the way out: {message}"
    );
    // And the draft is still there: a refusal is not a partial sync.
    assert!(
        workdir.join("edit.md").is_file(),
        "nothing was thrown away by the refused sync"
    );

    // Commit it. The draft is gone, but now the workdir has moved PAST its
    // baseline: the committed version is work the source does not carry.
    session.push(json!({
        "name": "document_patch",
        "arguments": {
            "operation_id": "op_guard_patch",
            "hunks": [{ "paragraph_id": "P4", "old": "智能响应材料与 ABC", "new": "智能响应材料与 XYZ" }],
        },
    }));
    let commit = call(
        "commit_sync",
        json!({ "operation_id": "op_guard_commit" }),
        &session,
    );
    assert_eq!(commit["outcome"], "success", "the commit: {commit}");
    assert!(
        workdir.join("workdir.json").is_file(),
        "a committed workdir carries a pointer"
    );

    // `extract` is a BASELINE CONSTRUCTOR, not a merge: it re-derives the
    // template and the projection. Running it now would make the incoming file
    // the current state and leave this version only in history — measured, and
    // measured again with a byte-identical source, which lost the committed
    // text just the same.
    let refused = extract(&source, &workdir);
    assert_eq!(
        refused["outcome"], "failure",
        "a committed version must block the sync: {refused}"
    );
    assert!(
        codes(&refused)
            .iter()
            .any(|code| code == "workdir-not-at-baseline"),
        "the refusal names the condition: {refused}"
    );
    let message = refused["diagnostics"][0]["message"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    // Exporting B keeps it, but does not put the workdir back on its baseline:
    // the incoming file would STILL replace the current state. A message that
    // offered `build_docx` as the way out would teach the caller the opposite.
    assert!(
        message.contains("--discard-committed-versions"),
        "the refusal names the explicit switch: {message}"
    );
    assert!(
        !message.contains("build_docx` first"),
        "exporting must not be presented as the way out: {message}"
    );

    // And the guard really is about the workdir's state, not about history: the
    // export above changed no file in the workdir, so the same sync is still
    // refused afterwards.
    let out = dir.join("exported.docx");
    let build = call(
        "build_docx",
        json!({ "operation_id": "op_guard_build", "output": out.to_str().expect("utf8") }),
        &session,
    );
    assert_eq!(build["outcome"], "success", "the export: {build}");
    assert!(out.is_file(), "the export landed");
    let still_refused = extract(&source, &workdir);
    assert!(
        codes(&still_refused)
            .iter()
            .any(|code| code == "workdir-not-at-baseline"),
        "a successful export must not clear the guard: {still_refused}"
    );

    // The declaration unblocks it: the caller has said the incoming file
    // becomes the current state.
    let declared = extract_with(&source, &workdir, true);
    assert_eq!(
        declared["outcome"], "success",
        "the explicit declaration must be honoured: {declared}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The other half of the pair, and the one the guard must NOT break: a workdir
/// that was never edited is at its baseline, so following a source that changed
/// elsewhere is exactly what in-place sync is for.
#[test]
fn a_workdir_at_its_baseline_follows_a_source_that_changed() {
    let dir = scratch("follow");
    let source = dir.join("source.docx");
    std::fs::copy(fixture("styled"), &source).expect("copy fixture");
    let workdir = dir.join("wd");
    assert_eq!(extract(&source, &workdir)["outcome"], "success");

    // Someone else edits the document and saves it to the same path.
    std::fs::copy(fixture("plain"), &source).expect("the source moves on");

    let synced = extract(&source, &workdir);
    assert_eq!(
        synced["outcome"], "success",
        "an untouched workdir may follow its source: {synced}"
    );
    // It really took the new content in, rather than reporting success.
    let typed = std::fs::read_to_string(workdir.join("typed.md")).expect("typed.md");
    assert!(
        !typed.contains("智能响应材料与 ABC"),
        "the baseline is the incoming document now"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A reset has to leave the workdir COHERENT, not half-reset.
///
/// Measured before this was pinned: after an allowed reset the template was the
/// incoming document while `islands.json` still held an edit against the old
/// one and `workdir.json` still pointed at the old version. The next build then
/// refused — and its only offered escape was to accept the tear. The store lane
/// was the same story one layer down: `.docx2typed-store/` without the pointer
/// that goes with it is a workdir the store refuses to open.
#[test]
fn an_allowed_reset_leaves_a_workdir_that_builds() {
    let dir = scratch("reset");
    let source = dir.join("source.docx");
    std::fs::copy(fixture("styled"), &source).expect("copy fixture");
    let workdir = dir.join("wd");
    assert_eq!(extract(&source, &workdir)["outcome"], "success");

    let mut session = vec![json!({
        "name": "workdir_open",
        "arguments": { "workdir": workdir.to_str().expect("utf8"), "track": true, "author": "reset" },
    })];
    let patch = call(
        "document_patch",
        json!({
            "operation_id": "op_reset_patch",
            "hunks": [{ "paragraph_id": "P4", "old": "智能响应材料与 ABC", "new": "智能响应材料与 XYZ" }],
        }),
        &session,
    );
    assert_eq!(patch["outcome"], "success", "the draft: {patch}");
    session.push(json!({
        "name": "document_patch",
        "arguments": {
            "operation_id": "op_reset_patch",
            "hunks": [{ "paragraph_id": "P4", "old": "智能响应材料与 ABC", "new": "智能响应材料与 XYZ" }],
        },
    }));
    assert_eq!(
        call(
            "commit_sync",
            json!({ "operation_id": "op_reset_commit" }),
            &session
        )["outcome"],
        "success"
    );
    assert!(
        workdir.join(".docx2typed-store").is_dir(),
        "the commit gave the workdir a store lane"
    );

    // The source becomes a different document, and the caller declares the
    // reset.
    std::fs::copy(fixture("complex"), &source).expect("the source becomes another document");
    assert_eq!(
        extract_with(&source, &workdir, true)["outcome"],
        "success",
        "the declared reset"
    );

    // Everything expressed against the old baseline is gone with it.
    for stale in [
        "islands.json",
        "structure.json",
        "edit.md",
        "edit.state.json",
        "workdir.json",
        ".docx2typed-store",
    ] {
        assert!(
            !workdir.join(stale).exists(),
            "{stale} belongs to the old baseline and must not survive the reset"
        );
    }

    // And the result is usable: it builds, and the build is the incoming
    // document rather than a mixture.
    let out = dir.join("reset.docx");
    let build = call(
        "build_docx",
        json!({ "operation_id": "op_reset_build", "output": out.to_str().expect("utf8") }),
        &session,
    );
    assert_eq!(
        build["outcome"], "success",
        "a reset workdir must build: {build}"
    );
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
    assert!(
        !document.contains("智能响应材料"),
        "the old document is gone from the output"
    );
    assert!(
        document.contains("EDIT-ME"),
        "the incoming document is the output"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A review session must not survive a baseline replacement.
///
/// `.review/` is the one lane extraction cannot clear (this layer does not
/// depend on the review crate, and the review crate depends on THIS one, so it
/// cannot be given the job either). Measured before the session learned to
/// invalidate itself: after a reset the session still reported the old
/// document's origin and a changed paragraph the new document does not have, and
/// the next edit was refused with `agent-preflight-required` — a reset workdir
/// was simply uneditable, and nothing said why.
///
/// The anchor is the template, not the projection: the projection's hash is a
/// PROGRESS marker that `publish_current` gates on having advanced, so treating
/// a difference as staleness would make that gate a tautology and refuse every
/// commit with `current-not-changed`.
#[test]
fn a_review_session_does_not_survive_a_baseline_replacement() {
    let dir = scratch("review");
    let source = dir.join("source.docx");
    std::fs::copy(fixture("styled"), &source).expect("copy fixture");
    let workdir = dir.join("wd");
    assert_eq!(extract(&source, &workdir)["outcome"], "success");

    // A real edit on A, which creates the review lane.
    let mut session = vec![json!({
        "name": "workdir_open",
        "arguments": { "workdir": workdir.to_str().expect("utf8"), "track": true, "author": "A" },
    })];
    let patch = call(
        "document_patch",
        json!({
            "operation_id": "op_rev_patch",
            "hunks": [{ "paragraph_id": "P4", "old": "智能响应材料与 ABC", "new": "智能响应材料与 A版" }],
        }),
        &session,
    );
    assert_eq!(patch["outcome"], "success", "the draft on A: {patch}");
    session.push(json!({
        "name": "document_patch",
        "arguments": {
            "operation_id": "op_rev_patch",
            "hunks": [{ "paragraph_id": "P4", "old": "智能响应材料与 ABC", "new": "智能响应材料与 A版" }],
        },
    }));
    assert_eq!(
        call(
            "commit_sync",
            json!({ "operation_id": "op_rev_commit" }),
            &session
        )["outcome"],
        "success"
    );
    let session_file = workdir.join(".review").join("session.json");
    assert!(session_file.is_file(), "the edit created a review session");

    // The baseline is replaced by a different document.
    std::fs::copy(fixture("complex"), &source).expect("the source becomes another document");
    assert_eq!(
        extract_with(&source, &workdir, true)["outcome"],
        "success",
        "the declared reset"
    );

    // A NEW connection (the server is spawned per call) edits the new document.
    let mut after = vec![json!({
        "name": "workdir_open",
        "arguments": { "workdir": workdir.to_str().expect("utf8"), "track": true, "author": "C" },
    })];
    let edit = call(
        "document_patch",
        json!({
            "operation_id": "op_rev_patch_c",
            "hunks": [{ "paragraph_id": "P3", "old": "EDIT-ME: ", "new": "EDIT-ME! " }],
        }),
        &after,
    );
    assert_eq!(
        edit["outcome"], "success",
        "a reset workdir must be editable: {edit}"
    );
    after.push(json!({
        "name": "document_patch",
        "arguments": {
            "operation_id": "op_rev_patch_c",
            "hunks": [{ "paragraph_id": "P3", "old": "EDIT-ME: ", "new": "EDIT-ME! " }],
        },
    }));
    let commit = call(
        "commit_sync",
        json!({ "operation_id": "op_rev_commit_c" }),
        &after,
    );
    assert_eq!(
        commit["outcome"], "success",
        "and the edit must still commit: {commit}"
    );

    // The session is about the NEW document, and the rebinding was recorded
    // rather than done quietly.
    let session_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&session_file).expect("session readable"))
            .expect("session json");
    let typed = std::fs::read(workdir.join("typed.md")).expect("typed.md");
    let typed_sha = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(&typed);
        format!("{:x}", hasher.finalize())
    };
    assert_eq!(
        session_json["current_snapshot"]["typed_sha256"], typed_sha,
        "the session is bound to the document that is actually here"
    );
    assert_ne!(
        session_json["current_snapshot"]["changed_paragraph_ids"],
        json!(["P4"]),
        "the old document's paragraph is not claimed by the new session"
    );
    let history =
        std::fs::read_to_string(workdir.join(".review").join("history.jsonl")).expect("history");
    assert!(
        history.contains("session-rebound"),
        "the rebinding is recorded: {history}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
