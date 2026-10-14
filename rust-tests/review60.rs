//! Issue #60 binary-level tracer: the secured review collaboration lane,
//! driven through the installed-style `docx2typed` binary exactly as the
//! qualification gate does.
//!
//! MCP(stdout) surface: the frozen tool inventory with published input
//! schemas, one-workdir connection state, draft projection lifecycle
//! (replace/insert/delete/diff/commit with changed-set detection and
//! byte-exact store replay / operation-id reuse), and a clean stdio
//! channel. HTTP surface (`review` subcommand): the zero-data bootstrap
//! shell, uniform authority 404s, Host allowlist, Origin + Sec-Fetch-Site
//! mutation gates, content-type/size caps, security headers on every
//! response (never CORS), bounded throttle, Idempotency-Key replay
//! byte-exact, concurrent publish one-winner (CAS), restart revoking the
//! capability while the store state survives, and redacted logs.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_docx2typed")
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fixture(name: &str) -> PathBuf {
    repo_root().join(format!("corpus/release/{name}.docx"))
}

fn scratch(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("docx2typed-review60-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn rust_json(args: &[&str]) -> (i32, Value) {
    let output: Output = Command::new(bin())
        .args(args)
        .output()
        .expect("binary runs");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let value: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|error| panic!("binary JSON parse failed: {error}; output: {stdout}"));
    (output.status.code().unwrap_or(-1), value)
}

fn extract(fixture_path: &Path, outdir: &Path) {
    let (rc, envelope) = rust_json(&[
        "extract",
        "--json",
        fixture_path.to_str().expect("utf8"),
        "-o",
        outdir.to_str().expect("utf8"),
    ]);
    assert_eq!(rc, 0, "extract failed: {envelope}");
    assert_eq!(envelope["outcome"], "success", "{envelope}");
}

/// The frozen MCP tool surface (mirror of `scripts/mcp_server.py`).
const TOOL_NAMES: [&str; 53] = [
    "engine_info",
    "workdir_open",
    "workdir_status",
    "list_paragraphs",
    "get_paragraph",
    "replace_text",
    "batch_edit",
    "insert_paragraph",
    "delete_paragraph",
    "diff_preview",
    "commit_sync",
    "accept_revision",
    "reject_revision",
    "reinsert_deleted_text",
    "delete_comment",
    "table_insert_row",
    "table_delete_row",
    "table_insert_col",
    "table_delete_col",
    "table_merge_cells",
    "table_split_cells",
    "decide_all",
    "list_comments",
    "get_comment",
    "review_preflight",
    "review_state",
    "review_external_preflight",
    "review_settlement_plan",
    "review_settle",
    "review_apply_patch",
    "review_apply_batch",
    "review_inbox",
    "review_ack",
    "revert",
    "build_docx",
    "verify_output",
    "history_list",
    "history_gc",
    "history_restore",
    "document_search",
    "document_read",
    "document_patch",
    "document_replace",
    "history_diff",
    "history_blame",
    "history_verify",
    "workspace_fork",
    "format_span",
    "workspace_adopt",
    "facade_locate",
    "facade_edit",
    "facade_verify",
    "facade_resolve",
];

/// Drive one MCP(stdout) session: each stdin line is
/// `{"tool","args"}`, each reply `OK <json>` / `ERR <msg>`.
fn mcp(requests: &[(&str, Value)]) -> (Vec<Value>, Vec<String>) {
    let mut input = String::new();
    for (tool, args) in requests {
        input.push_str(&format!("{}\n", json!({ "tool": tool, "args": args })));
    }
    let mut command = Command::new(bin());
    command
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn mcp");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .expect("stdout")
        .read_to_string(&mut stdout)
        .expect("read stdout");
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("stderr")
        .read_to_string(&mut stderr)
        .expect("read stderr");
    let status = child.wait().expect("wait");
    assert!(status.success(), "mcp exited {status:?}: {stderr}");
    let mut envelopes = Vec::new();
    let mut raw = Vec::new();
    for line in stdout.lines() {
        raw.push(line.to_string());
        if let Some(reply) = line.strip_prefix("OK ") {
            let value: Value = serde_json::from_str(reply).unwrap_or_else(|error| {
                panic!("mcp reply parse failed: {error}; line: {line}");
            });
            envelopes.push(value);
        }
    }
    (envelopes, raw)
}

fn envelope(result: &Value) -> &Value {
    &result["structuredContent"]
}

/// Free TCP port: bind port 0, read the ephemeral port, drop the listener.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral");
    listener.local_addr().expect("local addr").port()
}

/// A spawned review server with its capability and every stderr line.
struct ReviewServer {
    child: Child,
    capability: String,
    stderr_lines: Arc<Mutex<Vec<String>>>,
}

impl ReviewServer {
    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for ReviewServer {
    fn drop(&mut self) {
        // A panicking test must still release the child process; an
        // orphaned server holds the stderr pipe and hangs the harness.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn `docx2typed review <workdir> --port N` and wait for readiness.
fn spawn_review_server(workdir: &Path, port: u16) -> ReviewServer {
    let mut child = Command::new(bin())
        .args([
            "review",
            workdir.to_str().expect("utf8"),
            "--port",
            &port.to_string(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn review server");
    let stderr = child.stderr.take().expect("stderr");
    let (tx, rx) = mpsc::channel::<String>();
    let lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let collected = Arc::clone(&lines);
    std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines() {
            let line = line.unwrap_or_default();
            if let Ok(mut guard) = collected.lock() {
                guard.push(line.clone());
            }
            let _ = tx.send(line);
        }
    });
    let capability = rx
        .iter()
        .find_map(|line| {
            line.split_once("#token=")
                .map(|(_, token)| token.trim().to_string())
        })
        .unwrap_or_else(|| {
            let _ = child.kill();
            panic!("review server never printed its capability URL")
        });
    // Wait until the listener is actually accepting (the URL is printed
    // after bind()).
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("review server never accepted connections");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    ReviewServer {
        child,
        capability,
        stderr_lines: lines,
    }
}

/// One raw HTTP exchange; returns (status, headers, body).
fn http(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (u16, BTreeMap<String, String>, Vec<u8>) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect review server");
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .expect("read timeout");
    let mut request = format!("{method} {path} HTTP/1.0\r\n");
    let mut has_length = false;
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("content-length") {
            has_length = true;
        }
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    if !body.is_empty() && !has_length {
        request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).expect("write request");
    if !body.is_empty() {
        stream.write_all(body).expect("write body");
    }
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    let head_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("response has a header block");
    let head = String::from_utf8_lossy(&response[..head_end]).into_owned();
    let status: u16 = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .expect("status line");
    let mut headers_out = BTreeMap::new();
    for line in head.lines().skip(1) {
        if let Some((name, value)) = line.split_once(':') {
            headers_out.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    (status, headers_out, response[head_end + 4..].to_vec())
}

fn host_header(port: u16) -> String {
    format!("127.0.0.1:{port}")
}

/// One authenticated same-origin publish POST (used by the concurrent
/// one-winner test; thread-safe since it only uses its parameters).
fn publish_request(
    port: u16,
    host: &str,
    origin: &str,
    auth: &str,
    key: &str,
    expected_generation: &str,
    expected_manifest: &str,
) -> (u16, BTreeMap<String, String>, Vec<u8>) {
    let headers: Vec<(&str, &str)> = vec![
        ("Host", host),
        ("Authorization", auth),
        ("Origin", origin),
        ("Sec-Fetch-Site", "same-origin"),
        ("Content-Type", "application/json"),
        ("Idempotency-Key", key),
    ];
    http(
        port,
        "POST",
        "/api/reviews/publish",
        &headers,
        &json!({
            "expected_generation": expected_generation,
            "expected_generation_manifest_sha256": expected_manifest,
            "expected_parent_snapshot": "C0",
            "changed_paragraph_ids": ["P0"],
            "origin": "human_ui",
        })
        .to_string()
        .into_bytes(),
    )
}

fn body_text(body: &[u8]) -> String {
    String::from_utf8_lossy(body).into_owned()
}
fn frame_identity(port: u16, host: &str, auth: &str) -> (String, String) {
    let (status, _, body) = http(
        port,
        "GET",
        "/api/review-frame",
        &[("Host", host), ("Authorization", auth)],
        b"",
    );
    assert_eq!(status, 200, "review frame: {}", body_text(&body));
    let frame: Value = serde_json::from_slice(&body).expect("review frame JSON");
    let identity = frame.get("identity").expect("review frame identity");
    (
        identity["generation"]
            .as_str()
            .expect("review frame generation")
            .to_string(),
        identity["generation_manifest_sha256"]
            .as_str()
            .expect("review frame manifest")
            .to_string(),
    )
}

fn security_headers() -> [&'static str; 5] {
    [
        "cache-control",
        "referrer-policy",
        "x-content-type-options",
        "x-frame-options",
        "content-security-policy",
    ]
}

// ---------------------------------------------------------------------------
// MCP tool surface
// ---------------------------------------------------------------------------

#[test]
fn mcp_tool_surface_advertises_the_agent_surface_and_serves_the_rest() {
    // Two questions, two answers, and both must hold:
    //   - what may an AGENT call? the five in AGENT_TOOLS, because fifty-three
    //     exposed tools reach 32.1% task success (ToolMenuBench) and 89.1% of
    //     interface failures are misselection (ToolRobustBench). See
    //     docs/specs/08_agent_surface.md.
    //   - what does the ENGINE serve? every name in TOOL_NAMES, because the CLI
    //     and these suites call them by name.
    // This test used to assert tools/list equalled the 53-name frozen contract.
    // That contract is still the engine's schema set and still validates every
    // legacy tool; it is no longer what an agent is shown.
    let workdir = scratch("surface");
    extract(&fixture("plain"), &workdir);
    let (results, raw) = mcp(&[("tools/list", json!({}))]);
    assert_eq!(raw.len(), 1, "exactly one reply line: {raw:?}");
    let data = &envelope(&results[0])["data"];
    let tools = data["tools"].as_array().expect("tools array");

    let names: Vec<String> = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(
        names,
        docx2typed_protocol::AGENT_TOOLS,
        "tools/list advertises the agent surface, in order"
    );

    for tool in tools {
        let name = tool["name"].as_str().expect("tool name");
        let schema = &tool["inputSchema"];
        assert_eq!(schema["type"], "object", "{tool}");
        assert!(schema["properties"].is_object(), "{tool}");
        assert_eq!(
            schema["additionalProperties"], false,
            "{name} must refuse an unknown argument rather than ignore it"
        );
        let description = tool["description"].as_str().unwrap_or("");
        assert!(
            !description.trim().is_empty(),
            "{name} ships without a description; an agent has only names to guess from"
        );
    }

    // The engine's own surface is unchanged, and every one of its names still
    // dispatches: a name that is advertised nowhere but also cannot be called
    // would be a silent removal.
    let frozen: Value = serde_json::from_slice(
        &std::fs::read(repo_root().join(".mcp_schemas.json")).expect("schema asset"),
    )
    .expect("schema asset JSON");
    let frozen = frozen.as_object().expect("schema asset is an object");
    assert_eq!(
        frozen.len(),
        TOOL_NAMES.len(),
        "the engine still serves every frozen tool's schema"
    );
    for name in TOOL_NAMES {
        assert!(
            frozen.contains_key(name),
            "{name} lost its schema; it is still callable by name"
        );
    }
}
fn schema_smoke_value(name: &str, schema: &Value, workdir: &Path) -> Value {
    if name == "workdir" {
        return Value::String(workdir.to_string_lossy().into_owned());
    }
    if name == "operation_id" {
        return Value::String(format!("schema-smoke-{name}"));
    }
    if name == "output" {
        return Value::String(
            workdir
                .join("schema-smoke-output.docx")
                .to_string_lossy()
                .into_owned(),
        );
    }
    if name == "workdir_out" {
        return Value::String(
            workdir
                .join("schema-smoke-out")
                .to_string_lossy()
                .into_owned(),
        );
    }
    if name == "action" {
        return json!("accept");
    }
    if name == "event_ids" || name == "edits" {
        return json!([]);
    }
    if name == "after_id" {
        return json!("P0");
    }
    if name == "table_ref" {
        return json!("T0");
    }
    if name == "revision_key" || name == "expected_fingerprint" {
        return json!("schema-smoke");
    }

    if let Some(options) = schema.get("anyOf").and_then(Value::as_array) {
        if let Some(option) = options.iter().find(|option| option["type"] != "null") {
            return schema_smoke_value(name, option, workdir);
        }
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("array") => json!([]),
        Some("boolean") => json!(false),
        Some("integer") => json!(0),
        Some("object") => json!({}),
        Some("string") => json!(""),
        _ => Value::Null,
    }
}

fn schema_smoke_args(name: &str, schema: &Value, workdir: &Path) -> Value {
    let mut args = serde_json::Map::new();
    for required in schema["required"].as_array().into_iter().flatten() {
        let key = required.as_str().expect("required schema key");
        let property = &schema["properties"][key];
        args.insert(key.to_string(), schema_smoke_value(key, property, workdir));
    }
    if name == "workdir_open" {
        assert!(args.contains_key("workdir"));
    }
    Value::Object(args)
}

#[test]
fn mcp_tools_accept_schema_derived_minimal_arguments() {
    let workdir = scratch("schema-smoke");
    extract(&fixture("plain"), &workdir);
    let frozen: Value = serde_json::from_slice(
        &std::fs::read(repo_root().join(".mcp_schemas.json")).expect("schema asset"),
    )
    .expect("schema asset JSON");
    let mut requests = Vec::new();
    for &name in &TOOL_NAMES {
        let schema = &frozen[name];
        requests.push((name, schema_smoke_args(name, schema, &workdir)));
    }

    let (results, raw) = mcp(&requests);
    assert_eq!(
        raw.len(),
        TOOL_NAMES.len(),
        "one stdout line per tool: {raw:?}"
    );
    assert_eq!(
        results.len(),
        TOOL_NAMES.len(),
        "every tool returned JSON: {raw:?}"
    );
    for (name, result) in TOOL_NAMES.iter().zip(results.iter()) {
        if *name == "engine_info" {
            assert_eq!(
                result["schema"], "docx2typed-engine-descriptor-1",
                "{name}: {result}"
            );
            continue;
        }
        let result = envelope(result);
        assert_eq!(result["schema"], "docx2typed-result-1", "{name}: {result}");
        assert!(result["outcome"].is_string(), "{name}: {result}");
        for diagnostic in result["diagnostics"].as_array().into_iter().flatten() {
            let code = diagnostic["code"].as_str().unwrap_or("");
            assert!(
                !matches!(
                    code,
                    "operation-id-missing"
                        | "unknown-argument"
                        | "invalid-argument"
                        | "invalid-arguments"
                        | "unknown-tool"
                ),
                "{name} produced a parameter-contract failure: {result}"
            );
        }
    }
}

#[test]
fn mcp_history_list_reports_the_version_timeline() {
    let workdir = scratch("history");
    extract(&fixture("plain"), &workdir);
    let wd = workdir.to_string_lossy().into_owned();
    let (results, raw) = mcp(&[
        ("history_list", json!({})),
        ("workdir_open", json!({ "workdir": wd })),
        ("history_list", json!({ "limit": 5, "offset": 0 })),
    ]);
    assert_eq!(raw.len(), 3, "one reply per request: {raw:?}");

    // Nothing is open yet: the read-only tool refuses with the frozen code.
    let closed = envelope(&results[0]);
    assert_eq!(closed["outcome"], "failure", "{closed}");
    assert_eq!(closed["diagnostics"][0]["code"], "workdir-not-open");

    // An extracted workdir has never been saved, so the timeline is empty.
    let data = &envelope(&results[2])["data"];
    assert_eq!(data["schema"], "docx2typed-history-1");
    assert_eq!(data["total"], 0);
    assert_eq!(data["retained"], 0);
    assert_eq!(data["trimmed"], 0);
    assert_eq!(data["offset"], 0);
    assert_eq!(data["current"], Value::Null);
    assert_eq!(data["current_tree"], Value::Null);
    assert_eq!(data["versions"].as_array().map(Vec::len), Some(0));
    assert_eq!(data["version_dirty"], true, "never saved counts as dirty");
    assert_eq!(data["draft_dirty"], false);
}

#[test]
fn mcp_stdio_is_clean_and_one_workdir_is_enforced() {
    let workdir = scratch("stdio");
    extract(&fixture("plain"), &workdir);
    let wd = workdir.to_string_lossy().into_owned();
    let (results, raw) = mcp(&[
        ("engine_info", json!({})),
        ("workdir_open", json!({ "workdir": wd })),
        ("workdir_open", json!({ "workdir": wd })),
        ("workdir_status", json!({})),
        ("list_paragraphs", json!({})),
        ("review_state", json!({})),
        ("no_such_tool", json!({})),
    ]);
    assert_eq!(
        results.len(),
        6,
        "6 OK replies (engine_info + open + open-fail + status + list + review_state): {raw:?}"
    );
    for line in &raw {
        assert!(
            line.starts_with("OK ") || line.starts_with("ERR "),
            "stdio purity: every stdout line is OK/ERR: {line:?}"
        );
    }
    let second_open = envelope(&results[2]);
    assert_eq!(second_open["outcome"], "failure");
    assert_eq!(
        second_open["diagnostics"][0]["code"],
        "workdir-already-open"
    );
    let status = &envelope(&results[3])["data"];
    assert_eq!(status["state"], "clean");
    let paragraphs = &envelope(&results[4])["data"]["paragraphs"];
    assert!(!paragraphs.as_array().unwrap().is_empty());
    assert!(raw.last().unwrap().starts_with("ERR unknown tool"));
}

#[test]
fn mcp_draft_lifecycle_with_replay_and_changed_set() {
    let workdir = scratch("draft");
    extract(&fixture("plain"), &workdir);
    let wd = workdir.to_string_lossy().into_owned();
    let (results, _) = mcp(&[
        ("workdir_open", json!({ "workdir": wd })),
        ("get_paragraph", json!({ "paragraph_id": "P0" })),
        (
            "replace_text",
            json!({ "paragraph_id": "P0", "old": "本发明", "new": "我们发明", "operation_id": "op-replace-1" }),
        ),
        ("diff_preview", json!({})),
        (
            "insert_paragraph",
            json!({ "after_id": "P2", "text": "插入的段落", "operation_id": "op-insert-1" }),
        ),
        (
            "delete_paragraph",
            json!({ "paragraph_id": "P3", "operation_id": "op-delete-1" }),
        ),
        ("commit_sync", json!({ "operation_id": "op-commit-1" })),
        (
            "replace_text",
            json!({ "paragraph_id": "P0", "old": "本发明", "new": "我们发明", "operation_id": "op-replace-1" }),
        ),
        (
            "replace_text",
            json!({ "paragraph_id": "P0", "old": "本发明", "new": "不同输入", "operation_id": "op-replace-1" }),
        ),
        ("diff_preview", json!({})),
        ("revert", json!({ "operation_id": "op-revert-1" })),
        ("workdir_status", json!({})),
    ]);
    let get = &envelope(&results[1])["data"];
    let body = get["plain"].as_str().unwrap_or("").to_string();
    assert!(body.contains("本发明"), "P0 body: {body}");
    let replace = &envelope(&results[2])["data"];
    assert_eq!(replace["draft"], "dirty");
    let diff = &envelope(&results[3])["data"];
    assert_eq!(diff["state"], "dirty");
    assert_eq!(
        diff["changed_paragraph_ids"],
        json!(["P0"]),
        "changed-set detection"
    );
    let inserted = &envelope(&results[4])["data"];
    let temp_id = inserted["temp_id"].as_str().unwrap_or("").to_string();
    assert!(temp_id.starts_with('N'));
    let commit = &envelope(&results[6])["data"];
    let mut got_changed = commit["changed_paragraph_ids"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect::<Vec<_>>();
    got_changed.sort();
    // The draft temp id is draft-local bookkeeping: the commit reallocates it
    // to the next free `P<n>` (mirror of Python's `_next_paragraph_id`) and
    // reports THAT. So the set is the two edited baseline paragraphs plus one
    // freshly allocated id, and the temp id never appears.
    assert_eq!(got_changed.len(), 3, "commit changed set: {got_changed:?}");
    assert!(got_changed.contains(&"P0".to_string()), "{got_changed:?}");
    assert!(got_changed.contains(&"P3".to_string()), "{got_changed:?}");
    let allocated = got_changed
        .iter()
        .find(|id| **id != "P0" && **id != "P3")
        .expect("one freshly allocated paragraph id");
    assert!(
        allocated.starts_with('P') && allocated != "P0" && allocated != "P3",
        "allocated id must be a fresh P-number, got {allocated}"
    );
    assert!(
        !got_changed.contains(&temp_id),
        "the draft temp id {temp_id} must not survive into the commit"
    );
    assert_eq!(commit["current_snapshot"]["id"], "C1");
    assert_eq!(commit["state"], "clean");
    // Replay: same operation_id + same args returns the original envelope.
    assert_eq!(
        envelope(&results[7]),
        envelope(&results[2]),
        "byte-exact replay"
    );
    // Changed args with the same operation_id fail closed.
    let reused = envelope(&results[8]);
    assert_eq!(reused["outcome"], "failure");
    assert_eq!(reused["diagnostics"][0]["code"], "operation-id-reused");
    // After commit the draft is clean and the diff is empty.
    let diff_after = &envelope(&results[9])["data"];
    assert_eq!(diff_after["state"], "clean");
    assert_eq!(diff_after["changes"], json!([]));
    // Revert discards the draft; status is clean again.
    let revert = &envelope(&results[10])["data"];
    assert_eq!(revert["state"], "clean");
    let status = &envelope(&results[11])["data"];
    assert_eq!(status["state"], "clean");
}

// ---------------------------------------------------------------------------
// Review HTTP server
// ---------------------------------------------------------------------------

#[test]
fn review_server_bootstrap_and_authority_gates() {
    let workdir = scratch("http-gates");
    extract(&fixture("plain"), &workdir);
    let port = free_port();
    let mut server = spawn_review_server(&workdir, port);
    let capability = server.capability.clone();
    let host = host_header(port);
    let auth = format!("Bearer {capability}");
    let mut results: Vec<(String, bool)> = Vec::new();
    let mut record = |label: &str, ok: bool| results.push((label.to_string(), ok));

    // Zero-data bootstrap shell: public but Host-bound, carries no
    // document/review/session data and never the capability.
    let (status, headers, body) = http(port, "GET", "/", &[("Host", host.as_str())], b"");
    record(
        "shell-200",
        status == 200
            && headers
                .get("content-type")
                .unwrap_or(&String::new())
                .starts_with("text/html"),
    );
    let shell = body_text(&body);
    // The real console is same-origin static assets; the token remains only
    // in the fragment and is stripped before these requests are made.
    record(
        "shell-console-entry",
        shell.contains(r#"<script src="review.js"></script>"#)
            && !shell.contains(capability.as_str()),
    );
    let (status, headers, body) = http(port, "GET", "/review.css", &[("Host", host.as_str())], b"");
    record(
        "console-css",
        status == 200
            && headers
                .get("content-type")
                .unwrap_or(&String::new())
                .starts_with("text/css")
            && body_text(&body).contains(".document-paper"),
    );
    let (status, headers, body) = http(port, "GET", "/review.js", &[("Host", host.as_str())], b"");
    record(
        "console-js",
        status == 200
            && headers
                .get("content-type")
                .unwrap_or(&String::new())
                .starts_with("text/javascript")
            && body_text(&body).contains("docx2typed-review-frame-1"),
    );
    let (status, _, _) = http(
        port,
        "GET",
        "/review.js",
        &[("Host", "evil.example:80")],
        b"",
    );
    record("console-host-bound-404", status == 404);
    let (status, _, _) = http(port, "GET", "/", &[("Host", "evil.example:80")], b"");
    record("shell-host-bound-404", status == 404);
    let (status, _, _) = http(port, "GET", "/", &[], b"");
    record("shell-no-host-404", status == 404);

    // Uniform detail-free 404s for every authority failure.
    let probes: Vec<(&str, Vec<(&str, &str)>)> = vec![
        ("no-auth", vec![("Host", host.as_str())]),
        (
            "bad-token",
            vec![("Host", host.as_str()), ("Authorization", "Bearer wrong")],
        ),
        (
            "wrong-host",
            vec![
                ("Host", "evil.example:80"),
                ("Authorization", auth.as_str()),
            ],
        ),
        (
            "portless-host",
            vec![("Host", "127.0.0.1"), ("Authorization", auth.as_str())],
        ),
        (
            "unknown-route",
            vec![("Host", host.as_str()), ("Authorization", auth.as_str())],
        ),
    ];
    let mut bodies = std::collections::HashSet::new();
    for (label, headers) in &probes {
        let path = if *label == "unknown-route" {
            "/api/no-such-route"
        } else {
            "/api/reviews"
        };
        let (status, _, body) = http(port, "GET", path, headers, b"");
        let text = body_text(&body);
        record(
            &format!("uniform-404-{label}"),
            status == 404 && text == r#"{"error":"not-found"}"#,
        );
        bodies.insert(text);
    }
    record("uniform-404-identical", bodies.len() == 1);

    // Health is gated too (it is a protected read route).
    let (status, _, _) = http(port, "GET", "/health", &[("Host", host.as_str())], b"");
    record("health-gated-404", status == 404);
    let (status, _, body) = http(
        port,
        "GET",
        "/health",
        &[("Host", host.as_str()), ("Authorization", auth.as_str())],
        b"",
    );
    record(
        "health-authed-200",
        status == 200 && body_text(&body).contains("\"ok\":true"),
    );

    // Security headers on every response; never CORS.
    for (label, headers) in &probes {
        let (_, response_headers, _) = http(port, "GET", "/api/reviews", headers, b"");
        let mut ok = true;
        for name in security_headers() {
            ok &= response_headers.contains_key(name);
        }
        ok &= !response_headers
            .iter()
            .any(|(name, _)| name.starts_with("access-control"));
        record(&format!("sec-headers-{label}"), ok);
    }
    let (_, headers, _) = http(port, "GET", "/", &[("Host", host.as_str())], b"");
    record(
        "shell-sec-headers",
        security_headers()
            .iter()
            .all(|name| headers.contains_key(*name)),
    );

    server.stop();
    let failed: Vec<&str> = results
        .iter()
        .filter(|(_, ok)| !ok)
        .map(|(label, _)| label.as_str())
        .collect();
    assert!(
        failed.is_empty(),
        "review_server_bootstrap_and_authority_gates failed: {failed:?}"
    );
}

#[test]
fn review_server_mutation_gates_and_replay() {
    let workdir = scratch("http-mutation");
    extract(&fixture("plain"), &workdir);
    let port = free_port();
    let mut server = spawn_review_server(&workdir, port);
    let capability = server.capability.clone();
    let host = host_header(port);
    let auth = format!("Bearer {capability}");
    let origin = format!("http://{host}");
    let mut results: Vec<(String, bool)> = Vec::new();
    let mut record = |label: &str, ok: bool| results.push((label.to_string(), ok));

    let (expected_generation, expected_manifest) = frame_identity(port, &host, &auth);
    let payload = json!({
        "type": "comment",
        "expected_generation": expected_generation,
        "expected_generation_manifest_sha256": expected_manifest,
        "client_id": "http-client-1",
        "paragraph_id": "P0",
        "selected_text": "snippet",
        "note": "please check this region",
    });
    let body_bytes = payload.to_string().into_bytes();
    let write_headers = |key: &str| -> Vec<(&str, String)> {
        vec![
            ("Host", host.clone()),
            ("Authorization", auth.clone()),
            ("Origin", origin.clone()),
            ("Sec-Fetch-Site", "same-origin".to_string()),
            ("Content-Type", "application/json".to_string()),
            ("Idempotency-Key", key.to_string()),
        ]
    };
    let send_write = |key: &str, body: &[u8]| {
        let headers = write_headers(key);
        let header_refs: Vec<(&str, &str)> = headers
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        http(port, "POST", "/api/reviews", &header_refs, body)
    };

    // Origin gates (after the capability gate).
    let (status, _, body) = http(
        port,
        "POST",
        "/api/reviews",
        &[
            ("Host", host.as_str()),
            ("Authorization", auth.as_str()),
            ("Content-Type", "application/json"),
            ("Idempotency-Key", "key-no-origin-1"),
        ],
        &body_bytes,
    );
    record(
        "post-no-origin-403",
        status == 403 && body_text(&body).contains("origin-mismatch"),
    );
    let (status, _, body) = http(
        port,
        "POST",
        "/api/reviews",
        &[
            ("Host", host.as_str()),
            ("Authorization", auth.as_str()),
            ("Origin", "http://evil.example"),
            ("Sec-Fetch-Site", "same-origin"),
            ("Content-Type", "application/json"),
            ("Idempotency-Key", "key-bad-origin-1"),
        ],
        &body_bytes,
    );
    record(
        "post-bad-origin-403",
        status == 403 && body_text(&body).contains("origin-mismatch"),
    );
    let (status, _, body) = http(
        port,
        "POST",
        "/api/reviews",
        &[
            ("Host", host.as_str()),
            ("Authorization", auth.as_str()),
            ("Origin", origin.as_str()),
            ("Content-Type", "application/json"),
            ("Idempotency-Key", "key-no-fetch-1"),
        ],
        &body_bytes,
    );
    record(
        "post-no-fetch-site-403",
        status == 403 && body_text(&body).contains("fetch-site-mismatch"),
    );

    // Idempotency-Key syntax gate.
    let (status, _, body) = http(
        port,
        "POST",
        "/api/reviews",
        &[
            ("Host", host.as_str()),
            ("Authorization", auth.as_str()),
            ("Origin", origin.as_str()),
            ("Sec-Fetch-Site", "same-origin"),
            ("Content-Type", "application/json"),
        ],
        &body_bytes,
    );
    record(
        "post-missing-key-400",
        status == 400 && body_text(&body).contains("idempotency"),
    );

    // Content-type gate.
    let mut bad_ct = write_headers("key-bad-ct-1");
    bad_ct[4] = ("Content-Type", "text/plain".to_string());
    let header_refs: Vec<(&str, &str)> = bad_ct
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();
    let (status, _, body) = http(port, "POST", "/api/reviews", &header_refs, &body_bytes);
    record(
        "post-bad-content-type-400",
        status == 400 && body_text(&body).contains("unsupported-content-type"),
    );

    // Size cap: a >256 KiB body is rejected cleanly (400, connection kept).
    let big = vec![b'x'; 300 * 1024];
    let (status, _, body) = send_write("key-oversize-1", &big);
    record(
        "post-oversized-400",
        status == 400 && body_text(&body).contains("too-large"),
    );

    // A valid same-origin write succeeds and merges counts + session.
    let (status, _, body) = send_write("key-write-1", &body_bytes);
    let text = body_text(&body);
    record(
        "post-good-200",
        status == 200 && text.contains("\"counts\"") && text.contains("\"session\""),
    );
    let first = body.clone();
    // Idempotency-Key replay is byte-exact.
    let (status, _, body) = send_write("key-write-1", &body_bytes);
    record("replay-byte-exact", status == 200 && body == first);

    // The event is durable in the store: GET /api/reviews shows it.
    let (status, _, body) = http(
        port,
        "GET",
        "/api/reviews",
        &[("Host", host.as_str()), ("Authorization", auth.as_str())],
        b"",
    );
    let text = body_text(&body);
    record(
        "event-persisted",
        status == 200 && text.contains("http-client-1"),
    );

    // Unsupported method on a gated route: uniform 501 with headers.
    let (status, headers, body) = http(
        port,
        "PUT",
        "/api/reviews",
        &[("Host", host.as_str()), ("Authorization", auth.as_str())],
        b"",
    );
    record(
        "put-501",
        status == 501
            && body_text(&body) == r#"{"error":"method-not-supported"}"#
            && security_headers()
                .iter()
                .all(|name| headers.contains_key(*name)),
    );

    server.stop();
    let failed: Vec<&str> = results
        .iter()
        .filter(|(_, ok)| !ok)
        .map(|(label, _)| label.as_str())
        .collect();
    assert!(
        failed.is_empty(),
        "review_server_mutation_gates_and_replay failed: {failed:?}"
    );
}

#[test]
fn review_server_throttle_and_redacted_logs() {
    let workdir = scratch("http-throttle");
    extract(&fixture("plain"), &workdir);
    let port = free_port();
    let mut server = spawn_review_server(&workdir, port);
    let capability = server.capability.clone();
    let host = host_header(port);
    let mut statuses = Vec::new();
    for _ in 0..24 {
        let (status, _, body) = http(
            port,
            "GET",
            "/api/reviews",
            &[("Host", host.as_str()), ("Authorization", "Bearer wrong")],
            b"",
        );
        assert!(
            body_text(&body) == r#"{"error":"not-found"}"#,
            "throttled bodies stay uniform"
        );
        statuses.push(status);
    }
    let not_found = statuses.iter().filter(|status| **status == 404).count();
    let throttled = statuses.iter().filter(|status| **status == 429).count();
    assert_eq!(
        not_found + throttled,
        24,
        "every probe is 404 or 429: {statuses:?}"
    );
    assert!(
        not_found >= 1 && throttled >= 1,
        "bucket exhausts: {statuses:?}"
    );
    // Authorized requests are never throttled, even right after a flood.
    let (status, _, _) = http(
        port,
        "GET",
        "/api/reviews",
        &[
            ("Host", host.as_str()),
            ("Authorization", format!("Bearer {capability}").as_str()),
        ],
        b"",
    );
    assert_eq!(status, 200, "authorized request never consults the bucket");
    drop(statuses);

    // Redacted logs: every access-log line has the sanitized fields and
    // never the capability.
    server.stop();
    let stderr = {
        let guard = server.stderr_lines.lock().expect("stderr lines");
        guard.join("\n")
    };
    let log_lines: Vec<&str> = stderr
        .lines()
        .filter(|line| line.contains("[review-server]"))
        .collect();
    assert!(!log_lines.is_empty(), "access log lines exist");
    let mut saw_capability = 0usize;
    for line in &log_lines {
        if line.contains(&capability) {
            saw_capability += 1;
        }
        assert!(line.contains("cat="), "log has category: {line}");
        assert!(line.contains("result="), "log has result class: {line}");
        assert!(line.contains("src="), "log has source class: {line}");
        assert!(line.contains("sid="), "log has session hash: {line}");
    }
    assert_eq!(saw_capability, 0, "capability never appears in access logs");
}

#[test]
fn review_server_concurrent_publish_one_winner_and_restart_revocation() {
    let workdir = scratch("http-cas");
    extract(&fixture("plain"), &workdir);
    let port = free_port();
    let mut server = spawn_review_server(&workdir, port);
    let capability = server.capability.clone();
    let host = host_header(port);
    let origin = format!("http://{host}");
    let auth = format!("Bearer {capability}");
    let (bootstrap_generation, bootstrap_manifest) = frame_identity(port, &host, &auth);

    // Bootstrap the session first: C0 must pin the ORIGINAL typed.md so
    // the publish below observes a canonical change. A benign mutation
    // runs the post-mutation merge (document_state -> ensure_session).
    let (status, _, _) = http(
        port,
        "POST",
        "/api/reviews",
        &[
            ("Host", host.as_str()),
            ("Authorization", auth.as_str()),
            ("Origin", origin.as_str()),
            ("Sec-Fetch-Site", "same-origin"),
            ("Content-Type", "application/json"),
            ("Idempotency-Key", "key-bootstrap-1"),
        ],
        &json!({
            "type": "comment",
            "expected_generation": bootstrap_generation,
            "expected_generation_manifest_sha256": bootstrap_manifest,
            "client_id": "bootstrap",
            "paragraph_id": "P0",
            "selected_text": "x",
            "note": "bootstraps the session",
        })
        .to_string()
        .into_bytes(),
    );
    assert_eq!(status, 200, "session bootstrap mutation");

    // Change typed.md behind the session (the human publish lane publishes
    // canonical changes; the test owns the workdir).
    let typed_path = workdir.join("typed.md");
    let mut typed = std::fs::read_to_string(&typed_path).expect("typed.md");
    typed.push_str("<!--@p id=\"P0\"/>\nchanged by test\n");
    std::fs::write(&typed_path, typed).expect("write typed.md");

    // Two concurrent publishes use the same frame generation: exactly one
    // wins the CAS; the other is a stale-frame rejection.
    let (publish_generation, publish_manifest) = frame_identity(port, &host, &auth);
    let (host1, origin1, auth1) = (host.clone(), origin.clone(), auth.clone());
    let generation1 = publish_generation.clone();
    let manifest1 = publish_manifest.clone();
    let handle1 = std::thread::spawn(move || {
        publish_request(
            port,
            &host1,
            &origin1,
            &auth1,
            "key-publish-a1",
            &generation1,
            &manifest1,
        )
    });
    let (host2, origin2, auth2) = (host.clone(), origin.clone(), auth.clone());
    let handle2 = std::thread::spawn(move || {
        publish_request(
            port,
            &host2,
            &origin2,
            &auth2,
            "key-publish-a2",
            &publish_generation,
            &publish_manifest,
        )
    });
    let (status1, _, body1) = handle1.join().expect("thread 1");
    let (status2, _, body2) = handle2.join().expect("thread 2");
    let winners = [status1, status2]
        .iter()
        .filter(|status| **status == 200)
        .count();
    let losers = [status1, status2]
        .iter()
        .filter(|status| **status == 409)
        .count();
    assert_eq!(
        winners, 1,
        "exactly one concurrent publish wins: {status1} {status2}"
    );
    assert_eq!(
        losers, 1,
        "the loser fails the CAS with 409: {status1} {status2}"
    );
    let winner_body = if status1 == 200 { &body1 } else { &body2 };
    let winner: Value = serde_json::from_slice(winner_body).expect("winner json");
    assert_eq!(winner["current_snapshot"]["id"], "C1");
    let loser_body = if status1 == 409 { &body1 } else { &body2 };
    let loser: Value = serde_json::from_slice(loser_body).expect("loser json");
    assert_eq!(loser["code"], "stale-review-frame");

    // Restart: the old capability is revoked, the store state survives.
    server.stop();
    let mut server2 = spawn_review_server(&workdir, port);
    let old_auth = auth.clone();
    let (status, _, body) = http(
        port,
        "GET",
        "/api/reviews",
        &[
            ("Host", host.as_str()),
            ("Authorization", old_auth.as_str()),
        ],
        b"",
    );
    assert_eq!(status, 404, "old capability revoked after restart");
    assert_eq!(body_text(&body), r#"{"error":"not-found"}"#);
    let new_auth = format!("Bearer {}", server2.capability);
    let (status, _, body) = http(
        port,
        "GET",
        "/api/reviews",
        &[
            ("Host", host.as_str()),
            ("Authorization", new_auth.as_str()),
        ],
        b"",
    );
    assert_eq!(status, 200, "new capability works");
    let text = body_text(&body);
    assert!(
        text.contains("\"current_snapshot\""),
        "session state survived: {text}"
    );
    assert!(
        text.contains("C1"),
        "published snapshot survived restart: {text}"
    );
    server2.stop();
}

#[test]
fn review_server_settlement_and_wake_retry() {
    let workdir = scratch("http-settle");
    extract(&fixture("revisions"), &workdir);
    let port = free_port();
    let mut server = spawn_review_server(&workdir, port);
    let capability = server.capability.clone();
    let host = host_header(port);
    let origin = format!("http://{host}");
    let auth = format!("Bearer {capability}");
    let post = |path: &str, key: &str, payload: &Value| -> (u16, Vec<u8>) {
        let (generation, manifest) = frame_identity(port, &host, &auth);
        let mut framed = payload.clone();
        framed["expected_generation"] = json!(generation);
        framed["expected_generation_manifest_sha256"] = json!(manifest);
        let headers: Vec<(&str, String)> = vec![
            ("Host", host.clone()),
            ("Authorization", auth.clone()),
            ("Origin", origin.clone()),
            ("Sec-Fetch-Site", "same-origin".to_string()),
            ("Content-Type", "application/json".to_string()),
            ("Idempotency-Key", key.to_string()),
        ];
        let header_refs: Vec<(&str, &str)> = headers
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        let (status, _, body) = http(
            port,
            "POST",
            path,
            &header_refs,
            &framed.to_string().into_bytes(),
        );
        (status, body)
    };

    // Wake: dispatch with nothing staged is a clean empty batch.
    let (status, body) = post("/api/reviews/dispatch", "key-dispatch-1", &json!({}));
    assert_eq!(status, 200);
    assert!(
        body_text(&body).contains("\"events\":[]"),
        "empty dispatch: {}",
        body_text(&body)
    );

    // Stage one accept decision (frozen revision key from revisions.docx)
    // and one defer, then dispatch them into one batch.
    let accept = json!({
        "type": "decision",
        "client_id": "reviewer-accept",
        "paragraph_id": "P0",
        "decision": "accept",
        "revision_key": "word/document.xml|insert|1|888c104169b5",
        "revision_id": "1",
        "selected_text": "已插入内容",
        "comment": "",
    });
    let defer = json!({
        "type": "decision",
        "client_id": "reviewer-defer",
        "paragraph_id": "P2",
        "decision": "defer",
        "revision_key": "word/document.xml|insert|3|e3b0c44298fc",
        "revision_id": "3",
        "selected_text": "",
        "comment": "hold for now",
    });
    let (status, body) = post("/api/reviews", "key-decision-accept", &accept);
    assert_eq!(status, 200, "accept staged: {}", body_text(&body));
    let (status, body) = post("/api/reviews", "key-decision-defer", &defer);
    assert_eq!(status, 200, "defer staged: {}", body_text(&body));
    let (status, body) = post("/api/reviews/dispatch", "key-dispatch-2", &json!({}));
    assert_eq!(status, 200);
    let text = body_text(&body);
    assert_eq!(
        text.matches("\"status\":\"queued\"").count(),
        2,
        "both decisions queued: {text}"
    );
    // Wake retry: a second dispatch moves nothing new.
    let (status, body) = post("/api/reviews/dispatch", "key-dispatch-3", &json!({}));
    assert_eq!(status, 200);
    assert!(
        body_text(&body).contains("\"events\":[]"),
        "second wake is a no-op"
    );

    // Settle: the accept applies to the template, the defer carries forward.
    let (status, body) = post("/api/reviews/settle", "key-settle-1", &json!({}));
    assert_eq!(status, 200, "settle failed: {}", body_text(&body));
    let settled: Value = serde_json::from_slice(&body).expect("settle json");
    assert_eq!(settled["review_base"]["id"], "S1", "review base advances");
    assert_eq!(
        settled["decisions"].as_array().unwrap().len(),
        1,
        "one accept decision"
    );
    assert_eq!(settled["decisions"][0]["w_id"], "1");
    // Accept of an insertion UNWRAPS the revision (the text becomes live
    // content); the revision markup is removed from the template.
    assert_eq!(settled["decisions"][0]["operation"], "unwrap");
    assert_eq!(
        settled["carry_forward"].as_array().unwrap().len(),
        1,
        "defer carries forward"
    );
    assert_eq!(settled["carry_forward"][0]["review_decision"], "defer");
    // The template was actually settled: the accepted `<w:ins>` wrapper is
    // gone (exactly one fewer than the original fixture).
    let count_inserts = |package: &[u8]| -> usize {
        let xml = String::from_utf8_lossy(
            &docx2typed_core::govern::document_xml_bytes(package).expect("document xml"),
        )
        .into_owned();
        xml.matches("<w:ins ").count() + xml.matches("<w:ins>").count()
    };
    let fixture_inserts = count_inserts(&std::fs::read(fixture("revisions")).expect("fixture"));
    let template = std::fs::read(workdir.join("_template.docx")).expect("template");
    let template_inserts = count_inserts(&template);
    assert_eq!(
        template_inserts,
        fixture_inserts - 1,
        "one insert revision unwrapped: {template_inserts} vs {fixture_inserts}"
    );
    let decisions: Value = serde_json::from_slice(
        &std::fs::read(workdir.join("decisions.json")).expect("decisions.json"),
    )
    .expect("decisions json");
    assert_eq!(decisions["decisions"][0]["action"], "accept");
    assert_eq!(decisions["decisions"][0]["operation"], "unwrap");

    server.stop();
}

// ---------------------------------------------------------------------------
// Unknown arguments are rejected, not dropped.
// ---------------------------------------------------------------------------

/// Every published schema says `additionalProperties: false`; nothing enforced
/// it, so an unrecognised key was silently ignored and the call reported
/// success with the default applied (#41). The worked examples are the ones
/// that misled a real caller: `build_docx(out=...)` wrote a differently-named
/// file, and `insert_paragraph(after=...)` reported a missing *paragraph* for
/// a misspelled *argument*.
#[test]
fn unknown_arguments_are_rejected_and_near_misses_name_the_intent() {
    let cases: [(&str, Value, &str); 4] = [
        (
            "build_docx",
            json!({"out": "X.docx", "operation_id": "1"}),
            "output",
        ),
        (
            "build_docx",
            json!({"outptu": "X.docx", "operation_id": "1"}),
            "output",
        ),
        (
            "insert_paragraph",
            json!({"after": "P0", "text": "x", "operation_id": "1"}),
            "after_id",
        ),
        ("history_list", json!({"limt": 5}), "limit"),
    ];
    for (tool, args, expected) in cases {
        let (results, _) = mcp(&[(tool, args)]);
        let envelope = envelope(&results[0]);
        assert_eq!(
            envelope["outcome"], "failure",
            "{tool} must reject an unknown argument, not apply a default"
        );
        let diagnostics = envelope["diagnostics"].as_array().expect("diagnostics");
        assert_eq!(
            diagnostics[0]["code"], "invalid-arguments",
            "{tool}: wrong code"
        );
        let message = diagnostics[0]["message"].as_str().expect("message");
        assert!(
            message.contains(expected),
            "{tool}: the diagnostic must name the parameter the caller meant ({expected}); got {message}"
        );
    }
}

/// The guard must not reject a call that only uses declared parameters — the
/// failure mode of a validation added late is that it breaks the happy path.
#[test]
fn declared_arguments_still_dispatch() {
    let (results, _) = mcp(&[
        ("tools/list", json!({})),
        ("engine_info", json!({})),
        ("workdir_status", json!({})),
        ("history_verify", json!({})),
    ]);
    for (index, result) in results.iter().enumerate() {
        let envelope = envelope(result);
        assert_ne!(
            envelope["diagnostics"][0]["code"], "invalid-arguments",
            "declared-only call #{index} must not be rejected: {envelope}"
        );
    }
}

// ---------------------------------------------------------------------------
// format_span: the run-property lane (#33)
// ---------------------------------------------------------------------------

/// The tool's refusals, each with the code that names WHICH refusal fired.
/// Flattening them into one code would hide the difference between "the
/// document cannot express that" and "you addressed text that is not there".
#[test]
fn format_span_refusals_name_themselves() {
    let workdir = scratch("format-refusals");
    extract(&fixture("styled"), &workdir);
    let wd = workdir.to_string_lossy().into_owned();
    let open = ("workdir_open", json!({ "workdir": wd }));

    let cases: [(&str, Value, &str); 4] = [
        (
            "unknown paragraph",
            json!({"paragraph_id": "P99", "old": "x", "attributes": {"bold": true}, "operation_id": "1"}),
            "paragraph-not-found",
        ),
        (
            "text not found",
            json!({"paragraph_id": "P0", "old": "not-in-this-document", "attributes": {"bold": true}, "operation_id": "2"}),
            "text-not-found",
        ),
        (
            "style the document lacks",
            json!({"paragraph_id": "P0", "old": "前缀", "attributes": {"italic": true}, "operation_id": "3"}),
            "format-style-unavailable",
        ),
        (
            "both attributes and style_id",
            json!({"paragraph_id": "P0", "old": "前缀", "attributes": {"italic": true}, "style_id": "s_x", "operation_id": "4"}),
            "format-invalid-attribute",
        ),
    ];
    for (label, args, expected) in cases {
        let (results, _) = mcp(&[open.clone(), ("format_span", args)]);
        let envelope = envelope(&results[1]);
        assert_eq!(
            envelope["outcome"], "failure",
            "{label} must refuse: {envelope}"
        );
        assert_eq!(
            envelope["diagnostics"][0]["code"], expected,
            "{label}: wrong code; got {envelope}"
        );
    }
}

/// Tracked formatting is refused loudly rather than silently restyling in
/// place -- a `track: true` call that produced an invisible change would be
/// the worst outcome for a reviewer.
#[test]
fn format_span_refuses_tracked_mode_loudly() {
    let workdir = scratch("format-track");
    extract(&fixture("styled"), &workdir);
    let wd = workdir.to_string_lossy().into_owned();
    let (results, _) = mcp(&[
        ("workdir_open", json!({ "workdir": wd })),
        (
            "format_span",
            json!({"paragraph_id": "P0", "old": "前缀", "attributes": {"bold": true}, "track": true, "operation_id": "1"}),
        ),
    ]);
    let envelope = envelope(&results[1]);
    assert_eq!(envelope["outcome"], "failure");
    assert_eq!(
        envelope["diagnostics"][0]["code"],
        "format-tracked-unsupported"
    );
}

/// The lane works end to end on a real document: address text, restyle it to a
/// style the document carries, build, and see the properties in the output.
#[test]
fn format_span_restyles_a_span_and_the_build_applies_it() {
    let workdir = scratch("format-happy");
    extract(&fixture("styled"), &workdir);
    let wd = workdir.to_string_lossy().into_owned();

    // The document carries a Times New Roman style; restyle P0 to it.
    let styles: Value = serde_json::from_str(
        &fs::read_to_string(workdir.join("styles.json")).expect("styles.json"),
    )
    .expect("styles json");
    let target = styles["styles"]
        .as_object()
        .expect("styles")
        .iter()
        .find(|(_, entry)| entry["label"] == "Times New Roman")
        .map(|(id, _)| id.clone())
        .expect("a Times New Roman style");

    let (results, _) = mcp(&[
        ("workdir_open", json!({ "workdir": wd })),
        (
            "format_span",
            json!({"paragraph_id": "P0", "old": "前缀", "style_id": target, "operation_id": "1"}),
        ),
        (
            "build_docx",
            json!({ "output": workdir.join("out.docx").to_string_lossy(), "operation_id": "2" }),
        ),
    ]);
    assert_eq!(
        envelope(&results[1])["outcome"],
        "success",
        "{}",
        results[1]
    );
    assert_eq!(
        envelope(&results[2])["outcome"],
        "success",
        "{}",
        results[2]
    );

    // The record is on disk and the built document carries the properties.
    let record: Value = serde_json::from_str(
        &fs::read_to_string(workdir.join("rpr_edits.json")).expect("rpr_edits.json"),
    )
    .expect("record json");
    assert_eq!(record["edits"][0]["paragraph_id"], "P0");
    assert_eq!(record["edits"][0]["style_id"], target);

    let bytes = fs::read(workdir.join("out.docx")).expect("built docx");
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("zip");
    let mut xml = String::new();
    std::io::Read::read_to_string(
        &mut archive.by_name("word/document.xml").expect("document.xml"),
        &mut xml,
    )
    .expect("read");
    assert!(
        xml.contains("Times New Roman"),
        "the built document must carry the restyled properties"
    );
}
#[test]
fn document_replace_atomic_scope_and_dry_run() {
    let workdir = scratch("replace-atomic");
    extract(&fixture("plain"), &workdir);
    let (results, _) = mcp(&[
        (
            "workdir_open",
            json!({ "workdir": workdir.to_string_lossy() }),
        ),
        (
            "document_replace",
            json!({
                "find": "段落",
                "replace": "章节",
                "dry_run": true,
                "operation_id": "rep-dry",
            }),
        ),
        (
            "document_replace",
            json!({
                "find": "段落",
                "replace": "章节",
                "operation_id": "rep-apply",
            }),
        ),
        (
            "document_replace",
            json!({
                "find": "不存在的词",
                "replace": "X",
                "operation_id": "rep-none",
            }),
        ),
    ]);
    assert_eq!(envelope(&results[0])["outcome"], "success");
    let dry = &envelope(&results[1])["data"];
    assert_eq!(dry["dry_run"], true);
    assert_eq!(dry["changed"], 0);
    assert!(dry["matches"].as_u64().unwrap() >= 1);

    let applied = &envelope(&results[2])["data"];
    assert_eq!(applied["changed"], applied["matches"]);

    let none = &envelope(&results[3])["data"];
    assert_eq!(none["matches"], 0);
    assert_eq!(none["changed"], 0);
    assert_eq!(none["noop_reason"], "no-match");
}
