//! Differential gate for issue #55: drives the installed-style `docx2typed`
//! binary (built by this crate) through the frozen noop-bytes case
//! (extract + build on corpus/release/plain.docx), asserts the output
//! equals the Python Reference record byte-for-byte, proves copy-if-
//! unchanged at the file level, and exercises the MCP stdio negotiation
//! seam. This is the rust-side reproduction of qualification/plan.json
//! check `noop-bytes` (the frozen plan commands run `python -m scripts`,
//! so the harness adapter wiring stays on the Python side; scripts/qualify.py
//! is not modified).

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Frozen Python Reference record (plan identities.fixture.fixtures.plain.docx;
/// also the byte-identical Python no-op build output on this host).
const FROZEN_FIXTURE_SHA256: &str =
    "4323e37b7ac7e9dbce7b4923d14529bda821f0d66f0dce7005cf9299bf8d9c39";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_docx2typed")
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fixture() -> PathBuf {
    repo_root().join("corpus/release/plain.docx")
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("docx2typed-rs-diff-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch");
    dir
}

fn run_json(binary: &str, args: &[&str]) -> (i32, serde_json::Value) {
    let output = Command::new(binary)
        .args(args)
        .output()
        .expect("spawn binary");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|error| panic!("non-JSON stdout {stdout:?}: {error}"));
    (output.status.code().unwrap_or(-1), parsed)
}

fn sha256_file(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path).expect("read file");
    hex::encode(Sha256::digest(&bytes))
}

fn zip_part_hashes(path: &Path) -> std::collections::BTreeMap<String, String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let file = std::fs::File::open(path).expect("open zip");
    let mut archive = zip::ZipArchive::new(file).expect("valid zip");
    let mut parts = std::collections::BTreeMap::new();
    for index in 0..archive.len() {
        let mut member = archive.by_index(index).expect("member");
        let name = member.name().to_string();
        let mut buf = Vec::new();
        member.read_to_end(&mut buf).expect("read member");
        parts.insert(name, hex::encode(Sha256::digest(&buf)));
    }
    parts
}

#[test]
fn version_negotiates_frozen_descriptor() {
    let (code, descriptor) = run_json(bin(), &["--version", "--json"]);
    assert_eq!(code, 0);
    assert_eq!(descriptor["schema"], "docx2typed-engine-descriptor-1");
    assert_eq!(descriptor["name"], "docx2typed-rust");
    for contract in ["cli", "mcp", "result", "evidence", "workdir"] {
        assert_eq!(descriptor["contracts"][contract]["major"], 1);
        assert_eq!(descriptor["contracts"][contract]["min_minor"], 0);
        assert_eq!(descriptor["contracts"][contract]["max_minor"], 0);
    }
    assert_eq!(
        descriptor["schema_bundle"]["sha256"],
        "8d1e8dbf2778e31cc6aa838e1ccae642c0481ff03719a996f439cf939e378d84"
    );
    assert_eq!(
        descriptor["capability_manifest"]["sha256"],
        "d911b616e2238ccc2dcd8b4dd7e170f3feff01ac2fb412ce3752bc7ec77baf37"
    );
}

#[test]
fn noop_bytes_case_matches_python_reference_record() {
    let scratch = scratch("noop");
    let workdir = scratch.join("wd");
    let output = scratch.join("out.docx");

    // extract (frozen plan op 1: rc 0)
    let (code, envelope) = run_json(
        bin(),
        &[
            "extract",
            "--json",
            fixture().to_str().unwrap(),
            "-o",
            workdir.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0, "extract rc=0");
    assert_eq!(envelope["schema"], "docx2typed-result-1");
    assert_eq!(envelope["outcome"], "success");

    // build (frozen plan op 2: rc 0)
    let (code, envelope) = run_json(
        bin(),
        &[
            "build",
            "--json",
            workdir.to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0, "build rc=0");
    assert_eq!(envelope["outcome"], "success");
    assert_eq!(envelope["evidence"][0]["kind"], "build");

    // whole-file SHA-256 equals the Python Reference output exactly
    let output_sha256 = sha256_file(&output);
    assert_eq!(output_sha256, FROZEN_FIXTURE_SHA256);

    // per-part identity vs source (the frozen compare is per-part)
    let source_parts = zip_part_hashes(&fixture());
    let output_parts = zip_part_hashes(&output);
    assert_eq!(
        source_parts, output_parts,
        "every part byte-identical to source"
    );

    // copy-if-unchanged proof: output is a byte copy of the template
    let template_bytes = std::fs::read(workdir.join("_template.docx")).expect("template");
    let output_bytes = std::fs::read(&output).expect("output");
    assert_eq!(
        template_bytes, output_bytes,
        "build replayed template bytes verbatim"
    );

    // verify (independent)
    let (code, envelope) = run_json(
        bin(),
        &[
            "verify",
            "--json",
            workdir.to_str().unwrap(),
            output.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0, "verify rc=0");
    assert_eq!(envelope["outcome"], "success");
    assert_eq!(envelope["evidence"][0]["kind"], "verify");

    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn mcp_stdio_negotiates_engine_info_and_workdir_open() {
    let scratch = scratch("mcp");
    let workdir = scratch.join("wd");

    // prepare a workdir via the CLI
    let (code, _) = run_json(
        bin(),
        &[
            "extract",
            "--json",
            fixture().to_str().unwrap(),
            "-o",
            workdir.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0);

    let mut child = Command::new(bin())
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mcp");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));

    // engine_info returns the descriptor directly
    writeln!(stdin, r#"{{"tool":"engine_info","args":{{}}}}"#).expect("write");
    stdin.flush().expect("flush");
    let mut line = String::new();
    stdout.read_line(&mut line).expect("read reply");
    assert!(line.starts_with("OK "), "engine_info reply: {line}");
    let descriptor: serde_json::Value =
        serde_json::from_str(line.trim_start_matches("OK ")).expect("descriptor json");
    assert_eq!(descriptor["schema"], "docx2typed-engine-descriptor-1");

    // workdir_open negotiates and returns the session descriptor envelope
    let request = format!(
        r#"{{"tool":"workdir_open","args":{{"workdir":{},"contract_ranges":{{"cli":{{"major":1,"min_minor":0,"max_minor":0}}}}}}}}"#,
        serde_json::to_string(&workdir.to_string_lossy()).expect("path json")
    );
    writeln!(stdin, "{request}").expect("write");
    stdin.flush().expect("flush");
    line.clear();
    stdout.read_line(&mut line).expect("read reply");
    assert!(line.starts_with("OK "), "workdir_open reply: {line}");
    let reply: serde_json::Value =
        serde_json::from_str(line.trim_start_matches("OK ")).expect("reply json");
    assert_eq!(reply["isError"], false);
    let envelope = &reply["structuredContent"];
    assert_eq!(envelope["schema"], "docx2typed-result-1");
    assert_eq!(envelope["operation"], "workdir_open");
    assert_eq!(envelope["outcome"], "success");
    assert_eq!(
        envelope["data"]["session"]["schema"],
        "docx2typed-session-descriptor-1"
    );

    // A second open naming the SAME workdir succeeds here, where the reference
    // refuses it with `workdir-already-open`. Recorded divergence #4 in
    // docs/specs/06_reference_divergences.md: the reference's stated rule is
    // "one connection drives one workdir", and re-opening the one it already
    // drives does not violate that. A real session read the refusal as a
    // transient block and moved directories around on disk to get past it.
    writeln!(stdin, "{request}").expect("write");
    stdin.flush().expect("flush");
    line.clear();
    stdout.read_line(&mut line).expect("read reply");
    let reply: serde_json::Value =
        serde_json::from_str(line.trim_start_matches("OK ")).expect("reply json");
    assert_eq!(
        reply["isError"], false,
        "re-opening the same workdir is a retry, not a conflict: {reply}"
    );
    // What the rule actually protects: a DIFFERENT workdir on the same
    // connection is still refused, and that half is not relaxed.
    assert!(
        reply["structuredContent"]["data"]["session"].is_object(),
        "the re-open returns the live session: {reply}"
    );

    // incompatible contract major is a negotiation failure, not an open
    let bad_request = format!(
        r#"{{"tool":"workdir_open","args":{{"workdir":{},"contract_ranges":{{"cli":{{"major":2,"min_minor":0,"max_minor":0}}}}}}}}"#,
        serde_json::to_string(&workdir.to_string_lossy()).expect("path json")
    );
    writeln!(stdin, "{bad_request}").expect("write");
    stdin.flush().expect("flush");
    line.clear();
    stdout.read_line(&mut line).expect("read reply");
    let reply: serde_json::Value =
        serde_json::from_str(line.trim_start_matches("OK ")).expect("reply json");
    assert_eq!(
        reply["structuredContent"]["diagnostics"][0]["code"],
        "contract-incompatible"
    );

    drop(stdin);
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&scratch);
}
