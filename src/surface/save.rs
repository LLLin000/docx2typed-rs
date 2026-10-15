//! `document_save` — write the DOCX, and verify what was written.
//!
//! This is the only call that produces a file, and it does not trust its own
//! build: the output is verified against the workdir before success is reported.
//! A save that reports success while the file kept the old text is the failure
//! this whole line of work exists to remove, and it is not hypothetical — the
//! engine has a guard (`unrecorded-edits`) precisely because a build could
//! silently replay the template and drop committed edits.
//!
//! The caller is not asked to run the verification separately. There is exactly
//! one moment when it is meaningful — immediately after the write — and the
//! engine knows it.

use crate::mcp::McpSession;
use docx2typed_protocol::new_operation_id;
use serde_json::{json, Value};

/// `document_save` — build the output and verify it before saying so.
pub fn document_save(session: &mut McpSession, args: &Value, build_commit: &str) -> Value {
    let Some(output) = args.get("output").and_then(Value::as_str) else {
        return session.failure(
            "document_save",
            "invalid-arguments",
            "document_save requires `output`: the path to write",
            build_commit,
        );
    };
    if session.open_workdir().is_none() {
        return session.failure(
            "document_save",
            "workdir-not-open",
            "no document is open; call document_open first",
            build_commit,
        );
    }

    // A draft that has not been settled cannot be built (`edit-dirty`), and
    // settling it has exactly one correct answer, so the engine takes the step
    // rather than telling the caller to. This can only be non-empty for a
    // workdir a LEGACY tool left dirty — `document_edit` always settles — but an
    // agent should not have to know which tool left it that way.
    let settled = session.settle_draft(build_commit);
    if let Some(failure) = settled {
        return failure;
    }

    // The build transport requires a caller-supplied operation id; on this
    // surface the caller should not have to know that, and an id it never sees
    // is one it cannot reuse by mistake.
    let operation_id = new_operation_id();
    let built = session.build_docx(
        &json!({ "output": output, "operation_id": operation_id }),
        build_commit,
    );
    let Some(envelope) = built.get("structuredContent") else {
        return built;
    };
    if envelope.get("outcome").and_then(Value::as_str) != Some("success") {
        // The build refused, so nothing was written — or what was written is not
        // claimed. It already carries the reason and the recovery payload.
        return built;
    }

    // Verify what is on disk against the workdir. A disagreement is not a
    // warning: the file is not what the caller asked for, so the save did not
    // succeed.
    let verified = session.verify_output(&json!({ "output": output }), build_commit);
    let verify_envelope = verified
        .get("structuredContent")
        .cloned()
        .unwrap_or(Value::Null);
    if verify_envelope.get("outcome").and_then(Value::as_str) != Some("success") {
        let detail = verify_envelope
            .get("diagnostics")
            .and_then(Value::as_array)
            .and_then(|diagnostics| diagnostics.first())
            .map(|diagnostic| {
                format!(
                    "{}: {}",
                    diagnostic
                        .get("code")
                        .and_then(Value::as_str)
                        .unwrap_or("verify"),
                    diagnostic
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                )
            })
            .unwrap_or_else(|| "verification disagreed with the build".to_string());
        return session.failure(
            "document_save",
            "output-not-verified",
            &format!(
                "{output} was written but does not match the workdir, so this save did not \
                 produce the document you asked for: {detail}"
            ),
            build_commit,
        );
    }

    session.success(
        "document_save",
        json!({
            "output": output,
            "operation_id": operation_id,
            "build": envelope.get("data").cloned().unwrap_or(Value::Null),
            "verified": verify_envelope.get("data").cloned().unwrap_or(Value::Null),
        }),
        build_commit,
    )
}
