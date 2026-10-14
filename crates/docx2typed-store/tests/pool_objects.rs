//! Spec check 7 for the pool writer: a tree Rust wrote must be readable by the
//! reference's objectstore, and the digest must agree.

use std::fs;
use std::path::PathBuf;

use docx2typed_protocol::new_operation_id;

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

fn scratch(tag: &str) -> ScratchGuard {
    let dir = std::env::temp_dir().join(format!(
        "docx2typed-pool-{tag}-{}-{}",
        std::process::id(),
        new_operation_id().get(..8).unwrap_or("x")
    ));
    let _ = std::fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    ScratchGuard(dir)
}

fn state_dir(tag: &str) -> ScratchGuard {
    let dir = scratch(tag);
    fs::write(dir.join("typed.md"), "# typed\nP1\tfirst\n").unwrap();
    fs::write(dir.join("format.json"), "{\"P1\":{\"bold\":true}}\n").unwrap();
    fs::write(dir.join("styles.json"), "{\"s_1\":\"<w:rPr/>\"}\n").unwrap();
    fs::write(dir.join("_template.docx"), b"PK\x03\x04template-bytes").unwrap();
    fs::write(dir.join("revisions.json"), "[]\n").unwrap();
    dir
}

#[test]
fn build_tree_round_trips_through_this_writers_own_reader() {
    let root = scratch("roundtrip");
    let state = state_dir("source");
    let digest = docx2typed_store::canonical_tree_digest(&state).unwrap();
    let tree = docx2typed_store::build_tree(&root, &state, Some(&digest)).unwrap();
    let tree_id = tree["tree"].as_str().unwrap();

    assert!(docx2typed_store::objects::read_tree(&root, tree_id).is_some());
    for asset in docx2typed_store::objects::TREE_ASSETS {
        let blob = tree["parts"][asset]["whole"].as_str().unwrap();
        assert!(
            docx2typed_store::objects::read_object(&root, "blob", blob).is_some(),
            "{asset} blob missing"
        );
    }

    let destination = scratch("materialized");
    let written = docx2typed_store::materialize(&root, tree_id, &destination).unwrap();
    assert_eq!(written.len(), 5, "every tree asset materialises");
    for asset in docx2typed_store::objects::TREE_ASSETS {
        assert_eq!(
            fs::read(destination.join(asset)).unwrap(),
            fs::read(state.join(asset)).unwrap(),
            "{asset} round-tripped byte-for-byte"
        );
    }
}

#[test]
fn object_ids_are_kind_scoped_and_content_addressed() {
    let root = scratch("ids");
    let (blob, new) = docx2typed_store::objects::put(&root, "blob", b"same").unwrap();
    assert!(new, "first write is new");
    let (again, new_again) = docx2typed_store::objects::put(&root, "blob", b"same").unwrap();
    assert_eq!(blob, again);
    assert!(!new_again, "identical content is stored once");

    // Kind is part of the identity: the same bytes are a different object.
    let (tree, _) = docx2typed_store::objects::put(&root, "tree", b"same").unwrap();
    assert_ne!(blob, tree);
}

#[test]
fn the_digest_covers_canonical_assets_only() {
    let state = state_dir("digest");
    let before = docx2typed_store::canonical_tree_digest(&state).unwrap();
    // Evidence and derived views must never move the digest.
    fs::write(state.join("run.evidence.json"), "{}").unwrap();
    fs::write(state.join("edit.md"), "draft\n").unwrap();
    assert_eq!(
        docx2typed_store::canonical_tree_digest(&state).unwrap(),
        before,
        "evidence/draft are not canonical"
    );
    // revisions.json rides in the tree but is not canonical either.
    fs::write(state.join("revisions.json"), "[{\"x\":1}]\n").unwrap();
    assert_eq!(
        docx2typed_store::canonical_tree_digest(&state).unwrap(),
        before,
        "revisions.json is a tree asset, not a canonical one"
    );
    // A canonical asset does move it.
    fs::write(state.join("typed.md"), "# typed\nP1\tchanged\n").unwrap();
    assert_ne!(
        docx2typed_store::canonical_tree_digest(&state).unwrap(),
        before
    );
}

#[test]
fn a_commit_names_its_parent_so_the_chain_is_walkable() {
    let root = scratch("chain");
    let first = docx2typed_store::write_commit(
        &root,
        &serde_json::json!({ "version": "V1", "seq": 1, "parent_commit": serde_json::Value::Null }),
    )
    .unwrap();
    let second = docx2typed_store::write_commit(
        &root,
        &serde_json::json!({ "version": "V2", "seq": 2, "parent_commit": first }),
    )
    .unwrap();
    let commit = docx2typed_store::objects::read_commit(&root, &second).unwrap();
    assert_eq!(commit["schema"], "docx2typed-version-1");
    assert_eq!(commit["parent_commit"], first);
    let parent = docx2typed_store::objects::read_commit(&root, &first).unwrap();
    assert_eq!(parent["version"], "V1");
}

/// Emits the fixture the cross-language check reads: writes a pool with this
/// writer at a known path and records the tree id beside it. Run with
/// `--ignored` to refresh; the Python side is `scripts/check_cross_read.py`
/// mirrored in the CI qualification script.
#[test]
#[ignore]
fn emit_cross_read_fixture() {
    let root =
        PathBuf::from(std::env::var("POOL_FIXTURE").unwrap_or_else(|_| "C:/tmp/poolxread".into()));
    let _ = fs::remove_dir_all(&root);
    // STATE_DIR lets the cross-language check run against a REAL extracted
    // workdir rather than this hand-built fixture.
    let state = match std::env::var("STATE_DIR") {
        Ok(dir) => PathBuf::from(dir),
        Err(_) => root.join("state"),
    };
    if std::env::var("STATE_DIR").is_err() {
        fs::create_dir_all(&state).unwrap();
        fs::write(state.join("typed.md"), "# typed\nP1\tfirst\nP2\tsecond\n").unwrap();
        fs::write(state.join("format.json"), "{\"P1\":{\"bold\":true}}\n").unwrap();
        fs::write(state.join("styles.json"), "{\"s_1\":\"<w:rPr/>\"}\n").unwrap();
        fs::write(state.join("_template.docx"), b"PK\x03\x04template-bytes").unwrap();
        fs::write(state.join("revisions.json"), "[]\n").unwrap();
    }

    let digest = docx2typed_store::canonical_tree_digest(&state).unwrap();
    let tree = docx2typed_store::build_tree(&root, &state, Some(&digest)).unwrap();
    let tree_id = tree["tree"].as_str().unwrap();
    let commit = docx2typed_store::write_commit(
        &root,
        &serde_json::json!({
            "version": "V1", "seq": 1,
            "parent_commit": serde_json::Value::Null,
            "head_tree": digest,
            "tree_object": tree_id,
        }),
    )
    .unwrap();
    fs::write(
        root.join("fixture.json"),
        serde_json::json!({ "tree": tree_id, "commit": commit, "digest": digest }).to_string(),
    )
    .unwrap();
    println!(
        "fixture at {} tree={} commit={}",
        root.display(),
        tree_id,
        commit
    );
}
