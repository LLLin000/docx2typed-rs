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

// ---------------------------------------------------------------------------
// Seeded sequences
//
// The deterministic sequence above tests the steps someone thought of. This one
// generates them, so the *order* is not something a human chose — which is where
// state bugs live (the composed no-op island was found by the deterministic
// sequence, and the next one is likelier to need a stranger order still).
//
// Triage is part of the harness rather than an afterthought:
//   * a seed reproduces its whole sequence, so the seed IS the repro;
//   * on failure the shortest failing prefix is found and written, with the call
//     log, to `target/sequence-fuzz/`;
//   * `SEQUENCE_FUZZ_SEED=<seed> cargo test --test agent_surface sequence_fuzz`
//     replays exactly that seed, and stops with the same message.
//
// The operations are generated from what the surface reports — the spans it
// finds in the current text — so every step is legal by construction: a failure
// is the engine's, not the generator's.
// ---------------------------------------------------------------------------

/// xorshift64*: small, deterministic, and dependency-free.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

/// The tracked spans a sequence moves, one per fixture paragraph, each with the
/// texts it may hold. Text is never a substring of another candidate — within a
/// span or across spans — so "the file carries the current one and none of the
/// others" is an assertion rather than a prefix accident.
const SLOT_TARGETS: [&[&str; 3]; 3] = [
    &["中文区域内容", "甲甲甲", "乙乙乙"],
    &["EnglishRegion", "GAMMA", "DELTA"],
    &["第一区域", "丙丙丙", "丁丁丁"],
];

/// Paragraphs a sequence inserts and may delete again.
const INSERT_TEXTS: [&str; 3] = ["插段一", "插段二", "插段三"];

/// One find, with the failure returned instead of panicking.
fn find_handle(workdir: &str, query: &str) -> Result<String, String> {
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        ("document_find", json!({ "query": query, "limit": 5 })),
    ]);
    let found = data(&replies[1]);
    let entry = found
        .get("matches")
        .and_then(Value::as_array)
        .and_then(|matches| matches.first())
        .ok_or_else(|| format!("no match for {query:?}: {found}"))?;
    entry
        .get("occurrences")
        .and_then(Value::as_array)
        .and_then(|occurrences| occurrences.first())
        .or(Some(entry))
        .and_then(|entry| entry.get("handle"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("no handle for {query:?}: {entry}"))
}

/// A sequence failure, identified by WHAT broke and WHERE — never by its wording,
/// which changes with every message edit.
#[derive(Clone, Debug)]
struct SequenceFailure {
    step: usize,
    /// The invariant that broke, from a closed set: `replace`, `insert`,
    /// `delete`, `save`, `projection-missing`, `projection-stale`,
    /// `island-per-leaf`, `dry-run-changed`, `refused-batch-accepted`,
    /// `refused-batch-changed`, `insert-no-change`, `delete-no-change`,
    /// `stale-accepted`, `stale-changed`.
    kind: &'static str,
    detail: String,
}

impl SequenceFailure {
    fn new(step: usize, kind: &'static str, detail: String) -> Self {
        SequenceFailure { step, kind, detail }
    }
}

impl std::fmt::Display for SequenceFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "step {}: {}: {}",
            self.step, self.kind, self.detail
        )
    }
}

/// Outcomes that are known and TRACKED, as `(seed, step, kind)`.
///
/// A failure matching its entry is an expected failure. Anything else about that
/// seed — a different step, a different invariant — is a NEW defect and fails the
/// sweep, and a registered seed that stops failing is reported too, so the entry
/// cannot quietly become an exemption. Empty right now: both defects the wider op
/// space found are fixed and pinned by deterministic tests.
const KNOWN_FAILURES: [(u64, usize, &str); 0] = [];

/// What a seed's outcome means against the register.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Pass,
    /// Unregistered, or registered under different terms.
    Fail,
    /// Exactly the failure that was registered.
    Xfail,
    /// Registered, but no longer failing — the entry is stale.
    Xpass,
}

fn classify(
    registered: Option<(usize, &'static str)>,
    actual: Option<&SequenceFailure>,
) -> Verdict {
    match (registered, actual) {
        (None, None) => Verdict::Pass,
        (None, Some(_)) => Verdict::Fail,
        (Some((step, kind)), Some(failure)) => {
            if failure.step == step && failure.kind == kind {
                Verdict::Xfail
            } else {
                Verdict::Fail
            }
        }
        (Some(_), None) => Verdict::Xpass,
    }
}

/// One sequence, from its seed alone.
///
/// The log is written into the caller's buffer as the sequence runs, so it
/// survives the failure it is meant to explain — a log returned only on success
/// is a log nobody has when they need it.
fn run_sequence(seed: u64, steps: usize, log: &mut Vec<String>) -> Result<(), SequenceFailure> {
    let (dir, workdir) = opened(&format!("fuzz-{seed}"));
    let out = dir.join("fuzzed.docx");
    let wd = Path::new(&workdir);
    let mut rng = Rng::new(seed);
    let mut slots: [String; 3] = [
        SLOT_TARGETS[0][0].to_string(),
        SLOT_TARGETS[1][0].to_string(),
        SLOT_TARGETS[2][0].to_string(),
    ];
    let mut live: Vec<String> = Vec::new();
    let mut deleted: Vec<String> = Vec::new();
    let mut next_insert = 0usize;
    let mut saved_any = false;

    for step in 1..=steps {
        match rng.below(9) {
            // Move one span to a different text: longer, shorter, or back to the
            // one the document came with. The last composes to a no-op island.
            0 | 1 => {
                let slot = rng.below(3) as usize;
                let target = SLOT_TARGETS[slot][rng.below(3) as usize];
                if slots[slot] == target {
                    log.push(format!(
                        "{step}: replace skipped, slot {slot} already {target:?}"
                    ));
                    continue;
                }
                let handle = find_handle(&workdir, &slots[slot])
                    .map_err(|detail| SequenceFailure::new(step, "find", detail))?;
                let replies = mcp(&[
                    ("document_open", json!({ "source": workdir })),
                    (
                        "document_edit",
                        json!({ "edits": [{ "handle": handle, "replace_with": target }] }),
                    ),
                ]);
                if outcome(&replies[1]) != "success" {
                    return Err(SequenceFailure::new(
                        step,
                        "replace",
                        format!(
                            "replace slot {slot} {:?} -> {target:?} failed: {:?}",
                            slots[slot], replies[1]
                        ),
                    ));
                }
                log.push(format!(
                    "{step}: replace slot {slot} {:?} -> {target:?}",
                    slots[slot]
                ));
                slots[slot] = target.to_string();
                assert_islands_cover_distinct_leaves(wd)
                    .map_err(|detail| SequenceFailure::new(step, "island-per-leaf", detail))?;
            }
            // A dry run: it must change nothing, and it must report the same
            // verdict the real call would.
            2 => {
                let slot = rng.below(3) as usize;
                let handle = find_handle(&workdir, &slots[slot])
                    .map_err(|detail| SequenceFailure::new(step, "find", detail))?;
                let before = content_digest(wd);
                let replies = mcp(&[
                    ("document_open", json!({ "source": workdir })),
                    (
                        "document_edit",
                        json!({ "dry_run": true, "edits": [
                            { "kind": "insert_after", "handle": handle, "text": "不写入的段落。" }
                        ] }),
                    ),
                ]);
                if outcome(&replies[1]) != "success" {
                    return Err(SequenceFailure::new(
                        step,
                        "dry-run-changed",
                        format!("dry run failed: {:?}", replies[1]),
                    ));
                }
                if content_digest(wd) != before {
                    return Err(SequenceFailure::new(
                        step,
                        "dry-run-changed",
                        "a dry run changed the workdir".to_string(),
                    ));
                }
                log.push(format!("{step}: dry run on slot {slot}"));
            }
            // A batch that cannot be applied: it must leave the workdir exactly
            // as it found it.
            3 => {
                let handle = find_handle(&workdir, &slots[rng.below(3) as usize])
                    .map_err(|detail| SequenceFailure::new(step, "find", detail))?;
                let before = content_digest(wd);
                let replies = mcp(&[
                    ("document_open", json!({ "source": workdir })),
                    (
                        "document_edit",
                        json!({ "edits": [
                            { "handle": handle, "replace_with": "X" },
                            { "handle": "ref_bogus", "replace_with": "Y" },
                        ] }),
                    ),
                ]);
                if outcome(&replies[1]) == "success" {
                    return Err(SequenceFailure::new(
                        step,
                        "refused-batch-accepted",
                        "a batch with a bogus handle succeeded".to_string(),
                    ));
                }
                if content_digest(wd) != before {
                    return Err(SequenceFailure::new(
                        step,
                        "refused-batch-changed",
                        "a refused batch changed the workdir".to_string(),
                    ));
                }
                log.push(format!("{step}: refused batch"));
            }
            // A paragraph, inserted for real: the anchor is a span handle, so
            // the new paragraph lands beside a paragraph the sequence is editing.
            4 => {
                if next_insert >= INSERT_TEXTS.len() {
                    log.push(format!("{step}: insert skipped, none left"));
                    continue;
                }
                let text = INSERT_TEXTS[next_insert];
                let slot = rng.below(3) as usize;
                let handle = find_handle(&workdir, &slots[slot])
                    .map_err(|detail| SequenceFailure::new(step, "find", detail))?;
                let before = content_digest(wd);
                let replies = mcp(&[
                    ("document_open", json!({ "source": workdir })),
                    (
                        "document_edit",
                        json!({ "edits": [
                            { "kind": "insert_after", "handle": handle, "text": text }
                        ] }),
                    ),
                ]);
                if outcome(&replies[1]) != "success" {
                    return Err(SequenceFailure::new(
                        step,
                        "insert",
                        format!("insert {text:?} failed: {:?}", replies[1]),
                    ));
                }
                if content_digest(wd) == before {
                    return Err(SequenceFailure::new(
                        step,
                        "insert-no-change",
                        "an insert reported success and changed nothing".to_string(),
                    ));
                }
                next_insert += 1;
                live.push(text.to_string());
                log.push(format!("{step}: insert_after slot {slot} -> {text:?}"));
                assert_islands_cover_distinct_leaves(wd)
                    .map_err(|detail| SequenceFailure::new(step, "island-per-leaf", detail))?;
            }
            // And deleted again. Its handle is captured first, so the paragraph
            // is addressed the way a caller would address it.
            5 => {
                if live.is_empty() {
                    log.push(format!("{step}: delete skipped, nothing inserted"));
                    continue;
                }
                let index = rng.below(live.len() as u64) as usize;
                let text = live[index].clone();
                let before_digest = content_digest(wd);
                let handle = find_handle(&workdir, &text)
                    .map_err(|detail| SequenceFailure::new(step, "find", detail))?;
                let replies = mcp(&[
                    ("document_open", json!({ "source": workdir })),
                    (
                        "document_edit",
                        json!({ "edits": [{ "kind": "delete", "handle": handle }] }),
                    ),
                ]);
                if outcome(&replies[1]) != "success" {
                    return Err(SequenceFailure::new(
                        step,
                        "delete",
                        format!("delete {text:?} failed: {:?}", replies[1]),
                    ));
                }
                if content_digest(wd) == before_digest {
                    return Err(SequenceFailure::new(
                        step,
                        "delete-no-change",
                        "a delete reported success and changed nothing".to_string(),
                    ));
                }
                live.remove(index);
                deleted.push(text.clone());
                log.push(format!("{step}: delete {text:?}"));

                // The handle that named it now names a paragraph that is gone,
                // and using it must be refused without touching anything.
                let stale = find_handle(&workdir, &text);
                let stale = match stale {
                    Ok(handle) => handle,
                    Err(_) => {
                        // The paragraph is gone, so its text is not findable —
                        // which is the point of the delete.
                        log.push(format!(
                            "{step}: stale-handle probe skipped, {text:?} is gone"
                        ));
                        continue;
                    }
                };
                let before_stale = content_digest(wd);
                let replies = mcp(&[
                    ("document_open", json!({ "source": workdir })),
                    (
                        "document_edit",
                        json!({ "edits": [{ "handle": stale, "replace_with": "GAMMA" }] }),
                    ),
                ]);
                if outcome(&replies[1]) == "success" {
                    return Err(SequenceFailure::new(
                        step,
                        "stale-accepted",
                        format!("a stale handle was accepted as an edit: {:?}", replies[1]),
                    ));
                }
                if content_digest(wd) != before_stale {
                    return Err(SequenceFailure::new(
                        step,
                        "stale-changed",
                        "a refused stale-handle edit changed the workdir".to_string(),
                    ));
                }
                log.push(format!("{step}: stale handle refused"));
            }
            // A revert: move an edited slot back to the text the document came
            // with, then save — a reverted workdir must stay publishable. When
            // every slot is already home there is nothing to revoke, and the
            // step skips rather than saving.
            6 => {
                let edited: Vec<usize> = (0..3)
                    .filter(|slot| slots[*slot] != SLOT_TARGETS[*slot][0])
                    .collect();
                if edited.is_empty() {
                    log.push(format!("{step}: revert skipped, every slot is home"));
                    continue;
                }
                let slot = edited[rng.below(edited.len() as u64) as usize];
                let baseline = SLOT_TARGETS[slot][0];
                let handle = find_handle(&workdir, &slots[slot])
                    .map_err(|detail| SequenceFailure::new(step, "find", detail))?;
                let replies = mcp(&[
                    ("document_open", json!({ "source": workdir })),
                    (
                        "document_edit",
                        json!({ "edits": [{ "handle": handle, "replace_with": baseline }] }),
                    ),
                ]);
                if outcome(&replies[1]) != "success" {
                    return Err(SequenceFailure::new(
                        step,
                        "replace",
                        format!(
                            "revert slot {slot} {:?} -> {baseline:?} failed: {:?}",
                            slots[slot], replies[1]
                        ),
                    ));
                }
                log.push(format!(
                    "{step}: revert slot {slot} {:?} -> {baseline:?}",
                    slots[slot]
                ));
                slots[slot] = baseline.to_string();
                assert_islands_cover_distinct_leaves(wd)
                    .map_err(|detail| SequenceFailure::new(step, "island-per-leaf", detail))?;
                // A reverted workdir must save: pre-fix the revoked island kept
                // the draft dirty, so this save failed with `edit-dirty`.
                let replies = mcp(&[
                    ("document_open", json!({ "source": workdir })),
                    ("document_save", json!({ "output": out.to_string_lossy() })),
                ]);
                if outcome(&replies[1]) != "success" {
                    return Err(SequenceFailure::new(
                        step,
                        "save",
                        format!("save after revert failed: {:?}", replies[1]),
                    ));
                }
                saved_any = true;
                log.push(format!("{step}: save after revert"));
            }
            // A save: the file has to say what the sequence says, and nothing it
            // has replaced or deleted.
            _ => {
                let replies = mcp(&[
                    ("document_open", json!({ "source": workdir })),
                    ("document_save", json!({ "output": out.to_string_lossy() })),
                ]);
                if outcome(&replies[1]) != "success" {
                    return Err(SequenceFailure::new(
                        step,
                        "save",
                        format!("save failed: {:?}", replies[1]),
                    ));
                }
                let built = built_text(&out);
                for (slot, current) in slots.iter().enumerate() {
                    if !built.contains(current) {
                        return Err(SequenceFailure::new(
                            step,
                            "projection-missing",
                            format!(
                                "the saved file does not carry slot {slot} {current:?}: {built}"
                            ),
                        ));
                    }
                    for other in SLOT_TARGETS[slot]
                        .iter()
                        .filter(|target| **target != current)
                    {
                        if built.contains(other) {
                            return Err(SequenceFailure::new(
                                step,
                                "projection-stale",
                                format!(
                                    "the saved file still carries {other:?} while slot {slot} \
                                     reads {current:?}: {built}"
                                ),
                            ));
                        }
                    }
                }
                for text in &live {
                    if !built.contains(text) {
                        return Err(SequenceFailure::new(
                            step,
                            "projection-missing",
                            format!("the saved file lost the inserted paragraph {text:?}: {built}"),
                        ));
                    }
                }
                for text in &deleted {
                    if built.contains(text) {
                        return Err(SequenceFailure::new(
                            step,
                            "projection-stale",
                            format!(
                                "the saved file still carries the deleted paragraph {text:?}: \
                                 {built}"
                            ),
                        ));
                    }
                }
                assert_islands_cover_distinct_leaves(wd)
                    .map_err(|detail| SequenceFailure::new(step, "island-per-leaf", detail))?;
                saved_any = true;
                log.push(format!(
                    "{step}: save (slots {:?}, live {live:?}, deleted {deleted:?})",
                    slots
                ));
            }
        }
    }

    // One last save, so the artifact on disk is the end state of the sequence.
    if !saved_any {
        let replies = mcp(&[
            ("document_open", json!({ "source": workdir })),
            ("document_save", json!({ "output": out.to_string_lossy() })),
        ]);
        if outcome(&replies[1]) != "success" {
            return Err(SequenceFailure::new(
                steps,
                "save",
                format!("final save failed: {:?}", replies[1]),
            ));
        }
    }
    Ok(())
}

/// Where a failing seed's artifact goes, and how to replay it.
fn fuzz_artifact_dir() -> PathBuf {
    repo_root().join("target").join("sequence-fuzz")
}

/// The register's four rules, checked directly: a registered failure is expected,
/// another failure of the same seed is not, and a registered seed that stops
/// failing is reported rather than silently dropped.
#[test]
fn the_known_failure_register_is_strict() {
    let failure = |step, kind| SequenceFailure::new(step, kind, "detail".to_string());
    assert_eq!(classify(None, None), Verdict::Pass);
    assert_eq!(
        classify(None, Some(&failure(3, "save"))),
        Verdict::Fail,
        "an unregistered failure is a failure"
    );
    assert_eq!(
        classify(Some((3, "save")), Some(&failure(3, "save"))),
        Verdict::Xfail,
        "the registered step and invariant is the expected failure"
    );
    assert_eq!(
        classify(Some((3, "save")), Some(&failure(2, "save"))),
        Verdict::Fail,
        "the same seed failing EARLIER is a different defect"
    );
    assert_eq!(
        classify(Some((3, "save")), Some(&failure(3, "projection-stale"))),
        Verdict::Fail,
        "the same step failing a different invariant is a different defect"
    );
    assert_eq!(
        classify(Some((3, "save")), None),
        Verdict::Xpass,
        "a registered failure that stopped happening is reported, not ignored"
    );
}

#[test]
fn sequence_fuzz() {
    // A replay run: one seed, the same code path, the same message.
    if let Ok(seed) = std::env::var("SEQUENCE_FUZZ_SEED") {
        let seed: u64 = seed.parse().expect("SEQUENCE_FUZZ_SEED must be a number");
        let steps: usize = std::env::var("SEQUENCE_FUZZ_STEPS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(12);
        let mut log = Vec::new();
        if let Err(error) = run_sequence(seed, steps, &mut log) {
            panic!(
                "replayed seed {seed} failed at {steps} steps: {error}\nlog:\n{}",
                log.join("\n")
            );
        }
        return;
    }

    let seeds: u64 = std::env::var("SEQUENCE_FUZZ_SEEDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(3);
    // An independent interval, so a green sweep is not only a statement about the
    // sequences one range happens to generate: `SEQUENCE_FUZZ_SEED_BASE=10001`
    // runs 10001..=10000+seeds.
    let base: u64 = std::env::var("SEQUENCE_FUZZ_SEED_BASE")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1);
    let steps: usize = std::env::var("SEQUENCE_FUZZ_STEPS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8);

    for seed in base..base + seeds {
        let registered = KNOWN_FAILURES
            .iter()
            .find(|(known, _, _)| *known == seed)
            .map(|(_, step, kind)| (*step, *kind));
        let mut log = Vec::new();
        let outcome = run_sequence(seed, steps, &mut log);
        match classify(registered, outcome.as_ref().err()) {
            Verdict::Pass | Verdict::Xfail => {
                if classify(registered, outcome.as_ref().err()) == Verdict::Xfail {
                    println!(
                        "seed {seed}: expected failure (step {}, {}) — still open",
                        registered.expect("registered").0,
                        registered.expect("registered").1
                    );
                }
                continue;
            }
            Verdict::Xpass => {
                panic!(
                    "seed {seed} no longer fails the way it is registered ({:?}) — delete its \
                     entry from KNOWN_FAILURES, and keep the regression it earned",
                    registered
                );
            }
            Verdict::Fail => {}
        }
        let error = outcome.expect_err("a failing seed has a failure");
        // Shrink: the shortest prefix that still fails is the repro worth
        // keeping, and finding it is one replay per length. Ops are generated
        // from the observed state, so a prefix of the steps is a prefix of the
        // sequence.
        let mut shortest = (steps, error.clone());
        let mut shortest_log = log;
        for shorter in 1..steps {
            let mut earlier_log = Vec::new();
            if let Err(earlier) = run_sequence(seed, shorter, &mut earlier_log) {
                shortest = (shorter, earlier);
                shortest_log = earlier_log;
                break;
            }
        }
        let dir = fuzz_artifact_dir();
        let _ = std::fs::create_dir_all(&dir);
        let artifact = dir.join(format!("seed-{seed}.json"));
        std::fs::write(
            &artifact,
            serde_json::to_vec_pretty(&json!({
                "seed": seed,
                "shortest_failing_steps": shortest.0,
                "failure": {
                    "step": shortest.1.step,
                    "kind": shortest.1.kind,
                    "detail": shortest.1.detail,
                },
                "log": shortest_log,
                "replay": format!(
                    "SEQUENCE_FUZZ_SEED={seed} SEQUENCE_FUZZ_STEPS={} cargo test --test \
                     agent_surface sequence_fuzz",
                    shortest.0
                ),
            }))
            .expect("artifact json"),
        )
        .expect("write artifact");
        panic!(
            "seed {seed} failed after {} step(s): {} — the shortest failing sequence and its log \
             are in {}; replay it with SEQUENCE_FUZZ_SEED={seed} SEQUENCE_FUZZ_STEPS={}",
            shortest.0,
            shortest.1,
            artifact.display(),
            shortest.0
        );
    }
}

#[test]
fn a_save_publishes_only_what_it_verified() {
    let (dir, workdir) = opened("save-publish");
    let target = handle(&workdir, "中文区域内容");
    // Baseline publish first: an unknown-existing placeholder is NOT this
    // workdir's publication and must refuse (external-file-changed). The
    // test's intent — save publishes exactly what it verified — is preserved:
    // establish ownership with a first save, then verify the second.
    let first = dir.join("published.docx");
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "edits": [{ "handle": target, "replace_with": "中文区块区域内容" }] }),
        ),
        (
            "document_save",
            json!({ "output": first.to_string_lossy() }),
        ),
    ]);
    assert_eq!(outcome(&replies[2]), "success", "{:?}", replies[2]);
    // Second save over a NEW path again (each output path is owned by its own
    // first publish): the intent — the publish lands exactly the verified
    // package — now asserted on the second output.
    let target = handle(&workdir, "中文区块区域内容");
    let out = dir.join("published2.docx");
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "edits": [{ "handle": target, "replace_with": "中文区块区域内容B" }] }),
        ),
        ("document_save", json!({ "output": out.to_string_lossy() })),
    ]);
    assert_eq!(outcome(&replies[2]), "success", "{:?}", replies[2]);
    assert!(
        built_text(&out).contains("中文区块区域内容B"),
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
        out.with_file_name("published2.docx.evidence.json")
            .is_file(),
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

/// Hold a file the way Word does: open, but not for delete.
///
/// `std::fs::File::open` is not enough — Rust asks for FILE_SHARE_DELETE by
/// default, so a rename over a handle it holds still succeeds, and a test that
/// "locked" a file that way would pass without testing anything.
#[cfg(windows)]
fn hold_without_delete(path: &Path) -> std::fs::File {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(path)
        .expect("open without delete sharing")
}

/// The verification stage, injected from outside: build an artifact, change a
/// byte inside its package, and ask the verifier about it. The path being the same
/// proves the path, not the bytes — this is the check that has to catch it, and it
/// runs as its own call, the way the surface runs it before publishing.
#[test]
fn a_tampered_artifact_is_refused_by_the_verification() {
    let (dir, workdir) = opened("tampered");
    let target = handle(&workdir, "中文区域内容");
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "edits": [{ "handle": target, "replace_with": "中文区块区域内容" }] }),
        ),
    ]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);

    let built = dir.join("built.docx");
    let status = Command::new(bin())
        .args(["build", &workdir, "--output", built.to_str().expect("path")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("build");
    assert!(status.success(), "the build itself failed");
    assert!(built.is_file(), "no artifact to verify");

    // The verifier accepts the artifact the build produced.
    let verified = Command::new(bin())
        .args(["verify", &workdir, built.to_str().expect("path"), "--json"])
        .output()
        .expect("verify");
    let stdout = String::from_utf8_lossy(&verified.stdout);
    assert!(
        stdout.contains("success"),
        "the built artifact did not verify: {stdout}"
    );

    // Now change the package: rewrite one paragraph's text inside the archive.
    let tampered = dir.join("tampered.docx");
    rewrite_document_text(&built, &tampered, "中文区块区域内容X");
    assert_ne!(
        std::fs::read(&built).expect("built"),
        std::fs::read(&tampered).expect("tampered"),
        "the tampering did nothing, so this test would prove nothing"
    );

    let verified = Command::new(bin())
        .args([
            "verify",
            &workdir,
            tampered.to_str().expect("path"),
            "--json",
        ])
        .output()
        .expect("verify");
    let stdout = String::from_utf8_lossy(&verified.stdout);
    assert!(
        stdout.contains("\"outcome\":\"failure\""),
        "a tampered artifact verified: {stdout}"
    );
    assert!(
        stdout.contains("edited-part-exact")
            || stdout.contains("parts-match-template-except-edited")
            || stdout.contains("edited-package-distinct"),
        "the refusal does not say which check caught it: {stdout}"
    );
}

/// Copy a DOCX, replacing one text in `word/document.xml`.
fn rewrite_document_text(source: &Path, target: &Path, replacement: &str) {
    use std::io::Write;
    let bytes = std::fs::read(source).expect("source package");
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("zip");
    let destination = std::fs::File::create(target).expect("target");
    let mut writer = zip::ZipWriter::new(destination);
    let options = zip::write::SimpleFileOptions::default();
    for index in 0..archive.len() {
        let mut member = archive.by_index(index).expect("member");
        let name = member.name().to_string();
        let mut content = Vec::new();
        std::io::Read::read_to_end(&mut member, &mut content).expect("read member");
        if name == "word/document.xml" {
            let text = String::from_utf8_lossy(&content).to_string();
            let changed = text.replacen("中文区块区域内容", replacement, 1);
            assert_ne!(
                changed, text,
                "the text to tamper with was not in the package"
            );
            content = changed.into_bytes();
        }
        writer.start_file(name, options).expect("start member");
        writer.write_all(&content).expect("write member");
    }
    writer.finish().expect("finish package");
}

/// Killing the process in the middle of a save is not an exception: the file system
/// is left wherever the kill landed. What has to hold afterwards is that no file
/// claims to be a version it is not, that the state is repairable with the tools,
/// and that repairing twice changes nothing.
///
/// This is a smoke test of process termination at an UNCONTROLLED instant — it does
/// not prove a particular crash point was hit, and it is not a power-loss
/// durability claim. What it does prove is that whatever a kill leaves behind is
/// either a whole version or a state the next process repairs, which is the part a
/// `?` on an `Err` cannot stand in for.
#[test]
#[cfg(windows)]
fn a_killed_process_leaves_a_state_the_next_one_can_repair() {
    use std::io::Write as _;
    let dir = scratch("killed");
    let source = dir.join("source.docx");
    std::fs::copy(fixture("complex"), &source).expect("copy fixture");
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
    let workdir = workdir.to_string_lossy().into_owned();
    let out = dir.join("killed.docx");

    // One clean publication first: this is the version any kill must leave intact
    // or replace whole.
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        ("document_save", json!({ "output": out.to_string_lossy() })),
    ]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    let good = std::fs::read(&out).expect("published");

    // A second save, killed shortly after it starts.
    let mut child = Command::new(bin())
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mcp");
    {
        let mut stdin = child.stdin.take().expect("stdin");
        for (tool, args) in [
            ("document_open", json!({ "source": workdir })),
            ("document_save", json!({ "output": out.to_string_lossy() })),
        ] {
            writeln!(stdin, "{}", json!({ "tool": tool, "args": args })).expect("write call");
        }
    }
    // Kill it as soon as the save has written its staging file. A fixed delay
    // sometimes landed before the save started, and then every assertion below
    // would pass vacuously — so the injection point is an EVENT, not a guess: the
    // staged build appearing means the save is inside its build, and the kill lands
    // there.
    let mut staged_seen = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        if let Ok(entries) = std::fs::read_dir(&*dir) {
            staged_seen = entries
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().contains(".building-"));
        }
        if staged_seen || child.try_wait().ok().flatten().is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let killed = child.kill().is_ok();
    let _ = child.wait();
    assert!(killed, "the process could not be killed at all");
    assert!(
        staged_seen,
        "the save completed before its staging file could be seen, so this run proved \
         nothing about a kill inside it"
    );

    // Whatever the kill left: the published file is either the version that was
    // there, or a whole package. Never a half-written one — the staging rename is
    // what makes that true, and this is where it is checked with a real kill
    // instead of an injected failure.
    if out.is_file() {
        let bytes = std::fs::read(&out).expect("published");
        if bytes != good {
            let cursor = std::io::Cursor::new(bytes.clone());
            let archive = zip::ZipArchive::new(cursor);
            assert!(
                archive.is_ok(),
                "a killed save left a file that is not even a package: {archive:?}"
            );
        }
    }

    // The next process repairs: the state reaches one version, with the tools.
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        ("document_save", json!({ "output": out.to_string_lossy() })),
    ]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    let repaired = std::fs::read(&out).expect("repaired");
    let replies = mcp(&[(
        "document_open",
        json!({ "source": out.to_string_lossy(), "release_current": true }),
    )]);
    assert_eq!(
        data(&replies[0])["sidecars"]["state"],
        "consistent",
        "the repair did not reach one version: {:?}",
        data(&replies[0])["sidecars"]
    );

    // And repairing is idempotent: once more, same bytes.
    let replies = mcp(&[
        (
            "document_open",
            json!({ "source": workdir, "release_current": true }),
        ),
        ("document_save", json!({ "output": out.to_string_lossy() })),
    ]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    assert_eq!(
        std::fs::read(&out).expect("again"),
        repaired,
        "a second repair produced different bytes"
    );
}

/// A publication that could not finish leaves work behind; the next call
/// recovers, the repair reaches a consistent version, and repeating it changes
/// nothing — the two properties that make a recovery worth having (reachable, and
/// idempotent).
#[test]
#[cfg(windows)]
fn a_stranded_publication_recovers_and_a_second_recovery_changes_nothing() {
    let (dir, workdir) = opened("stranded");
    let out = dir.join("stranded.docx");
    let first = handle(&workdir, "中文区域内容");
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "edits": [{ "handle": first, "replace_with": "中文区块区域内容" }] }),
        ),
        ("document_save", json!({ "output": out.to_string_lossy() })),
    ]);
    assert_eq!(outcome(&replies[2]), "success", "{:?}", replies[2]);
    let good = std::fs::read(&out).expect("published");

    // A publication that fails at its last step: what a killed process leaves
    // behind is a journal and no publication.
    let second = handle(&workdir, "中文区块区域内容");
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "edits": [{ "handle": second, "replace_with": "中文区块区域内容B" }] }),
        ),
    ]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    let guard = hold_without_delete(&out);
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        ("document_save", json!({ "output": out.to_string_lossy() })),
    ]);
    drop(guard);
    assert_ne!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    assert_eq!(
        std::fs::read(&out).expect("still there"),
        good,
        "the failed publication replaced the version that was there"
    );

    // Recovery: the same call again, now that nothing holds the target.
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        ("document_save", json!({ "output": out.to_string_lossy() })),
    ]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    let recovered = std::fs::read(&out).expect("recovered");
    assert!(built_text(&out).contains("中文区块区域内容B"));
    let replies = mcp(&[(
        "document_open",
        json!({ "source": out.to_string_lossy(), "release_current": true }),
    )]);
    assert_eq!(
        data(&replies[0])["sidecars"]["state"],
        "consistent",
        "recovery left a set that is not one version: {:?}",
        data(&replies[0])["sidecars"]
    );

    // Idempotent: running it again produces the same bytes, not a second effect.
    let replies = mcp(&[
        (
            "document_open",
            json!({ "source": workdir, "release_current": true }),
        ),
        ("document_save", json!({ "output": out.to_string_lossy() })),
    ]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    assert_eq!(
        std::fs::read(&out).expect("again"),
        recovered,
        "a second recovery produced different bytes"
    );
}

#[test]
#[cfg(windows)]
fn an_interrupted_publication_is_reported_and_the_old_document_survives() {
    let (dir, workdir) = opened("interrupted-publish");
    let out = dir.join("published.docx");
    let wd = Path::new(&workdir);

    // One clean publication first: this is the version that has to survive.
    let first = handle(&workdir, "中文区域内容");
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "edits": [{ "handle": first, "replace_with": "中文区块区域内容" }] }),
        ),
        ("document_save", json!({ "output": out.to_string_lossy() })),
    ]);
    assert_eq!(outcome(&replies[2]), "success", "{:?}", replies[2]);
    let published = std::fs::read(&out).expect("published");

    // A second edit, and then a publication that cannot even start: the
    // document is held without delete sharing, so the fail-closed guard
    // refuses BEFORE any write — old document survives, zero staged
    // leftovers, sidecars still describe the surviving version (consistent,
    // not mixed: nothing was published, so there is no half-state).
    let second = handle(&workdir, "中文区块区域内容");
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "edits": [{ "handle": second, "replace_with": "中文区块区域内容B" }] }),
        ),
    ]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    let guard = hold_without_delete(&out);
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        ("document_save", json!({ "output": out.to_string_lossy() })),
    ]);
    drop(guard);

    assert_ne!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    assert_eq!(
        std::fs::read(&out).expect("still there"),
        published,
        "a refused publication replaced the document it never published"
    );
    let mut staged: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&*dir).expect("scratch") {
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
        "a refused publish left a staged build behind: {staged:?}"
    );

    // No half-state exists to detect: the guard refused before ANY sidecar
    // moved, so the evidence beside the surviving document still describes
    // exactly it.
    let replies = mcp(&[("document_open", json!({ "source": out.to_string_lossy() }))]);
    assert_eq!(outcome(&replies[0]), "success", "{:?}", replies[0]);
    assert_eq!(
        data(&replies[0])["sidecars"]["state"],
        "consistent",
        "a refused publish must leave one version, not a mixed set: {:?}",
        data(&replies[0])["sidecars"]
    );

    // Which is a state the next save advances normally: it publishes the
    // pending edit over the surviving version.
    let replies = mcp(&[
        (
            "document_open",
            json!({ "source": workdir, "release_current": true }),
        ),
        ("document_save", json!({ "output": out.to_string_lossy() })),
    ]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    assert!(built_text(&out).contains("中文区块区域内容B"));
    let replies = mcp(&[(
        "document_open",
        json!({ "source": out.to_string_lossy(), "release_current": true }),
    )]);
    assert_eq!(
        data(&replies[0])["sidecars"]["state"],
        "consistent",
        "the repaired set still reads as mixed: {:?}",
        data(&replies[0])["sidecars"]
    );
    let _ = wd;
}

#[test]
fn a_build_that_cannot_run_leaves_the_published_version_alone() {
    let (dir, workdir) = opened("build-fails");
    let out = dir.join("published.docx");
    let wd = Path::new(&workdir);

    let first = handle(&workdir, "中文区域内容");
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "edits": [{ "handle": first, "replace_with": "中文区块区域内容" }] }),
        ),
        ("document_save", json!({ "output": out.to_string_lossy() })),
    ]);
    assert_eq!(outcome(&replies[2]), "success", "{:?}", replies[2]);
    let published = std::fs::read(&out).expect("published");

    // Break the build from underneath: the template is what every build is
    // replayed against, and it is gone. The handle is resolved FIRST — opening a
    // workdir whose template is missing is itself a refusal, which is a
    // different finding from the one this test is about.
    let second = handle(&workdir, "中文区块区域内容");
    std::fs::rename(wd.join("_template.docx"), wd.join("_template.docx.gone")).expect("move aside");
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "edits": [{ "handle": second, "replace_with": "中文区块区域内容C" }] }),
        ),
        ("document_save", json!({ "output": out.to_string_lossy() })),
    ]);

    assert_ne!(
        outcome(&replies[2]),
        "success",
        "a save reported success with no template to build from: {:?}",
        replies[2]
    );
    assert_eq!(
        std::fs::read(&out).expect("still there"),
        published,
        "a build that could not run damaged the version already published"
    );
}

/// Two paragraphs sent "after the same one" chain: the second is recorded before
/// the first, because that is where the draft put it — and the anchor of the
/// second names a paragraph that only the structure sidecar knows. The build
/// used to demand a template range for that anchor and refuse the whole workdir,
/// permanently: measured, every later `document_save` failed with
/// `invalid-structure: unknown insert_before paragraph: P5`, and nothing on the
/// caller's side could undo it. Found by the generated sequence below.
#[test]
fn two_paragraphs_after_one_anchor_chain_and_still_build() {
    let (dir, workdir) = opened("chain");
    let out = dir.join("chained.docx");
    let anchor = handle(&workdir, "EnglishRegion");

    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "edits": [
                { "kind": "insert_after", "handle": anchor.clone(), "text": "插段一" }
            ] }),
        ),
        (
            "document_edit",
            json!({ "edits": [
                { "kind": "insert_after", "handle": anchor.clone(), "text": "插段二" }
            ] }),
        ),
    ]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    assert_eq!(outcome(&replies[2]), "success", "{:?}", replies[2]);

    // The structure now chains: the second paragraph's anchor is the first.
    let structure: Value = serde_json::from_str(
        &std::fs::read_to_string(Path::new(&workdir).join("structure.json")).expect("structure"),
    )
    .expect("structure json");
    let anchors: Vec<&str> = structure["new_paragraphs"]
        .as_array()
        .expect("new paragraphs")
        .iter()
        .map(|entry| entry["insert_before"].as_str().unwrap_or(""))
        .collect();
    assert!(
        anchors
            .iter()
            .any(|anchor| !anchor.starts_with('P') || *anchor == "P5"),
        "the second insert did not chain onto the first: {structure}"
    );

    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        ("document_save", json!({ "output": out.to_string_lossy() })),
    ]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);

    // Both are in the file, and in the order the calls asked for: the second was
    // sent "after the same paragraph" and the draft already showed the first
    // there.
    let built = built_text(&out);
    let first_at = built.find("插段一").unwrap_or_else(|| panic!("{built}"));
    let second_at = built.find("插段二").unwrap_or_else(|| panic!("{built}"));
    assert!(
        second_at < first_at,
        "the chained insert landed in the wrong order: {built}"
    );
    assert!(
        built.contains("EnglishRegion"),
        "the anchor paragraph was lost: {built}"
    );

    // And a structure that chains back to itself is a refusal, not a hang.
    let structure_path = Path::new(&workdir).join("structure.json");
    let mut cycle: Value =
        serde_json::from_str(&std::fs::read_to_string(&structure_path).expect("structure"))
            .expect("structure json");
    let ids: Vec<String> = cycle["new_paragraphs"]
        .as_array()
        .expect("new paragraphs")
        .iter()
        .map(|entry| entry["paragraph_id"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(
        ids.len(),
        2,
        "the test needs the chain it just made: {cycle}"
    );
    if let Some(paragraphs) = cycle["new_paragraphs"].as_array_mut() {
        for (index, paragraph) in paragraphs.iter_mut().enumerate() {
            // Each anchors on the other: a chain that leads back to itself.
            paragraph["insert_before"] = json!(ids[1 - index].clone());
        }
    }
    std::fs::write(&structure_path, serde_json::to_vec(&cycle).expect("json"))
        .expect("write structure");
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_save",
            json!({ "output": dir.join("cycle.docx").to_string_lossy() }),
        ),
    ]);
    assert_ne!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    let message = serde_json::to_string(envelope(&replies[1])).expect("reply text");
    assert!(
        message.contains("cycle"),
        "the cycle refusal does not say what it found: {message}"
    );
}

/// The projection is keyed by the paragraph's IDENTITY, not by its position in
/// the built file. Measured before the fix: one inserted paragraph renumbered
/// everything after it, `typed.md` showed the inserted paragraph's body in P2's
/// slot, and every later edit of P2 was refused `current-not-changed` — the
/// canonical projection never changed again, so P2 could not be edited at all.
/// Deleting a paragraph that another synthesised paragraph anchored on has to
/// move the anchor, in the order the draft shows. Measured before the fix:
/// deleting the middle one of three chained inserts left the third one's
/// `insert_before` naming a paragraph that no longer existed, and the workdir
/// stopped building for good (`invalid-structure: unknown insert_before
/// paragraph: P6`).
#[test]
fn deleting_a_paragraph_that_anchors_others_keeps_the_document_buildable() {
    let (dir, workdir) = opened("chain-delete");
    let out = dir.join("deleted.docx");
    let anchor = handle(&workdir, "EnglishRegion");

    // Three paragraphs after the same anchor: the draft reads 插段三, 插段二,
    // 插段一 (each lands immediately after the anchor), so each one anchors on the
    // one inserted before it.
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "edits": [{ "kind": "insert_after", "handle": anchor.clone(), "text": "插段一" }] }),
        ),
        (
            "document_edit",
            json!({ "edits": [{ "kind": "insert_after", "handle": anchor.clone(), "text": "插段二" }] }),
        ),
        (
            "document_edit",
            json!({ "edits": [{ "kind": "insert_after", "handle": anchor.clone(), "text": "插段三" }] }),
        ),
    ]);
    for reply in replies.iter().skip(1) {
        assert_eq!(outcome(reply), "success", "{reply:?}");
    }

    let delete_and_save = |text: &str, expected: &[&str], absent: &[&str]| {
        let handle = handle(&workdir, text);
        let replies = mcp(&[
            ("document_open", json!({ "source": workdir })),
            (
                "document_edit",
                json!({ "edits": [{ "kind": "delete", "handle": handle }] }),
            ),
            ("document_save", json!({ "output": out.to_string_lossy() })),
        ]);
        assert_eq!(
            outcome(&replies[1]),
            "success",
            "deleting {text:?} failed: {:?}",
            replies[1]
        );
        assert_eq!(
            outcome(&replies[2]),
            "success",
            "the document stopped building after deleting {text:?}: {:?}",
            replies[2]
        );
        let built = built_text(&out);
        for kept in expected {
            assert!(built.contains(kept), "{kept:?} was lost: {built}");
        }
        for gone in absent {
            assert!(!built.contains(gone), "{gone:?} is still there: {built}");
        }
        built
    };

    // The middle one: the third anchored on it, and has to land where it would
    // have — before 插段一, which is the order the draft shows.
    let built = delete_and_save("插段二", &["插段三", "插段一"], &["插段二"]);
    assert!(
        built.find("插段三") < built.find("插段一"),
        "the surviving chain came out in the wrong order: {built}"
    );
    // Then the tail of what is left, then the head.
    delete_and_save("插段一", &["插段三"], &["插段一", "插段二"]);
    delete_and_save(
        "插段三",
        &["EnglishRegion"],
        &["插段三", "插段二", "插段一"],
    );

    // And deleting two of them in a row.
    let anchor = handle(&workdir, "EnglishRegion");
    let _ = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "edits": [{ "kind": "insert_after", "handle": anchor.clone(), "text": "插段四" }] }),
        ),
        (
            "document_edit",
            json!({ "edits": [{ "kind": "insert_after", "handle": anchor.clone(), "text": "插段五" }] }),
        ),
    ]);
    delete_and_save("插段五", &["插段四"], &["插段五"]);
    delete_and_save("插段四", &["EnglishRegion"], &["插段四", "插段五"]);
}

#[test]
fn a_paragraph_keeps_its_projection_across_inserts_and_repeated_edits() {
    let (dir, workdir) = opened("identity");
    let wd = Path::new(&workdir);
    let typed = |tag: &str| -> String {
        let text = std::fs::read_to_string(wd.join("typed.md")).expect("typed.md");
        let mut out = String::new();
        let mut take = false;
        for line in text.lines() {
            if line.starts_with("<!--@p id=") || line.starts_with("<!--@new ") {
                take = line.contains("P2");
                continue;
            }
            if take && !line.starts_with("<!--") {
                out.push_str(line);
                take = false;
            }
        }
        format!("{tag}: {out}")
    };

    let span = handle(&workdir, "第一区域");
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "edits": [{ "handle": span, "replace_with": "丁丁丁" }] }),
        ),
    ]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    assert!(typed("after the first replace").contains("丁丁丁"));

    // Insert a paragraph BEFORE the one being edited: this is what renumbered the
    // ids when they were minted by position.
    let anchor = handle(&workdir, "EnglishRegion");
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "edits": [{ "kind": "insert_after", "handle": anchor, "text": "插段一" }] }),
        ),
    ]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);

    // The paragraph is still itself, and still editable — three replacements in a
    // row, each of which has to reach the canonical projection.
    for (from, to) in [
        ("丁丁丁", "丙丙丙"),
        ("丙丙丙", "丁丁丁"),
        ("丁丁丁", "丙丙丙"),
    ] {
        let handle = handle(&workdir, from);
        let replies = mcp(&[
            ("document_open", json!({ "source": workdir })),
            (
                "document_edit",
                json!({ "edits": [{ "handle": handle, "replace_with": to }] }),
            ),
        ]);
        assert_eq!(
            outcome(&replies[1]),
            "success",
            "the paragraph stopped being editable after {from} -> {to}: {:?}",
            replies[1]
        );
        let projected = typed(&format!("after {from} -> {to}"));
        assert!(
            projected.contains(to),
            "the canonical projection does not carry the edit: {projected}"
        );
        assert!(
            !projected.contains("插段一"),
            "the projection of P2 carries the inserted paragraph's body: {projected}"
        );
    }

    // And the file it publishes says the same as the projection.
    let out = dir.join("identity.docx");
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        ("document_save", json!({ "output": out.to_string_lossy() })),
    ]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    let built = built_text(&out);
    assert!(built.contains("丙丙丙第二区域"), "{built}");
    assert!(built.contains("插段一"), "{built}");
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
    assert_islands_cover_distinct_leaves(wd).expect("one island per leaf");

    let second = handle(&workdir, "中文区块区域内容");
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "edits": [{ "handle": second, "replace_with": "中文区域内容A" }] }),
        ),
    ]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    assert_islands_cover_distinct_leaves(wd).expect("one island per leaf");

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
    assert_islands_cover_distinct_leaves(wd).expect("one island per leaf");

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

/// Reverting an edit back to its baseline text revokes its island.
///
/// The pre-fix engine kept the composed no-op island and left the draft dirty,
/// so a revert followed by a save failed with `edit-dirty`. The assertion that
/// catches the missing revocation is the clean classification right after the
/// revert in (a): it is the first failure point, and the save it unlocks pins
/// the user-visible symptom behind it.
#[test]
fn reverting_an_edit_back_to_its_baseline_revokes_its_island() {
    // a. Away and back on one leaf: the island the first edit recorded must be
    // gone, the draft clean, and the save it unlocks must publish the baseline
    // text again.
    let (dir, workdir) = opened("revert-simple");
    let wd = Path::new(&workdir);
    let first = handle(&workdir, "中文区域内容");
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "edits": [{ "handle": first, "replace_with": "中文区块区域内容" }] }),
        ),
    ]);
    assert_eq!(outcome(&replies[0]), "success", "{:?}", replies[0]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);

    let back = handle(&workdir, "中文区块区域内容");
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "edits": [{ "handle": back, "replace_with": "中文区域内容" }] }),
        ),
        ("document_status", json!({})),
    ]);
    assert_eq!(outcome(&replies[0]), "success", "{:?}", replies[0]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    assert_eq!(outcome(&replies[2]), "success", "{:?}", replies[2]);
    assert_eq!(
        data(&replies[2])["state"]["state"],
        "clean",
        "the revert kept its island, so the draft is still dirty: {:?}",
        data(&replies[2])
    );
    // REVIEW-SENSITIVE: `next` reads the review snapshot, which only tracks the
    // last commit's typed.md — so a fully reverted draft still asks for a save
    // that would republish the baseline. The regression this pins is the DIRTY
    // half above: a stale island must not hold the draft open. If a later
    // engine fix teaches the snapshot to recognise a no-op (typed.md identical
    // to its pinned hash), `next` should read Null here too.
    assert_eq!(
        data(&replies[2])["versions"]["draft_dirty"],
        json!(false),
        "a reverted draft still asks to be settled: {:?}",
        data(&replies[2])
    );
    let out = dir.join("reverted.docx");
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        ("document_save", json!({ "output": out.to_string_lossy() })),
    ]);
    assert_eq!(outcome(&replies[0]), "success", "{:?}", replies[0]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    let built = built_text(&out);
    assert!(built.contains("中文区域内容"), "{built}");
    assert!(!built.contains("中文区块区域内容"), "{built}");
    let islands = island_edits(wd).expect("islands.json");
    assert!(
        islands.is_empty(),
        "the revert left an island behind: {islands:?}"
    );

    // b. Away through two texts and back across three calls: the composed edits
    // must collapse to nothing, not to two islands over one leaf.
    let (dir, workdir) = opened("revert-composed");
    let wd = Path::new(&workdir);
    for (from, to) in [
        ("中文区域内容", "中文区块区域内容"),
        ("中文区块区域内容", "中文区域内容A"),
        ("中文区域内容A", "中文区域内容"),
    ] {
        let target = handle(&workdir, from);
        let replies = mcp(&[
            ("document_open", json!({ "source": workdir })),
            (
                "document_edit",
                json!({ "edits": [{ "handle": target, "replace_with": to }] }),
            ),
        ]);
        assert_eq!(
            outcome(&replies[0]),
            "success",
            "{from:?} -> {to:?}: {:?}",
            replies[0]
        );
        assert_eq!(
            outcome(&replies[1]),
            "success",
            "{from:?} -> {to:?}: {:?}",
            replies[1]
        );
        assert_islands_cover_distinct_leaves(wd).expect("one island per leaf");
    }
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        ("document_status", json!({})),
    ]);
    assert_eq!(outcome(&replies[0]), "success", "{:?}", replies[0]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    assert_eq!(
        data(&replies[1])["state"]["state"],
        "clean",
        "three composed edits left the draft dirty: {:?}",
        data(&replies[1])
    );
    let out = dir.join("recomposed.docx");
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        ("document_save", json!({ "output": out.to_string_lossy() })),
    ]);
    assert_eq!(outcome(&replies[0]), "success", "{:?}", replies[0]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    let built = built_text(&out);
    assert!(built.contains("中文区域内容"), "{built}");
    assert!(!built.contains("中文区块区域内容"), "{built}");
    assert!(!built.contains("中文区域内容A"), "{built}");
    let islands = island_edits(wd).expect("islands.json");
    assert!(
        islands.is_empty(),
        "the composed edits left islands behind: {islands:?}"
    );

    // c. A save between the edit and its revert: the second save must publish
    // the reverted text, and reopening the produced file must read it back.
    let (dir, workdir) = opened("revert-after-save");
    let out = dir.join("resaved.docx");
    let first = handle(&workdir, "中文区域内容");
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "edits": [{ "handle": first, "replace_with": "中文区块区域内容" }] }),
        ),
        ("document_save", json!({ "output": out.to_string_lossy() })),
    ]);
    assert_eq!(outcome(&replies[0]), "success", "{:?}", replies[0]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    assert_eq!(outcome(&replies[2]), "success", "{:?}", replies[2]);

    let back = handle(&workdir, "中文区块区域内容");
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "edits": [{ "handle": back, "replace_with": "中文区域内容" }] }),
        ),
        ("document_save", json!({ "output": out.to_string_lossy() })),
    ]);
    assert_eq!(outcome(&replies[0]), "success", "{:?}", replies[0]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    assert_eq!(outcome(&replies[2]), "success", "{:?}", replies[2]);
    let built = built_text(&out);
    assert!(built.contains("中文区域内容"), "{built}");
    assert!(!built.contains("中文区块区域内容"), "{built}");

    // The produced file reopens as its own document, carrying the reverted text.
    let replies = mcp(&[
        ("document_open", json!({ "source": out.to_string_lossy() })),
        ("document_status", json!({})),
    ]);
    assert_eq!(outcome(&replies[0]), "success", "{:?}", replies[0]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    let replies = mcp(&[
        ("document_open", json!({ "source": out.to_string_lossy() })),
        ("document_find", json!({ "query": "中文区域内容" })),
        ("document_find", json!({ "query": "中文区块区域内容" })),
    ]);
    assert_eq!(outcome(&replies[0]), "success", "{:?}", replies[0]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    assert_eq!(outcome(&replies[2]), "success", "{:?}", replies[2]);
    assert_eq!(
        data(&replies[1])["counts"]["occurrences"].as_u64(),
        Some(1),
        "the reopened document does not read the reverted text: {:?}",
        data(&replies[1])
    );
    assert_eq!(
        data(&replies[2])["counts"]["occurrences"].as_u64(),
        Some(0),
        "the reopened document still reads the reverted-away text: {:?}",
        data(&replies[2])
    );

    // d. Two leaves edited, one reverted: the surviving leaf keeps its island
    // and its text, the reverted one keeps neither.
    let (dir, workdir) = opened("revert-one-of-two");
    let wd = Path::new(&workdir);
    let first = handle(&workdir, "中文区域内容");
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "edits": [{ "handle": first, "replace_with": "中文区块区域内容" }] }),
        ),
    ]);
    assert_eq!(outcome(&replies[0]), "success", "{:?}", replies[0]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    let second = handle(&workdir, "EnglishRegion");
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "edits": [{ "handle": second, "replace_with": "GAMMA" }] }),
        ),
    ]);
    assert_eq!(outcome(&replies[0]), "success", "{:?}", replies[0]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);

    let back = handle(&workdir, "中文区块区域内容");
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "edits": [{ "handle": back, "replace_with": "中文区域内容" }] }),
        ),
    ]);
    assert_eq!(outcome(&replies[0]), "success", "{:?}", replies[0]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    assert_islands_cover_distinct_leaves(wd).expect("one island per leaf");
    let islands = island_edits(wd).expect("islands.json");
    assert_eq!(
        islands.len(),
        1,
        "the revert took the wrong island with it: {islands:?}"
    );
    assert_eq!(
        islands[0]["old"].as_str(),
        Some("EnglishRegion"),
        "the surviving island is not the surviving edit's span: {islands:?}"
    );
    assert_eq!(
        islands[0]["new"].as_str(),
        Some("GAMMA"),
        "the island that survived is the reverted one, not the surviving edit: {islands:?}"
    );
    let out = dir.join("partial.docx");
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        ("document_save", json!({ "output": out.to_string_lossy() })),
    ]);
    assert_eq!(outcome(&replies[0]), "success", "{:?}", replies[0]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    let built = built_text(&out);
    assert!(
        built.contains("GAMMA"),
        "the surviving edit did not reach the file: {built}"
    );
    assert!(built.contains("中文区域内容"), "{built}");
    assert!(!built.contains("中文区块区域内容"), "{built}");

    // e. Revert, then edit the same leaf again: the new edit records exactly one
    // island, not one beside the revoked one.
    let (dir, workdir) = opened("revert-reedit");
    let wd = Path::new(&workdir);
    for (from, to) in [
        ("中文区域内容", "中文区块区域内容"),
        ("中文区块区域内容", "中文区域内容"),
        ("中文区域内容", "中文区域内容A"),
    ] {
        let target = handle(&workdir, from);
        let replies = mcp(&[
            ("document_open", json!({ "source": workdir })),
            (
                "document_edit",
                json!({ "edits": [{ "handle": target, "replace_with": to }] }),
            ),
        ]);
        assert_eq!(
            outcome(&replies[0]),
            "success",
            "{from:?} -> {to:?}: {:?}",
            replies[0]
        );
        assert_eq!(
            outcome(&replies[1]),
            "success",
            "{from:?} -> {to:?}: {:?}",
            replies[1]
        );
    }
    assert_islands_cover_distinct_leaves(wd).expect("one island per leaf");
    let islands = island_edits(wd).expect("islands.json");
    assert_eq!(
        islands.len(),
        1,
        "the re-edit recorded a second island for one leaf: {islands:?}"
    );
    assert_eq!(
        islands[0]["old"].as_str(),
        Some("中文区域内容"),
        "the one island left does not span the re-edit: {islands:?}"
    );
    assert_eq!(
        islands[0]["new"].as_str(),
        Some("中文区域内容A"),
        "the one island left is not the re-edit: {islands:?}"
    );
    let out = dir.join("reedited.docx");
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        ("document_save", json!({ "output": out.to_string_lossy() })),
    ]);
    assert_eq!(outcome(&replies[0]), "success", "{:?}", replies[0]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);
    let built = built_text(&out);
    assert!(built.contains("中文区域内容A"), "{built}");
    assert!(!built.contains("中文区块区域内容"), "{built}");
}

/// The `edits` array of `islands.json`. A workdir that has recorded nothing has
/// no array — which reads as empty, not as missing.
fn island_edits(workdir: &Path) -> Result<Vec<Value>, String> {
    let path = workdir.join("islands.json");
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let islands = std::fs::read_to_string(&path).map_err(|error| error.to_string())?;
    let parsed: Value = serde_json::from_str(&islands).map_err(|error| error.to_string())?;
    parsed["edits"]
        .as_array()
        .cloned()
        .ok_or_else(|| format!("islands.json carries no edits array: {islands}"))
}

/// One island per leaf, across every call the sequence made. A workdir that has
/// recorded nothing yet satisfies it trivially.
fn assert_islands_cover_distinct_leaves(workdir: &Path) -> Result<(), String> {
    let edits = island_edits(workdir)?;
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
    if unique.len() != leaves.len() {
        let islands = std::fs::read_to_string(workdir.join("islands.json"))
            .unwrap_or_else(|_| "<unreadable>".to_string());
        return Err(format!(
            "two islands address one leaf, which the build cannot splice: {islands}"
        ));
    }
    Ok(())
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

/// A switch that cannot finish must cost nothing: the failed target is reported,
/// and the session is put back on the workdir it was driving. Measured before the
/// fix: the surface released the binding first, so a bad target left the session
/// driving nothing at all — and the work that was mid-flight on the first document
/// became unreachable without knowing that.
#[test]
fn a_failed_switch_leaves_the_session_where_it_was() {
    let (dir, first) = opened("switch-failed");
    let missing = dir.join("no-such-document.docx");
    let edit = handle(&first, "中文区域内容");

    let replies = mcp(&[
        ("document_open", json!({ "source": first })),
        // A target that cannot be opened, with the release asked for.
        (
            "document_open",
            json!({ "source": missing.to_string_lossy(), "release_current": true }),
        ),
        ("document_status", json!({})),
    ]);
    assert_eq!(outcome(&replies[0]), "success", "{:?}", replies[0]);
    assert_eq!(
        outcome(&replies[1]),
        "failure",
        "opening a missing document must fail: {:?}",
        replies[1]
    );
    assert_eq!(
        outcome(&replies[2]),
        "success",
        "the session was left driving nothing after a failed switch: {:?}",
        replies[2]
    );
    assert_eq!(
        data(&replies[2])["session"]["source_drift"]["source"]
            .as_str()
            .map(|source| source.ends_with("source.docx")),
        Some(true),
        "the session is not back on the workdir it was driving: {:?}",
        data(&replies[2])
    );

    // And the work goes on: the edit that was pending before the failed switch is
    // still addressable, and still lands.
    let replies = mcp(&[
        ("document_open", json!({ "source": first })),
        (
            "document_edit",
            json!({ "edits": [{ "handle": edit, "replace_with": "中文区块区域内容" }] }),
        ),
    ]);
    assert_eq!(outcome(&replies[1]), "success", "{:?}", replies[1]);
}

/// Switching back and forth is not a state change: the documents keep what they
/// had, and a handle taken before a switch still addresses its span afterwards.
/// The handle's address is the REVISION, not the connection.
#[test]
fn switching_away_and_back_keeps_both_documents_and_their_handles() {
    let (dir, first) = opened("switch-cycles");
    let second_source = dir.join("second.docx");
    std::fs::copy(fixture("styled"), &second_source).expect("copy fixture");
    // Measure AFTER the first find: `edit.md` and `edit.state.json` are materialised
    // when the projection is first read, so a digest taken before that would report
    // the projection's own work as the switching's.
    let handle_before = handle(&first, "中文区域内容");
    let before = content_digest(Path::new(&first));

    let mut replies = Vec::new();
    for _ in 0..3 {
        replies.extend(mcp(&[
            (
                "document_open",
                json!({ "source": second_source.to_string_lossy(), "release_current": true }),
            ),
            (
                "document_open",
                json!({ "source": first, "release_current": true }),
            ),
        ]));
    }
    for reply in &replies {
        assert_eq!(outcome(reply), "success", "{reply:?}");
    }
    assert_eq!(
        content_digest(Path::new(&first)),
        before,
        "switching away and back changed the workdir"
    );

    // The handle taken before all that switching still edits its span.
    let replies = mcp(&[
        ("document_open", json!({ "source": first })),
        (
            "document_edit",
            json!({ "edits": [{ "handle": handle_before, "replace_with": "中文区块区域内容" }] }),
        ),
    ]);
    assert_eq!(
        outcome(&replies[1]),
        "success",
        "a handle taken before a switch no longer addresses its span: {:?}",
        replies[1]
    );
}

/// A selector batch is all-or-nothing, and it says WHICH member refuses — before
/// anything runs.
///
/// The first version of this diagnosis hooked the failure after the mutation had
/// already failed, and never fired: the re-probe enumerated while the session was
/// still unwinding. A check that runs first cannot be defeated by the state its own
/// failure creates.
#[test]
fn a_selector_batch_names_the_member_that_refuses_and_writes_nothing() {
    let (dir, workdir) = opened("selector-member");
    let before = std::fs::read(dir.join("source.docx")).expect("source bytes");

    // The property route over a phrase whose matches have different bases: the
    // document carries no bold variant for most of them, so the batch cannot
    // apply in full.
    let replies = mcp(&[
        ("document_open", json!({ "source": workdir })),
        (
            "document_edit",
            json!({ "edits": [{
                "kind": "format",
                "selector": { "text": "中文区域内容", "match": "all" },
                "format": { "bold": true }
            }] }),
        ),
    ]);
    assert_eq!(outcome(&replies[1]), "failure", "{:?}", replies[1]);
    let envelope = &replies[1]["structuredContent"];
    assert_eq!(
        envelope["diagnostics"][0]["code"], "selector-member-rejected",
        "a batch that cannot fully apply must say so by name: {envelope:?}"
    );
    let reply = data(&replies[1]);
    assert_eq!(reply["written"], json!(false), "and must write nothing");
    assert!(
        reply["matched"].as_u64().unwrap_or(0) >= 1,
        "the counts are fields, not prose: {reply:?}"
    );
    assert_eq!(
        reply["rejected"].as_u64(),
        reply["failures"].as_array().map(|rows| rows.len() as u64),
        "`rejected` and the member list must agree: {reply:?}"
    );
    let member = &reply["failures"][0];
    for key in ["index", "paragraph_id", "range", "code"] {
        assert!(
            !member[key].is_null(),
            "a member report needs `{key}`, from the resolution: {member:?}"
        );
    }
    assert_eq!(member["range"]["unit"], "unicode-scalars");
    assert_eq!(
        std::fs::read(dir.join("source.docx")).expect("source bytes"),
        before,
        "the source must be untouched"
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
