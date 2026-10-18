//! Focused issue #57 tests: generation Store semantics, Writer lane
//! outcomes, durability ordering, Operation-ID exactly-once, deterministic
//! fault cuts (kill before/after every journal/pointer/external cut,
//! ENOSPC, short write, flush failure, corruption, CAS race), startup
//! recovery, and filesystem qualification — mirroring the frozen semantics
//! of `tests/test_store_recovery.py`.
//!
//! The fault map is process-global, so every test takes the suite lock:
//! fault cuts are deterministic and never leak across tests.

use std::fs;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use docx2typed_protocol::{canonical_json, new_operation_id};
use docx2typed_store::store::{
    clear_faults, kill_at, set_fault, Fault, KillPanic, Store, StoreMutateRequest, Transaction,
    GENERATION_CONFLICT, NEEDS_RECOVERY, OPERATION_ID_REUSED, RESERVE_BYTES, RESERVE_DEPLETED,
    STORE_DIR_NAME, UNSUPPORTED_BY_DESIGN,
};
use docx2typed_store::{MutateRun, RunOutcome, StoreError};
use serde_json::{json, Value};

static SUITE_LOCK: Mutex<()> = Mutex::new(());

/// Tests intentionally panic (simulated process death) while holding the
/// suite lock, so poisoning is expected and benign.
fn suite_lock() -> std::sync::MutexGuard<'static, ()> {
    SUITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn op() -> String {
    new_operation_id()
}

/// Minimal real workdir (extract-shaped enough for the store: the store
/// itself does not care about Word semantics).
fn make_workdir(root: &Path) {
    fs::create_dir_all(root).unwrap();
    fs::write(root.join("typed.md"), "hello\nworld\n").unwrap();
    fs::write(root.join("edit.md"), "# draft\n").unwrap();
    fs::write(root.join("format.json"), "{}").unwrap();
    fs::write(root.join("styles.json"), "{}").unwrap();
    fs::write(root.join("_template.docx"), b"PK\x03\x04not-a-real-docx").unwrap();
}

fn init_store(root: &Path) -> Store {
    make_workdir(root);
    Store::init(root, &op(), "input-sha").expect("init")
}

fn simple_run(text: &str) -> MutateRun {
    let text = text.to_string();
    Box::new(move |gen_dir: &Path, _tx: &mut Transaction| {
        fs::write(gen_dir.join("typed.md"), &text).map_err(StoreError::Io)?;
        fs::write(gen_dir.join("note.txt"), "note\n").map_err(StoreError::Io)?;
        Ok(RunOutcome {
            outcome: "success".to_string(),
            data: json!({ "changed": ["P0"] }),
            kind: "mutation".to_string(),
            payload: json!({ "checks": [{ "name": "mutated", "status": "pass" }] }),
            diagnostics: vec![],
        })
    })
}

fn mutate(store: &Store, operation_id: &str) -> Result<Value, StoreError> {
    mutate_as(store, operation_id, false)
}

/// `save_boundary` is what lets a commit record a Version (#37).
fn mutate_as(store: &Store, operation_id: &str, save_boundary: bool) -> Result<Value, StoreError> {
    let pin = store.pin().expect("pin");
    store.mutate(StoreMutateRequest {
        workdir: store.root.clone(),
        operation: "edit".to_string(),
        operation_id: operation_id.to_string(),
        canonical: format!("canonical-{operation_id}"),
        input_sha256: pin.manifest_sha256.clone().unwrap_or_default(),
        expected_generation: pin.generation,
        generation: true,
        save_boundary,
        ledger_anchor: None,
        ledger_directory: true,
        evidence_path: None,
        kind: "mutation".to_string(),
        lock_timeout_ms: 0,
        run: simple_run("hello\nworld\n"),
    })
}

fn old_content(store: &Store) -> String {
    fs::read_to_string(store.pin().expect("pin").path.join("typed.md")).expect("read typed.md")
}

fn assert_kill(result: std::thread::Result<Result<Value, StoreError>>) {
    match result {
        Ok(_) => panic!("expected simulated kill"),
        Err(payload) => {
            let _ = payload
                .downcast_ref::<KillPanic>()
                .expect("KillPanic payload");
        }
    }
}

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
        "docx2typed-store-{tag}-{}-{}",
        std::process::id(),
        new_operation_id().get(..8).unwrap_or("x")
    ));
    let _ = std::fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    ScratchGuard(dir)
}

// ---------------------------------------------------------------------------
// Bullet 1: readers pin one generation; writer contention/CAS use frozen
// Diagnostics and bounded wait semantics.
// ---------------------------------------------------------------------------

#[test]
fn reader_pins_one_immutable_generation() {
    let _suite = suite_lock();
    let root = scratch("pin");
    let store = init_store(&root);
    let pinned = store.pin().expect("pin");
    let pinned_path = pinned.path.clone();
    let typed_before = fs::read(pinned_path.join("typed.md")).unwrap();
    // A writer commits the next generation.
    mutate(&store, &op()).expect("mutate");
    // The pinned generation directory is immutable: still the old content.
    assert_eq!(
        fs::read(pinned_path.join("typed.md")).unwrap(),
        typed_before
    );
    // New readers pin the committed generation.
    let fresh = Store::open(&root).expect("open").pin().expect("pin");
    assert_ne!(fresh.generation, pinned.generation);
    assert_eq!(
        fs::read_to_string(fresh.path.join("typed.md")).unwrap(),
        "hello\nworld\n"
    );
    assert_eq!(docx2typed_store::read_root(&root), fresh.path);
}

#[test]
fn writer_busy_and_bounded_timeout_use_frozen_codes() {
    let _suite = suite_lock();
    let root = scratch("busy");
    let store = init_store(&root);
    // Thread A holds the Writer lane.
    let holder_store = Store::open(&root).unwrap();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        let _guard = holder_store.writer(0).expect("holder acquires lane");
        ready_tx.send(()).unwrap();
        let _ = release_rx.recv();
    });
    ready_rx.recv().unwrap();
    // Immediate contention -> writer-busy.
    let error = mutate(&store, &op()).expect_err("expected writer-busy");
    assert_eq!(error.code(), Some("writer-busy"));
    // Bounded wait that expires -> writer-timeout.
    let pin = store.pin().unwrap();
    let started = std::time::Instant::now();
    let request = StoreMutateRequest {
        workdir: store.root.clone(),
        operation: "edit".to_string(),
        operation_id: op(),
        canonical: "c".to_string(),
        input_sha256: pin.manifest_sha256.clone().unwrap_or_default(),
        expected_generation: pin.generation,
        generation: true,
        save_boundary: false,
        ledger_anchor: None,
        ledger_directory: true,
        evidence_path: None,
        kind: "mutation".to_string(),
        lock_timeout_ms: 300,
        run: simple_run("x"),
    };
    let timeout_error = store.mutate(request).expect_err("expected writer-timeout");
    assert_eq!(timeout_error.code(), Some("writer-timeout"));
    assert!(started.elapsed().as_secs() < 10);
    release_tx.send(()).unwrap();
    holder.join().unwrap();
}

#[test]
fn lock_holder_death_releases_lane() {
    let _suite = suite_lock();
    let root = scratch("death");
    let store = init_store(&root);
    // Dropping the guard is exactly what process death does: the OS
    // advisory lock lives on the open file description.
    {
        let _guard = Store::open(&root).unwrap().writer(0).expect("lane");
        let error = mutate(&store, &op()).expect_err("expected writer-busy");
        assert_eq!(error.code(), Some("writer-busy"));
    }
    mutate(&store, &op()).expect("mutate after release");
}

#[test]
fn stale_writer_generation_conflict() {
    let _suite = suite_lock();
    let root = scratch("cas");
    let first = init_store(&root);
    let second = Store::open(&root).unwrap();
    let planned = first.pin().unwrap();
    // A concurrent writer commits N+1.
    mutate(&second, &op()).expect("second commits");
    // The stale planner now commits with expected=N -> generation-conflict.
    let error = first
        .mutate(StoreMutateRequest {
            workdir: first.root.clone(),
            operation: "edit".to_string(),
            operation_id: op(),
            canonical: "stale".to_string(),
            input_sha256: planned.manifest_sha256.clone().unwrap_or_default(),
            expected_generation: planned.generation,
            generation: true,
            save_boundary: false,
            ledger_anchor: None,
            ledger_directory: true,
            evidence_path: None,
            kind: "mutation".to_string(),
            lock_timeout_ms: 0,
            run: simple_run("stale"),
        })
        .expect_err("expected generation-conflict");
    assert_eq!(error.code(), Some(GENERATION_CONFLICT));
    // The committed generation is untouched.
    assert_eq!(
        fs::read_to_string(second.pin().unwrap().path.join("typed.md")).unwrap(),
        "hello\nworld\n"
    );
}

#[test]
fn cas_race_yields_one_winner() {
    let _suite = suite_lock();
    let root = scratch("race");
    init_store(&root);
    let store_a = Store::open(&root).unwrap();
    let store_b = Store::open(&root).unwrap();
    // Both writers planned against the same generation before racing.
    let planned = Store::open(&root).unwrap().pin().unwrap();
    let planned_gen = planned.generation.clone();
    let planned_manifest = planned.manifest_sha256.clone().unwrap_or_default();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let mut handles = Vec::new();
    for store in [store_a, store_b] {
        let barrier = barrier.clone();
        let planned_gen = planned_gen.clone();
        let planned_manifest = planned_manifest.clone();
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            let operation_id = op();
            store.mutate(StoreMutateRequest {
                workdir: store.root.clone(),
                operation: "edit".to_string(),
                operation_id: operation_id.clone(),
                canonical: format!("canonical-{operation_id}"),
                input_sha256: planned_manifest,
                expected_generation: planned_gen,
                generation: true,
                save_boundary: false,
                ledger_anchor: None,
                ledger_directory: true,
                evidence_path: None,
                kind: "mutation".to_string(),
                lock_timeout_ms: 5_000,
                run: simple_run("hello\nworld\n"),
            })
        }));
    }
    let mut outcomes = Vec::new();
    for handle in handles {
        outcomes.push(handle.join().unwrap());
    }
    for outcome in &outcomes {
        if let Err(error) = outcome {
            eprintln!("cas race loser: {error:?}");
        }
    }
    let winners = outcomes.iter().filter(|outcome| outcome.is_ok()).count();
    let conflicts = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, Err(e) if e.code() == Some(GENERATION_CONFLICT)))
        .count();
    // Exactly one winner; the loser fails closed with generation-conflict.
    assert_eq!(winners, 1, "outcomes: {outcomes:?}");
    assert_eq!(conflicts, 1);
}

// ---------------------------------------------------------------------------
// Bullet 2: success reports only after all durability and Evidence
// conditions hold.
// ---------------------------------------------------------------------------

#[test]
fn success_durability_includes_all_required_state() {
    let _suite = suite_lock();
    let root = scratch("durable");
    let store = init_store(&root);
    let before = store.pin().unwrap();
    let operation_id = op();
    let envelope = mutate(&store, &operation_id).expect("mutate");
    assert_eq!(envelope["outcome"], "success");
    let after = store.pin().unwrap();
    assert_ne!(after.generation, before.generation);
    let gen_dir = after.path.clone();
    // Generation assets are complete and authoritative.
    assert_eq!(
        fs::read_to_string(gen_dir.join("typed.md")).unwrap(),
        "hello\nworld\n"
    );
    assert!(gen_dir.join("note.txt").is_file());
    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(gen_dir.join("generation.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["parent"], before.generation);
    assert_eq!(manifest["operation_id"], operation_id);
    // Evidence and ledger are durable inside the committed generation.
    assert!(gen_dir.join("run.evidence.json").is_file());
    let evidence: Value =
        serde_json::from_str(&fs::read_to_string(gen_dir.join("run.evidence.json")).unwrap())
            .unwrap();
    assert_eq!(evidence["operation_id"], operation_id);
    let ledger: Value =
        serde_json::from_str(&fs::read_to_string(gen_dir.join("operation-ledger.json")).unwrap())
            .unwrap();
    assert!(ledger["records"][&operation_id].is_object());
    assert_eq!(
        ledger["records"][&operation_id]["envelope"]["outcome"],
        "success"
    );
    // Pointer selects the new generation with the manifest hash.
    let pointer: Value =
        serde_json::from_str(&fs::read_to_string(root.join("workdir.json")).unwrap()).unwrap();
    assert_eq!(pointer["generation"], after.generation);
    assert_eq!(pointer["manifest_sha256"], manifest["assets_sha256"]);
    // The root is the materialized mirror of the committed generation.
    assert_eq!(
        fs::read_to_string(root.join("typed.md")).unwrap(),
        "hello\nworld\n"
    );
    // Recovery finds nothing to do and no journals linger.
    let summary = Store::open(&root).unwrap().recover(true).unwrap();
    assert!(summary.needs_recovery.is_empty());
    assert!(summary.rolled_back.is_empty());
    let tx_dir = root.join(STORE_DIR_NAME).join("transactions");
    assert!(!tx_dir.exists() || fs::read_dir(&tx_dir).unwrap().next().is_none());
}

#[test]
fn replay_same_operation_id_no_second_effect() {
    let _suite = suite_lock();
    let root = scratch("replay");
    let store = init_store(&root);
    let operation_id = op();
    let envelope1 = mutate(&store, &operation_id).expect("first mutate");
    let generation1 = store.pin().unwrap().generation;
    // Same op-id + identical canonical input: replay the original envelope.
    let envelope2 = mutate(&store, &operation_id).expect("replay");
    assert_eq!(envelope1, envelope2);
    assert_eq!(store.pin().unwrap().generation, generation1);
    // Changed canonical input with the same op-id: rejected, no effect.
    let pin = store.pin().unwrap();
    let error = store
        .mutate(StoreMutateRequest {
            workdir: store.root.clone(),
            operation: "edit".to_string(),
            operation_id: operation_id.clone(),
            canonical: "changed-input".to_string(),
            input_sha256: pin.manifest_sha256.clone().unwrap_or_default(),
            expected_generation: pin.generation,
            generation: true,
            save_boundary: false,
            ledger_anchor: None,
            ledger_directory: true,
            evidence_path: None,
            kind: "mutation".to_string(),
            lock_timeout_ms: 0,
            run: simple_run("changed"),
        })
        .expect_err("expected operation-id-reused");
    assert_eq!(error.code(), Some(OPERATION_ID_REUSED));
    assert_eq!(store.pin().unwrap().generation, generation1);
}

// ---------------------------------------------------------------------------
// Bullet 3: deterministic fault cuts yield only old / new / needs-recovery.
// ---------------------------------------------------------------------------

/// Kill before the pointer commit: recovery rolls back to the complete old
/// state.
#[test]
fn kill_before_pointer_commit_leaves_old() {
    let _suite = suite_lock();
    for cut in [
        "journal-write-prepared",
        "journal-flush-prepared",
        "journal-rename-prepared",
        "generation-copy",
        "pointer-write",
        "pointer-flush",
        "pointer-rename",
    ] {
        clear_faults();
        let root = scratch("old");
        let store = init_store(&root);
        let old_gen = store.pin().unwrap().generation;
        let operation_id = op();
        kill_at(cut);
        let result = catch_unwind(AssertUnwindSafe(|| mutate(&store, &operation_id)));
        assert_kill(result);
        let old_content = old_content(&store);
        clear_faults(); // the crashed process is gone; a new process recovers
        let recovered = Store::open(&root).unwrap().recover(true).unwrap();
        assert!(
            recovered.needs_recovery.is_empty(),
            "cut {cut}: {:?}",
            recovered.needs_recovery
        );
        assert!(
            !recovered.rolled_back.is_empty(),
            "cut {cut}: expected rollback"
        );
        let fresh = Store::open(&root).unwrap().pin().unwrap();
        assert_eq!(fresh.generation, old_gen, "cut {cut}");
        assert_eq!(
            fs::read_to_string(fresh.path.join("typed.md")).unwrap(),
            old_content,
            "cut {cut}"
        );
    }
}

/// Kill after the pointer commit: recovery rolls forward to the complete
/// new state with the ledger durable.
#[test]
fn kill_after_pointer_commit_recovers_forward() {
    let _suite = suite_lock();
    for cut in [
        "materialize",
        "ledger-write",
        "journal-write-generation-committed",
        "journal-write-completed",
    ] {
        clear_faults();
        let root = scratch("new");
        let store = init_store(&root);
        let operation_id = op();
        kill_at(cut);
        let result = catch_unwind(AssertUnwindSafe(|| mutate(&store, &operation_id)));
        assert_kill(result);
        clear_faults();
        let recovered = Store::open(&root).unwrap().recover(true).unwrap();
        assert!(
            recovered.needs_recovery.is_empty(),
            "cut {cut}: {:?}",
            recovered.needs_recovery
        );
        let fresh = Store::open(&root).unwrap();
        // Pointer committed -> the new generation is complete and materialized.
        assert_eq!(
            fs::read_to_string(fresh.pin().unwrap().path.join("typed.md")).unwrap(),
            "hello\nworld\n",
            "cut {cut}"
        );
        assert_eq!(
            fs::read_to_string(root.join("typed.md")).unwrap(),
            "hello\nworld\n",
            "cut {cut}"
        );
        // Ledger durable after roll-forward.
        let ledger: Value = serde_json::from_str(
            &fs::read_to_string(fresh.pin().unwrap().path.join("operation-ledger.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            ledger["records"][&operation_id]["envelope"]["outcome"], "success",
            "cut {cut}"
        );
    }
}

/// Kill before the prepared record: recovery rolls back to the complete old
/// state.
#[test]
fn kill_before_prepared_recovers_old() {
    let _suite = suite_lock();
    for cut in [
        "journal-write-intent",
        "journal-flush-intent",
        "journal-rename-intent",
    ] {
        clear_faults();
        let root = scratch("intent");
        let store = init_store(&root);
        let old_gen = store.pin().unwrap().generation;
        let operation_id = op();
        kill_at(cut);
        let result = catch_unwind(AssertUnwindSafe(|| mutate(&store, &operation_id)));
        assert_kill(result);
        let old_content = old_content(&store);
        clear_faults();
        let recovered = Store::open(&root).unwrap().recover(true).unwrap();
        assert!(recovered.needs_recovery.is_empty());
        assert_eq!(
            Store::open(&root).unwrap().pin().unwrap().generation,
            old_gen
        );
        assert_eq!(
            fs::read_to_string(
                Store::open(&root)
                    .unwrap()
                    .pin()
                    .unwrap()
                    .path
                    .join("typed.md")
            )
            .unwrap(),
            old_content
        );
    }
}

/// Kill during external publication: the verified backup restores the prior
/// output; nothing is half-published.
#[test]
fn kill_during_external_publish_rolls_back_verified_backup() {
    let _suite = suite_lock();
    clear_faults();
    let root = scratch("extrollback");
    let store = init_store(&root);
    let output = root.join("out.docx");
    // Baseline publish FIRST: without it the target is unknown-existing and
    // the precondition must refuse (see external_file_version.rs) — the
    // kill test needs an owned target to roll back to.
    {
        let pin = store.pin().unwrap();
        let out = output.clone();
        let out2 = output.clone();
        store
            .mutate(StoreMutateRequest {
                workdir: store.root.clone(),
                operation: "build".to_string(),
                operation_id: op(),
                canonical: "baseline".to_string(),
                input_sha256: pin.manifest_sha256.clone().unwrap_or_default(),
                expected_generation: pin.generation,
                generation: false,
                save_boundary: false,
                ledger_anchor: Some(out.clone()),
                ledger_directory: false,
                evidence_path: Some(PathBuf::from(format!("{}.evidence.json", output.display()))),
                kind: "build".to_string(),
                lock_timeout_ms: 0,
                run: Box::new(move |_target: &Path, tx: &mut Transaction| {
                    let staged = tx.staging("out.docx");
                    fs::write(&staged, b"prior output").map_err(StoreError::Io)?;
                    tx.stage_external(&out2, &staged, "replace")?;
                    Ok(RunOutcome {
                        outcome: "success".to_string(),
                        data: json!({ "output": out2.to_string_lossy().into_owned() }),
                        kind: "build".to_string(),
                        payload: json!({ "checks": [] }),
                        diagnostics: vec![],
                    })
                }),
            })
            .expect("baseline publish");
    }
    let operation_id = op();
    let output_for_run = output.clone();
    let output_for_run2 = output.clone();
    kill_at("external-publish-out.docx");
    let pin = store.pin().unwrap();
    let result = catch_unwind(AssertUnwindSafe(|| {
        store.mutate(StoreMutateRequest {
            workdir: store.root.clone(),
            operation: "build".to_string(),
            operation_id: operation_id.clone(),
            canonical: "c".to_string(),
            input_sha256: pin.manifest_sha256.clone().unwrap_or_default(),
            expected_generation: pin.generation,
            generation: false,
            save_boundary: false,
            ledger_anchor: Some(output_for_run.clone()),
            ledger_directory: false,
            evidence_path: Some(PathBuf::from(format!("{}.evidence.json", output.display()))),
            kind: "build".to_string(),
            lock_timeout_ms: 0,
            run: Box::new(move |_target: &Path, tx: &mut Transaction| {
                let staged = tx.staging("out.docx");
                fs::write(&staged, b"new output").map_err(StoreError::Io)?;
                tx.stage_external(&output_for_run2, &staged, "replace")?;
                Ok(RunOutcome {
                    outcome: "success".to_string(),
                    data: json!({ "output": output_for_run2.to_string_lossy().into_owned() }),
                    kind: "build".to_string(),
                    payload: json!({ "checks": [] }),
                    diagnostics: vec![],
                })
            }),
        })
    }));
    assert_kill(result);
    clear_faults();
    let recovered = Store::open(&root).unwrap().recover(true).unwrap();
    assert!(recovered.needs_recovery.is_empty());
    assert!(!recovered.rolled_back.is_empty());
    // Prior output restored from the verified backup; nothing half-published.
    assert_eq!(fs::read(&output).unwrap(), b"prior output");
    assert!(Store::open(&root).unwrap().pin().is_ok());
}

/// Kill after the external publish (journal record never lands): recovery
/// rolls forward; the published output is the complete new state.
#[test]
fn kill_after_external_publish_recovers_forward() {
    let _suite = suite_lock();
    clear_faults();
    let root = scratch("extforward");
    let store = init_store(&root);
    let output = root.join("out.docx");
    {
        let pin = store.pin().unwrap();
        let out = output.clone();
        let out2 = output.clone();
        store
            .mutate(StoreMutateRequest {
                workdir: store.root.clone(),
                operation: "build".to_string(),
                operation_id: op(),
                canonical: "baseline".to_string(),
                input_sha256: pin.manifest_sha256.clone().unwrap_or_default(),
                expected_generation: pin.generation,
                generation: false,
                save_boundary: false,
                ledger_anchor: Some(out.clone()),
                ledger_directory: false,
                evidence_path: Some(PathBuf::from(format!("{}.evidence.json", output.display()))),
                kind: "build".to_string(),
                lock_timeout_ms: 0,
                run: Box::new(move |_target: &Path, tx: &mut Transaction| {
                    let staged = tx.staging("out.docx");
                    fs::write(&staged, b"prior output").map_err(StoreError::Io)?;
                    tx.stage_external(&out2, &staged, "replace")?;
                    Ok(RunOutcome {
                        outcome: "success".to_string(),
                        data: json!({ "output": out2.to_string_lossy().into_owned() }),
                        kind: "build".to_string(),
                        payload: json!({ "checks": [] }),
                        diagnostics: vec![],
                    })
                }),
            })
            .expect("baseline publish");
    }
    let operation_id = op();
    let output_for_run = output.clone();
    let output_for_run2 = output.clone();
    kill_at("journal-write-external-published"); // publish already landed
    let pin = store.pin().unwrap();
    let result = catch_unwind(AssertUnwindSafe(|| {
        store.mutate(StoreMutateRequest {
            workdir: store.root.clone(),
            operation: "build".to_string(),
            operation_id: operation_id.clone(),
            canonical: "c".to_string(),
            input_sha256: pin.manifest_sha256.clone().unwrap_or_default(),
            expected_generation: pin.generation,
            generation: false,
            save_boundary: false,
            ledger_anchor: Some(output_for_run.clone()),
            ledger_directory: false,
            evidence_path: Some(PathBuf::from(format!("{}.evidence.json", output.display()))),
            kind: "build".to_string(),
            lock_timeout_ms: 0,
            run: Box::new(move |_target: &Path, tx: &mut Transaction| {
                let staged = tx.staging("out.docx");
                fs::write(&staged, b"new output").map_err(StoreError::Io)?;
                tx.stage_external(&output_for_run2, &staged, "replace")?;
                Ok(RunOutcome {
                    outcome: "success".to_string(),
                    data: json!({ "output": output_for_run2.to_string_lossy().into_owned() }),
                    kind: "build".to_string(),
                    payload: json!({ "checks": [] }),
                    diagnostics: vec![],
                })
            }),
        })
    }));
    assert_kill(result);
    clear_faults();
    assert_eq!(fs::read(&output).unwrap(), b"new output");
    let recovered = Store::open(&root).unwrap().recover(true).unwrap();
    assert!(recovered.needs_recovery.is_empty());
    // Roll forward: the published output is the complete new state.
    assert_eq!(fs::read(&output).unwrap(), b"new output");
    assert!(PathBuf::from(format!("{}.operation-ledger.json", output.display())).is_file());
    assert!(PathBuf::from(format!("{}.evidence.json", output.display())).is_file());
}

/// Kill AFTER the external publish, then Word saves C over the published
/// bytes before recovery runs: recovery must quarantine (needs-recovery),
/// NEVER roll C back to the journal's bytes. C stays exact, sidecars
/// untouched.
#[test]
fn kill_after_external_publish_then_word_c_recovers_quarantine_leaving_c() {
    let _suite = suite_lock();
    clear_faults();
    let root = scratch("extforward-wordc");
    let store = init_store(&root);
    let output = root.join("out.docx");
    let sidecar = PathBuf::from(format!("{}.evidence.json", output.to_string_lossy()));
    {
        let pin = store.pin().unwrap();
        let out = output.clone();
        let out2 = output.clone();
        store
            .mutate(StoreMutateRequest {
                workdir: store.root.clone(),
                operation: "build".to_string(),
                operation_id: op(),
                canonical: "baseline".to_string(),
                input_sha256: pin.manifest_sha256.clone().unwrap_or_default(),
                expected_generation: pin.generation,
                generation: false,
                save_boundary: false,
                ledger_anchor: Some(out.clone()),
                ledger_directory: false,
                evidence_path: Some(sidecar.clone()),
                kind: "build".to_string(),
                lock_timeout_ms: 0,
                run: Box::new(move |_target: &Path, tx: &mut Transaction| {
                    let staged = tx.staging("out.docx");
                    fs::write(&staged, b"prior output").map_err(StoreError::Io)?;
                    tx.stage_external(&out2, &staged, "replace")?;
                    Ok(RunOutcome {
                        outcome: "success".to_string(),
                        data: json!({ "output": out2.to_string_lossy().into_owned() }),
                        kind: "build".to_string(),
                        payload: json!({ "checks": [] }),
                        diagnostics: vec![],
                    })
                }),
            })
            .expect("baseline publish");
    }
    let operation_id = op();
    let output_for_run = output.clone();
    let output_for_run2 = output.clone();
    kill_at("journal-write-external-published"); // publish already landed
    let pin = store.pin().unwrap();
    let result = catch_unwind(AssertUnwindSafe(|| {
        store.mutate(StoreMutateRequest {
            workdir: store.root.clone(),
            operation: "build".to_string(),
            operation_id: operation_id.clone(),
            canonical: "c".to_string(),
            input_sha256: pin.manifest_sha256.clone().unwrap_or_default(),
            expected_generation: pin.generation,
            generation: false,
            save_boundary: false,
            ledger_anchor: Some(output_for_run.clone()),
            ledger_directory: false,
            evidence_path: Some(sidecar.clone()),
            kind: "build".to_string(),
            lock_timeout_ms: 0,
            run: Box::new(move |_target: &Path, tx: &mut Transaction| {
                let staged = tx.staging("out.docx");
                fs::write(&staged, b"new output").map_err(StoreError::Io)?;
                tx.stage_external(&output_for_run2, &staged, "replace")?;
                Ok(RunOutcome {
                    outcome: "success".to_string(),
                    data: json!({ "output": output_for_run2.to_string_lossy().into_owned() }),
                    kind: "build".to_string(),
                    payload: json!({ "checks": [] }),
                    diagnostics: vec![],
                })
            }),
        })
    }));
    assert_kill(result);
    clear_faults();
    // Word saves C over the just-published bytes BEFORE recovery runs.
    fs::write(&output, b"C-word-edit-after-publish").unwrap();
    let sidecar_c = fs::read(&sidecar).expect("sidecar still present");
    let recovered = Store::open(&root).unwrap().recover(true).unwrap();
    assert!(
        !recovered.needs_recovery.is_empty(),
        "external C over a prepared-but-unclosed publish must quarantine, not roll forward/back: {:?}",
        recovered.needs_recovery
    );
    // C exact, sidecar untouched — recovery never overwrites the external file.
    // (The sidecar on disk is the killed run's own evidence — operation B,
    // not the baseline's — and recovery must not touch it either way.)
    assert_eq!(fs::read(&output).unwrap(), b"C-word-edit-after-publish");
    assert_eq!(fs::read(&sidecar).unwrap(), sidecar_c);
}

/// Kill BEFORE the external publish (prepared journal only), then Word
/// creates C at the target before recovery runs: recovery must NOT delete
/// C and must NOT complete the publish — quarantine, target left exact.
#[test]
fn kill_before_external_publish_then_c_appears_recovers_quarantine_keeping_c() {
    let _suite = suite_lock();
    clear_faults();
    let root = scratch("extprepared-wordc");
    let store = init_store(&root);
    let output = root.join("out.docx");
    let operation_id = op();
    let output_for_run = output.clone();
    let output_for_run2 = output.clone();
    // Crash after the prepared record, before any external publish. The
    // fire hook on the publish cut fires first — kill there.
    kill_at("external-publish-out.docx");
    let pin = store.pin().unwrap();
    let result = catch_unwind(AssertUnwindSafe(|| {
        store.mutate(StoreMutateRequest {
            workdir: store.root.clone(),
            operation: "build".to_string(),
            operation_id: operation_id.clone(),
            canonical: "c".to_string(),
            input_sha256: pin.manifest_sha256.clone().unwrap_or_default(),
            expected_generation: pin.generation,
            generation: false,
            save_boundary: false,
            ledger_anchor: Some(output_for_run.clone()),
            ledger_directory: false,
            evidence_path: Some(PathBuf::from(format!("{}.evidence.json", output.display()))),
            kind: "build".to_string(),
            lock_timeout_ms: 0,
            run: Box::new(move |_target: &Path, tx: &mut Transaction| {
                let staged = tx.staging("out.docx");
                fs::write(&staged, b"new output").map_err(StoreError::Io)?;
                tx.stage_external(&output_for_run2, &staged, "replace")?;
                Ok(RunOutcome {
                    outcome: "success".to_string(),
                    data: json!({ "output": output_for_run2.to_string_lossy().into_owned() }),
                    kind: "build".to_string(),
                    payload: json!({ "checks": [] }),
                    diagnostics: vec![],
                })
            }),
        })
    }));
    assert_kill(result);
    clear_faults();
    assert!(!output.exists(), "nothing published before the kill");
    // Word creates C at the target BEFORE recovery runs.
    fs::write(&output, b"C-appeared-before-recovery").unwrap();
    let recovered = Store::open(&root).unwrap().recover(true).unwrap();
    assert!(
        !recovered.needs_recovery.is_empty(),
        "C appearing before recovery of a prepared publish must quarantine, not delete C: {:?}",
        recovered.needs_recovery
    );
    assert_eq!(
        fs::read(&output).unwrap(),
        b"C-appeared-before-recovery",
        "recovery must not delete external C"
    );
}

/// ENOSPC releases the 1 MiB reserve and locks the workdir read-only until
/// replenished.
#[test]
fn enospc_releases_reserve_and_locks_readonly() {
    let _suite = suite_lock();
    clear_faults();
    let root = scratch("enospc");
    let store = init_store(&root);
    let operation_id = op();
    set_fault("journal-write-prepared", Some(Fault::Enospc));
    let error = mutate(&store, &operation_id).expect_err("expected reserve-depleted");
    assert_eq!(error.code(), Some(RESERVE_DEPLETED));
    clear_faults();
    // Reserve released; workdir is read-only until replenished.
    let reserve = root.join(STORE_DIR_NAME).join("reserve");
    assert!(fs::metadata(&reserve).unwrap().len() < RESERVE_BYTES);
    assert!(root
        .join(STORE_DIR_NAME)
        .join("reserve-depleted.json")
        .is_file());
    let error2 = mutate(&store, &op()).expect_err("expected reserve-depleted");
    assert_eq!(error2.code(), Some(RESERVE_DEPLETED));
    // Replenishing re-enables mutations.
    store.replenish_reserve().expect("replenish");
    let envelope = mutate(&store, &op()).expect("mutate after replenish");
    assert_eq!(envelope["outcome"], "success");
}

/// A short write (torn record) is detected and rejected: the record never
/// lands, so recovery rolls back to the complete old state.
#[test]
fn short_write_detected_rolls_back_old() {
    let _suite = suite_lock();
    clear_faults();
    let root = scratch("short");
    let store = init_store(&root);
    let old_gen = store.pin().unwrap().generation;
    let operation_id = op();
    set_fault("journal-flush-prepared", Some(Fault::Truncate(4)));
    let old_content = old_content(&store);
    let error = mutate(&store, &operation_id).expect_err("expected short-write io error");
    assert!(
        error.message().contains("short write"),
        "{}",
        error.message()
    );
    clear_faults();
    let recovered = Store::open(&root).unwrap().recover(true).unwrap();
    assert!(recovered.needs_recovery.is_empty());
    let fresh = Store::open(&root).unwrap().pin().unwrap();
    assert_eq!(fresh.generation, old_gen);
    assert_eq!(
        fs::read_to_string(fresh.path.join("typed.md")).unwrap(),
        old_content
    );
}

/// A flush failure aborts the journal write; the in-process abort restores
/// the pointer: complete old, never mixed.
#[test]
fn flush_failure_leaves_no_partial_record() {
    let _suite = suite_lock();
    clear_faults();
    let root = scratch("flush");
    let store = init_store(&root);
    let old_gen = store.pin().unwrap().generation;
    let operation_id = op();
    set_fault(
        "journal-flush-generation-committed",
        Some(Fault::Error("flush failed".to_string())),
    );
    let old_content = old_content(&store);
    let error = mutate(&store, &operation_id).expect_err("expected io error");
    assert!(
        error.message().contains("flush failed"),
        "{}",
        error.message()
    );
    clear_faults();
    let recovered = Store::open(&root).unwrap().recover(true).unwrap();
    assert!(recovered.needs_recovery.is_empty());
    // The pointer was restored by the abort: complete old, never mixed.
    let fresh = Store::open(&root).unwrap().pin().unwrap();
    assert_eq!(fresh.generation, old_gen);
    assert_eq!(
        fs::read_to_string(fresh.path.join("typed.md")).unwrap(),
        old_content
    );
}

/// Corrupt pointer / corrupt generation manifest / corrupt journal chain
/// all yield needs-recovery — never guessed into repair.
#[test]
fn corruption_yields_needs_recovery() {
    let _suite = suite_lock();
    clear_faults();
    // Corrupt pointer -> needs-recovery from mutate (the pointer read inside
    // mutate fails; the caller's earlier pin still holds the planned values).
    let root = scratch("corrupt-pointer");
    let store = init_store(&root);
    let planned = store.pin().unwrap();
    let planned_gen = planned.generation.clone();
    let planned_manifest = planned.manifest_sha256.clone().unwrap_or_default();
    fs::write(root.join("workdir.json"), "{not json").unwrap();
    let error = store
        .mutate(StoreMutateRequest {
            workdir: store.root.clone(),
            operation: "edit".to_string(),
            operation_id: op(),
            canonical: "c".to_string(),
            input_sha256: planned_manifest,
            expected_generation: planned_gen,
            generation: true,
            save_boundary: false,
            ledger_anchor: None,
            ledger_directory: true,
            evidence_path: None,
            kind: "mutation".to_string(),
            lock_timeout_ms: 0,
            run: simple_run("x"),
        })
        .expect_err("expected needs-recovery");
    assert_eq!(error.code(), Some(NEEDS_RECOVERY));
    // Corrupt generation manifest -> needs-recovery from pin.
    let root2 = scratch("corrupt-manifest");
    let store2 = init_store(&root2);
    fs::write(
        store2.pin().unwrap().path.join("generation.json"),
        "{broken",
    )
    .unwrap();
    let error = Store::open(&root2)
        .unwrap()
        .pin()
        .expect_err("expected needs-recovery");
    assert_eq!(error.code(), Some(NEEDS_RECOVERY));
    // Corrupt journal chain -> needs-recovery from recovery.
    let root3 = scratch("corrupt-journal");
    let store3 = init_store(&root3);
    let operation_id = op();
    kill_at("journal-write-prepared");
    let result = catch_unwind(AssertUnwindSafe(|| mutate(&store3, &operation_id)));
    assert_kill(result);
    clear_faults();
    let tx_dir = root3
        .join(STORE_DIR_NAME)
        .join("transactions")
        .join(&operation_id);
    fs::write(
        tx_dir.join("intent.json"),
        "{\"schema\":\"docx2typed-transaction-journal-1\",\"phase\":\"intent\",\"tampered\":true}\n",
    )
    .unwrap();
    let recovered = Store::open(&root3).unwrap().recover(true).unwrap();
    assert!(!recovered.needs_recovery.is_empty());
}

// ---------------------------------------------------------------------------
// Bullet 4: qualified filesystems pass startup probes; unsupported/unprobed
// filesystems fail before mutation.
// ---------------------------------------------------------------------------

#[test]
fn probe_qualifies_and_force_unqualified_fails_closed() {
    let _suite = suite_lock();
    clear_faults();
    let root = scratch("probe");
    make_workdir(&root);
    let store = Store::init(&root, &op(), "input").expect("init probes the volume");
    // Probe cache persisted and qualified.
    let probe: Value = serde_json::from_str(
        &fs::read_to_string(root.join(STORE_DIR_NAME).join("probe.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(probe["schema"], "docx2typed-store-probe-1");
    assert_eq!(probe["qualified"], true);
    for check in [
        "atomic_replace",
        "file_durability",
        "advisory_lock",
        "stable_identity",
    ] {
        assert_eq!(probe["checks"][check], true, "check {check}");
    }
    // Mutations succeed on the qualified volume.
    mutate(&store, &op()).expect("qualified mutation");
    // DOCX2TYPED_FORCE_UNQUALIFIED=1: every store fails closed before
    // mutation with unsupported-by-design.
    std::env::set_var("DOCX2TYPED_FORCE_UNQUALIFIED", "1");
    let root2 = scratch("unqualified");
    make_workdir(&root2);
    let error = Store::init(&root2, &op(), "input").expect_err("expected unsupported-by-design");
    assert_eq!(error.code(), Some(UNSUPPORTED_BY_DESIGN));
    // No store directory was left behind (fail before mutation).
    assert!(!root2.join(STORE_DIR_NAME).exists());
    std::env::remove_var("DOCX2TYPED_FORCE_UNQUALIFIED");
}

/// Startup recovery runs at mutation entry points (no recover command), and
/// leftover transactions settle before the next mutation commits.
#[test]
fn startup_recovery_runs_at_next_mutation_entry() {
    let _suite = suite_lock();
    clear_faults();
    let root = scratch("startup");
    let store = init_store(&root);
    let old_gen = store.pin().unwrap().generation;
    // Crash mid-mutation (kill before pointer commit).
    let operation_id = op();
    kill_at("journal-write-prepared");
    let result = catch_unwind(AssertUnwindSafe(|| mutate(&store, &operation_id)));
    assert_kill(result);
    clear_faults();
    // A fresh mutation entry first recovers (rolls back), then commits.
    let fresh_op = op();
    let envelope =
        mutate(&Store::open(&root).unwrap(), &fresh_op).expect("entry recovery + commit");
    assert_eq!(envelope["outcome"], "success");
    // The crashed transaction was rolled back; the new commit landed.
    assert_ne!(store.pin().unwrap().generation, old_gen);
    assert_eq!(
        fs::read_to_string(store.pin().unwrap().path.join("typed.md")).unwrap(),
        "hello\nworld\n"
    );
    // No pending transactions remain.
    let state = Store::open(&root).unwrap();
    assert!(state.pending_transactions().is_empty());
}

// ---------------------------------------------------------------------------
// HEAD carries the version it names. A plain mutation carries those fields
// forward (canonical may drift ahead of HEAD -- that drift is `version_dirty`,
// ADR 0044); only a save boundary replaces them.
// ---------------------------------------------------------------------------

/// A pointer that names a version, as the Python reference writes it at a save
/// boundary. Returns the seeded pointer for comparison.
fn seed_version_head(root: &Path) -> Value {
    let path = root.join("workdir.json");
    let mut pointer: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    pointer["head_version"] = json!("V1");
    pointer["head_version_generation"] = pointer["generation"].clone();
    pointer["version_seq"] = json!(1);
    pointer["head_tree"] = json!("tree-digest");
    pointer["head_tree_object"] = json!("tree-object-digest");
    pointer["head_commit"] = json!("commit-digest");
    pointer["baseline_epoch"] = json!(1);
    fs::write(&path, canonical_json(&pointer)).unwrap();
    pointer
}

#[test]
fn a_mutation_carries_the_version_head_forward() {
    let _guard = suite_lock();
    let root = scratch("carry-forward");
    let store = init_store(&root);
    let before = seed_version_head(&root);

    mutate(&store, &op()).expect("mutate");

    let after: Value =
        serde_json::from_str(&fs::read_to_string(root.join("workdir.json")).unwrap()).unwrap();
    for field in [
        "head_version",
        "head_version_generation",
        "version_seq",
        "head_tree",
        "head_tree_object",
        "head_commit",
        "baseline_epoch",
    ] {
        assert_eq!(
            after[field], before[field],
            "{field} must be carried forward by a plain mutation"
        );
    }
    // The mutation committed a new generation; it did not claim a new version.
    assert_ne!(after["generation"], before["generation"]);
}

#[test]
fn a_mutation_does_not_invent_a_version_head() {
    let _guard = suite_lock();
    let root = scratch("no-version");
    let store = init_store(&root);

    mutate(&store, &op()).expect("mutate");

    let after: Value =
        serde_json::from_str(&fs::read_to_string(root.join("workdir.json")).unwrap()).unwrap();
    for field in ["head_version", "version_seq", "head_tree", "head_commit"] {
        assert!(
            after.get(field).is_none(),
            "{field} must not be invented: a mutation is not a save"
        );
    }
}

// ---------------------------------------------------------------------------
// The save boundary decides whether a Version is recorded (#37).
// ---------------------------------------------------------------------------

/// A mutation that is NOT a save boundary must not record a version, even
/// though it moves the canonical tree.
///
/// This is the regression for a real defect found while wiring the writer:
/// Rust's draft lane writes `typed.md` at edit time, so deriving the boundary
/// from the tree digest recorded V1 for a `replace_text` and V2 for the first
/// `commit_sync` -- where the reference gives nothing for the edit and V1 for
/// the save. The flag is what makes the two distinguishable.
#[test]
fn a_draft_edit_records_no_version_but_a_save_does() {
    let _guard = suite_lock();
    let root = scratch("boundary");
    let store = init_store(&root);

    // A draft edit: same tree-moving run, no boundary.
    mutate_as(&store, &op(), false).expect("draft mutate");
    let after_draft: Value =
        serde_json::from_str(&fs::read_to_string(root.join("workdir.json")).unwrap()).unwrap();
    assert!(
        after_draft.get("head_version").is_none(),
        "a draft edit must not name a version"
    );

    // A save: identical run, boundary set.
    mutate_as(&store, &op(), true).expect("save mutate");
    let after_save: Value =
        serde_json::from_str(&fs::read_to_string(root.join("workdir.json")).unwrap()).unwrap();
    assert_eq!(after_save["head_version"], "V1", "the first save is V1");
    assert_eq!(after_save["version_seq"], 1);
    assert_eq!(
        after_save["head_version_generation"], after_save["generation"],
        "a version names the generation it was recorded at"
    );
    assert!(after_save["head_tree"].is_string());
    assert!(after_save["head_commit"].is_string());
    assert!(after_save["head_tree_object"].is_string());

    // The objects it names are really in the pool, and the tree materialises
    // back to the state that was saved.
    let tree_id = after_save["head_tree_object"].as_str().unwrap();
    let destination = scratch("boundary-materialized");
    let written = docx2typed_store::materialize(&root, tree_id, &destination).expect("materialize");
    assert!(written.contains(&"typed.md".to_string()));
    assert_eq!(
        fs::read_to_string(destination.join("typed.md")).unwrap(),
        fs::read_to_string(store.pin().expect("pin").path.join("typed.md")).unwrap()
    );

    // A second save whose tree is UNCHANGED records no version, which is the
    // reference's measured behaviour for a no-op save.
    mutate_as(&store, &op(), true).expect("no-op save");
    let after_noop: Value =
        serde_json::from_str(&fs::read_to_string(root.join("workdir.json")).unwrap()).unwrap();
    assert_eq!(after_noop["head_version"], "V1");
    assert_eq!(after_noop["version_seq"], 1);
}
