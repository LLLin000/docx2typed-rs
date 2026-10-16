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

/// The texts a sequence moves between. Mutually non-containing, so "the file
/// says the current one and not the others" is a real assertion.
const TARGETS: [&str; 3] = ["中文区域内容", "甲甲甲", "乙乙乙"];

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

/// One sequence, from its seed alone.
///
/// The log is written into the caller's buffer as the sequence runs, so it
/// survives the failure it is meant to explain — a log returned only on success
/// is a log nobody has when they need it.
fn run_sequence(seed: u64, steps: usize, log: &mut Vec<String>) -> Result<(), String> {
    let (dir, workdir) = opened(&format!("fuzz-{seed}"));
    let out = dir.join("fuzzed.docx");
    let wd = Path::new(&workdir);
    let mut rng = Rng::new(seed);
    let mut model = TARGETS[0].to_string();
    let mut saved_any = false;

    for step in 1..=steps {
        match rng.below(5) {
            // Move the span to a different text: longer, shorter, or back to the
            // one the document came with. The last of those is the shape that
            // composes a no-op island.
            0 | 1 => {
                let target = TARGETS[rng.below(TARGETS.len() as u64) as usize];
                if target == model {
                    log.push(format!("{step}: replace skipped, already {target:?}"));
                    continue;
                }
                let handle = find_handle(&workdir, &model)?;
                let replies = mcp(&[
                    ("document_open", json!({ "source": workdir })),
                    (
                        "document_edit",
                        json!({ "edits": [{ "handle": handle, "replace_with": target }] }),
                    ),
                ]);
                if outcome(&replies[1]) != "success" {
                    return Err(format!(
                        "{step}: replace {model:?} -> {target:?} failed: {:?}",
                        replies[1]
                    ));
                }
                log.push(format!("{step}: replace {model:?} -> {target:?}"));
                model = target.to_string();
                assert_islands_cover_distinct_leaves(wd);
            }
            // A dry run: it must change nothing, and it must report the same
            // verdict the real call would.
            2 => {
                let handle = find_handle(&workdir, &model)?;
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
                    return Err(format!("{step}: dry run failed: {:?}", replies[1]));
                }
                if content_digest(wd) != before {
                    return Err(format!("{step}: a dry run changed the workdir"));
                }
                log.push(format!("{step}: dry run"));
            }
            // A batch that cannot be applied: it must leave the workdir exactly
            // as it found it.
            3 => {
                let handle = find_handle(&workdir, &model)?;
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
                    return Err(format!("{step}: a batch with a bogus handle succeeded"));
                }
                if content_digest(wd) != before {
                    return Err(format!("{step}: a refused batch changed the workdir"));
                }
                log.push(format!("{step}: refused batch"));
            }
            // A save: the file has to say what the model says, and nothing else.
            _ => {
                let replies = mcp(&[
                    ("document_open", json!({ "source": workdir })),
                    ("document_save", json!({ "output": out.to_string_lossy() })),
                ]);
                if outcome(&replies[1]) != "success" {
                    return Err(format!("{step}: save failed: {:?}", replies[1]));
                }
                let built = built_text(&out);
                if !built.contains(&model) {
                    return Err(format!(
                        "{step}: the saved file does not carry {model:?}: {built}"
                    ));
                }
                for other in TARGETS.iter().filter(|target| **target != model) {
                    // Exactly one target is in the document, and it is the model:
                    // every other text was replaced and must be gone. The targets
                    // are chosen not to contain one another, so this is a real
                    // assertion rather than a prefix accident.
                    if built.contains(other) {
                        return Err(format!(
                            "{step}: the saved file still carries {other:?} while the model says \
                             {model:?}: {built}"
                        ));
                    }
                }
                assert_islands_cover_distinct_leaves(wd);
                saved_any = true;
                log.push(format!("{step}: save -> {model:?}"));
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
            return Err(format!("final save failed: {:?}", replies[1]));
        }
    }
    Ok(())
}

/// Where a failing seed's artifact goes, and how to replay it.
fn fuzz_artifact_dir() -> PathBuf {
    repo_root().join("target").join("sequence-fuzz")
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
        .unwrap_or(4);
    let steps: usize = std::env::var("SEQUENCE_FUZZ_STEPS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(10);

    for seed in 1..=seeds {
        let mut log = Vec::new();
        let Err(error) = run_sequence(seed, steps, &mut log) else {
            continue;
        };
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
                "failure": shortest.1,
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

    // A second edit, and then a publication that cannot finish: the document is
    // held, so the last rename of the publication fails while the evidence has
    // already been moved into place.
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
        "an interrupted publication replaced the document it could not finish replacing"
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
        "a staged build was left behind: {staged:?}"
    );

    // And the half-state is *detectable*: the evidence beside the surviving
    // document describes the build that did not land.
    let replies = mcp(&[("document_open", json!({ "source": out.to_string_lossy() }))]);
    assert_eq!(outcome(&replies[0]), "success", "{:?}", replies[0]);
    assert_eq!(
        data(&replies[0])["sidecars"]["state"],
        "mixed",
        "the interrupted publication left a set that claims to be one version: {:?}",
        data(&replies[0])["sidecars"]
    );

    // Which is a state the next save clears: it rewrites both halves.
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

/// One island per leaf, across every call the sequence made. A workdir that has
/// recorded nothing yet satisfies it trivially.
fn assert_islands_cover_distinct_leaves(workdir: &Path) {
    let path = workdir.join("islands.json");
    if !path.is_file() {
        return;
    }
    let islands = std::fs::read_to_string(&path).expect("islands.json");
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
