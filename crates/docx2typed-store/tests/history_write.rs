//! `history_gc` (retention) and `history_restore` (copy an earlier version
//! forward) — the writer half of the timeline, mirroring `store.history_gc`
//! and the `history_restore` mutation.

use std::fs;
use std::path::{Path, PathBuf};

use docx2typed_protocol::{bytes_sha256, canonical_json, new_operation_id};
use docx2typed_store::store::{Store, StoreMutateRequest, Transaction};
use docx2typed_store::versions::{history_gc, history_list, history_restore, trimmed_versions};
use docx2typed_store::{read_root, MutateRun, RunOutcome, StoreError};
use serde_json::{json, Value};

struct ScratchGuard(PathBuf);

impl std::ops::Deref for ScratchGuard {
    type Target = std::path::Path;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<std::path::Path> for ScratchGuard {
    fn as_ref(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for ScratchGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn temp_dir(tag: &str) -> ScratchGuard {
    let dir = std::env::temp_dir().join(format!(
        "docx2typed-history-write-{tag}-{}-{}",
        std::process::id(),
        new_operation_id().get(..8).unwrap_or("x")
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    ScratchGuard(dir)
}

/// Store one object at the Python address (`sha256(kind + "\0" + bytes)`).
fn put_object(root: &Path, kind: &str, payload: &Value) -> String {
    let bytes = canonical_json(payload).into_bytes();
    let mut identity = kind.as_bytes().to_vec();
    identity.push(0);
    identity.extend_from_slice(&bytes);
    let digest = bytes_sha256(&identity);
    let path = root
        .join(".docx2typed-store/objects")
        .join(kind)
        .join(&digest[..2])
        .join(&digest[2..]);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, bytes).unwrap();
    digest
}

fn write_pointer(root: &Path, fields: &Value) {
    let mut pointer = json!({
        "schema": "docx2typed-workdir-pointer-1",
        "generation": "g2",
        "operation_id": "op",
        "manifest_sha256": "manifest",
        "written_at": "2026-01-01T00:00:00Z",
    });
    for (key, value) in fields.as_object().unwrap() {
        pointer[key] = value.clone();
    }
    fs::write(root.join("workdir.json"), canonical_json(&pointer)).unwrap();
}

/// A Python-shaped two-version pool: V1 (tree t1) then V2 (tree t2), with a
/// generation for each. `pin_v1` pins V1 as a retention root.
fn pool_workdir(tag: &str, pin_v1: bool) -> ScratchGuard {
    let root = temp_dir(tag);
    fs::write(root.join("typed.md"), "v2\n").unwrap();
    fs::write(root.join("format.json"), "{}").unwrap();
    fs::write(root.join("styles.json"), "{}").unwrap();
    fs::write(root.join("_template.docx"), b"PK\x03\x04").unwrap();

    let blob_v1 = put_object(&root, "blob", &json!("v1"));
    let blob_v2 = put_object(&root, "blob", &json!("v2"));
    let tree_v1 = put_object(
        &root,
        "tree",
        &json!({ "parts": { "typed.md": { "whole": blob_v1 } } }),
    );
    let tree_v2 = put_object(
        &root,
        "tree",
        &json!({ "parts": { "typed.md": { "whole": blob_v2 } } }),
    );
    let commit_v1 = put_object(
        &root,
        "version",
        &json!({
            "schema": "docx2typed-version-1",
            "version": "V1",
            "seq": 1,
            "parent_commit": null,
            "tree_object": tree_v1,
            "pin": pin_v1,
            "origin": "commit_sync",
            "created_at": "2026-01-01T00:00:00Z",
        }),
    );
    let commit_v2 = put_object(
        &root,
        "version",
        &json!({
            "schema": "docx2typed-version-1",
            "version": "V2",
            "seq": 2,
            "parent_commit": commit_v1,
            "tree_object": tree_v2,
            "pin": false,
            "origin": "commit_sync",
            "created_at": "2026-01-01T00:00:00Z",
        }),
    );

    // Generations g1/g2 carry the version records retention reads to decide
    // what a generation is still for.
    for (generation, parent, version, tree) in [
        ("g1", None, "V1", tree_v1.as_str()),
        ("g2", Some("g1"), "V2", tree_v2.as_str()),
    ] {
        let dir = root.join(".docx2typed-store/generations").join(generation);
        fs::create_dir_all(&dir).unwrap();
        let _ = tree;
        fs::write(
            dir.join("generation.json"),
            canonical_json(&json!({
                "schema": "docx2typed-generation-manifest-1",
                "generation": generation,
                "parent": parent,
                "operation_id": "op",
                "input_sha256": "input",
                "assets": [],
                "assets_sha256": "assets",
                "created_at": "2026-01-01T00:00:00Z",
                "version": { "version": version, "tree_object": tree },
            })),
        )
        .unwrap();
    }

    // The recorded tree must match the canonical mirror so HEAD is not dirty.
    write_pointer(
        &root,
        &json!({
            "head_version": "V2",
            "head_version_generation": "g2",
            "head_commit": commit_v2,
            "head_tree": "tree-digest-V2",
            "head_tree_object": tree_v2,
            "baseline_epoch": 1,
            "version_seq": 2,
        }),
    );
    root
}

#[test]
fn gc_dry_run_reports_the_plan_and_touches_nothing() {
    let root = pool_workdir("gc-dry", false);
    let report = history_gc(&root, 1, true).expect("gc");

    assert_eq!(report["schema"], "docx2typed-history-gc-1");
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["versions_total"], 2);
    assert_eq!(report["versions_retained"], 1);
    assert_eq!(report["trees_kept"], 1);
    assert_eq!(report["versions_trimmed"], json!(["V1"]));
    assert_eq!(report["swept_objects"], 0);
    assert_eq!(report["freed_bytes"], 0);
    assert_eq!(report["generations_reclaimable"], 1);

    // Nothing marked or swept.
    assert!(trimmed_versions(&root).expect("trimmed").is_empty());
    assert!(root.join(".docx2typed-store/generations/g1").is_dir());
}

#[test]
fn gc_apply_trims_sweeps_and_reclaims() {
    let root = pool_workdir("gc-apply", false);
    let report = history_gc(&root, 1, false).expect("gc");

    assert_eq!(report["versions_trimmed"], json!(["V1"]));
    assert_eq!(report["generations_reclaimed"], 1);
    assert!(report["swept_objects"].as_u64().unwrap() >= 1);
    assert!(report["freed_bytes"].as_u64().unwrap() >= 1);

    let trimmed = trimmed_versions(&root).expect("trimmed");
    assert!(trimmed.contains("V1"));
    assert!(!root.join(".docx2typed-store/generations/g1").exists());

    // V1's own content is gone; V2's is retained.
    let history = history_list(&root, 20, 0).expect("list");
    assert_eq!(history["trimmed"], 1);
    assert_eq!(history["retained"], 1);
    assert_eq!(history["versions"][0]["version"], "V2");
    assert_eq!(history["versions"][0]["content"], "retained");
    assert_eq!(history["versions"][1]["content"], "trimmed");

    // Idempotent: a second run has nothing new to trim, so it reports the plan
    // unchanged (no sweep keys — exactly the Python contract).
    let again = history_gc(&root, 1, false).expect("gc again");
    assert_eq!(again["versions_retained"], 1);
    assert_eq!(again["swept_objects"], 0);
    assert_eq!(again["generations_reclaimed"], Value::Null);
}

#[test]
fn gc_keeps_pinned_versions() {
    let root = pool_workdir("gc-pin", true);
    let report = history_gc(&root, 1, true).expect("gc");
    assert_eq!(report["versions_retained"], 2);
    assert_eq!(report["versions_trimmed"], json!([]));
    // A pinned version's generation is still reclaimable: its content lives in
    // the pool now, so the generation directory is redundant, not the version.
    assert_eq!(report["generations_reclaimable"], 1);
}

// --- restore ---------------------------------------------------------------

const TYPED_HEADER: &str = "<!--@typed schema=\"1\" format=\"format.json\" styles=\"styles.json\" template=\"_template.docx\" source=\"x.docx\"-->";

fn block(id: &str, text: &str) -> String {
    format!("<!--@p id=\"{id}\" base=\"s1\"-->\n{text}")
}

fn typed_md(p0: &str, p1: &str) -> String {
    format!(
        "{TYPED_HEADER}\n\n{}\n\n{}\n",
        block("P0", p0),
        block("P1", p1)
    )
}

fn format_json(p0_tokens: Value, p1_tokens: Value) -> String {
    let mut out = serde_json::to_string_pretty(&json!({
        "schema": "docx2typed-format-1",
        "paragraphs": [
            { "id": "P0", "base": "s1", "token_ids": p0_tokens },
            { "id": "P1", "base": "s1", "token_ids": p1_tokens },
        ]
    }))
    .unwrap();
    out.push('\n');
    out
}

fn make_workdir(root: &Path, p0: &str, p1: &str, p0_tokens: Value) {
    fs::create_dir_all(root).unwrap();
    fs::write(root.join("typed.md"), typed_md(p0, p1)).unwrap();
    fs::write(root.join("format.json"), format_json(p0_tokens, json!([]))).unwrap();
    fs::write(root.join("styles.json"), "{}").unwrap();
    fs::write(root.join("_template.docx"), b"PK\x03\x04").unwrap();
}

/// A run that writes one canonical state into the generation.
fn state_run(p0: String, p1: String, p0_tokens: Value) -> MutateRun {
    Box::new(move |target: &Path, _tx: &mut Transaction| {
        fs::write(target.join("typed.md"), typed_md(&p0, &p1)).map_err(StoreError::Io)?;
        fs::write(
            target.join("format.json"),
            format_json(p0_tokens.clone(), json!([])),
        )
        .map_err(StoreError::Io)?;
        Ok(RunOutcome {
            outcome: "success".to_string(),
            data: json!({}),
            kind: "mutation".to_string(),
            payload: json!({"checks": []}),
            diagnostics: vec![],
        })
    })
}

fn commit(store: &Store, run: MutateRun) -> Value {
    let pin = store.pin().expect("pin");
    let operation_id = new_operation_id();
    let canonical = canonical_json(&json!({ "run": operation_id }));
    store
        .mutate(StoreMutateRequest {
            workdir: store.root.clone(),
            operation: "commit_sync".to_string(),
            operation_id: operation_id.clone(),
            canonical,
            input_sha256: pin.manifest_sha256.clone().unwrap_or_default(),
            expected_generation: pin.generation.clone(),
            generation: true,
            save_boundary: true,
            ledger_anchor: None,
            ledger_directory: true,
            evidence_path: None,
            kind: "mutation".to_string(),
            lock_timeout_ms: 0,
            run,
        })
        .expect("mutate")
}

/// V1 = (alpha, beta), V2 = (ALPHA, BETA). Returns the workdir root.
fn two_versions(tag: &str, p0_tokens: Value) -> ScratchGuard {
    let root = temp_dir(tag);
    make_workdir(&root, "alpha", "beta", p0_tokens.clone());
    let store = Store::init(&root, &new_operation_id(), "input").expect("init");
    commit(&store, state_run("alpha".into(), "beta".into(), p0_tokens));
    commit(&store, state_run("ALPHA".into(), "BETA".into(), json!([])));
    root
}

fn read_typed(root: &Path) -> String {
    let generation = read_root(root);
    fs::read_to_string(generation.join("typed.md")).unwrap()
}

#[test]
fn restore_copies_content_forward_into_a_new_version() {
    let root = two_versions("restore-whole", json!([]));
    let result = history_restore(&root, "V1", None).expect("restore");

    assert_eq!(result["restored_from"], "V1");
    assert_eq!(result["noop"], false);
    assert_eq!(result["cherry_picked"], Value::Null);
    assert_eq!(result["state"], "clean");

    // The document now holds V1's content again.
    assert_eq!(read_typed(&root), typed_md("alpha", "beta"));

    // A NEW version was appended; nothing rewound.
    let history = history_list(&root, 20, 0).expect("list");
    assert_eq!(history["versions"][0]["version"], "V3");
    assert_eq!(history["versions"][0]["restored_from"], "V1");
    assert_eq!(history["versions"][0]["origin"], "restore");
    assert_eq!(history["versions"][1]["version"], "V2");
    assert_eq!(history["versions"][2]["version"], "V1");
    assert!(result["changed_paragraph_ids"]
        .as_array()
        .unwrap()
        .contains(&json!("P0")));
}

#[test]
fn restore_of_a_clean_no_op_creates_no_version() {
    let root = two_versions("restore-noop", json!([]));
    // Restoring the version HEAD already names is a no-op.
    let result = history_restore(&root, "V2", None).expect("restore");
    assert_eq!(result["noop"], true);
    assert_eq!(history_list(&root, 20, 0).expect("list")["total"], 2);
}

#[test]
fn restore_unknown_version_is_version_not_found() {
    let root = two_versions("restore-unknown", json!([]));
    let error = history_restore(&root, "V99", None).expect_err("error");
    assert_eq!(error.code(), Some("version-not-found"));
}

#[test]
fn restore_trimmed_version_is_version_trimmed() {
    let root = two_versions("restore-trimmed", json!([]));
    history_gc(&root, 1, false).expect("gc");
    let error = history_restore(&root, "V1", None).expect_err("error");
    assert_eq!(error.code(), Some("version-trimmed"));
}

#[test]
fn selective_restore_refuses_coupled_paragraphs() {
    let root = two_versions("restore-coupled", json!([["rev-open", 3]]));
    let error = history_restore(&root, "V1", Some(&["P0".to_string()])).expect_err("error");
    assert_eq!(error.code(), Some("partial-restore-needs-dependent-state"));
}

#[test]
fn selective_restore_picks_a_dependency_free_paragraph() {
    let root = two_versions("restore-pick", json!([]));
    let result = history_restore(&root, "V1", Some(&["P0".to_string()])).expect("restore");

    assert_eq!(result["cherry_picked"], json!(["P0"]));
    let text = read_typed(&root);
    assert!(text.contains("alpha"), "P0 came from V1: {text}");
    assert!(text.contains("BETA"), "P1 kept the later change: {text}");
}

#[test]
fn selective_restore_of_an_empty_selection_is_refused() {
    let root = two_versions("restore-empty", json!([]));
    let error = history_restore(&root, "V1", Some(&[])).expect_err("error");
    assert_eq!(error.code(), Some("restore-empty-selection"));
}
