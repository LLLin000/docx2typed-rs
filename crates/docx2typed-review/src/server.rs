//! Single-session review HTTP server (issue #60) — std-threaded mirror of
//! `scripts/review_server.py` (issue #51 security contract).
//!
//! One process-scoped 256-bit capability is generated at startup and held
//! only in memory; restarting the server revokes it while the store-backed
//! workdir state (sessions, snapshots, journals, generations) survives.
//! Every protected route passes the Host allowlist + capability gate; every
//! authority failure is a byte-identical `{"error":"not-found"}` 404, with
//! a bounded per-client token bucket upgrading repeated probes to 429.
//! Mutating POSTs additionally require a same-origin browser write (Origin
//! equals an advertised origin + `Sec-Fetch-Site: same-origin`), a
//! `application/json` body capped at 256 KiB, and an Idempotency-Key
//! (8-128 `[A-Za-z0-9_-]`) that replays byte-exact through the store
//! ledger. Every response carries the strict security header set and never
//! a CORS header. Unsupported methods that pass the gate get a uniform 501.
//! Logging is redacted: category, result class, source class, and an
//! irreversible truncated session hash only.
//!
//! No tokio: one std thread per connection, bounded by a socket read
//! timeout so stalled clients release their thread.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use docx2typed_protocol::{canonical_operation_input, new_operation_id, resolve_path, utc_now_iso};
use docx2typed_store::store::{Store, StoreMutateRequest};
use serde_json::{json, Value};

use crate::collab::{
    document_state, external_write_guard, publish_current, settle_decisions, stage_patch,
    CollaborationError,
};
use crate::queue::{dispatch, snapshot, upsert_event};
use crate::security::{
    build_allowlist, content_type_allowed, generate_capability, session_fingerprint,
    ReviewSecurity, UnauthorizedThrottle, CONTENT_SECURITY_POLICY, NOT_FOUND_BODY,
};

const MAX_BODY: usize = 256 * 1024;
const SOCKET_TIMEOUT_SECS: u64 = 30;
const UNSUPPORTED_BODY: &str = r#"{"error":"method-not-supported"}"#;

/// Stable per-code details for store failures surfaced to the browser
/// (never absolute paths, drive letters, or temp filenames) — mirror of
/// `_STORE_ERROR_DETAIL`.
fn store_error_detail(code: &str) -> Option<&'static str> {
    match code {
        "store-invalid" => Some("store state is invalid; inspect the workdir for recovery status"),
        "writer-busy" => Some("another writer is active; retry after it finishes"),
        "writer-timeout" => Some("writer lane timed out; retry after the current writer finishes"),
        "generation-conflict" => {
            Some("workdir changed since planning; re-read the workdir and retry")
        }
        "needs-recovery" => Some("workdir needs recovery; run the recovery pass before retrying"),
        "reserve-depleted" => {
            Some("workdir recovery reserve is depleted; the workdir is read-only")
        }
        "unsupported-by-design" => {
            Some("filesystem does not meet the store durability requirements")
        }
        "operation-journal-conflict" => {
            Some("operation journal conflict; retry with a fresh operation id")
        }
        "stale-review-frame" => {
            Some("the review frame is stale; re-read /api/review-frame and retry")
        }
        "workdir-unreadable" => Some("workdir cannot be opened as a store-backed workdir"),
        _ => None,
    }
}

/// One mutation failure surfaced to the browser (code + stable detail).
#[derive(Clone, Debug)]
pub struct MutationError {
    pub code: String,
    pub detail: String,
}

impl MutationError {
    pub fn new(code: impl Into<String>, detail: impl Into<String>) -> Self {
        MutationError {
            code: code.into(),
            detail: detail.into(),
        }
    }
}

impl From<CollaborationError> for MutationError {
    fn from(error: CollaborationError) -> Self {
        MutationError::new(error.code, error.detail)
    }
}

/// One mutation failure body (`{"error": detail, "code": code}`).
fn error_payload(code: &str, detail: &str) -> Value {
    json!({ "error": detail, "code": code })
}

/// The zero-data bootstrap shell: a static console page carrying no
/// document, review, session, or workdir data. The capability rides only
/// in the URL fragment printed at startup; CSS and JavaScript are separate
/// same-origin assets so the strict CSP stays enabled.
const BOOTSTRAP_SHELL: &str = include_str!("../assets/index.html");
const REVIEW_CSS: &str = include_str!("../assets/review.css");
const REVIEW_JS: &str = include_str!("../assets/review.js");

/// The zero-data shell, rendered per bind (Host-bound like the Python
/// `render_html(server_mode=True)` shell; still carries no workdir data).
fn shell_payload() -> Vec<u8> {
    BOOTSTRAP_SHELL.as_bytes().to_vec()
}

/// A parsed HTTP request (the minimal surface the server needs).
struct HttpRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// The per-process review session context shared by every handler thread.
struct ReviewSession {
    security: ReviewSecurity,
    throttle: Mutex<UnauthorizedThrottle>,
    /// Serializes every store mutation in this process. The store Writer
    /// lane already guarantees cross-process one-writer fail-fast; this
    /// in-process mutex additionally guarantees a concurrent HTTP POST
    /// never races the store's read paths (open/probe/pin) against an
    /// active mutation, so the loser deterministically fails the CAS
    /// (current-parent-mismatch / generation-conflict) instead of tearing
    /// store reads.
    mutation_lock: Mutex<()>,
    workdir: PathBuf,
    shell: Vec<u8>,
    mutation_routes: [&'static str; 6],
}

impl ReviewSession {
    /// Bounded per-client unauthorized-request throttle (Python default:
    /// capacity 12, refill 1/s, 4096 clients).
    fn throttle() -> UnauthorizedThrottle {
        UnauthorizedThrottle::new(12.0, 1.0, 4096)
    }

    /// Host + capability gate for every protected route; failures are the
    /// uniform 404 (or 429 under throttle pressure). Returns true when the
    /// request is authorized.
    fn gate(&self, request: &HttpRequest, respond: &mut Response) -> bool {
        let host = request.header("Host").unwrap_or("").to_string();
        let authorization = request.header("Authorization").unwrap_or("");
        let (scheme, token) = match authorization.split_once(' ') {
            Some((scheme, token)) => (scheme, token),
            None => ("", ""),
        };
        if self.security.host_allowed(&host)
            && scheme.eq_ignore_ascii_case("bearer")
            && self.security.verify(token)
        {
            return true;
        }
        self.deny(respond);
        false
    }

    /// Uniform detail-free authority refusal with bounded throttling.
    fn deny(&self, respond: &mut Response) {
        let allowed = {
            let mut throttle = self.throttle.lock().expect("throttle mutex");
            throttle.allow(respond.client())
        };
        if allowed {
            respond.json(404, NOT_FOUND_BODY);
        } else {
            respond.json_with(429, NOT_FOUND_BODY, &[("Retry-After", "1")]);
        }
    }

    /// Browser-origin gates for mutating POSTs (after the capability gate).
    fn gate_mutation(&self, request: &HttpRequest, respond: &mut Response) -> bool {
        let origin = request.header("Origin").unwrap_or("");
        if origin.is_empty() || !self.security.origin_allowed(origin) {
            respond.json(403, r#"{"error":"forbidden","code":"origin-mismatch"}"#);
            return false;
        }
        if request.header("Sec-Fetch-Site") != Some("same-origin") {
            respond.json(403, r#"{"error":"forbidden","code":"fetch-site-mismatch"}"#);
            return false;
        }
        true
    }
}

/// One HTTP response being assembled (security headers always present).
struct Response {
    status: u16,
    content_type: &'static str,
    payload: Vec<u8>,
    extra_headers: Vec<(&'static str, String)>,
    head: bool,
    client: String,
}

impl Response {
    fn new(head: bool, client: &str) -> Self {
        Response {
            status: 200,
            content_type: "application/json; charset=utf-8",
            payload: Vec::new(),
            extra_headers: Vec::new(),
            head,
            client: client.to_string(),
        }
    }

    fn client(&self) -> &str {
        &self.client
    }

    fn json(&mut self, status: u16, body: &str) {
        self.json_with(status, body, &[]);
    }

    fn json_with(&mut self, status: u16, body: &str, extra: &[(&'static str, &str)]) {
        self.status = status;
        self.content_type = "application/json; charset=utf-8";
        self.payload = body.as_bytes().to_vec();
        for (name, value) in extra {
            self.extra_headers.push((*name, value.to_string()));
        }
    }

    fn html(&mut self, status: u16, body: Vec<u8>) {
        self.status = status;
        self.content_type = "text/html; charset=utf-8";
        self.payload = body;
    }
    fn asset(&mut self, status: u16, content_type: &'static str, body: &[u8]) {
        self.status = status;
        self.content_type = content_type;
        self.payload = body.to_vec();
    }

    /// Write the response with the full security header set. Never CORS.
    fn write(&self, stream: &mut TcpStream) {
        let mut head = String::new();
        head.push_str(&format!(
            "HTTP/1.0 {} {}\r\n",
            self.status,
            reason(self.status)
        ));
        head.push_str(&format!("Content-Type: {}\r\n", self.content_type));
        head.push_str(&format!("Content-Length: {}\r\n", self.payload.len()));
        head.push_str("Cache-Control: no-store\r\n");
        head.push_str("Referrer-Policy: no-referrer\r\n");
        head.push_str("X-Content-Type-Options: nosniff\r\n");
        head.push_str("X-Frame-Options: DENY\r\n");
        head.push_str(&format!(
            "Content-Security-Policy: {CONTENT_SECURITY_POLICY}\r\n"
        ));
        for (name, value) in &self.extra_headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        let _ = stream.write_all(head.as_bytes());
        if !self.head {
            let _ = stream.write_all(&self.payload);
        }
        let _ = stream.flush();
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        _ => "Unknown",
    }
}

/// Read the request line + headers + body from one connection, bounded by
/// the socket timeout. `None` means the connection should be dropped
/// silently (stalled or malformed).
fn read_request(stream: &mut TcpStream) -> Option<HttpRequest> {
    let mut reader = std::io::BufReader::new(stream.try_clone().ok()?);
    let mut header_bytes = Vec::with_capacity(1024);
    // Body bytes that arrived in the same TCP segment as the header block:
    // they were consumed by the BufReader with the headers, so keep them
    // instead of truncating them away (a lost body would drop the request).
    let body_prefix: Vec<u8>;
    let mut buffer = [0u8; 1024];
    loop {
        let read = reader.read(&mut buffer).ok()?;
        if read == 0 {
            return None;
        }
        header_bytes.extend_from_slice(&buffer[..read]);
        if header_bytes.len() > 64 * 1024 {
            return None;
        }
        if let Some(index) = find_header_end(&header_bytes) {
            body_prefix = header_bytes.split_off(index + 4);
            break;
        }
    }
    let text = String::from_utf8_lossy(&header_bytes);
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or("").to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
    if method.is_empty() || !path.starts_with('/') {
        return None;
    }
    // Body: bounded by the size cap + 1 (a lying Content-Length is never
    // followed beyond the cap).
    let length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = body_prefix;
    if length > 0 && body.len() < length {
        let remaining = length - body.len();
        let mut limited = (&mut reader).take(remaining.min(MAX_BODY + 1) as u64);
        let _ = limited.read_to_end(&mut body);
        // Drain the declared remainder (bounded) so closing the connection
        // never resets a peer that is still sending a rejected oversized
        // body; the body itself stays capped at MAX_BODY + 1.
        if body.len() < length {
            let mut sink = (&mut reader).take((length - body.len()).min(4 * 1024 * 1024) as u64);
            let _ = std::io::copy(&mut sink, &mut std::io::sink());
        }
    } else if length == 0 {
        // A body without Content-Length is ignored (mirror of the Python
        // server, which reads only the declared length).
        body.clear();
    }
    Some(HttpRequest {
        method,
        path,
        headers,
        body,
    })
}

/// Locate the end of the header block (`\r\n\r\n`).
fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

/// Parse a request path into its path component (query dropped).
fn path_only(raw: &str) -> &str {
    match raw.split_once('?') {
        Some((path, _)) => path,
        None => raw,
    }
}

/// The idempotency-key syntax mirror (`^[A-Za-z0-9_-]{8,128}$`).
fn valid_idempotency_key(key: &str) -> bool {
    (8..=128).contains(&key.len())
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// Read and validate a JSON object body (content type + size + parse).
fn read_json_body(request: &HttpRequest) -> Result<Value, MutationError> {
    let content_type = request.header("Content-Type").unwrap_or("");
    if request.body.is_empty() {
        return Err(MutationError::new(
            "request-body-required",
            "this endpoint requires a JSON object body",
        ));
    }
    if !content_type_allowed(content_type) {
        return Err(MutationError::new(
            "unsupported-content-type",
            "only application/json bodies are accepted",
        ));
    }
    if request.body.len() > MAX_BODY {
        return Err(MutationError::new(
            "request-body-too-large",
            "request body exceeds 256 KiB",
        ));
    }
    let value: Value = serde_json::from_slice(&request.body)
        .map_err(|_| MutationError::new("invalid-json-body", "request body is not valid JSON"))?;
    if !value.is_object() {
        return Err(MutationError::new(
            "request-body-not-object",
            "request body must be an object",
        ));
    }
    Ok(value)
}

/// The mandatory Idempotency-Key for mutating POSTs.
fn idempotency_key(request: &HttpRequest) -> Result<String, MutationError> {
    let key = request.header("Idempotency-Key").unwrap_or("");
    if !valid_idempotency_key(key) {
        return Err(MutationError::new(
            "idempotency-key-required",
            "Idempotency-Key header required for mutating POSTs (8-128 chars of [A-Za-z0-9_-])",
        ));
    }
    Ok(key.to_string())
}

/// Map a store failure onto the mutation surface. The store's own
/// `generation-conflict` (the CAS failing on `StoreMutateRequest.
/// expected_generation`) is the same client condition as the review layer's
/// early frame comparison, so it surfaces with the same `stale-review-frame`
/// code — the client re-reads the frame and retries either way.
pub fn mutation_from_store(error: &docx2typed_store::StoreError) -> MutationError {
    let code = error.code().unwrap_or("server-error").to_string();
    let code = if code == "generation-conflict" {
        "stale-review-frame".to_string()
    } else {
        code
    };
    let detail = match store_error_detail(&code) {
        Some(stable) => stable.to_string(),
        None => error.message().to_string(),
    };
    MutationError::new(code, detail)
}

/// Run one mutation through the immutable-generation store (Writer lane,
/// CAS, durable journal, recovery, materialize) and return the response
/// data. `operation_id` + canonical args replay byte-exact through the
/// ledger (identical input returns the original committed data; changed
/// input fails operation-id-reused). Legacy callers without a client frame
/// expectation pass through here; the HTTP layer uses
/// `store_mutation_framed` so the client's expected generation is carried
/// into the same CAS.
pub fn store_mutation(
    workdir: &Path,
    operation: &str,
    operation_id: &str,
    canonical_args: &Value,
    run: impl FnOnce(&Path) -> Result<Value, MutationError> + Send + 'static,
) -> Result<Value, MutationError> {
    store_mutation_framed(
        workdir,
        operation,
        operation_id,
        canonical_args,
        "",
        "",
        run,
    )
}

/// Attach the committed generation metadata resolved from the store ledger
/// (never guessed from the current root): the generation directory whose
/// ledger holds this operation's record, and that generation's manifest.
/// Omitted when the ledger cannot resolve it (birth transactions never
/// write a ledger record; the response then simply lacks the metadata and
/// the client re-reads the frame).
fn attach_committed_generation(
    store: &docx2typed_store::Store,
    operation_id: &str,
    data: &mut Value,
) {
    if let Some((generation, manifest)) = store.committed_generation(operation_id) {
        data["committed_generation"] = json!({
            "generation": generation,
            "generation_manifest_sha256": manifest,
        });
    }
}

/// Run one review mutation with the client's frame expectation (atomic
/// review frame contract). Order, deliberately:
///
/// 1. Read-side gates only — no writer work: open the store, resolve the
///    Idempotency-Key replay from the ledger FIRST. The key is a retry
///    identity, never a concurrency token: a byte-exact replay returns the
///    original committed data even when the client's expected generation is
///    now stale (a timed-out retry must not be punished with 409).
/// 2. Pin the current generation and compare the client's
///    `expected_generation` (and optional manifest) → mismatch fails with
///    409 `stale-review-frame` before any store generation, queue, or
///    history write.
/// 3. Run the mutation with the client expectation carried into
///    `StoreMutateRequest.expected_generation` / `input_sha256`, so the
///    store's CAS enforces the same constraint under the Writer lane — no
///    check-then-act window between the comparison and the commit.
/// 4. Attach the committed generation metadata resolved from the store
///    ledger so history records and the client's frame can advance.
pub fn store_mutation_framed(
    workdir: &Path,
    operation: &str,
    operation_id: &str,
    canonical_args: &Value,
    expected_generation: &str,
    expected_manifest_sha256: &str,
    run: impl FnOnce(&Path) -> Result<Value, MutationError> + Send + 'static,
) -> Result<Value, MutationError> {
    store_mutation_boundary(
        workdir,
        operation,
        operation_id,
        canonical_args,
        expected_generation,
        expected_manifest_sha256,
        false,
        run,
    )
}

/// A mutation that IS a save boundary, and so may record a Version (#37).
///
/// Only the tools the reference marks with `tx.mark_save_boundary` belong
/// here: `commit_sync`, `history_restore`, `decide_all` and the table ops.
/// Anything else must not, or a draft edit would mint a version.
pub fn store_save_boundary(
    workdir: &Path,
    operation: &str,
    operation_id: &str,
    canonical_args: &Value,
    run: impl FnOnce(&Path) -> Result<Value, MutationError> + Send + 'static,
) -> Result<Value, MutationError> {
    store_mutation_boundary(
        workdir,
        operation,
        operation_id,
        canonical_args,
        "",
        "",
        true,
        run,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn store_mutation_boundary(
    workdir: &Path,
    operation: &str,
    operation_id: &str,
    canonical_args: &Value,
    expected_generation: &str,
    expected_manifest_sha256: &str,
    save_boundary: bool,
    run: impl FnOnce(&Path) -> Result<Value, MutationError> + Send + 'static,
) -> Result<Value, MutationError> {
    let canonical = canonical_operation_input(operation, canonical_args);
    let backed = docx2typed_store::has_store(workdir);
    let mut expected = expected_generation.to_string();
    let mut manifest = expected_manifest_sha256.to_string();

    let store = if backed {
        let store = Store::open(workdir).map_err(|error| mutation_from_store(&error))?;
        // (1) byte-exact Idempotency-Key replay wins over staleness.
        let (record, _corrupt) = store
            .lookup_ledger(operation_id, true, None, true)
            .map_err(|error| mutation_from_store(&error))?;
        if let Some(record) = record {
            if record.get("input_sha256").and_then(Value::as_str) == Some(canonical.as_str()) {
                let mut data = record
                    .get("envelope")
                    .and_then(|envelope| envelope.get("data"))
                    .cloned()
                    .ok_or_else(|| {
                        MutationError::new("server-error", "ledger envelope missing data")
                    })?;
                attach_committed_generation(&store, operation_id, &mut data);
                return Ok(data);
            }
            return Err(MutationError::new(
                "operation-id-reused",
                format!(
                    "operation_id {operation_id} was already used with different canonical input"
                ),
            ));
        }
        // (2) pin + compare the client expectation.
        let pin = store.pin().map_err(|error| mutation_from_store(&error))?;
        if !expected.is_empty() && expected != pin.generation {
            return Err(MutationError::new(
                "stale-review-frame",
                "the review frame is stale; re-read /api/review-frame and retry",
            ));
        }
        if !manifest.is_empty() && manifest != pin.manifest_sha256.clone().unwrap_or_default() {
            return Err(MutationError::new(
                "stale-review-frame",
                "the review frame is stale (generation manifest changed); re-read /api/review-frame and retry",
            ));
        }
        // No client expectation (legacy callers): pin the current
        // generation, exactly like the pre-frame store_mutation.
        if expected.is_empty() {
            expected = pin.generation.clone();
        }
        if manifest.is_empty() {
            manifest = pin.manifest_sha256.clone().unwrap_or_default();
        }
        store
    } else {
        // Legacy non-store workdir: no generation exists, so a client frame
        // cannot name one. The first mutation births generation 0; the
        // committed metadata in the response teaches the client the new
        // generation identity.
        if !expected.is_empty() || !manifest.is_empty() {
            return Err(MutationError::new(
                "stale-review-frame",
                "the workdir is not generation-backed (frame reported backed:false); no generation to compare",
            ));
        }
        let store = Store::ensure(workdir, operation_id, &canonical)
            .map_err(|error| mutation_from_store(&error))?;
        let pin = store.pin().map_err(|error| mutation_from_store(&error))?;
        expected = pin.generation.clone();
        if manifest.is_empty() {
            manifest = pin.manifest_sha256.clone().unwrap_or_default();
        }
        store
    };

    let mut run = Some(run);
    let request = StoreMutateRequest {
        workdir: workdir.to_path_buf(),
        operation: operation.to_string(),
        operation_id: operation_id.to_string(),
        canonical,
        input_sha256: manifest,
        expected_generation: expected,
        generation: true,
        save_boundary,
        ledger_anchor: None,
        ledger_directory: true,
        evidence_path: None,
        kind: "mutation".to_string(),
        lock_timeout_ms: 0,
        run: Box::new(
            move |target: &Path, _tx: &mut docx2typed_store::Transaction| {
                let run = run.take().expect("run closure called once");
                let data = run(target).map_err(|error| {
                    docx2typed_store::StoreError::store(&error.code, &error.detail)
                })?;
                Ok(docx2typed_store::RunOutcome {
                    outcome: "success".to_string(),
                    data,
                    kind: "mutation".to_string(),
                    payload: json!({"checks": [{"name": "mutation", "status": "pass"}]}),
                    diagnostics: vec![],
                })
            },
        ),
    };
    let envelope = store
        .mutate(request)
        .map_err(|error| mutation_from_store(&error))?;
    let mut data = envelope
        .get("data")
        .cloned()
        .ok_or_else(|| MutationError::new("server-error", "store envelope missing data"))?;
    // (4) committed generation metadata from the store ledger.
    attach_committed_generation(&store, operation_id, &mut data);
    Ok(data)
}

/// Run one review mutation through the immutable-generation store. `key` is
/// the gate-validated Idempotency-Key; replay semantics come from
/// `store_mutation_framed` (the key is a retry identity, never a
/// concurrency token — the client's `expected_generation` is the frame
/// token, required on store-backed workdirs and compared against the same
/// pinned generation the store CAS enforces).
fn post_mutation(
    session: &ReviewSession,
    key: &str,
    path: &str,
    payload: &Value,
    expected_generation: &str,
    expected_manifest_sha256: &str,
    run: impl FnOnce(&Path) -> Result<Value, MutationError> + Send + 'static,
) -> Result<Value, MutationError> {
    // A store-backed workdir has a generation identity; a frame-dependent
    // mutation without one cannot be CAS-bound. Legacy non-store workdirs
    // correctly send null (nothing to compare until the first mutation
    // births generation 0).
    if docx2typed_store::has_store(&session.workdir) && expected_generation.is_empty() {
        return Err(MutationError::new(
            "invalid-arguments",
            "expected_generation is required for store-backed review frames",
        ));
    }
    let _serial = session.mutation_lock.lock().expect("review mutation mutex");
    store_mutation_framed(
        &session.workdir,
        "review_post",
        key,
        &json!({"path": path, "payload": payload}),
        expected_generation,
        expected_manifest_sha256,
        run,
    )
}

/// The client frame token from a frame-dependent mutation payload:
/// `expected_generation` is required (string or null); the manifest hash is
/// optional (string or null). Returns ("", "") for null values.
fn expected_generation_from(payload: &Value) -> Result<(String, String), MutationError> {
    let generation = match payload.get("expected_generation") {
        None => {
            return Err(MutationError::new(
                "invalid-arguments",
                "expected_generation is required for frame-dependent mutations",
            ))
        }
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) => String::new(),
        Some(_) => {
            return Err(MutationError::new(
                "invalid-arguments",
                "expected_generation must be a string or null",
            ))
        }
    };
    let manifest = match payload.get("expected_generation_manifest_sha256") {
        None => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) => String::new(),
        Some(_) => {
            return Err(MutationError::new(
                "invalid-arguments",
                "expected_generation_manifest_sha256 must be a string or null",
            ))
        }
    };
    Ok((generation, manifest))
}

/// Handle one connection. Returns when the request is fully served.
fn handle_connection(session: &ReviewSession, stream: &mut TcpStream, client: &str) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(SOCKET_TIMEOUT_SECS)));
    let request = match read_request(stream) {
        Some(request) => request,
        None => return, // stalled or malformed: drop silently
    };
    let method = request.method.as_str();
    let head = method == "HEAD";
    let mut respond = Response::new(head, client);
    let category = match method {
        "GET" | "HEAD" => {
            let path = path_only(&request.path);
            if path == "/" {
                "bootstrap"
            } else if path == "/health" || path.starts_with("/api/") {
                "read"
            } else {
                "unknown"
            }
        }
        "POST" if session.mutation_routes.contains(&path_only(&request.path)) => "write",
        "POST" => "unknown",
        _ => "unknown",
    };
    match method {
        "GET" | "HEAD" => route_read(session, &request, &mut respond),
        "POST" => route_post(session, &request, &mut respond),
        _ => method_not_supported(session, &request, &mut respond),
    }
    respond.write(stream);
    log_line(
        category,
        respond.status,
        client,
        request.header("Authorization").unwrap_or(""),
    );
}

/// Unsupported methods pass the same Host+capability gate and throttle as
/// every other route: gated requests get the uniform 501, unauthorized ones
/// the byte-identical 404, both with the full security header set.
fn method_not_supported(session: &ReviewSession, request: &HttpRequest, respond: &mut Response) {
    if !session.gate(request, respond) {
        return;
    }
    respond.json(501, UNSUPPORTED_BODY);
}

/// Pin the current generation once for a partial read (item 3): store-backed
/// workdirs read from the pinned immutable generation directory and report
/// its identity; legacy workdirs read the root with a null identity. On a
/// degenerate store (needs recovery) the read falls back to `read_root`
/// exactly like the pre-frame server — partial reads stay resilient while
/// `/api/review-frame` fails closed.
fn pinned_read(session: &ReviewSession) -> (PathBuf, Option<String>, Option<String>) {
    if !docx2typed_store::has_store(&session.workdir) {
        return (session.workdir.clone(), None, None);
    }
    match Store::open(&session.workdir).and_then(|store| store.pin()) {
        Ok(pin) => (pin.path, Some(pin.generation), pin.manifest_sha256),
        Err(_) => (docx2typed_store::read_root(&session.workdir), None, None),
    }
}

/// Parse `?history=<opaque-history-id>` from a raw request path (the query
/// is dropped by `path_only`, so the frame route reads it here).
fn history_query(raw: &str) -> Option<String> {
    raw.split_once('?').and_then(|(_, query)| {
        query.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            (key == "history" && !value.is_empty()).then(|| value.to_string())
        })
    })
}

/// GET/HEAD routing: the bootstrap shell is public but Host-bound; every
/// API read pins the current immutable generation and never mutates.
fn route_read(session: &ReviewSession, request: &HttpRequest, respond: &mut Response) {
    let path = path_only(&request.path);
    if matches!(path, "/" | "/review.css" | "/review.js") {
        if !session
            .security
            .host_allowed(request.header("Host").unwrap_or(""))
        {
            session.deny(respond);
            return;
        }
        match path {
            "/" => respond.html(200, session.shell.clone()),
            "/review.css" => respond.asset(200, "text/css; charset=utf-8", REVIEW_CSS.as_bytes()),
            "/review.js" => {
                respond.asset(200, "text/javascript; charset=utf-8", REVIEW_JS.as_bytes())
            }
            _ => unreachable!("static route was matched above"),
        }
        return;
    }
    if !session.gate(request, respond) {
        return;
    }
    match path {
        "/api/review-frame" => {
            let history_id = history_query(&request.path);
            match crate::frame::review_frame(&session.workdir, history_id.as_deref()) {
                Ok(data) => respond.json(200, &serde_json::to_string(&data).expect("serializes")),
                Err(error) => {
                    let status = if error.code == "history-not-found" {
                        404
                    } else {
                        409
                    };
                    respond.json(
                        status,
                        &serde_json::to_string(&error_payload(&error.code, &error.detail))
                            .expect("serializes"),
                    );
                }
            }
        }
        "/api/reviews" => {
            let (root, generation, manifest) = pinned_read(session);
            let mut data = crate::queue::snapshot_readonly(&root);
            data["session"] = crate::collab::document_state_readonly(&root);
            data["generation"] = json!(generation);
            data["generation_manifest_sha256"] = json!(manifest);
            respond.json(200, &serde_json::to_string(&data).expect("serializes"));
        }
        "/api/document-state" => {
            let (root, generation, manifest) = pinned_read(session);
            let mut data = crate::collab::document_state_readonly(&root);
            data["generation"] = json!(generation);
            data["generation_manifest_sha256"] = json!(manifest);
            respond.json(200, &serde_json::to_string(&data).expect("serializes"));
        }
        "/api/review-history" => {
            let (root, generation, manifest) = pinned_read(session);
            let store = Store::new(&session.workdir);
            let mut data = crate::history::list(&root, &|generation| {
                store.generation_manifest_sha256(generation)
            });
            data["generation"] = json!(generation);
            data["generation_manifest_sha256"] = json!(manifest);
            respond.json(200, &serde_json::to_string(&data).expect("serializes"));
        }
        "/health" => {
            respond.json(200, r#"{"ok":true,"service":"docx2typed-review"}"#);
        }
        _ => {
            if let Some(history_id) = path.strip_prefix("/api/review-history/") {
                if !history_id.is_empty() {
                    let (root, generation, manifest) = pinned_read(session);
                    let store = Store::new(&session.workdir);
                    let record = crate::history::read(&root, history_id, &|generation| {
                        store.generation_manifest_sha256(generation)
                    });
                    match record {
                        Some(record) => {
                            let data = json!({
                                "schema": crate::history::HISTORY_SCHEMA,
                                "record": record,
                                "generation": generation,
                                "generation_manifest_sha256": manifest,
                            });
                            respond.json(200, &serde_json::to_string(&data).expect("serializes"));
                        }
                        None => respond.json(404, NOT_FOUND_BODY),
                    }
                    return;
                }
            }
            respond.json(404, NOT_FOUND_BODY);
        }
    }
}

/// POST routing: capability gate, mutation-route whitelist, same-origin
/// browser gate, then the idempotent store mutation.
fn route_post(session: &ReviewSession, request: &HttpRequest, respond: &mut Response) {
    let path = path_only(&request.path);
    if !session.gate(request, respond) {
        return;
    }
    if !session.mutation_routes.contains(&path) {
        respond.json(404, NOT_FOUND_BODY);
        return;
    }
    if !session.gate_mutation(request, respond) {
        return;
    }
    // The Idempotency-Key is validated before any body work so a malformed
    // key never reaches the store.
    let key = match idempotency_key(request) {
        Ok(key) => key,
        Err(error) => {
            respond.json(
                400,
                &serde_json::to_string(&error_payload(&error.code, &error.detail))
                    .expect("serializes"),
            );
            return;
        }
    };
    let result: Result<Value, MutationError> = route_mutation(session, path, request, &key);
    match result {
        Ok(data) => {
            respond.json(200, &serde_json::to_string(&data).expect("serializes"));
        }
        Err(error) => {
            let status = if error.code == "idempotency-key-required"
                || error.code.starts_with("request-body-")
                || error.code == "unsupported-content-type"
                || error.code == "invalid-json-body"
                || error.code == "request-body-not-object"
                || error.code == "invalid-arguments"
            {
                400
            } else {
                409
            };
            respond.json(
                status,
                &serde_json::to_string(&error_payload(&error.code, &error.detail))
                    .expect("serializes"),
            );
        }
    }
}

/// Dispatch one authorized mutation route. Every frame-dependent mutation
/// reads `expected_generation` (+ optional manifest) from the payload, so
/// the client's frame token binds the same CAS the store enforces — a
/// stale frame never reaches the queue, session, or history.
fn route_mutation(
    session: &ReviewSession,
    path: &str,
    request: &HttpRequest,
    key: &str,
) -> Result<Value, MutationError> {
    match path {
        "/api/reviews" => {
            let payload = read_json_body(request)?;
            let (expected_generation, expected_manifest) = expected_generation_from(&payload)?;
            let payload_for_run = payload.clone();
            let key = key.to_string();
            post_mutation(
                session,
                &key,
                path,
                &payload,
                &expected_generation,
                &expected_manifest,
                move |target| {
                    let mut data =
                        if payload_for_run.get("type").and_then(Value::as_str) == Some("patch") {
                            stage_patch(target, &payload_for_run)?
                        } else {
                            upsert_event(target, &payload_for_run)
                                .map_err(|error| MutationError::new("workdir-unreadable", error))?
                        };
                    merge_extra(target, &mut data);
                    Ok(data)
                },
            )
        }
        "/api/reviews/patch" => {
            let payload = read_json_body(request)?;
            let (expected_generation, expected_manifest) = expected_generation_from(&payload)?;
            let payload_for_run = payload.clone();
            let key = key.to_string();
            post_mutation(
                session,
                &key,
                path,
                &payload,
                &expected_generation,
                &expected_manifest,
                move |target| {
                    let mut data = stage_patch(target, &payload_for_run)?;
                    merge_extra(target, &mut data);
                    Ok(data)
                },
            )
        }
        "/api/reviews/dispatch" => {
            let payload = read_json_body(request)?;
            let (expected_generation, expected_manifest) = expected_generation_from(&payload)?;
            let key = key.to_string();
            post_mutation(
                session,
                &key,
                path,
                &payload,
                &expected_generation,
                &expected_manifest,
                move |target| {
                    let mut data = dispatch(target)
                        .map(|events| json!({ "events": events }))
                        .map_err(|error| MutationError::new("workdir-unreadable", error))?;
                    merge_extra(target, &mut data);
                    Ok(data)
                },
            )
        }
        "/api/reviews/external-preflight" => {
            let payload = read_json_body(request)?;
            let (expected_generation, expected_manifest) = expected_generation_from(&payload)?;
            let expected = payload
                .get("expected_parent_snapshot")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let operation = payload
                .get("operation")
                .and_then(Value::as_str)
                .unwrap_or("import")
                .to_string();
            let key = key.to_string();
            post_mutation(
                session,
                &key,
                path,
                &payload,
                &expected_generation,
                &expected_manifest,
                move |target| {
                    let data = external_write_guard(target, &expected, &operation)?;
                    Ok(data)
                },
            )
        }
        "/api/reviews/settle" => {
            let payload = read_json_body(request)?;
            let (expected_generation, expected_manifest) = expected_generation_from(&payload)?;
            let event_ids: Option<Vec<String>> = match payload.get("event_ids") {
                None => None,
                Some(Value::Array(ids)) => {
                    let mut out = Vec::new();
                    for id in ids {
                        match id.as_str() {
                            Some(text) => out.push(text.to_string()),
                            None => {
                                return Err(MutationError::new(
                                    "invalid-arguments",
                                    "event_ids must be a string array",
                                ))
                            }
                        }
                    }
                    Some(out)
                }
                Some(_) => {
                    return Err(MutationError::new(
                        "invalid-arguments",
                        "event_ids must be a string array",
                    ))
                }
            };
            let key = key.to_string();
            post_mutation(
                session,
                &key,
                path,
                &payload,
                &expected_generation,
                &expected_manifest,
                move |target| {
                    let mut data = settle_decisions(target, event_ids.as_deref())?;
                    merge_extra(target, &mut data);
                    Ok(data)
                },
            )
        }
        "/api/reviews/publish" => {
            let payload = read_json_body(request)?;
            let (expected_generation, expected_manifest) = expected_generation_from(&payload)?;
            let changed: Vec<String> = match payload.get("changed_paragraph_ids") {
                Some(Value::Array(ids)) => {
                    let mut out = Vec::new();
                    for id in ids {
                        match id.as_str() {
                            Some(text) => out.push(text.to_string()),
                            None => {
                                return Err(MutationError::new(
                                    "invalid-arguments",
                                    "changed_paragraph_ids must be a string array",
                                ))
                            }
                        }
                    }
                    out
                }
                _ => {
                    return Err(MutationError::new(
                        "invalid-arguments",
                        "changed_paragraph_ids must be a string array",
                    ))
                }
            };
            let expected_parent = payload
                .get("expected_parent_snapshot")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let origin = payload
                .get("origin")
                .and_then(Value::as_str)
                .unwrap_or("human_ui")
                .to_string();
            let batch_id = payload
                .get("batch_id")
                .and_then(Value::as_str)
                .map(str::to_string);
            let key = key.to_string();
            post_mutation(
                session,
                &key,
                path,
                &payload,
                &expected_generation,
                &expected_manifest,
                move |target| {
                    let mut data = publish_current(
                        target,
                        &expected_parent,
                        &origin,
                        &changed,
                        batch_id.as_deref(),
                    )?;
                    merge_extra(target, &mut data);
                    Ok(data)
                },
            )
        }
        _ => Err(MutationError::new("server-error", "unhandled route")),
    }
}

/// Merge the post-mutation extras (queue counts + fresh session state) into
/// the response data so the next review POST needs no extra GET.
fn merge_extra(target: &Path, data: &mut Value) {
    let counts = snapshot(target).get("counts").cloned().unwrap_or_default();
    data["counts"] = counts;
    let session = match document_state(target) {
        Ok(state) => state,
        Err(_) => Value::Null,
    };
    data["session"] = session;
}

/// Sanitized access log: timestamp, request category, result class, source
/// class, and an irreversible truncated session hash. Never logs
/// Authorization, fragment, query, body, or workdir paths.
fn log_line(category: &str, status: u16, client: &str, authorization: &str) {
    let result_class = format!("{}xx", status / 100);
    let source = if client == "127.0.0.1" || client == "::1" {
        "loopback"
    } else {
        "remote"
    };
    let token = authorization
        .split_once(' ')
        .map(|(_, token)| token)
        .unwrap_or("");
    let sid = if token.is_empty() {
        "-".to_string()
    } else {
        session_fingerprint(token)
    };
    eprintln!(
        "[review-server] {} cat={} result={} src={} sid={}",
        utc_now_iso(),
        category,
        result_class,
        source,
        sid
    );
}

/// Bind and serve one review session. The capability is generated here, so
/// every process start revokes the previous one while the store state
/// survives. Blocks until the listener fails or the process is killed.
pub fn serve(workdir: &Path, host: &str, port: u16) -> std::io::Result<()> {
    let workdir = resolve_path(workdir);
    if !workdir.join("typed.md").is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("not a typed workdir: {}", workdir.to_string_lossy()),
        ));
    }
    if !docx2typed_store::has_store(&workdir) {
        Store::ensure(&workdir, &new_operation_id(), "")
            .map_err(|error| std::io::Error::other(error.to_string()))?;
    }
    let (allowed_hosts, allowed_origins) = build_allowlist(host, port);
    let security = ReviewSecurity::new(
        generate_capability(),
        allowed_hosts,
        allowed_origins,
        port,
        if host == "127.0.0.1" || host == "localhost" {
            "loopback".to_string()
        } else {
            "tailnet".to_string()
        },
    );
    let session = Arc::new(ReviewSession {
        security,
        throttle: Mutex::new(ReviewSession::throttle()),
        mutation_lock: Mutex::new(()),
        workdir,
        shell: shell_payload(),
        mutation_routes: [
            "/api/reviews",
            "/api/reviews/patch",
            "/api/reviews/dispatch",
            "/api/reviews/settle",
            "/api/reviews/publish",
            "/api/reviews/external-preflight",
        ],
    });
    let listener = TcpListener::bind((host, port))?;
    // The full fragment URL is printed exactly once; later output shows only
    // the token-free origin.
    eprintln!(
        "review session: http://{host}:{port}/#token={}",
        session.security.capability
    );
    eprintln!("advertised origin: http://{host}:{port}/");
    eprintln!("capability is single-session and memory-only; restarting the server revokes it");
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(_) => continue,
        };
        let client = stream
            .peer_addr()
            .map(|address| address.ip().to_string())
            .unwrap_or_else(|_| "unknown".to_string());
        let session = Arc::clone(&session);
        std::thread::spawn(move || {
            let mut stream = stream;
            handle_connection(&session, &mut stream, &client);
        });
    }
    Ok(())
}
