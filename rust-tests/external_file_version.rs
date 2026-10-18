//! External-output version guard: the workdir refuses to publish over a
//! target that changed outside the engine.
//!
//! Consumer-visible: actual output bytes and refusal codes, never store
//! wiring. No builds or lints here — the parent runs the full batch after the
//! patch lands.

use std::fs;
use std::path::{Path, PathBuf};

use docx2typed_protocol::file_sha256;
use docx2typed_store::store::{clear_faults, Store, StoreMutateRequest, Transaction};
use docx2typed_store::{MutateRun, RunOutcome, StoreError};

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("extver-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("scratch");
    dir
}

fn make_workdir(root: &Path) {
    // Real extract shape: the workdir lives NEXT TO its source
    // (`<dir>/source.docx` + `<dir>/source.docx2typed/`), never inside it.
    // `_template.docx` is byte-identical to the source at extract time, and
    // `format.json:source_path` is the relpath extract records
    // (`../source.docx`), resolved against the WORKDIR (root.join(rel)).
    let source = root.join("source.docx");
    fs::write(&source, b"PK\x03\x04source-A-package").unwrap();
    let workdir = root.join("source.docx2typed");
    fs::create_dir_all(&workdir).unwrap();
    fs::write(workdir.join("typed.md"), "hello\nworld\n").unwrap();
    fs::write(workdir.join("edit.md"), "# draft\n").unwrap();
    fs::copy(&source, workdir.join("_template.docx")).unwrap();
    let source_sha = docx2typed_protocol::file_sha256(&source).unwrap();
    let template_sha = docx2typed_protocol::file_sha256(&workdir.join("_template.docx")).unwrap();
    assert_eq!(source_sha, template_sha, "template must equal source bytes");
    fs::write(
        workdir.join("format.json"),
        format!(
            "{{\"schema\": \"typed-format-1\", \"source\": \"source.docx\", \
              \"source_path\": \"../source.docx\", \"source_sha256\": \"{source_sha}\", \
              \"template_sha256\": \"{template_sha}\"}}\n"
        ),
    )
    .unwrap();
    fs::write(workdir.join("styles.json"), "{}").unwrap();
    // Baseline epoch 1 so expectations are versioned.
    fs::write(
        workdir.join("baseline.json"),
        "{\"schema\": \"docx2typed-baseline-1\", \"epoch\": 1}\n",
    )
    .unwrap();
}

/// The store-backed workdir root (the `.docx2typed` dir), not the scratch dir.
fn workdir_of(root: &Path) -> PathBuf {
    root.join("source.docx2typed")
}

fn build_run(target: PathBuf, bytes: Vec<u8>) -> MutateRun {
    build_run_with(target, bytes, None)
}

/// `sabotage`, when set, runs INSIDE the mutation closure: after
/// `stage_external` captured the staging-time view, before the journaled
/// publish. This is the real during-build race — not a write before `mutate`.
fn build_run_with(target: PathBuf, bytes: Vec<u8>, sabotage: Option<Vec<u8>>) -> MutateRun {
    Box::new(move |_gen: &Path, tx: &mut Transaction| {
        let staged = tx.staging("out.docx");
        fs::write(&staged, &bytes).map_err(StoreError::Io)?;
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&staged)
            .map_err(StoreError::Io)?
            .sync_all()
            .map_err(StoreError::Io)?;
        tx.stage_external(&target, &staged, "replace")?;
        if let Some(other) = sabotage.clone() {
            // Word saves C over the target DURING the build, after staging
            // captured B. The publication re-check must refuse, discard the
            // staged B, and leave C untouched.
            fs::write(&target, &other).map_err(StoreError::Io)?;
        }
        Ok(RunOutcome {
            outcome: "success".to_string(),
            data: serde_json::json!({}),
            kind: "build".to_string(),
            payload: serde_json::json!({ "checks": [] }),
            diagnostics: vec![],
        })
    })
}

fn build_request(
    store: &Store,
    operation_id: &str,
    target: &Path,
    bytes: Vec<u8>,
) -> StoreMutateRequest {
    let pin = store.pin().expect("pin");
    StoreMutateRequest {
        workdir: store.root.clone(),
        operation: "build".to_string(),
        operation_id: operation_id.to_string(),
        canonical: format!("canonical-{operation_id}"),
        input_sha256: pin.manifest_sha256.clone().unwrap_or_default(),
        expected_generation: pin.generation,
        generation: false,
        save_boundary: false,
        ledger_anchor: Some(target.to_path_buf()),
        ledger_directory: false,
        evidence_path: Some(PathBuf::from(format!("{}.evidence.json", target.display()))),
        kind: "build".to_string(),
        lock_timeout_ms: 0,
        run: build_run(target.to_path_buf(), bytes),
    }
}

fn build_request_sabotage(
    store: &Store,
    operation_id: &str,
    target: &Path,
    bytes: Vec<u8>,
    sabotage: Vec<u8>,
) -> StoreMutateRequest {
    let pin = store.pin().expect("pin");
    StoreMutateRequest {
        workdir: store.root.clone(),
        operation: "build".to_string(),
        operation_id: operation_id.to_string(),
        canonical: format!("canonical-{operation_id}"),
        input_sha256: pin.manifest_sha256.clone().unwrap_or_default(),
        expected_generation: pin.generation,
        generation: false,
        save_boundary: false,
        ledger_anchor: Some(target.to_path_buf()),
        ledger_directory: false,
        evidence_path: Some(PathBuf::from(format!("{}.evidence.json", target.display()))),
        kind: "build".to_string(),
        lock_timeout_ms: 0,
        run: build_run_with(target.to_path_buf(), bytes, Some(sabotage)),
    }
}

/// Snapshot every asset under the workdir root that a refused publish must
/// leave untouched: all workdir files, all target-dir files, sidecars, and
/// the registry. Excluded: the Writer-lane lockfile (opened by lane
/// acquisition itself, before any guard decision runs), tx/staging scratch
/// (removed on abort), and the generation pointer (durable commit marker;
/// compared separately below). Excluding the lock is not excluding evidence:
/// the lock is the harness's own lane file, not a mutation product.
fn workdir_snapshot(root: &Path) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            // The lane lockfile is opened (and created) by acquire_lane
            // BEFORE the entry check runs: refusing work must still show a
            // lockfile. That is the harness's own lane, not a mutation.
            if rel.ends_with("/lock")
                || rel == ".docx2typed-store/lock"
                || rel.starts_with(".docx2typed-store/transactions/")
                || rel.starts_with(".docx2typed-store/staging/")
            {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                let bytes = std::fs::read(&path).unwrap_or_default();
                out.insert(rel, docx2typed_protocol::bytes_sha256(&bytes));
            }
        }
    }
    out
}

/// Snapshot helper that ALSO pins the generation pointer: refusal must not
/// advance it either.
fn workdir_snapshot_strict(
    store: &docx2typed_store::Store,
    root: &Path,
) -> (std::collections::BTreeMap<String, String>, String) {
    let gen = store.pin().map(|p| p.generation).unwrap_or_default();
    (workdir_snapshot(root), gen)
}

fn code_of(error: &StoreError) -> Option<&str> {
    error.code()
}

/// B -> B repeat: the second publication over the workdir's own bytes succeeds
/// and the output still holds B.
#[test]
fn same_workdir_repeat_publish_over_own_bytes_succeeds() {
    clear_faults();
    let root = scratch("repeat");
    make_workdir(&root);
    let wd = workdir_of(&root);
    let store = Store::init(&wd, &docx2typed_protocol::new_operation_id(), "input").unwrap();
    let target = root.join("out.docx");
    store
        .mutate(build_request(
            &store,
            &docx2typed_protocol::new_operation_id(),
            &target,
            b"B".to_vec(),
        ))
        .expect("first publish");
    assert_eq!(fs::read(&target).unwrap(), b"B");
    store
        .mutate(build_request(
            &store,
            &docx2typed_protocol::new_operation_id(),
            &target,
            b"B2".to_vec(),
        ))
        .expect("repeat publish over own bytes");
    assert_eq!(fs::read(&target).unwrap(), b"B2");
    let _ = fs::remove_dir_all(&root);
}

/// Unknown existing target refuses before any workdir mutation: the store
/// tree is unchanged and the target keeps its bytes.
#[test]
fn unknown_existing_target_refuses_without_mutation() {
    clear_faults();
    let root = scratch("unknown");
    make_workdir(&root);
    let wd = workdir_of(&root);
    let store = Store::init(&wd, &docx2typed_protocol::new_operation_id(), "input").unwrap();
    let target = root.join("C.docx");
    fs::write(&target, b"C-by-someone-else").unwrap();
    // Full snapshot: the entry check must refuse BEFORE any journal dir,
    // staging, snapshot, or registry write. The lane lockfile is excluded by
    // workdir_snapshot by design (opened by lane acquisition itself); the
    // pointer generation is pinned and compared separately.
    let (before, gen_before) = workdir_snapshot_strict(&store, &wd);
    let error = store
        .mutate(build_request(
            &store,
            &docx2typed_protocol::new_operation_id(),
            &target,
            b"B".to_vec(),
        ))
        .expect_err("unknown existing must refuse");
    assert_eq!(code_of(&error), Some("external-file-changed"), "{error}");
    assert_eq!(fs::read(&target).unwrap(), b"C-by-someone-else");
    let (after, gen_after) = workdir_snapshot_strict(&store, &wd);
    assert_eq!(after, before, "initial refusal mutated workdir files");
    assert_eq!(gen_after, gen_before, "initial refusal moved the pointer");
    let _ = fs::remove_dir_all(&root);
}

/// Late external modify between build staging and publication refuses: the
/// target keeps C, the staged B is discarded, the registry still expects B's
/// predecessor.
#[test]
fn late_external_modify_during_build_refuses() {
    clear_faults();
    let root = scratch("late");
    make_workdir(&root);
    let wd = workdir_of(&root);
    let store = Store::init(&wd, &docx2typed_protocol::new_operation_id(), "input").unwrap();
    let target = root.join("out.docx");
    store
        .mutate(build_request(
            &store,
            &docx2typed_protocol::new_operation_id(),
            &target,
            b"B".to_vec(),
        ))
        .expect("baseline publish");
    let expected_b = file_sha256(&target).unwrap();
    let (files_before, gen_before) = workdir_snapshot_strict(&store, &wd);
    // Word saves C DURING the build: after stage_external captured the
    // staging-time view of B, before the journaled publish. The publication
    // re-check catches it — this is the gate the entry check alone cannot.
    let error = store
        .mutate(build_request_sabotage(
            &store,
            &docx2typed_protocol::new_operation_id(),
            &target,
            b"B2".to_vec(),
            b"C-word-edit".to_vec(),
        ))
        .expect_err("during-build modify must refuse");
    assert_eq!(code_of(&error), Some("external-file-changed"), "{error}");
    assert_eq!(fs::read(&target).unwrap(), b"C-word-edit");
    // Full-asset no-write on the final race: the sabotage wrote the target
    // itself (that is the race under test); no workdir file, no sidecar, no
    // registry entry, no pointer move may accompany the refusal.
    let (files_after, gen_after) = workdir_snapshot_strict(&store, &wd);
    assert_eq!(
        files_after, files_before,
        "refused publish wrote workdir files"
    );
    assert_eq!(gen_after, gen_before, "refused publish moved the pointer");
    // Registry still expects B, not C: a later save must still refuse.
    let registry = docx2typed_store::read_external_targets(&wd);
    let key = docx2typed_protocol::resolve_path(&target)
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        registry
            .get(&key)
            .and_then(|e| e.expected_sha256.as_deref()),
        Some(expected_b.as_str()),
        "refusal must not advance the expectation"
    );
    let _ = fs::remove_dir_all(&root);
}

/// Absent target observed absent can be created; an appearance before publish
/// refuses.
#[test]
fn absent_target_created_once_then_guarded() {
    clear_faults();
    let root = scratch("absent");
    make_workdir(&root);
    let wd = workdir_of(&root);
    let store = Store::init(&wd, &docx2typed_protocol::new_operation_id(), "input").unwrap();
    let target = root.join("new.docx");
    assert!(!target.exists());
    store
        .mutate(build_request(
            &store,
            &docx2typed_protocol::new_operation_id(),
            &target,
            b"N".to_vec(),
        ))
        .expect("absent target may be created");
    assert_eq!(fs::read(&target).unwrap(), b"N");
    // Now the path is known: overwriting via the same workdir succeeds (own
    // bytes), while externally changed bytes refuse.
    store
        .mutate(build_request(
            &store,
            &docx2typed_protocol::new_operation_id(),
            &target,
            b"N2".to_vec(),
        ))
        .expect("own bytes repeat");
    // External creator appears DURING the build, after observed-absent was
    // recorded: the no-clobber create must refuse without touching the target.
    // All-asset proof: no workdir file, no sidecar, no registry entry moves.
    let clash = root.join("clash.docx");
    assert!(!clash.exists());
    let (files_before, gen_before) = workdir_snapshot_strict(&store, &wd);
    let error = store
        .mutate(build_request_sabotage(
            &store,
            &docx2typed_protocol::new_operation_id(),
            &clash,
            b"mine".to_vec(),
            b"theirs".to_vec(),
        ))
        .expect_err("during-build appearance must refuse");
    assert_eq!(code_of(&error), Some("external-file-changed"), "{error}");
    assert_eq!(fs::read(&clash).unwrap(), b"theirs");
    let (files_after, gen_after) = workdir_snapshot_strict(&store, &wd);
    assert_eq!(
        files_after, files_before,
        "refused create wrote workdir files"
    );
    assert_eq!(gen_after, gen_before, "refused create moved the pointer");
    // And a plain external change to a known path still refuses at entry.
    fs::write(&target, b"external").unwrap();
    let error = store
        .mutate(build_request(
            &store,
            &docx2typed_protocol::new_operation_id(),
            &target,
            b"N3".to_vec(),
        ))
        .expect_err("external change must refuse");
    assert_eq!(code_of(&error), Some("external-file-changed"), "{error}");
    assert_eq!(fs::read(&target).unwrap(), b"external");
    let _ = fs::remove_dir_all(&root);
}
/// Source-baseline re-extract epoch: target IS the workdir's canonical
/// source and still holds the baseline bytes ⇒ writable even with an empty
/// registry. After a simulated re-extract (template := C, epoch 2), saving
/// over C succeeds and the registry follows B2, not the overwritten C.
#[test]
fn source_baseline_reextract_allows_overwrite_then_registry_wins() {
    clear_faults();
    let root = scratch("sourcere");
    make_workdir(&root);
    let wd = workdir_of(&root);
    let store = Store::init(&wd, &docx2typed_protocol::new_operation_id(), "input").unwrap();
    let source = root.join("source.docx");
    // No registry yet: saving over the canonical source holding baseline
    // bytes is allowed (initial-external-file-changed acceptance case).
    store
        .mutate(build_request(
            &store,
            &docx2typed_protocol::new_operation_id(),
            &source,
            b"B1".to_vec(),
        ))
        .expect("source baseline overwrite allowed");
    assert_eq!(fs::read(&source).unwrap(), b"B1");
    // Simulate explicit re-extract of C: source := C bytes, template := C
    // bytes (extract stores the source package verbatim), epoch 1 -> 2.
    // The baseline fallback (no current-epoch entry) allows the source that
    // still holds the baseline bytes.
    fs::write(&source, b"C-word-package").unwrap();
    fs::copy(&source, wd.join("_template.docx")).unwrap();
    fs::write(
        wd.join("baseline.json"),
        "{\"schema\": \"docx2typed-baseline-1\", \"epoch\": 2}\n",
    )
    .unwrap();
    store
        .mutate(build_request(
            &store,
            &docx2typed_protocol::new_operation_id(),
            &source,
            b"B2".to_vec(),
        ))
        .expect("re-baselined source overwrite allowed");
    assert_eq!(fs::read(&source).unwrap(), b"B2");
    // Registry wins over baseline: an external revert to the OLD baseline
    // bytes (A) still refuses once B2 is confirmed — the confirmed version
    // beats the reverted baseline.
    let baseline_a = docx2typed_protocol::file_sha256(&wd.join("_template.docx")).unwrap();
    let _ = baseline_a;
    fs::write(&source, b"PK\x03\x04source-A-package").unwrap();
    let error = store
        .mutate(build_request(
            &store,
            &docx2typed_protocol::new_operation_id(),
            &source,
            b"B3".to_vec(),
        ))
        .expect_err("reverted baseline must refuse once B is confirmed");
    assert_eq!(code_of(&error), Some("external-file-changed"), "{error}");
    assert_eq!(fs::read(&source).unwrap(), b"PK\x03\x04source-A-package");
    // And a fresh external C refuses too.
    fs::write(&source, b"C-again").unwrap();
    let error = store
        .mutate(build_request(
            &store,
            &docx2typed_protocol::new_operation_id(),
            &source,
            b"B3".to_vec(),
        ))
        .expect_err("stale source must refuse after registry advanced");
    assert_eq!(code_of(&error), Some("external-file-changed"), "{error}");
    assert_eq!(fs::read(&source).unwrap(), b"C-again");
    let _ = fs::remove_dir_all(&root);
}

/// Same-workdir B -> next: after B1 publishes, a normal second save over
/// the same path succeeds and advances the registry to B2.
#[test]
fn same_workdir_second_save_advances_registry() {
    clear_faults();
    let root = scratch("samewd");
    make_workdir(&root);
    let wd = workdir_of(&root);
    let store = Store::init(&wd, &docx2typed_protocol::new_operation_id(), "input").unwrap();
    let target = root.join("out.docx");
    store
        .mutate(build_request(
            &store,
            &docx2typed_protocol::new_operation_id(),
            &target,
            b"B1".to_vec(),
        ))
        .expect("first publish");
    let key = docx2typed_protocol::resolve_path(&target)
        .to_string_lossy()
        .into_owned();
    let b1 = file_sha256(&target).unwrap();
    assert_eq!(
        docx2typed_store::read_external_targets(&wd)
            .get(&key)
            .and_then(|e| e.expected_sha256.as_deref()),
        Some(b1.as_str()),
    );
    store
        .mutate(build_request(
            &store,
            &docx2typed_protocol::new_operation_id(),
            &target,
            b"B2".to_vec(),
        ))
        .expect("second save over own bytes");
    assert_eq!(fs::read(&target).unwrap(), b"B2");
    let b2 = file_sha256(&target).unwrap();
    assert_ne!(b1, b2);
    assert_eq!(
        docx2typed_store::read_external_targets(&wd)
            .get(&key)
            .and_then(|e| e.expected_sha256.as_deref()),
        Some(b2.as_str()),
        "success must advance the registry to the new bytes"
    );
    let _ = fs::remove_dir_all(&root);
}

/// Symlink/alias: the canonical resolved path is the registry key — writing
/// through an alias still matches the expectation recorded under the
/// canonical path.
#[test]
#[cfg(unix)]
fn symlink_alias_shares_the_canonical_expectation() {
    use std::os::unix::fs::symlink;
    clear_faults();
    let root = scratch("alias");
    make_workdir(&root);
    let wd = workdir_of(&root);
    let store = Store::init(&wd, &docx2typed_protocol::new_operation_id(), "input").unwrap();
    let target = root.join("out.docx");
    store
        .mutate(build_request(
            &store,
            &docx2typed_protocol::new_operation_id(),
            &target,
            b"B".to_vec(),
        ))
        .expect("baseline publish");
    let alias = root.join("alias.docx");
    symlink(&target, &alias).unwrap();
    // The alias resolves to the same canonical path: publishing through the
    // alias sees the same expected bytes.
    store
        .mutate(build_request(
            &store,
            &docx2typed_protocol::new_operation_id(),
            &alias,
            b"B2".to_vec(),
        ))
        .expect("alias publish over own bytes");
    assert_eq!(fs::read(&target).unwrap(), b"B2");
    let _ = fs::remove_dir_all(&root);
}
