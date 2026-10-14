//! Engine chain tests: extract -> no-op build -> independent verify against
//! the committed corpus fixture, asserting the frozen Python Reference
//! output hash and the copy-if-unchanged byte identity.

use std::path::PathBuf;

use docx2typed_app::{
    BuildArgs, Engine, ExtractArgs, Operation, OperationArgs, OperationContext, Outcome, VerifyArgs,
};
use docx2typed_protocol::bytes_sha256;

/// SHA-256 of corpus/release/plain.docx, recorded in the frozen plan
/// (qualification/plan.json identities.fixture.fixtures.plain.docx) and
/// reproduced byte-for-byte by the Python Reference no-op build
/// (`python -m scripts extract && build`, verified on this host).
const FROZEN_FIXTURE_SHA256: &str =
    "4323e37b7ac7e9dbce7b4923d14529bda821f0d66f0dce7005cf9299bf8d9c39";

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../corpus/release/plain.docx")
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("docx2typed-rs-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[test]
fn extract_build_verify_chain_reproduces_python_reference_hash() {
    let scratch = temp_dir("chain");
    let workdir = scratch.join("wd");
    let output = scratch.join("out.docx");

    let engine = Engine::new();

    // extract
    let outcome = engine
        .execute(
            Operation::Extract,
            OperationContext::new(docx2typed_protocol::new_operation_id()),
            OperationArgs::Extract(ExtractArgs {
                input: fixture(),
                outdir: workdir.clone(),
                discard_committed_versions: false,
            }),
        )
        .expect("engine runs");
    assert_eq!(outcome.outcome, Outcome::Success, "extract succeeds");
    let template = workdir.join("_template.docx");
    assert!(template.is_file(), "_template.docx materialized");

    // build (no-op)
    let outcome = engine
        .execute(
            Operation::Build,
            OperationContext::new(docx2typed_protocol::new_operation_id()),
            OperationArgs::Build(BuildArgs {
                workdir: workdir.clone(),
                output: Some(output.clone()),
                lock_timeout_ms: 0,
            }),
        )
        .expect("engine runs");
    assert_eq!(outcome.outcome, Outcome::Success, "build succeeds");
    assert_eq!(outcome.evidence.len(), 1, "build publishes one evidence");

    // copy-if-unchanged proof: output bytes == template bytes
    let output_bytes = std::fs::read(&output).expect("output readable");
    let template_bytes = std::fs::read(&template).expect("template readable");
    assert_eq!(
        output_bytes, template_bytes,
        "output is a byte copy of the template"
    );

    // whole-file hash equals the Python Reference no-op output exactly
    let output_sha256 = bytes_sha256(&output_bytes);
    assert_eq!(output_sha256, FROZEN_FIXTURE_SHA256);

    // verify (independent)
    let outcome = engine
        .execute(
            Operation::Verify,
            OperationContext::new(docx2typed_protocol::new_operation_id()),
            OperationArgs::Verify(VerifyArgs {
                workdir: workdir.clone(),
                output: output.clone(),
            }),
        )
        .expect("engine runs");
    assert_eq!(outcome.outcome, Outcome::Success, "verify succeeds");
    assert_eq!(outcome.evidence.len(), 1, "verify publishes one evidence");

    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn tampered_output_fails_independent_verification() {
    let scratch = temp_dir("tamper");
    let workdir = scratch.join("wd");
    let output = scratch.join("out.docx");

    let engine = Engine::new();
    engine
        .execute(
            Operation::Extract,
            OperationContext::new(docx2typed_protocol::new_operation_id()),
            OperationArgs::Extract(ExtractArgs {
                input: fixture(),
                outdir: workdir.clone(),
                discard_committed_versions: false,
            }),
        )
        .expect("extract");
    engine
        .execute(
            Operation::Build,
            OperationContext::new(docx2typed_protocol::new_operation_id()),
            OperationArgs::Build(BuildArgs {
                workdir: workdir.clone(),
                output: Some(output.clone()),
                lock_timeout_ms: 0,
            }),
        )
        .expect("build");

    // Tamper one byte of the output package.
    let mut bytes = std::fs::read(&output).expect("output readable");
    let flip = bytes.len() / 2;
    bytes[flip] ^= 0x01;
    std::fs::write(&output, bytes).expect("tamper write");

    let outcome = engine
        .execute(
            Operation::Verify,
            OperationContext::new(docx2typed_protocol::new_operation_id()),
            OperationArgs::Verify(VerifyArgs {
                workdir: workdir.clone(),
                output: output.clone(),
            }),
        )
        .expect("engine runs");
    assert_eq!(
        outcome.outcome,
        Outcome::Failure,
        "tampered output fails verify"
    );
    assert_eq!(
        outcome.diagnostics[0].code, "workdir-invalid",
        "verify failure carries the domain diagnostic"
    );

    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn edited_typed_md_fails_noop_build() {
    let scratch = temp_dir("edited");
    let workdir = scratch.join("wd");
    let output = scratch.join("out.docx");

    let engine = Engine::new();
    engine
        .execute(
            Operation::Extract,
            OperationContext::new(docx2typed_protocol::new_operation_id()),
            OperationArgs::Extract(ExtractArgs {
                input: fixture(),
                outdir: workdir.clone(),
                discard_committed_versions: false,
            }),
        )
        .expect("extract");

    // Simulate an edit: append a typed paragraph to typed.md.
    let typed_path = workdir.join("typed.md");
    let mut typed = std::fs::read_to_string(&typed_path).expect("typed.md readable");
    typed.push_str("\n<!--@p id=\"P0\" base=\"s_x\"-->\nedited text\n");
    std::fs::write(&typed_path, typed).expect("edit write");

    let outcome = engine
        .execute(
            Operation::Build,
            OperationContext::new(docx2typed_protocol::new_operation_id()),
            OperationArgs::Build(BuildArgs {
                workdir: workdir.clone(),
                output: Some(output.clone()),
                lock_timeout_ms: 0,
            }),
        )
        .expect("engine runs");
    assert_eq!(
        outcome.outcome,
        Outcome::Failure,
        "edited workdir fails build"
    );
    assert!(!output.exists(), "no output published for a rejected build");

    let _ = std::fs::remove_dir_all(&scratch);
}
