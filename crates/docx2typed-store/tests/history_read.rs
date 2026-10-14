//! Paragraph-level history reads (`history_diff` / `history_blame` /
//! `history_verify`) against a Python-shaped object pool: a chunked tree, a
//! whole-file tree, a legacy generation-only version, retention trims, and the
//! stable error codes.

use std::fs;
use std::path::{Path, PathBuf};

use docx2typed_protocol::{bytes_sha256, canonical_json, new_operation_id};
use docx2typed_store::versions::{history_blame, history_diff, history_verify};
use serde_json::{json, Value};

const TYPED_HEADER: &str = "<!--@typed schema=\"docx2typed-typed-1\"-->";

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
        "docx2typed-history-read-{tag}-{}-{}",
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

/// Store one object at the Python address (`sha256(kind + "\0" + bytes)`).
fn put_bytes(root: &Path, kind: &str, bytes: &[u8]) -> String {
    let mut identity = kind.as_bytes().to_vec();
    identity.push(0);
    identity.extend_from_slice(bytes);
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

fn put_object(root: &Path, kind: &str, payload: &Value) -> String {
    put_bytes(root, kind, canonical_json(payload).as_bytes())
}

fn put_blob(root: &Path, text: &str) -> String {
    put_bytes(root, "blob", text.as_bytes())
}

/// Python `objectstore.put_map`: `{key -> blob id}` fanned out into buckets.
fn put_map(root: &Path, items: &[(&str, String)]) -> String {
    let mut sorted: Vec<(&str, &String)> = items.iter().map(|(key, value)| (*key, value)).collect();
    sorted.sort_by_key(|(key, _)| *key);
    let buckets: Vec<String> = sorted
        .chunks(64)
        .map(|chunk| {
            let entries = chunk
                .iter()
                .map(|(key, value)| (key.to_string(), json!(value)))
                .collect::<serde_json::Map<String, Value>>();
            put_object(
                root,
                "map",
                &json!({"schema": "docx2typed-bucket-1", "entries": entries}),
            )
        })
        .collect();
    put_object(
        root,
        "map",
        &json!({"schema": "docx2typed-map-1", "buckets": buckets}),
    )
}

/// One canonical paragraph block (`objectstore.split_typed`'s unit: no
/// trailing newline).
fn block(id: &str, text: &str) -> String {
    format!("<!--@p id=\"{id}\"-->\n{text}")
}

/// The canonical typed.md the splitter round-trips.
fn typed_md(paragraphs: &[(&str, &str)]) -> String {
    let blocks: Vec<String> = paragraphs
        .iter()
        .map(|(id, text)| block(id, text))
        .collect();
    format!("{TYPED_HEADER}\n\n{}\n", blocks.join("\n\n"))
}

/// A chunked `typed.md` part: the shape the pool splitter writes.
fn chunked_tree(root: &Path, paragraphs: &[(&str, &str)]) -> String {
    let order = put_object(
        root,
        "blob",
        &json!(paragraphs.iter().map(|(id, _)| *id).collect::<Vec<_>>()),
    );
    let chunks = put_map(
        root,
        &paragraphs
            .iter()
            .map(|(id, text)| (*id, put_blob(root, &block(id, text))))
            .collect::<Vec<_>>(),
    );
    put_object(
        root,
        "tree",
        &json!({
            "schema": "docx2typed-tree-1",
            "parts": {
                "typed.md": {
                    "header": put_blob(root, TYPED_HEADER),
                    "order": order,
                    "chunks": chunks,
                },
                "format.json": {"whole": put_blob(root, "{}")},
            },
        }),
    )
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
            "label": if version == "V1" { Value::Null } else { json!("label") },
            "pin": false,
            "restored_from": null,
            "created_at": "2026-01-01T00:00:00Z",
            "generation": format!("gen-{seq}"),
        }),
    )
}

fn write_pointer(root: &Path, head_commit: &str) {
    let pointer = json!({
        "schema": "docx2typed-workdir-pointer-1",
        "generation": "0000000000000000",
        "operation_id": "op",
        "manifest_sha256": "manifest",
        "written_at": "2026-01-01T00:00:00Z",
        "head_version": "V3",
        "head_version_generation": "gen-3",
        "head_commit": head_commit,
        "head_tree": "tree-digest-V3",
        "head_tree_object": "unused",
        "baseline_epoch": 1,
        "version_seq": 3,
    });
    fs::write(root.join("workdir.json"), canonical_json(&pointer)).unwrap();
}

/// A three-version chain: V1 {P1,P2,P3}, V2 {P1,P2(edited),P4}, V3 == V2.
struct Chain {
    tree_v1: String,
    tree_v2: String,
}

fn three_versions(root: &Path) -> Chain {
    let tree_v1 = chunked_tree(root, &[("P1", "one"), ("P2", "two"), ("P3", "three")]);
    let v1 = commit(root, "V1", 1, None, &tree_v1);
    let tree_v2 = chunked_tree(root, &[("P1", "one"), ("P2", "two edited"), ("P4", "four")]);
    let v2 = commit(root, "V2", 2, Some(&v1), &tree_v2);
    let tree_v3 = chunked_tree(root, &[("P1", "one"), ("P2", "two edited"), ("P4", "four")]);
    let v3 = commit(root, "V3", 3, Some(&v2), &tree_v3);
    write_pointer(root, &v3);
    Chain { tree_v1, tree_v2 }
}

fn write_trim_ledger(root: &Path, versions: &[&str]) {
    let store = root.join(".docx2typed-store");
    fs::create_dir_all(&store).unwrap();
    let body: String = versions
        .iter()
        .map(|version| {
            format!("{{\"version\":\"{version}\",\"trimmed_at\":\"2026-01-01T00:00:00Z\"}}\n")
        })
        .collect();
    fs::write(store.join("history-trim.jsonl"), body).unwrap();
}

fn remove_object(root: &Path, kind: &str, digest: &str) {
    fs::remove_file(
        root.join(".docx2typed-store/objects")
            .join(kind)
            .join(&digest[..2])
            .join(&digest[2..]),
    )
    .unwrap();
}

/// The `{key -> blob id}` entries of a chunked tree's `typed.md` part.
fn chunk_blobs(root: &Path, tree: &str, key: &str) -> String {
    let tree: Value = serde_json::from_slice(
        &fs::read(
            root.join(".docx2typed-store/objects/tree")
                .join(&tree[..2])
                .join(&tree[2..]),
        )
        .unwrap(),
    )
    .unwrap();
    let chunks = tree["parts"]["typed.md"]["chunks"].as_str().unwrap();
    let top: Value = serde_json::from_slice(
        &fs::read(
            root.join(".docx2typed-store/objects/map")
                .join(&chunks[..2])
                .join(&chunks[2..]),
        )
        .unwrap(),
    )
    .unwrap();
    let bucket = top["buckets"][0].as_str().unwrap();
    let bucket: Value = serde_json::from_slice(
        &fs::read(
            root.join(".docx2typed-store/objects/map")
                .join(&bucket[..2])
                .join(&bucket[2..]),
        )
        .unwrap(),
    )
    .unwrap();
    bucket["entries"][key].as_str().unwrap().to_string()
}

#[test]
fn diff_of_two_commits_reports_the_touched_paragraphs_and_previews() {
    let root = workdir("diff");
    three_versions(&root);

    let diff = history_diff(&root, "V3", None).expect("diff");
    assert_eq!(diff["schema"], "docx2typed-history-diff-1");
    assert_eq!(diff["version"], "V3");
    assert_eq!(diff["against"], "V2");
    assert_eq!(diff["label"], "label");
    assert_eq!(diff["origin"], "commit_sync");
    assert_eq!(diff["created_at"], "2026-01-01T00:00:00Z");
    assert_eq!(diff["added"], json!([]));
    assert_eq!(diff["changed"], json!([]));
    assert_eq!(diff["removed"], json!([]));
    assert_eq!(diff["unchanged"], 3);
    assert_eq!(diff["preview_limit"], 20);
    assert_eq!(diff["previews"], json!({}));

    // V2 is where the edit landed.
    let edit = history_diff(&root, "V2", None).expect("diff");
    assert_eq!(edit["against"], "V1");
    assert_eq!(edit["added"], json!(["P4"]));
    assert_eq!(edit["changed"], json!(["P2"]));
    assert_eq!(edit["removed"], json!(["P3"]));
    assert_eq!(edit["unchanged"], 1);
    assert_eq!(edit["previews"]["P4"]["before"], Value::Null);
    assert_eq!(edit["previews"]["P4"]["after"], "four");
    assert_eq!(edit["previews"]["P2"]["before"], "two");
    assert_eq!(edit["previews"]["P2"]["after"], "two edited");
    assert_eq!(edit["previews"]["P3"]["before"], "three");
    assert_eq!(edit["previews"]["P3"]["after"], Value::Null);
}

#[test]
fn diff_against_an_explicit_version_skips_the_intermediate_save() {
    let root = workdir("diff-against");
    three_versions(&root);

    let diff = history_diff(&root, "V3", Some("V1")).expect("diff");
    assert_eq!(diff["against"], "V1");
    assert_eq!(diff["added"], json!(["P4"]));
    assert_eq!(diff["changed"], json!(["P2"]));
    assert_eq!(diff["removed"], json!(["P3"]));
    assert_eq!(diff["unchanged"], 1);
}

#[test]
fn diff_compares_a_whole_file_tree_against_a_chunked_one() {
    let root = workdir("diff-shapes");
    // V1 keeps typed.md whole (a state the splitter could not round-trip).
    let whole = put_object(
        &root,
        "tree",
        &json!({
            "parts": {
                "typed.md": {
                    "whole": put_blob(
                        &root,
                        &typed_md(&[("P1", "one"), ("P2", "two")]),
                    ),
                },
            },
        }),
    );
    let v1 = commit(&root, "V1", 1, None, &whole);
    let v2 = commit(
        &root,
        "V2",
        2,
        Some(&v1),
        &chunked_tree(&root, &[("P1", "one"), ("P2", "two")]),
    );
    write_pointer(&root, &v2);

    let diff = history_diff(&root, "V2", None).expect("diff");
    assert_eq!(diff["added"], json!([]));
    assert_eq!(diff["changed"], json!([]));
    assert_eq!(diff["removed"], json!([]));
    assert_eq!(diff["unchanged"], 2);
}

#[test]
fn diff_reads_a_legacy_generation_version() {
    let root = workdir("diff-legacy");
    // V1 has no pooled tree: its content is the generation's typed.md.
    let generation = root.join(".docx2typed-store/generations/gen-1");
    fs::create_dir_all(&generation).unwrap();
    fs::write(generation.join("typed.md"), typed_md(&[("P1", "one")])).unwrap();
    let v1 = put_object(
        &root,
        "version",
        &json!({
            "schema": "docx2typed-version-1",
            "version": "V1",
            "seq": 1,
            "parent_commit": null,
            "tree_object": null,
            "generation": "gen-1",
            "created_at": "2026-01-01T00:00:00Z",
        }),
    );
    let v2 = commit(
        &root,
        "V2",
        2,
        Some(&v1),
        &chunked_tree(&root, &[("P1", "one"), ("P2", "two")]),
    );
    write_pointer(&root, &v2);

    let diff = history_diff(&root, "V2", None).expect("diff");
    assert_eq!(diff["added"], json!(["P2"]));
    assert_eq!(diff["unchanged"], 1);
}

#[test]
fn blame_finds_the_version_that_changed_a_paragraph() {
    let root = workdir("blame");
    three_versions(&root);

    let changed = history_blame(&root, "P2").expect("blame");
    assert_eq!(changed["schema"], "docx2typed-history-blame-1");
    assert_eq!(changed["state"], "modified");
    assert_eq!(changed["version"], "V2");
    assert_eq!(changed["text_preview"], "two edited");
    assert_eq!(changed["previous"]["version"], "V1");
    assert_eq!(changed["previous"]["text_preview"], "two");
    assert_eq!(changed["versions"], 3);

    // P1 never changed: blame lands on the version that added it.
    let added = history_blame(&root, "P1").expect("blame");
    assert_eq!(added["state"], "added");
    assert_eq!(added["version"], "V1");
    assert_eq!(added["text_preview"], "one");
    assert_eq!(added["previous"], Value::Null);
    assert_eq!(added["versions"], 3);
}

#[test]
fn blame_of_a_paragraph_absent_from_the_current_state_is_absent() {
    let root = workdir("blame-absent");
    three_versions(&root);

    let absent = history_blame(&root, "P9").expect("blame");
    assert_eq!(absent["state"], "absent");
    assert_eq!(absent["version"], Value::Null);
    assert_eq!(absent["versions"], 3);
}

#[test]
fn verify_reports_retained_and_deliberately_trimmed_versions() {
    let root = workdir("verify");
    let chain = three_versions(&root);
    // V1's content is dropped on purpose: the tree object is swept AND the
    // ledger records the trim. It must read as trimmed, never as damage.
    write_trim_ledger(&root, &["V1"]);
    remove_object(&root, "tree", &chain.tree_v1);

    let verify = history_verify(&root).expect("verify");
    assert_eq!(verify["schema"], "docx2typed-history-verify-1");
    assert_eq!(verify["ok"], true, "{verify}");
    let versions = verify["versions"].as_array().expect("versions");
    assert_eq!(versions.len(), 3);
    let trimmed = versions
        .iter()
        .find(|record| record["version"] == "V1")
        .expect("V1");
    assert_eq!(trimmed["content"], "trimmed");
    assert_eq!(trimmed["missing"], json!([]));
    for record in versions.iter().filter(|record| record["version"] != "V1") {
        assert_eq!(record["content"], "retained");
        assert_eq!(record["missing"], json!([]));
    }
}

#[test]
fn verify_reports_retention_trims_when_the_trim_ledger_only_has_them() {
    let root = workdir("verify-trim-only");
    let chain = three_versions(&root);
    // The generator's failure mode: the ledger says trimmed, the objects are
    // still on disk. Verify trusts the ledger, never the file listing.
    write_trim_ledger(&root, &["V1"]);
    assert!(root
        .join(".docx2typed-store/objects/tree")
        .join(&chain.tree_v1[..2])
        .join(&chain.tree_v1[2..])
        .is_file());

    let verify = history_verify(&root).expect("verify");
    assert_eq!(verify["ok"], true);
    let trimmed = verify["versions"]
        .as_array()
        .expect("versions")
        .iter()
        .find(|record| record["version"] == "V1")
        .expect("V1");
    assert_eq!(trimmed["content"], "trimmed");
}

#[test]
fn verify_names_missing_objects_and_legacy_generations() {
    let root = workdir("verify-missing");
    let paragraphs = [("P1", "one"), ("P2", "two")];
    let tree = chunked_tree(&root, &paragraphs);
    let p2_blob = chunk_blobs(&root, &tree, "P2");
    let v1 = commit(&root, "V1", 1, None, &tree);
    // V2: a legacy version with no pooled tree, generation present.
    let v2 = put_object(
        &root,
        "version",
        &json!({
            "schema": "docx2typed-version-1",
            "version": "V2",
            "seq": 2,
            "parent_commit": v1,
            "tree_object": null,
            "generation": "gen-2",
            "created_at": "2026-01-02T00:00:00Z",
        }),
    );
    fs::create_dir_all(root.join(".docx2typed-store/generations/gen-2")).unwrap();
    // V3: a legacy version whose generation was pruned.
    let v3 = put_object(
        &root,
        "version",
        &json!({
            "schema": "docx2typed-version-1",
            "version": "V3",
            "seq": 3,
            "parent_commit": v2,
            "tree_object": null,
            "generation": "gen-3",
            "created_at": "2026-01-03T00:00:00Z",
        }),
    );
    write_pointer(&root, &v3);

    let verify = history_verify(&root).expect("verify");
    // The pruned generation is the one damaged version; the other two are fine.
    assert_eq!(verify["ok"], false, "{verify}");
    let versions = verify["versions"].as_array().expect("versions");
    assert_eq!(versions[0]["version"], "V3");
    assert_eq!(versions[0]["content"], "missing");
    assert_eq!(versions[0]["missing"], json!(["generation"]));
    assert_eq!(versions[1]["version"], "V2");
    assert_eq!(versions[1]["content"], "retained");
    assert_eq!(versions[2]["version"], "V1");
    assert_eq!(versions[2]["content"], "retained");

    // Drop P2's chunk blob: the corruption is named per version, not silent.
    remove_object(&root, "blob", &p2_blob);
    let verify = history_verify(&root).expect("verify");
    assert_eq!(verify["ok"], false);
    let broken = verify["versions"]
        .as_array()
        .expect("versions")
        .iter()
        .find(|record| record["version"] == "V1")
        .expect("V1");
    assert_eq!(broken["content"], "missing");
    assert_eq!(broken["missing"], json!(["typed.md:P2"]));
}

#[test]
fn a_trimmed_version_diff_is_version_trimmed_not_corruption() {
    let root = workdir("diff-trimmed");
    let chain = three_versions(&root);
    write_trim_ledger(&root, &["V2"]);
    remove_object(&root, "tree", &chain.tree_v2);

    let error = history_diff(&root, "V2", None).expect_err("trimmed");
    assert_eq!(error.code(), Some("version-trimmed"));
    assert!(error.message().contains("trimmed by retention"), "{error}");
}

#[test]
fn an_unknown_version_is_version_not_found() {
    let root = workdir("unknown");
    three_versions(&root);

    let error = history_diff(&root, "V99", None).expect_err("unknown version");
    assert_eq!(error.code(), Some("version-not-found"));
    let error = history_diff(&root, "V3", Some("V99")).expect_err("unknown against");
    assert_eq!(error.code(), Some("version-not-found"));
}

#[test]
fn a_version_with_missing_content_is_version_content_missing() {
    let root = workdir("content-missing");
    let chain = three_versions(&root);
    // V2's tree is gone without a retention record: the objects are just lost.
    remove_object(&root, "tree", &chain.tree_v2);

    let error = history_diff(&root, "V2", None).expect_err("missing content");
    assert_eq!(error.code(), Some("version-content-missing"));
    assert!(!error.message().contains("trimmed"), "{error}");
}

#[test]
fn blame_without_versions_is_version_not_found() {
    let root = workdir("blame-empty");
    let error = history_blame(&root, "P1").expect_err("no versions");
    assert_eq!(error.code(), Some("version-not-found"));
}
