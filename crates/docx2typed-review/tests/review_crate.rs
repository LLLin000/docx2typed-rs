//! Issue #60 focused crate tests: the secured review collaboration lane.
//!
//! These exercise the public `docx2typed-review` API directly (no binary):
//! transport-security primitives, the file-backed event queue, the
//! document/session collaboration state machine (patch staging, agent
//! gate, CAS publish, settlement carry-forward, batch application and its
//! rollback), the MCP draft projection model, and the store-wrapped
//! mutation contract (byte-exact replay, operation-id reuse, concurrent
//! writer one-winner). Workdirs are produced by the real extraction engine
//! (`docx2typed-app`), so every test runs against store-backed,
//! format-carrying workdirs exactly like the binary.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use docx2typed_app::{Engine, ExtractArgs, Operation, OperationArgs, OperationContext};
use docx2typed_review::collab::{
    ensure_agent_ready, ensure_session, external_write_guard, preflight, publish_current,
    review_apply_batch, settle_decisions, settlement_plan, stage_patch, validate_patch,
};
use docx2typed_review::draft;
use docx2typed_review::queue;
use docx2typed_review::security::{
    build_allowlist, constant_time_equal, content_type_allowed, generate_capability,
    session_fingerprint, split_host, ReviewSecurity, UnauthorizedThrottle, NOT_FOUND_BODY,
};
use docx2typed_review::server::{store_mutation, MutationError};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Workdir scaffolding
// ---------------------------------------------------------------------------

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fixture(name: &str) -> PathBuf {
    repo_root().join(format!("corpus/release/{name}.docx"))
}

/// Fresh store-backed workdir extracted from one fixture (mutation-safe:
/// every test gets its own scratch dir).
fn workdir(tag: &str, fixture_name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "review60-crate-{tag}-{}-{}",
        std::process::id(),
        thread::current().name().unwrap_or("t").replace("::", "-")
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create scratch dir");
    let engine = Engine::new();
    let outcome = engine
        .execute(
            Operation::Extract,
            OperationContext::new(docx2typed_protocol::new_operation_id()),
            OperationArgs::Extract(ExtractArgs {
                input: fixture(fixture_name),
                outdir: dir.clone(),
            }),
        )
        .expect("extraction engine runs");
    let envelope = outcome.into_envelope("extract", "");
    assert_eq!(envelope.outcome, "success", "{:?}", envelope.diagnostics);
    docx2typed_store::Store::ensure(&dir, &docx2typed_protocol::new_operation_id(), "")
        .expect("store initialization");
    dir
}

fn event_id(value: &Value) -> String {
    value
        .get("event_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// A valid human patch targeting the first paragraph of a fresh `plain`
/// workdir: parent snapshot C0, range [0, 3) ("本发明" is 3 scalars).
fn patch_args(paragraph_id: &str, start: usize, end: usize, before: &str, after: &str) -> Value {
    json!({
        "type": "patch",
        "client_id": format!("client-{paragraph_id}-{start}"),
        "origin": "human_ui",
        "author": "tester",
        "parent_snapshot": "C0",
        "paragraph_id": paragraph_id,
        "kind": "replace",
        "before": before,
        "after": after,
        "target": {
            "start_offset": start,
            "end_offset": end,
            "expected_text": before,
            "left_context": "",
            "right_context": "",
            "paragraph_fingerprint": "",
            "region_fingerprint": "",
        },
        "review_item_id": format!("item-{paragraph_id}-{start}"),
    })
}

// ---------------------------------------------------------------------------
// security: issue #31 transport primitives
// ---------------------------------------------------------------------------

#[test]
fn capability_bootstrap_is_256_bit_and_unique() {
    let a = generate_capability();
    let b = generate_capability();
    // 32 raw bytes -> ceil(32/3)*4 - 1 padding chars = 43 base64url chars.
    assert_eq!(a.len(), 43);
    assert!(a
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'));
    assert_ne!(a, b, "two bootstrap draws must differ");
}

#[test]
fn constant_time_equal_handles_lengths_and_mismatches() {
    assert!(constant_time_equal("abc", "abc"));
    assert!(!constant_time_equal("abc", "abd"));
    assert!(!constant_time_equal("abc", "abcd"));
    assert!(!constant_time_equal("", "x"));
    assert!(constant_time_equal("", ""));
}

#[test]
fn session_fingerprint_is_irreversible_truncated_hash() {
    let fingerprint = session_fingerprint("some-token");
    assert_eq!(fingerprint.len(), 10);
    assert!(fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit()));
    // Same token -> same fingerprint; the capability itself never appears.
    assert_eq!(session_fingerprint("some-token"), fingerprint);
    assert_ne!(session_fingerprint("some-token2"), fingerprint);
}

#[test]
fn split_host_rejects_ipv6_and_malformed_ports() {
    let (host, port) = split_host("127.0.0.1:8876").expect("loopback with port");
    assert_eq!(host, "127.0.0.1");
    assert_eq!(port, Some(8876));
    let (host, port) = split_host("localhost").expect("port-less host");
    assert_eq!(host, "localhost");
    assert_eq!(port, None);
    assert!(
        split_host("[::1]:80").is_err(),
        "IPv6 literals are rejected"
    );
    assert!(
        split_host("host:port").is_err(),
        "non-numeric port rejected"
    );
    assert!(split_host("").is_err(), "empty host rejected");
    assert!(split_host("a:99999").is_err(), "out-of-range port rejected");
}

#[test]
fn build_allowlist_covers_loopback_and_tailscale_forms() {
    // Loopback on a non-80 port: only the explicit host:port forms.
    let (hosts, origins) = build_allowlist("127.0.0.1", 8876);
    assert!(hosts.contains(&"127.0.0.1:8876".to_string()));
    assert!(hosts.contains(&"localhost:8876".to_string()));
    assert!(!hosts.contains(&"127.0.0.1".to_string()));
    assert!(origins.contains(&"http://127.0.0.1:8876".to_string()));
    // Port 80 additionally admits the port-less browser form.
    let (hosts, origins) = build_allowlist("localhost", 80);
    assert!(hosts.contains(&"localhost".to_string()));
    assert!(origins.contains(&"http://localhost".to_string()));
    // A Tailscale bind admits exactly the discovered IPv4 address.
    let (hosts, origins) = build_allowlist("100.64.0.5", 8876);
    assert_eq!(hosts, vec!["100.64.0.5:8876".to_string()]);
    assert_eq!(origins, vec!["http://100.64.0.5:8876".to_string()]);
}

#[test]
fn content_type_allowed_accepts_only_json_with_charset_variants() {
    assert!(content_type_allowed("application/json"));
    assert!(content_type_allowed("application/json; charset=utf-8"));
    assert!(content_type_allowed("application/json;charset=utf8"));
    assert!(content_type_allowed("Application/JSON; Charset = UTF-8"));
    assert!(content_type_allowed(" application/json ; charset = utf-8 "));
    assert!(!content_type_allowed("text/plain"));
    assert!(!content_type_allowed(
        "application/json; charset=iso-8859-1"
    ));
    assert!(
        !content_type_allowed("application/json; charset=\"utf-8\""),
        "quoted charset is rejected (Python regex parity)"
    );
    assert!(
        !content_type_allowed("application/json; charset=utf-8; foo=bar"),
        "extra parameters rejected"
    );
    assert!(!content_type_allowed("multipart/form-data"));
    assert!(!content_type_allowed(""));
    assert!(!content_type_allowed("application/xml"));
}

#[test]
fn throttle_is_bounded_per_client_and_refills() {
    let mut throttle = UnauthorizedThrottle::new(3.0, 1.0, 4);
    assert!(throttle.allow("a"));
    assert!(throttle.allow("a"));
    assert!(throttle.allow("a"));
    assert!(!throttle.allow("a"), "capacity exhausted");
    assert!(throttle.allow("b"), "other clients are unaffected");
    assert_eq!(throttle.len(), 2);
    std::thread::sleep(
        docx2typed_review::security::throttle_idle_after() + std::time::Duration::from_millis(50),
    );
    assert!(throttle.allow("a"), "bucket refills after idle");
}

#[test]
fn throttle_evicts_least_recently_seen_client() {
    let mut throttle = UnauthorizedThrottle::new(1.0, 1.0, 2);
    assert!(throttle.allow("a"));
    assert!(throttle.allow("b"));
    assert_eq!(throttle.len(), 2);
    // A third client evicts the least recently seen one; the table stays bounded.
    assert!(throttle.allow("c"));
    assert!(throttle.len() <= 2);
}

#[test]
fn review_security_verifies_token_and_binds_host_origin() {
    let capability = generate_capability();
    let (hosts, origins) = build_allowlist("127.0.0.1", 8876);
    let security = ReviewSecurity::new(capability.clone(), hosts, origins, 8876, "loopback".into());
    assert!(security.verify(&capability));
    assert!(!security.verify("wrong"));
    assert!(!security.verify(""));
    assert!(security.host_allowed("127.0.0.1:8876"));
    assert!(!security.host_allowed("127.0.0.1"));
    assert!(!security.host_allowed("evil.example:80"));
    assert!(security.origin_allowed("http://127.0.0.1:8876"));
    assert!(!security.origin_allowed("http://evil.example"));
    assert!(!security.origin_allowed(""));
    assert_eq!(security.session_hash.len(), 10);
}

#[test]
fn authority_failure_body_is_detail_free_and_byte_identical() {
    // The uniform 404 body must be byte-identical for every authority
    // failure so a probe cannot distinguish token/host/route classes.
    assert_eq!(NOT_FOUND_BODY, r#"{"error":"not-found"}"#);
}

// ---------------------------------------------------------------------------
// queue: file-backed review events
// ---------------------------------------------------------------------------

#[test]
fn queue_upsert_dispatch_acknowledge_lifecycle() {
    let wd = workdir("queue-lifecycle", "plain");
    let comment = json!({
        "type": "comment",
        "client_id": "c1",
        "paragraph_id": "P0",
        "selected_text": "snippet",
        "note": "please check",
    });
    let staged = queue::upsert_event(&wd, &comment).expect("upsert");
    assert_eq!(staged["status"], "draft");
    let id = event_id(&staged);
    // Re-upsert with the same client_id replaces the draft in place.
    let mut replaced = comment.clone();
    replaced["note"] = json!("revised note");
    let again = queue::upsert_event(&wd, &replaced).expect("upsert replaces draft");
    assert_eq!(event_id(&again), id, "same client draft keeps its event_id");
    assert_eq!(again["note"], "revised note");
    let snapshot = queue::snapshot_readonly(&wd);
    assert_eq!(snapshot["counts"]["draft"], 1);
    // Dispatch moves drafts into one queued batch.
    let queued = queue::dispatch(&wd).expect("dispatch");
    assert_eq!(queued.len(), 1);
    let batch_id = queued[0]["batch_id"].as_str().unwrap_or("").to_string();
    assert!(batch_id.starts_with("batch-"));
    let snapshot = queue::snapshot_readonly(&wd);
    assert_eq!(snapshot["counts"]["queued"], 1);
    // Acknowledge a subset; already-acknowledged is idempotent.
    let acked = queue::acknowledge(&wd, std::slice::from_ref(&id)).expect("ack");
    assert_eq!(acked.len(), 1);
    assert_eq!(acked[0]["status"], "acknowledged");
    let again = queue::acknowledge(&wd, &[id]).expect("ack idempotent");
    assert_eq!(again.len(), 1);
    let snapshot = queue::snapshot_readonly(&wd);
    assert_eq!(snapshot["counts"]["acknowledged"], 1);
}

#[test]
fn queue_validates_events_and_bounds_text() {
    let wd = workdir("queue-validate", "plain");
    // Comment requires selected_text + note.
    let bad = json!({ "type": "comment", "client_id": "c1", "paragraph_id": "P0" });
    assert!(queue::upsert_event(&wd, &bad).is_err());
    // Decision requires a valid decision value.
    let bad = json!({
        "type": "decision", "client_id": "c1", "paragraph_id": "P0", "decision": "maybe"
    });
    assert!(queue::upsert_event(&wd, &bad).is_err());
    // Oversized text is rejected.
    let bad = json!({
        "type": "comment", "client_id": "c1", "paragraph_id": "P0",
        "selected_text": "x", "note": "n".repeat(8_001),
    });
    assert!(queue::upsert_event(&wd, &bad).is_err());
    // Invalid event ids are rejected by the update path.
    let good = json!({
        "type": "decision", "client_id": "c2", "paragraph_id": "P0",
        "decision": "defer", "revision_key": "word/document.xml|ins|w1|fp",
    });
    let staged = queue::upsert_event(&wd, &good).expect("valid decision stages");
    assert_eq!(staged["review_decision"], "defer");
    assert!(queue::update_event(&wd, "../../escape", &json!({})).is_err());
}

#[test]
fn queue_readonly_snapshot_never_creates_directories() {
    let wd = workdir("queue-readonly", "plain");
    assert!(!wd.join(".review").exists());
    let snapshot = queue::snapshot_readonly(&wd);
    assert_eq!(snapshot["counts"]["draft"], 0);
    assert!(
        !wd.join(".review").exists(),
        "read-only snapshot has no side effect"
    );
}

// ---------------------------------------------------------------------------
// collab: session state machine
// ---------------------------------------------------------------------------

#[test]
fn session_bootstrap_creates_c0_and_pins_filesystem() {
    let wd = workdir("collab-bootstrap", "plain");
    let state = ensure_session(&wd).expect("session bootstraps");
    assert_eq!(state["schema"], "docx2typed-review-session-1");
    assert_eq!(state["current_snapshot"]["id"], "C0");
    assert_eq!(state["review_base"]["id"], "S0");
    assert!(wd.join(".review/snapshots/C0.json").is_file());
    assert!(wd.join(".review/history.jsonl").is_file());
    let document = docx2typed_review::collab::document_state(&wd).expect("document state");
    assert_eq!(document["current_matches_filesystem"], true);
    // The readonly view never mutates.
    let readonly = docx2typed_review::collab::document_state_readonly(&wd);
    assert_eq!(readonly["current_snapshot"]["id"], "C0");
}

#[test]
fn staged_patch_blocks_agent_gate_until_dispatched() {
    let wd = workdir("collab-gate", "plain");
    let patch = patch_args("P0", 0, 3, "本发明", "我们发明");
    let staged = stage_patch(&wd, &patch).expect("patch stages");
    assert_eq!(staged["delivery_state"], "staged");
    let event_id = event_id(&staged);
    // A staged (not yet dispatched) patch is not in the queued set: the
    // agent gate stays open because a draft is not a queued human patch.
    let plan = preflight(&wd);
    assert_eq!(plan["ready"], true);
    // Dispatch it: the queued human patch now blocks the agent.
    queue::dispatch(&wd).expect("dispatch");
    let plan = preflight(&wd);
    assert_eq!(plan["ready"], false);
    assert!(plan["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r == "queued-human-patch"));
    assert_eq!(plan["blocked_patches"][0]["event_id"], event_id);
    let error = ensure_agent_ready(&wd).expect_err("agent gate blocks");
    assert_eq!(error.code, "agent-preflight-required");
}

#[test]
fn publish_current_is_cas_with_one_winner() {
    let wd = workdir("collab-cas", "plain");
    let error = publish_current(&wd, "C0", "human_ui", &["P0".to_string()], None)
        .expect_err("unchanged typed.md cannot publish");
    assert_eq!(error.code, "current-not-changed");
    // Change typed.md so the live file differs from the pinned hash.
    fs::write(
        wd.join("typed.md"),
        "<!--@typed schema=\"1\" template=\"_template.docx\"/>\nP0 changed\n",
    )
    .expect("touch typed.md");
    let published = publish_current(&wd, "C0", "human_ui", &["P0".to_string()], None)
        .expect("first publish wins");
    assert_eq!(published["current_snapshot"]["id"], "C1");
    // A stale expected parent can never republish: one-winner CAS.
    let error = publish_current(&wd, "C0", "human_ui", &["P1".to_string()], None)
        .expect_err("stale parent rejected");
    assert_eq!(error.code, "current-parent-mismatch");
    // The session advanced and the snapshot was persisted.
    let state = ensure_session(&wd).expect("session");
    assert_eq!(state["current_snapshot"]["id"], "C1");
    assert!(wd.join(".review/snapshots/C1.json").is_file());
}

#[test]
fn external_write_guard_issues_and_fails_closed() {
    let wd = workdir("collab-guard", "plain");
    let error = external_write_guard(&wd, "C9", "import").expect_err("wrong parent");
    assert_eq!(error.code, "current-parent-mismatch");
    let error = external_write_guard(&wd, "C0", "frobnicate").expect_err("bad operation");
    assert_eq!(error.code, "external-operation");
    let guard = external_write_guard(&wd, "C0", "import").expect("guard issued");
    assert_eq!(guard["schema"], "docx2typed-review-external-guard-1");
    assert_eq!(guard["operation"], "import");
    assert_eq!(guard["expected_parent_snapshot"], "C0");
    // Drift blocks the guard.
    fs::write(
        wd.join("typed.md"),
        "<!--@typed schema=\"1\" template=\"_template.docx\"/>\ndrifted\n",
    )
    .expect("touch typed.md");
    let error = external_write_guard(&wd, "C0", "rollback").expect_err("drift blocks");
    assert_eq!(error.code, "current-snapshot-drift");
}

#[test]
fn settlement_plan_carries_deferred_and_stale_patches() {
    let wd = workdir("collab-plan", "plain");
    let defer = json!({
        "type": "decision", "client_id": "d1", "paragraph_id": "P0",
        "decision": "defer", "revision_key": "word/document.xml|ins|w1|fp",
    });
    queue::upsert_event(&wd, &defer).expect("defer decision stages");
    queue::dispatch(&wd).expect("dispatch");
    let plan = settlement_plan(&wd, None);
    assert_eq!(plan["schema"], "docx2typed-review-settlement-1");
    assert_eq!(plan["decisions"].as_array().unwrap().len(), 1);
    // A defer decision always carries forward into the next review round.
    assert_eq!(plan["carry_forward"].as_array().unwrap().len(), 1);
    // A fresh patch against the current snapshot (C0) applies in the batch.
    let patch = patch_args("P0", 0, 3, "本发明", "我们发明");
    stage_patch(&wd, &patch).expect("patch stages");
    queue::dispatch(&wd).expect("dispatch");
    let plan = settlement_plan(&wd, None);
    assert_eq!(plan["patches"].as_array().unwrap().len(), 1);
    assert_eq!(
        plan["carry_forward"].as_array().unwrap().len(),
        1,
        "only the defer carries; the patch targets the current snapshot"
    );
    // Advance the snapshot behind the queued patch: its parent is now stale
    // and it carries forward alongside the defer.
    fs::write(
        wd.join("typed.md"),
        "<!--@typed schema=\"1\" template=\"_template.docx\"/>\nchanged\n",
    )
    .expect("touch typed.md");
    publish_current(&wd, "C0", "human_ui", &["P0".to_string()], None).expect("publish C1");
    let plan = settlement_plan(&wd, None);
    assert_eq!(
        plan["carry_forward"].as_array().unwrap().len(),
        2,
        "defer + stale patch carry forward"
    );
}

#[test]
fn settle_decisions_carries_deferred_forward_and_advances_base() {
    let wd = workdir("collab-settle-defer", "plain");
    let defer = json!({
        "type": "decision", "client_id": "d2", "paragraph_id": "P0",
        "decision": "defer", "revision_key": "word/document.xml|ins|w1|fp",
    });
    let staged = queue::upsert_event(&wd, &defer).expect("defer stages");
    let id = event_id(&staged);
    queue::dispatch(&wd).expect("dispatch");
    let settled = settle_decisions(&wd, None).expect("defer settles");
    assert_eq!(settled["schema"], "docx2typed-review-settlement-1");
    assert!(settled["decisions"].as_array().unwrap().is_empty());
    assert_eq!(settled["deferred"][0]["event_id"], id);
    assert_eq!(settled["carry_forward"][0]["event_id"], id);
    assert_eq!(settled["review_base"]["id"], "S1");
    // The deferred event is marked applied with the carry-forward flag.
    let events = queue::list_events_readonly(&wd);
    let record = events
        .iter()
        .find(|e| event_id(e) == id)
        .expect("event exists");
    assert_eq!(record["delivery_state"], "applied");
    assert_eq!(record["carry_forward"], true);
}

#[test]
fn review_apply_batch_applies_patches_and_publishes_one_snapshot() {
    let wd = workdir("collab-batch", "plain");
    let p0 = docx2typed_review::draft::get_paragraph(&wd, "P0").expect("draft paragraph");
    let body = p0["plain"].as_str().unwrap_or("").to_string();
    // Two non-overlapping patches on P0 at char boundaries: chars 0..6 and
    // chars 8..10 of the (CJK) paragraph body.
    let char_at = |index: usize| body.char_indices().nth(index).map(|(pos, _)| pos).unwrap();
    let before1 = &body[char_at(0)..char_at(6)];
    let before2 = &body[char_at(8)..char_at(10)];
    let patch1 = patch_args("P0", 0, 6, before1, "AAAA");
    let mut patch2 = patch_args("P0", 8, 10, before2, "BB");
    stage_patch(&wd, &patch1).expect("patch1 stages");
    // The second patch chains onto the staged snapshot created by the first
    // (Python staged-snapshot chaining semantics).
    let staged_id = docx2typed_review::collab::document_state(&wd)
        .expect("session")
        .get("staged_snapshot")
        .and_then(|staged| staged.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    assert!(staged_id.starts_with('H'));
    patch2["parent_snapshot"] = json!(staged_id);
    stage_patch(&wd, &patch2).expect("patch2 stages");
    queue::dispatch(&wd).expect("dispatch");
    let applied = review_apply_batch(&wd, None, None).expect("batch applies");
    assert_eq!(applied["state"], "applied");
    assert_eq!(applied["events"].as_array().unwrap().len(), 2);
    let snapshot_id = applied["commit"]["current_snapshot"]["id"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert_eq!(snapshot_id, "C1");
    // Every patch landed in the canonical draft and typed.md.
    let listed = docx2typed_review::draft::list_paragraphs(&wd).expect("list");
    let p0_now = listed["paragraphs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "P0")
        .expect("P0 listed");
    assert!(p0_now["summary"].as_str().unwrap_or("").starts_with("AAAA"));
    let events = queue::list_events_readonly(&wd);
    assert!(events.iter().all(|e| e["delivery_state"] == "applied"));
    assert!(events.iter().all(|e| e["review_decision"] == "adjusted"));
    // Re-running the batch re-selects the applied events (status stays
    // queued, mirroring Python) and fails the stale parent chain — the
    // one-shot apply cannot double-publish.
    let error = review_apply_batch(&wd, None, None).expect_err("repeat batch");
    assert_eq!(error.code, "patch-parent-mismatch");
}

#[test]
fn review_apply_batch_rolls_back_on_precondition_failure() {
    let wd = workdir("collab-batch-fail", "plain");
    // Stage a patch whose expected_text no longer matches after we mutate
    // the draft underneath it.
    let patch = patch_args("P0", 0, 3, "本发明", "新发明");
    stage_patch(&wd, &patch).expect("patch stages");
    queue::dispatch(&wd).expect("dispatch");
    // Break the precondition by editing edit.md directly (the agent gate
    // correctly blocks the draft tools here; a human would re-read first).
    draft::ensure_projection(&wd).expect("projection");
    let edit_path = wd.join("edit.md");
    let edited = fs::read_to_string(&edit_path)
        .expect("edit.md")
        .replacen("本发明", "弄坏", 1);
    fs::write(&edit_path, edited).expect("write edited draft");
    let error = review_apply_batch(&wd, None, None).expect_err("precondition fails");
    assert_eq!(error.code, "patch-apply-failed");
    // The draft was discarded (regenerated clean) and events requeued.
    let status = draft::workdir_status(&wd).expect("status");
    assert_eq!(status["state"], "clean");
    let events = queue::list_events_readonly(&wd);
    assert!(events.iter().all(|e| e["delivery_state"] == "queued"));
    assert!(events.iter().any(|e| e["last_error"]
        .as_str()
        .unwrap_or("")
        .contains("target text")));
}

#[test]
fn validate_patch_enforces_ranges_origins_and_bounds() {
    let good = patch_args("P0", 0, 3, "本发明", "新发明");
    assert!(validate_patch(&good).is_ok());
    let mut bad = good.clone();
    bad["origin"] = json!("robot");
    assert_eq!(
        validate_patch(&bad).expect_err("bad origin").code,
        "patch-origin"
    );
    let mut bad = good.clone();
    bad["target"]["start_offset"] = json!(6);
    bad["target"]["end_offset"] = json!(2);
    assert_eq!(
        validate_patch(&bad).expect_err("inverted range").code,
        "patch-range"
    );
    let mut bad = good.clone();
    bad["target"]["expected_text"] = json!("不一致");
    assert_eq!(
        validate_patch(&bad).expect_err("before != expected").code,
        "patch-precondition"
    );
    let mut bad = good.clone();
    bad["before"] = json!("x".repeat(8_001));
    assert_eq!(
        validate_patch(&bad).expect_err("oversized").code,
        "patch-too-large"
    );
}

// ---------------------------------------------------------------------------
// draft: MCP projection model
// ---------------------------------------------------------------------------

#[test]
fn draft_projection_initializes_reads_and_lists() {
    let wd = workdir("draft-init", "plain");
    assert!(!wd.join("edit.md").exists());
    let state = draft::ensure_projection(&wd).expect("projection initializes");
    assert_eq!(state.state, "clean");
    assert!(wd.join("edit.md").is_file());
    assert!(wd.join("edit.state.json").is_file());
    // The header carries the base hashes (header rebind target).
    let text = fs::read_to_string(wd.join("edit.md")).expect("edit.md readable");
    assert!(text.lines().next().unwrap_or("").starts_with("<!--@edit"));
    let listed = draft::list_paragraphs(&wd).expect("list");
    let paragraphs = listed["paragraphs"].as_array().unwrap();
    assert!(!paragraphs.is_empty());
    assert_eq!(paragraphs[0]["kind"], "p");
    let got = draft::get_paragraph(&wd, "P0").expect("get");
    assert!(!got["plain"].as_str().unwrap_or("").is_empty());
    let missing = draft::get_paragraph(&wd, "P9").expect_err("missing paragraph");
    assert_eq!(missing.code, "paragraph-not-found");
}

#[test]
fn draft_replace_insert_delete_diff_commit_revert() {
    let wd = workdir("draft-lifecycle", "plain");
    draft::ensure_projection(&wd).expect("projection");
    // replace_text: unique match, ambiguous, missing.
    let body = draft::get_paragraph(&wd, "P0").expect("get")["plain"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let old = &body[0..6];
    draft::replace_text(&wd, "P0", old, "新文本").expect("replace");
    assert_eq!(
        draft::get_paragraph(&wd, "P0").expect("get")["plain"],
        format!("新文本{}", &body[6..])
    );
    let error = draft::replace_text(&wd, "P0", "不存在的文本", "x").expect_err("missing");
    assert_eq!(error.code, "text-not-found");
    // diff_preview reports exactly the changed id.
    let diff = draft::diff_preview(&wd).expect("diff");
    assert_eq!(diff["state"], "dirty");
    assert_eq!(diff["changed_paragraph_ids"], json!(["P0"]));
    // insert after P0 (only P0 exists in the tracer inventory? no — every
    // paragraph exists; insert after P2).
    let inserted = draft::insert_paragraph(&wd, "P2", "插入段落", None).expect("insert");
    let temp_id = inserted["temp_id"].as_str().unwrap_or("").to_string();
    assert!(temp_id.starts_with('N'));
    let diff = draft::diff_preview(&wd).expect("diff");
    let changed = diff["changed_paragraph_ids"].as_array().unwrap();
    assert!(changed.contains(&json!("P0")));
    assert!(changed.contains(&json!(temp_id)));
    // delete P3.
    draft::delete_paragraph(&wd, "P3").expect("delete");
    let listed = draft::list_paragraphs(&wd).expect("list");
    let p3 = listed["paragraphs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "P3")
        .expect("P3 listed as deleted");
    assert_eq!(p3["deleted"], true);
    // commit: changed set = {P0, P3, <freshly allocated id>}. The draft works
    // in temp ids, but the commit reallocates the temp id to the next free
    // `P<n>` (mirror of Python's `_next_paragraph_id`) and reports THAT.
    let changed = draft::apply_projection(&wd).expect("commit");
    assert!(changed.contains(&"P0".to_string()));
    assert!(changed.contains(&"P3".to_string()));
    let allocated = changed
        .iter()
        .find(|id| **id != "P0" && **id != "P3")
        .expect("one freshly allocated paragraph id")
        .clone();
    assert!(
        allocated.starts_with('P'),
        "the commit must report an allocated P-id, got {allocated}"
    );
    assert!(
        !changed.contains(&temp_id),
        "the draft temp id {temp_id} must not survive into the commit"
    );
    let status = draft::workdir_status(&wd).expect("status");
    assert_eq!(status["state"], "clean");
    let text = fs::read_to_string(wd.join("edit.md")).expect("edit.md");
    let header = text.lines().next().unwrap_or("");
    assert!(
        header.contains("base-typed-sha256=\""),
        "header rebound: {header}"
    );
    // No-op commit reports an empty set and leaves typed.md untouched.
    let typed_before = fs::read_to_string(wd.join("typed.md")).expect("typed.md");
    let noop = draft::apply_projection(&wd).expect("noop commit");
    assert!(noop.is_empty());
    assert_eq!(
        fs::read_to_string(wd.join("typed.md")).expect("typed.md"),
        typed_before
    );
    // revert regenerates the clean projection (on a fresh workdir: the
    // committed workdir above has an unpublished canonical drift, which
    // correctly blocks the agent gate).
    let wd = workdir("draft-revert", "plain");
    draft::ensure_projection(&wd).expect("projection");
    draft::replace_text(&wd, "P0", "本发明", "回滚").expect("replace");
    assert_eq!(draft::diff_preview(&wd).expect("diff")["state"], "dirty");
    draft::discard_projection(&wd).expect("revert");
    let status = draft::workdir_status(&wd).expect("status");
    assert_eq!(status["state"], "clean");
    let listed = draft::list_paragraphs(&wd).expect("list");
    assert_eq!(listed["paragraphs"].as_array().unwrap().len(), 6);
}

#[test]
fn draft_blocks_container_and_part_paragraph_edits() {
    let wd = workdir("draft-containers", "plain");
    draft::ensure_projection(&wd).expect("projection");
    let error = draft::insert_paragraph(&wd, "T0", "x", None).expect_err("table target");
    assert_eq!(error.code, "table-structure-immutable");
    let error = draft::insert_paragraph(&wd, "B1", "x", None).expect_err("text box target");
    assert_eq!(error.code, "table-structure-immutable");
    let error = draft::delete_paragraph(&wd, "T0").expect_err("table delete");
    assert_eq!(error.code, "table-structure-immutable");
    let error = draft::delete_paragraph(&wd, "B1").expect_err("text box delete");
    assert_eq!(error.code, "table-structure-immutable");
    // A `P`-prefixed dotted id passes the structural guard and fails on the
    // missing paragraph (mirror of the Python guard).
    let error = draft::delete_paragraph(&wd, "P0.3").expect_err("part paragraph");
    assert_eq!(error.code, "paragraph-not-found");
}

#[test]
fn replace_in_body_enforces_unique_match_and_marker_rules() {
    // The primitive behind replace_text and batch_edit: pure, fails before
    // any write, so a failed edit can never leave a partial draft.
    let body = "本发明涉及生物医用材料。剂量为 20 mg。";
    let replaced = draft::replace_in_body(body, "本发明", "我们", "P0").expect("replace");
    assert_eq!(replaced, "我们涉及生物医用材料。剂量为 20 mg。");
    // Ambiguous text is rejected with the frozen code.
    let error = draft::replace_in_body("相同相同", "相同", "x", "P0").expect_err("ambiguous");
    assert_eq!(error.code, "text-ambiguous");
    let error = draft::replace_in_body("no match here", "缺失", "x", "P0").expect_err("missing");
    assert_eq!(error.code, "text-not-found");
    // Placeholder markers are never accepted as `old`.
    let error = draft::replace_in_body("a⟦b⟧c", "⟦b⟧", "x", "P0").expect_err("markers");
    assert_eq!(error.code, "text-not-found");
}

// ---------------------------------------------------------------------------
// server: store-wrapped mutation contract
// ---------------------------------------------------------------------------

#[test]
fn store_mutation_replays_byte_exact_and_rejects_reuse() {
    let wd = workdir("server-replay", "plain");
    let operation = "test-op";
    let operation_id = docx2typed_protocol::new_operation_id();
    let args = json!({ "paragraph_id": "P0", "note": "hello" });
    let run = |target: &Path| {
        let marker = target.join("replay-marker.json");
        fs::write(&marker, b"{\"applied\":true}\n").expect("marker");
        Ok(json!({ "data": { "note": "hello" }, "applied": true }))
    };
    let first = store_mutation(&wd, operation, &operation_id, &args, run).expect("first mutation");
    assert_eq!(first["applied"], true);
    // Identical operation_id + canonical args replays the original envelope
    // without running the closure a second time.
    let marker = wd.join("replay-marker.json");
    let first_bytes = fs::read(&marker).expect("marker exists");
    let rerun = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let rerun_flag = Arc::clone(&rerun);
    let replay = store_mutation(
        &wd,
        operation,
        &operation_id,
        &args,
        move |target: &Path| {
            rerun_flag.store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = fs::write(target.join("replay-marker.json"), b"{\"rerun\":true}\n");
            Ok(json!({ "rerun": true }))
        },
    )
    .expect("replay returns the original envelope");
    assert_eq!(
        replay["applied"], true,
        "replay must return the committed envelope"
    );
    assert!(
        !rerun.load(std::sync::atomic::Ordering::SeqCst),
        "the closure must not run again on replay"
    );
    assert_eq!(
        fs::read(wd.join("replay-marker.json")).expect("marker"),
        first_bytes,
        "replay is byte-exact with no second effect"
    );
    // Changed canonical args with the same operation_id is rejected.
    let changed_args = json!({ "paragraph_id": "P1", "note": "bye" });
    let error = store_mutation(&wd, operation, &operation_id, &changed_args, run)
        .expect_err("operation id reuse with different input");
    assert_eq!(error.code, "operation-id-reused");
}

#[test]
fn store_mutation_writer_lane_fails_fast_on_second_writer() {
    // The store Writer lane with lock_timeout 0 is fail-fast: a second
    // mutation while the first holds the lane gets writer-busy (never
    // silently queues or deadlocks). The HTTP layer additionally
    // serializes mutations per process (server::ReviewSession), so the
    // deterministic concurrent-publish CAS (one 200, one
    // current-parent-mismatch) is exercised end-to-end in review60.rs.
    let wd = workdir("server-lane", "plain");
    let first_id = docx2typed_protocol::new_operation_id();
    let first = store_mutation(
        &wd,
        "lane-test",
        &first_id,
        &json!({ "n": 1 }),
        |target: &Path| {
            std::thread::sleep(std::time::Duration::from_millis(150));
            fs::write(target.join("lane-marker.json"), b"{\"first\":true}\n").expect("marker");
            Ok(json!({ "committed": true }))
        },
    )
    .expect("first mutation");
    assert_eq!(first["committed"], true);
    // The first mutation released the lane; a fresh mutation proceeds.
    let second_id = docx2typed_protocol::new_operation_id();
    let second = store_mutation(
        &wd,
        "lane-test",
        &second_id,
        &json!({ "n": 2 }),
        |target: &Path| {
            fs::write(target.join("lane-marker.json"), b"{\"second\":true}\n").expect("marker");
            Ok(json!({ "committed": true }))
        },
    )
    .expect("second mutation");
    assert_eq!(second["committed"], true);
    // Both effects are durable in the committed generation chain.
    let mut committed = 0;
    for entry in fs::read_dir(wd.join(".docx2typed-store/generations")).expect("generations") {
        let entry = entry.expect("entry");
        if entry.path().join("lane-marker.json").is_file() {
            committed += 1;
        }
    }
    assert_eq!(committed, 2, "each mutation committed its own generation");
}

#[test]
fn store_mutation_surfaces_stable_errors() {
    let wd = workdir("server-errors", "plain");
    let operation_id = docx2typed_protocol::new_operation_id();
    let run = |_target: &Path| -> Result<Value, MutationError> {
        Err(MutationError::new(
            "patch-precondition",
            "target text no longer matches",
        ))
    };
    let error = store_mutation(&wd, "fail-test", &operation_id, &json!({}), run)
        .expect_err("closure failure surfaces");
    assert_eq!(error.code, "patch-precondition");
    assert_eq!(error.detail, "target text no longer matches");
}

#[test]
fn store_mutation_framed_rejects_stale_generation_without_side_effects() {
    let wd = workdir("frame-stale", "plain");
    let pin = docx2typed_store::Store::open(&wd).unwrap().pin().unwrap();
    let operation_id = docx2typed_protocol::new_operation_id();
    let args = json!({ "note": "hi" });
    let error = docx2typed_review::server::store_mutation_framed(
        &wd,
        "review_post",
        &operation_id,
        &args,
        "not-a-real-generation",
        "",
        |_target: &Path| Ok(json!({ "applied": true })),
    )
    .expect_err("stale generation must be rejected");
    assert_eq!(error.code, "stale-review-frame");
    // The stale frame POST touched neither the store generation, the queue,
    // nor the history trail.
    let after = docx2typed_store::Store::open(&wd).unwrap().pin().unwrap();
    assert_eq!(after.generation, pin.generation, "pointer must not move");
    let events = docx2typed_review::queue::snapshot_readonly(&wd);
    assert!(
        events["events"].as_array().unwrap().is_empty(),
        "queue untouched"
    );
    let history = docx2typed_review::history::list(&wd, &|_generation| None);
    assert!(
        history["records"].as_array().unwrap().is_empty(),
        "history untouched"
    );
}

#[test]
fn store_mutation_framed_replays_byte_exact_even_when_stale() {
    let wd = workdir("frame-replay", "plain");
    let pin = docx2typed_store::Store::open(&wd).unwrap().pin().unwrap();
    let operation_id = docx2typed_protocol::new_operation_id();
    let args = json!({ "note": "hi" });
    let first = docx2typed_review::server::store_mutation_framed(
        &wd,
        "review_post",
        &operation_id,
        &args,
        &pin.generation,
        "",
        |_target: &Path| Ok(json!({ "applied": true })),
    )
    .expect("first mutation commits");
    assert_eq!(first["applied"], true);
    // The frame is now stale (the mutation advanced the generation), but a
    // byte-exact retry with the same Idempotency-Key replays the original
    // committed data: the key is a retry identity, never a concurrency
    // token, so the stale expectation is not compared on replay.
    let replay = docx2typed_review::server::store_mutation_framed(
        &wd,
        "review_post",
        &operation_id,
        &args,
        &pin.generation,
        "",
        |_target: &Path| -> Result<Value, MutationError> {
            panic!("replay must not run the mutation closure")
        },
    )
    .expect("byte-exact replay");
    assert_eq!(replay, first, "replay is byte-exact");
    let committed = replay["committed_generation"]["generation"]
        .as_str()
        .expect("committed generation metadata");
    assert_ne!(
        committed, pin.generation,
        "the committed generation advanced past the pinned frame"
    );
}

#[test]
fn history_records_bind_generation() {
    let wd = workdir("history-gen", "plain");
    let state = docx2typed_review::collab::ensure_session(&wd).expect("session bootstraps");
    assert_eq!(state["current_snapshot"]["id"], "C0");
    let pin = docx2typed_store::Store::open(&wd).unwrap().pin().unwrap();
    let store = docx2typed_store::Store::new(&wd);
    let resolve = |generation: &str| store.generation_manifest_sha256(generation);
    let history = docx2typed_review::history::list(&wd, &resolve);
    let records = history["records"].as_array().expect("records");
    assert_eq!(records.len(), 1, "session-created record");
    let record = &records[0];
    assert_eq!(
        record["generation"].as_str(),
        Some(pin.generation.as_str()),
        "record is bound to the current generation"
    );
    let history_id = record["history_id"].as_str().expect("opaque history id");
    assert!(!history_id.is_empty());
    let manifest = record["generation_manifest_sha256"]
        .as_str()
        .expect("enriched manifest");
    assert_eq!(
        manifest.len(),
        64,
        "manifest is the generation assets sha256"
    );
    // Read-by-id round-trips the same record.
    let read = docx2typed_review::history::read(&wd, history_id, &resolve)
        .expect("history record resolves by opaque id");
    assert_eq!(read["history_id"], record["history_id"]);
    assert_eq!(read["generation"], record["generation"]);
}

#[test]
fn commit_records_island_edits_for_build() {
    let wd = workdir("islands-commit", "plain");
    draft::ensure_projection(&wd).expect("projection");
    let body = draft::get_paragraph(&wd, "P0").expect("get")["plain"]
        .as_str()
        .expect("plain")
        .to_string();
    let new_body = format!("改写{}", &body[6..]);
    draft::replace_text(&wd, "P0", &body, &new_body).expect("replace");
    let changed = draft::apply_projection(&wd).expect("commit");
    assert_eq!(changed, vec!["P0".to_string()]);
    // The island sidecar records the committed text edit.
    let islands = docx2typed_core::prose::load_islands(&wd).expect("islands");
    assert_eq!(islands.len(), 1, "one island edit recorded");
    assert_eq!(islands[0].paragraph_id, "P0");
    assert_eq!(islands[0].old, body);
    assert_eq!(islands[0].new, new_body);
    // The engine build applies the island edit to the template bytes.
    let plan = docx2typed_core::plan_build(&wd).expect("build plan");
    assert_eq!(plan.edits.len(), 1);
}
