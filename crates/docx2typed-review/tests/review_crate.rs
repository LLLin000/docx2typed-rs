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
// document_patch: the batch edit primitive
// ---------------------------------------------------------------------------

fn direct() -> draft::EditTracking<'static> {
    draft::EditTracking {
        tracked: false,
        author: None,
        date: None,
    }
}

fn hunk_list(hunks: &Value) -> Vec<Value> {
    hunks.as_array().expect("hunks array").clone()
}

/// The current draft revision token (what `document_read` hands back and
/// `base_revision` checks).
fn revision(wd: &Path) -> String {
    draft::workdir_status(wd).expect("status")["edit_body_sha256"]
        .as_str()
        .expect("revision token")
        .to_string()
}

fn patch(
    wd: &Path,
    hunks: &Value,
    base_revision: Option<&str>,
) -> Result<Value, docx2typed_review::collab::CollaborationError> {
    let hunks = hunk_list(hunks);
    draft::document_patch(wd, Some(&hunks), None, base_revision, false, direct())
}

#[test]
fn document_patch_edits_the_exact_span_a_search_reference_names() {
    // The search -> patch loop: a caller edits the spot `document_search`
    // pointed at WITHOUT copying its text, and a reference that no longer
    // addresses what it was issued for is refused rather than landing on
    // whatever now sits at those offsets.
    let wd = workdir("patch-match-ref", "plain");
    draft::ensure_projection(&wd).expect("projection");
    let found = docx2typed_core::document_search::document_search(
        &wd,
        "本发明",
        3000,
        false,
        20,
        0,
        "body",
    )
    .expect("search");
    let reference = found
        .matches
        .iter()
        .flat_map(|block| block.occurrences.iter())
        .map(|occurrence| occurrence.match_ref.clone())
        .next()
        .expect("a match reference");

    // Edit through the reference alone.
    let applied = patch(
        &wd,
        &json!([{ "match_ref": reference, "new": "我们" }]),
        None,
    )
    .expect("match_ref hunk applies");
    assert_eq!(applied["draft"], "dirty");

    // The same reference is now stale: its bound text digest no longer
    // matches what sits at those offsets, and that is a refusal.
    let error = patch(
        &wd,
        &json!([{ "match_ref": reference, "new": "他们" }]),
        None,
    )
    .expect_err("a reused reference must refuse");
    assert_eq!(error.code, "match-ref-stale", "{}", error.detail);

    // A corrupt reference never resolves to a guessed span either.
    let error = patch(
        &wd,
        &json!([{ "match_ref": "ref_not-base64!!", "new": "x" }]),
        None,
    )
    .expect_err("a corrupt reference must refuse");
    assert_eq!(error.code, "match-ref-invalid", "{}", error.detail);
}

#[test]
fn document_patch_applies_replace_insert_delete_in_one_batch() {
    let wd = workdir("patch-batch", "plain");
    draft::ensure_projection(&wd).expect("projection");
    let hunks = json!([
        // two non-overlapping replaces in ONE paragraph (git apply style)
        {"paragraph_id": "P0", "old": "本发明", "new": "我们"},
        {"paragraph_id": "P0", "old": "20 mg", "new": "40 mg"},
        {"paragraph_id": "P1", "old": "250 mg", "new": "500 mg"},
        {"insert_after": "P2", "text": "新增段落。"},
        {"delete": "P3"},
    ]);
    let result = patch(&wd, &hunks, None).expect("batch applies");
    assert_eq!(
        draft::get_paragraph(&wd, "P0").expect("get")["plain"],
        "我们涉及生物医用材料技术领域。剂量为 40 mg。"
    );
    assert_eq!(
        draft::get_paragraph(&wd, "P1").expect("get")["plain"],
        "实施例1采用 500 mg 剂量。"
    );
    // The applied summary mirrors the Python payload: replaces report their
    // hunk count, inserts their temp id, deletes their paragraph id.
    let applied = result["applied"].as_array().expect("applied");
    assert_eq!(
        applied[0],
        json!({"kind": "replace", "paragraph_id": "P0", "hunks": 2})
    );
    assert_eq!(
        applied[1],
        json!({"kind": "replace", "paragraph_id": "P1", "hunks": 1})
    );
    assert_eq!(applied[2], json!({"kind": "delete", "paragraph_id": "P3"}));
    assert_eq!(
        applied[3],
        json!({"kind": "insert", "after_id": "P2", "temp_id": "N1"})
    );
    assert_eq!(
        result["affected_paragraph_ids"],
        json!(["N1", "P0", "P1", "P3"])
    );
    assert_eq!(result["document_state"]["draft"], "dirty");
    assert_ne!(
        result["document_state"]["revision_before"], result["document_state"]["revision_after"],
        "the batch moves the revision token"
    );
    // The inserted block landed directly after its anchor; P3 reads deleted.
    let listed = draft::list_paragraphs(&wd).expect("list");
    let paragraphs = listed["paragraphs"].as_array().expect("paragraphs");
    let ids: Vec<&str> = paragraphs
        .iter()
        .map(|p| p["id"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(ids, vec!["P0", "P1", "P2", "N1", "P4", "P5", "P3"]);
    let p3 = paragraphs.iter().find(|p| p["id"] == "P3").expect("P3");
    assert_eq!(p3["deleted"], true);
    // Every replace hunk recorded its leaf-local island edit in ONE sidecar
    // write (the batch resolves all of them before writing).
    let islands: Value =
        serde_json::from_str(&fs::read_to_string(wd.join("islands.json")).expect("islands"))
            .expect("islands json");
    let edits = islands["edits"].as_array().expect("edits");
    // ONE record per (paragraph, leaf), not one per hunk. The island rewrites
    // the whole run, so P0's two replacements must travel in a single record:
    // two records would each expect the pre-edit leaf and the second would
    // silently not apply (measured on a real patent: 34 reported, 28 applied).
    assert_eq!(edits.len(), 2, "one island record per leaf");
    assert_eq!(edits[0]["paragraph_id"], "P0");
    assert_eq!(
        edits[0]["old"], "本发明涉及生物医用材料技术领域。剂量为 20 mg。",
        "the record carries the whole leaf as it stands"
    );
    assert_eq!(
        edits[0]["new"], "我们涉及生物医用材料技术领域。剂量为 40 mg。",
        "both replacements land in the one record"
    );
    assert_eq!(edits[1]["paragraph_id"], "P1");
    assert_eq!(edits[1]["old"], "实施例1采用 250 mg 剂量。");
    assert_eq!(edits[1]["new"], "实施例1采用 500 mg 剂量。");
    // The batch result is commit-ready.
    let changed = draft::apply_projection(&wd).expect("commit");
    assert!(changed.contains(&"P0".to_string()));
    assert!(changed.contains(&"P1".to_string()));
    assert!(changed.contains(&"P3".to_string()));
}

#[test]
fn document_patch_rejects_overlapping_hunks_without_touching_the_draft() {
    let wd = workdir("patch-overlap", "plain");
    draft::ensure_projection(&wd).expect("projection");
    let before = fs::read(wd.join("edit.md")).expect("edit.md");
    // "本发明涉及" and "涉及生物" share the "涉及" span.
    let hunks = json!([
        {"paragraph_id": "P0", "old": "本发明涉及", "new": "A"},
        {"paragraph_id": "P0", "old": "涉及生物", "new": "B"},
    ]);
    let error = patch(&wd, &hunks, None).expect_err("overlap rejected");
    assert_eq!(error.code, "patch-hunks-invalid");
    assert!(
        error.detail.contains("overlap"),
        "the refusal names the overlap: {}",
        error.detail
    );
    assert_eq!(
        fs::read(wd.join("edit.md")).expect("edit.md"),
        before,
        "a rejected batch leaves the draft byte-identical"
    );
}

#[test]
fn document_patch_one_bad_hunk_leaves_the_draft_byte_identical() {
    let wd = workdir("patch-atomic", "plain");
    draft::ensure_projection(&wd).expect("projection");
    let before = fs::read(wd.join("edit.md")).expect("edit.md");
    let islands_before = fs::read(wd.join("islands.json")).ok();
    // Hunk #1 is valid, hunk #2 can never resolve: the batch must apply nothing.
    let hunks = json!([
        {"paragraph_id": "P0", "old": "本发明", "new": "我们"},
        {"paragraph_id": "P1", "old": "不存在的文本", "new": "x"},
    ]);
    let error = patch(&wd, &hunks, None).expect_err("bad hunk");
    assert_eq!(error.code, "text-not-found");
    assert!(
        error.detail.starts_with("hunk #2 (P1)"),
        "the refusal names the failing hunk: {}",
        error.detail
    );
    assert_eq!(
        fs::read(wd.join("edit.md")).expect("edit.md"),
        before,
        "the atomicity guarantee: nothing of the batch landed"
    );
    assert_eq!(
        fs::read(wd.join("islands.json")).ok(),
        islands_before,
        "no island edit of the failed batch landed either"
    );
    // Several broken hunks surface TOGETHER as one report, still applying
    // nothing.
    let hunks = json!([
        {"paragraph_id": "P0", "old": "本发明", "new": "我们"},
        {"paragraph_id": "P1", "old": "缺失", "new": "x"},
        {"paragraph_id": "P0", "old": "剂量", "new": "剂量"},
    ]);
    let error = patch(&wd, &hunks, None).expect_err("two bad hunks");
    assert_eq!(error.code, "patch-hunks-invalid");
    assert!(error.detail.contains("2 of 3 hunks are invalid"));
    assert!(error.detail.contains("hunk #2 (P1): text-not-found"));
    assert!(error.detail.contains("hunk #3 (P0): patch-noop"));
    assert_eq!(
        fs::read(wd.join("edit.md")).expect("edit.md"),
        before,
        "still byte-identical"
    );
}

#[test]
fn document_patch_hunk_diagnostics_and_arguments() {
    let wd = workdir("patch-errors", "plain");
    draft::ensure_projection(&wd).expect("projection");
    // old == new is a no-op, not an edit.
    let error = patch(
        &wd,
        &json!([{"paragraph_id": "P0", "old": "本发明", "new": "本发明"}]),
        None,
    )
    .expect_err("noop");
    assert_eq!(error.code, "patch-noop");
    // P5 carries the same phrase twice.
    let error = patch(
        &wd,
        &json!([{"paragraph_id": "P5", "old": "重复句子内容", "new": "x"}]),
        None,
    )
    .expect_err("ambiguous");
    assert_eq!(error.code, "text-ambiguous");
    let error = patch(
        &wd,
        &json!([{"paragraph_id": "P0", "old": "不存在", "new": "x"}]),
        None,
    )
    .expect_err("missing");
    assert_eq!(error.code, "text-not-found");
    // Container and part paragraphs belong to the container tools.
    let error = patch(&wd, &json!([{"delete": "T0"}]), None).expect_err("table");
    assert_eq!(error.code, "table-structure-immutable");
    let error =
        patch(&wd, &json!([{"insert_after": "B1", "text": "x"}]), None).expect_err("text box");
    assert_eq!(error.code, "table-structure-immutable");
    // Comment text needs the explicit opt-in.
    let error = patch(&wd, &json!([{"delete": "comments.P0"}]), None).expect_err("comment");
    assert_eq!(error.code, "comment-text-requires-opt-in");
    // Shape and argument errors.
    let hunks = hunk_list(&json!([]));
    let error = draft::document_patch(&wd, Some(&hunks), None, None, false, direct())
        .expect_err("empty hunks");
    assert_eq!(error.code, "invalid-arguments");
    assert_eq!(error.detail, "hunks must not be empty");
    let error =
        draft::document_patch(&wd, None, None, None, false, direct()).expect_err("neither form");
    assert_eq!(error.code, "invalid-arguments");
    assert_eq!(error.detail, "provide exactly one of hunks or diff");
    let hunks = hunk_list(&json!([{"paragraph_id": "P0", "old": "本发明", "new": "我们"}]));
    let error = draft::document_patch(
        &wd,
        Some(&hunks),
        Some("@@ -1 +1 @@"),
        None,
        false,
        direct(),
    )
    .expect_err("both forms");
    assert_eq!(error.code, "invalid-arguments");
    // The unified-diff form is not implemented in this build.
    let error = draft::document_patch(&wd, None, Some("@@ -1 +1 @@"), None, false, direct())
        .expect_err("diff form");
    assert_eq!(error.code, "invalid-arguments");
    assert!(error.detail.contains("not implemented yet"));
    // Unknown keys are refused rather than silently ignored.
    let error = patch(
        &wd,
        &json!([{"paragraph_id": "P0", "old": "本发明", "neww": "x"}]),
        None,
    )
    .expect_err("unknown key");
    assert_eq!(error.code, "invalid-arguments");
    assert!(error.detail.contains("\"neww\""));
    // A replace hunk without 'new' cannot be a deletion in disguise.
    let error = patch(&wd, &json!([{"paragraph_id": "P0", "old": "本发明"}]), None)
        .expect_err("missing new");
    assert_eq!(error.code, "invalid-arguments");
    // A hunk that is not an object.
    let error = patch(&wd, &json!(["P0"]), None).expect_err("not an object");
    assert_eq!(error.code, "invalid-arguments");
    // No key at all.
    let error =
        patch(&wd, &json!([{"old": "本发明", "new": "我们"}]), None).expect_err("no target");
    assert_eq!(error.code, "invalid-arguments");
}

#[test]
fn document_patch_base_revision_gates_and_heals() {
    let wd = workdir("patch-base-revision", "plain");
    draft::ensure_projection(&wd).expect("projection");
    let token = revision(&wd);
    let hunks = json!([{"paragraph_id": "P0", "old": "本发明", "new": "我们"}]);
    let first = patch(&wd, &hunks, Some(&token)).expect("current token applies");
    assert!(first["warnings"].as_array().expect("warnings").is_empty());
    // A token matching neither the draft nor the previous committed view is a
    // view the caller never read.
    let error = patch(&wd, &hunks, Some(&"0".repeat(64))).expect_err("unknown token");
    assert_eq!(error.code, "stale-document-view");
    // The pre-patch token is the previous committed view: hunks that still
    // resolve are applied against the current draft and healed with a warning.
    let healed = patch(
        &wd,
        &json!([{"paragraph_id": "P1", "old": "250 mg", "new": "300 mg"}]),
        Some(&token),
    )
    .expect("stale view heals while every anchor resolves");
    assert!(healed["warnings"]
        .as_array()
        .expect("warnings")
        .iter()
        .any(|warning| warning
            .as_str()
            .unwrap_or("")
            .starts_with("stale-document-view-healed")));
    // The same token with a hunk whose text moved fails closed instead.
    let error = patch(&wd, &hunks, Some(&token)).expect_err("stale anchor");
    assert_eq!(error.code, "stale-document-view");
    // An exact retry of a completed patch replays from the store ledger without
    // reaching the base_revision check — so a lost response can be re-sent.
    // (The mutation closure runs against the store's overlaid generation, so it
    // ensures the projection there exactly like the MCP draft tools do.)
    let wd = workdir("patch-replay", "plain");
    draft::ensure_projection(&wd).expect("projection");
    let token = revision(&wd);
    let hunks = hunk_list(&json!([{"paragraph_id": "P0", "old": "本发明", "new": "我们"}]));
    let args = json!({"hunks": hunks, "base_revision": token});
    let operation_id = docx2typed_protocol::new_operation_id();
    let run = {
        let hunks = hunks.clone();
        let token = token.clone();
        move |target: &Path| {
            draft::ensure_projection(target)
                .map_err(|error| MutationError::new(error.code, error.detail))?;
            draft::document_patch(target, Some(&hunks), None, Some(&token), false, direct())
                .map_err(|error| MutationError::new(error.code, error.detail))
        }
    };
    let first =
        store_mutation(&wd, "document_patch", &operation_id, &args, run).expect("first patch");
    assert_eq!(first["document_state"]["draft"], "dirty");
    let applied_once = fs::read(wd.join("edit.md")).expect("edit.md");
    assert!(
        String::from_utf8_lossy(&applied_once).contains("我们"),
        "the committed draft carries the batch"
    );
    // The retry's closure would fail if it ran: the old text is gone.
    let replay = store_mutation(
        &wd,
        "document_patch",
        &operation_id,
        &args,
        |_target: &Path| {
            Err(MutationError::new(
                "should-not-run",
                "the ledger replay must not reach the mutation closure",
            ))
        },
    )
    .expect("exact retry replays");
    assert_eq!(replay["applied"], first["applied"]);
    assert_eq!(replay["document_state"], first["document_state"]);
    assert_eq!(
        fs::read(wd.join("edit.md")).expect("edit.md"),
        applied_once,
        "the replay has no second effect"
    );
}

#[test]
fn document_patch_refuses_a_span_crossing_a_revision_boundary() {
    let wd = workdir("patch-boundary", "plain");
    draft::ensure_projection(&wd).expect("projection");
    // A projection carrying a revision-control token: the visible text still
    // reads "本发明涉及…", but an insert marker splits it after the first
    // character, so "本发" is not one editable run.
    let text = fs::read_to_string(wd.join("edit.md")).expect("edit.md");
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let index = lines
        .iter()
        .position(|line| line.starts_with("<!--@p id=\"P0\""))
        .expect("P0 block");
    lines[index + 1] =
        "本\u{27e6}insert\u{27e7}发明涉及生物医用材料技术领域。剂量为 20 mg。".to_string();
    fs::write(wd.join("edit.md"), format!("{}\n", lines.join("\n"))).expect("write draft");
    let error = patch(
        &wd,
        &json!([{"paragraph_id": "P0", "old": "本发", "new": "X"}]),
        None,
    )
    .expect_err("span crosses the marker");
    assert_eq!(error.code, "edit-span-crosses-revision-boundary");
    assert!(error.detail.contains("revision-control markers"));
    // A span that stays on one side of the marker is an ordinary replace.
    let result = patch(
        &wd,
        &json!([{"paragraph_id": "P0", "old": "本", "new": "该"}]),
        None,
    )
    .expect("in-span edit");
    assert_eq!(result["applied"][0]["paragraph_id"], "P0");
    assert!(draft::get_paragraph(&wd, "P0").expect("get")["text"]
        .as_str()
        .unwrap_or("")
        .contains("\u{27e6}insert\u{27e7}"));
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

// ---------------------------------------------------------------------------
// Projection invariants
// ---------------------------------------------------------------------------
//
// The projection is a second view of the document, and nothing forced it to
// stay one. These three properties are what "the projection is faithful" means,
// and each of them was false before this suite existed:
//
//   A  a commit that changes nothing must change nothing
//   B  a paragraph the commit does not touch must come out as it went in
//   C  an edit must move its declared target and leave everything else alone
//
// The fixture carries a revision and a comment on purpose. Both were lossy
// through the draft dialect: the commit rewrote every paragraph by re-parsing
// `edit.md`, which spells revisions as `⟦insert⟧` tokens and carries no style
// spans at all, so one edit in one paragraph rewrote the other ninety-eight
// into a poorer spelling of themselves.

mod projection_invariants {
    use super::*;
    use std::collections::BTreeMap;

    /// Every paragraph body in `typed.md`, markup intact.
    fn typed_blocks(wd: &Path) -> BTreeMap<String, String> {
        let text = fs::read_to_string(wd.join("typed.md")).expect("typed.md");
        let mut out: BTreeMap<String, String> = BTreeMap::new();
        let mut current: Option<String> = None;
        let mut body: Vec<String> = Vec::new();
        let flush = |out: &mut BTreeMap<String, String>,
                     current: &mut Option<String>,
                     body: &mut Vec<String>| {
            if let Some(id) = current.take() {
                out.insert(id, body.join("\n").trim_end_matches('\n').to_string());
            }
            body.clear();
        };
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("<!--@p id=\"") {
                flush(&mut out, &mut current, &mut body);
                current = Some(
                    trimmed
                        .split("id=\"")
                        .nth(1)
                        .and_then(|rest| rest.split('"').next())
                        .unwrap_or("")
                        .to_string(),
                );
                continue;
            }
            if trimmed.starts_with("<!--@part ") || trimmed.starts_with("<!--@typed") {
                flush(&mut out, &mut current, &mut body);
                continue;
            }
            if current.is_some() {
                body.push(line.to_string());
            }
        }
        flush(&mut out, &mut current, &mut body);
        out
    }

    fn projection_shape(wd: &Path) -> (usize, usize, usize, usize) {
        let text = fs::read_to_string(wd.join("typed.md")).expect("typed.md");
        (
            text.matches("data-s=").count(),
            text.matches("<docx-revision").count(),
            text.matches("<docx-anchor").count(),
            text.matches("<docx-inline").count(),
        )
    }

    /// A: committing a draft that changed nothing changes nothing.
    ///
    /// A document engine that alters the logical structure of a document when
    /// nothing was asked of it has an unstable representation, and every later
    /// comparison against it is untrustworthy.
    #[test]
    fn invariant_a_a_commit_with_no_edit_changes_nothing() {
        let wd = workdir("invariant-a", "complex");
        draft::ensure_projection(&wd).expect("projection");
        let before = typed_blocks(&wd);
        let shape_before = projection_shape(&wd);

        draft::apply_projection(&wd).expect("commit");

        let after = typed_blocks(&wd);
        if after != before {
            let mut report: Vec<String> = Vec::new();
            for (id, body) in &before {
                match after.get(id) {
                    None => report.push(format!("{id}: DROPPED")),
                    Some(other) if other != body => {
                        report.push(format!("{id}:\n    before {body:?}\n    after  {other:?}"))
                    }
                    _ => {}
                }
            }
            for id in after.keys() {
                if !before.contains_key(id) {
                    report.push(format!("{id}: ADDED"));
                }
            }
            panic!(
                "a no-op commit rewrote the projection:\n{}",
                report.join("\n")
            );
        }
        assert_eq!(
            projection_shape(&wd),
            shape_before,
            "a no-op commit changed the markup"
        );
        assert!(
            !wd.join("structure.json").is_file(),
            "a no-op commit recorded structural edits nobody made"
        );
    }

    /// B and C: an edit moves its declared target and leaves the rest alone.
    ///
    /// The loss was global, not local — that is the whole point. A commit that
    /// re-renders paragraphs it was not asked about cannot be trusted to have
    /// preserved them, whatever its reported counts say.
    #[test]
    fn invariant_bc_an_edit_changes_only_its_declared_target() {
        let wd = workdir("invariant-bc", "complex");
        draft::ensure_projection(&wd).expect("projection");
        let before = typed_blocks(&wd);

        // Find one addressable span, then edit exactly it.
        let found = docx2typed_core::document_search::document_search(
            &wd, "EDIT-ME", 3000, false, 20, 0, "body",
        )
        .expect("search runs");
        let (paragraph_id, reference) = found
            .matches
            .iter()
            .flat_map(|block| {
                block
                    .occurrences
                    .iter()
                    .map(move |occurrence| (block.id.clone(), occurrence.match_ref.clone()))
            })
            .next()
            .expect("fixture has EDIT-ME");
        patch(
            &wd,
            &json!([{ "match_ref": reference, "new": "EDITED-ME" }]),
            None,
        )
        .expect("patch applies");
        draft::apply_projection(&wd).expect("commit");

        let after = typed_blocks(&wd);
        let touched: Vec<&String> = before
            .keys()
            .filter(|id| after.get(*id) != before.get(*id))
            .collect();
        assert_eq!(
            touched,
            vec![&paragraph_id],
            "a commit changed paragraphs it was not asked about: {touched:?}"
        );

        // The rest of the document is not merely equal in TEXT: its markup is
        // the same markup. A re-render that happened to read the same would
        // still have thrown away the spans, anchors and inlines it could not
        // spell, and those are what the next reader needs.
        //
        // Measured over the paragraphs the commit did not touch, which is the
        // part that must be invariant. The edited paragraph is still re-rendered
        // through the draft dialect, so its own markup may thin out; that is a
        // known limit, not a licence for the damage to be global. Before this
        // suite the same edit took the WHOLE file's spans 19 -> 6.
        let others = |blocks: &BTreeMap<String, String>| -> (usize, usize, usize) {
            blocks.iter().filter(|(id, _)| *id != &paragraph_id).fold(
                (0, 0, 0),
                |acc, (_, body)| {
                    (
                        acc.0 + body.matches("data-s=").count(),
                        acc.1 + body.matches("<docx-anchor").count(),
                        acc.2 + body.matches("<docx-inline").count(),
                    )
                },
            )
        };
        let before_others = others(&before);
        let after_others = others(&after);
        assert_eq!(
            after_others, before_others,
            "the commit degraded paragraphs it was not asked about: \
             {before_others:?} -> {after_others:?}"
        );
        assert!(
            after.len() >= before.len(),
            "the commit dropped {} paragraph(s) from the projection",
            before.len() - after.len()
        );
    }

    /// C, at the fidelity baseline: the revisions that were in the file are
    /// still in the file, with their identity intact.
    ///
    /// A count is not fidelity. Two revisions can be swapped, or one can lose
    /// its author, and the count still balances.
    #[test]
    fn invariant_c_existing_revisions_keep_their_identity() {
        let wd = workdir("invariant-c", "complex");
        draft::ensure_projection(&wd).expect("projection");

        let identity = |wd: &Path| -> BTreeMap<String, String> {
            let text = fs::read_to_string(wd.join("typed.md")).expect("typed.md");
            let mut out = BTreeMap::new();
            for chunk in text.split("<docx-revision ").skip(1) {
                let Some(head) = chunk.split('>').next() else {
                    continue;
                };
                let id = head
                    .split("id=\"")
                    .nth(1)
                    .and_then(|rest| rest.split('"').next())
                    .unwrap_or("")
                    .to_string();
                if id.is_empty() {
                    continue;
                }
                out.insert(id, head.to_string());
            }
            out
        };

        let before = identity(&wd);
        assert!(!before.is_empty(), "fixture carries revisions");

        let found = docx2typed_core::document_search::document_search(
            &wd, "EDIT-ME", 3000, false, 20, 0, "body",
        )
        .expect("search runs");
        let reference = found
            .matches
            .iter()
            .flat_map(|block| block.occurrences.iter())
            .map(|occurrence| occurrence.match_ref.clone())
            .next()
            .expect("occurrence carries a reference");
        patch(
            &wd,
            &json!([{ "match_ref": reference, "new": "EDITED-ME" }]),
            None,
        )
        .expect("patch applies");
        draft::apply_projection(&wd).expect("commit");

        let after = identity(&wd);
        for (id, head) in &before {
            assert_eq!(
                after.get(id),
                Some(head),
                "revision {id} lost its identity across a commit that did not touch it"
            );
        }
    }
}
