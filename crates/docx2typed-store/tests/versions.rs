//! Version-timeline reader tests (`history_list`): a workdir that has never
//! been saved, a Python-shaped commit chain in the object pool, HEAD
//! dirtiness, retention trims, the legacy generation-manifest fallback, and
//! the malformed-ledger error case.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use docx2typed_protocol::{bytes_sha256, canonical_json, new_operation_id};
use docx2typed_store::versions::{head_version, history_list, trimmed_versions};
use serde_json::{json, Value};

/// A workdir with the four canonical assets at the root (the materialized
/// mirror), and no store lane yet.
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

fn workdir(tag: &str) -> ScratchGuard {
    let dir = std::env::temp_dir().join(format!(
        "docx2typed-versions-{tag}-{}-{}",
        std::process::id(),
        new_operation_id().get(..8).unwrap_or("x")
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("typed.md"), "hello\n").unwrap();
    fs::write(dir.join("format.json"), "{}").unwrap();
    fs::write(dir.join("styles.json"), "{}").unwrap();
    fs::write(dir.join("_template.docx"), b"PK\x03\x04").unwrap();
    ScratchGuard(dir)
}

/// Store one object at the Python address (`sha256(kind + "\0" + bytes)`),
/// returning its digest.
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

/// Write the pointer (HEAD) with the version fields the timeline reads.
fn write_pointer(root: &Path, fields: &Value) {
    let mut pointer = json!({
        "schema": "docx2typed-workdir-pointer-1",
        "generation": "0000000000000000",
        "operation_id": "op",
        "manifest_sha256": "manifest",
        "written_at": "2026-01-01T00:00:00Z",
    });
    for (key, value) in fields.as_object().expect("pointer fields") {
        pointer[key] = value.clone();
    }
    fs::write(root.join("workdir.json"), canonical_json(&pointer)).unwrap();
}

fn commit(root: &Path, version: &str, seq: i64, parent: Option<&str>, tree: &str) -> String {
    put_object(
        root,
        "version",
        &json!({
            "schema": "docx2typed-version-1",
            "version": version,
            "seq": seq,
            "parent_version": if seq > 1 { Some(format!("V{}", seq - 1)) } else { None },
            "parent_generation": null,
            "parent_commit": parent,
            "head_tree": format!("tree-digest-{version}"),
            "tree_object": tree,
            "baseline_epoch": 1,
            "origin": "commit_sync",
            "label": null,
            "pin": false,
            "restored_from": null,
            "created_at": "2026-01-01T00:00:00Z",
            "generation": format!("gen-{seq}"),
        }),
    )
}

#[test]
fn a_workdir_that_was_never_saved_has_no_versions() {
    let root = workdir("fresh");
    let history = history_list(&root, 20, 0).expect("list");

    assert_eq!(history["schema"], "docx2typed-history-1");
    assert_eq!(history["total"], 0);
    assert_eq!(history["retained"], 0);
    assert_eq!(history["trimmed"], 0);
    assert_eq!(history["offset"], 0);
    assert_eq!(history["current"], Value::Null);
    assert_eq!(history["current_tree"], Value::Null);
    assert_eq!(history["versions"].as_array().map(Vec::len), Some(0));
    // No recorded tree == the document has never been saved.
    assert_eq!(history["version_dirty"], true);
    assert_eq!(head_version(&root)["version"], Value::Null);
}

#[test]
fn the_commit_chain_lists_newest_first_and_windows() {
    let root = workdir("chain");
    let tree_v1 = put_object(
        &root,
        "tree",
        &json!({ "parts": { "typed.md": { "whole": "a" } } }),
    );
    let tree_v2 = put_object(
        &root,
        "tree",
        &json!({ "parts": { "typed.md": { "whole": "b" } } }),
    );
    let commit_v1 = commit(&root, "V1", 1, None, &tree_v1);
    let commit_v2 = commit(&root, "V2", 2, Some(&commit_v1), &tree_v2);
    // The recorded tree is the reader's own digest of the working state, so
    // HEAD is clean (`version_dirty: false`) rather than drifting.
    let clean = head_version(&root)["canonical_tree"].clone();
    write_pointer(
        &root,
        &json!({
            "head_version": "V2",
            "head_version_generation": "gen-2",
            "head_commit": commit_v2,
            "head_tree": clean.clone(),
            "head_tree_object": tree_v2,
            "baseline_epoch": 1,
            "version_seq": 2,
        }),
    );

    let history = history_list(&root, 20, 0).expect("list");
    let versions = history["versions"].as_array().expect("versions");
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0]["version"], "V2");
    assert_eq!(versions[0]["commit"], json!(commit_v2));
    assert_eq!(versions[0]["content"], "retained");
    assert_eq!(versions[1]["version"], "V1");
    assert_eq!(versions[1]["parent_version"], Value::Null);
    assert_eq!(history["total"], 2);
    assert_eq!(history["retained"], 2);
    assert_eq!(history["trimmed"], 0);
    assert_eq!(history["current"], "V2");
    assert_eq!(history["current_tree"], clean);
    assert_eq!(history["version_dirty"], false);

    let page = history_list(&root, 1, 1).expect("page");
    assert_eq!(
        page["total"], 2,
        "counts describe the chain, not the window"
    );
    assert_eq!(page["offset"], 1);
    let window = page["versions"].as_array().expect("versions");
    assert_eq!(window.len(), 1);
    assert_eq!(window[0]["version"], "V1");

    let clamped = history_list(&root, 0, -5).expect("clamped");
    assert_eq!(clamped["offset"], 0, "offset clamps to 0");
    assert_eq!(
        clamped["versions"].as_array().map(Vec::len),
        Some(1),
        "limit floors at 1"
    );
}

#[test]
fn head_dirty_tracks_the_canonical_mirror() {
    let root = workdir("dirty");
    // The reader's own digest of the working state is the clean baseline.
    let clean = head_version(&root)["canonical_tree"].clone();
    write_pointer(
        &root,
        &json!({ "head_version": "V1", "head_version_generation": "gen-1", "head_tree": clean }),
    );
    assert_eq!(
        history_list(&root, 20, 0).expect("list")["version_dirty"],
        false
    );

    fs::write(root.join("typed.md"), "hello, edited by hand\n").unwrap();
    assert_eq!(
        history_list(&root, 20, 0).expect("list")["version_dirty"],
        true
    );
}

#[test]
fn retention_trims_and_legacy_generation_manifests_are_reported() {
    let root = workdir("legacy");
    let generation = root.join(".docx2typed-store/generations/gen-1");
    fs::create_dir_all(&generation).unwrap();
    fs::write(
        generation.join("generation.json"),
        canonical_json(&json!({
            "schema": "docx2typed-generation-manifest-1",
            "generation": "gen-1",
            "parent": null,
            "operation_id": "op",
            "input_sha256": "input",
            "assets": [],
            "assets_sha256": "assets",
            "created_at": "2026-01-01T00:00:00Z",
            "version": {
                "version": "V1",
                "seq": 1,
                "parent_version": null,
                "parent_generation": null,
                "parent_commit": null,
                "head_tree": "tree-digest-V1",
                "tree_object": null,
                "baseline_epoch": 1,
                "origin": "commit_sync",
                "label": "first save",
                "pin": true,
                "restored_from": null,
                "created_at": "2026-01-01T00:00:00Z",
            },
        })),
    )
    .unwrap();
    write_pointer(
        &root,
        &json!({
            "head_version": "V1",
            "head_version_generation": "gen-1",
            "head_tree": "tree-digest-V1",
            "version_seq": 1,
        }),
    );

    let history = history_list(&root, 20, 0).expect("list");
    let version = &history["versions"][0];
    assert_eq!(version["version"], "V1");
    assert_eq!(
        version["generation"], "gen-1",
        "the legacy chain names its generation"
    );
    assert_eq!(version["label"], "first save");
    assert_eq!(version["pin"], true);
    assert_eq!(version["content"], "retained");
    assert_eq!(history["total"], 1);

    fs::write(
        root.join(".docx2typed-store/history-trim.jsonl"),
        "{\"version\":\"V1\",\"tree_object\":null}\n",
    )
    .unwrap();
    let trimmed = history_list(&root, 20, 0).expect("list");
    assert_eq!(trimmed["versions"][0]["content"], "trimmed");
    assert_eq!(trimmed["trimmed"], 1);
    assert_eq!(trimmed["retained"], 0);
    assert_eq!(
        trimmed_versions(&root).expect("ledger"),
        BTreeSet::from(["V1".to_string()])
    );
}

#[test]
fn a_malformed_trim_ledger_is_store_invalid() {
    let root = workdir("ledger");
    fs::create_dir_all(root.join(".docx2typed-store")).unwrap();
    let ledger = root.join(".docx2typed-store/history-trim.jsonl");

    fs::write(&ledger, "{not json}\n").unwrap();
    let error = history_list(&root, 20, 0).expect_err("malformed ledger must refuse");
    assert_eq!(error.code(), Some("store-invalid"));
    assert!(error.message().contains("line 1"), "{}", error.message());

    fs::write(&ledger, "\n{\"tree_object\":\"x\"}\n").unwrap();
    let error = history_list(&root, 20, 0).expect_err("a record without a version must refuse");
    assert_eq!(error.code(), Some("store-invalid"));
    assert!(error.message().contains("line 2"), "{}", error.message());
}
