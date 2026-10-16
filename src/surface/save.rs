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

/// The files a save produces beside the document: the package itself and the
/// evidence, verification and ledger sidecars the build writes next to it.
const SIDECARS: [&str; 3] = [
    ".evidence.json",
    ".verify.evidence.json",
    ".operation-ledger.json",
];

/// Remove a staged build that must not survive a failed save — the package and
/// whatever sidecars it wrote.
fn discard_build(path: &str) {
    let _ = std::fs::remove_file(path);
    for suffix in SIDECARS {
        let _ = std::fs::remove_file(format!("{path}{suffix}"));
    }
}

/// Move a verified build to the path the caller asked for, sidecars included.
///
/// The rename of the package is the moment the save becomes visible; the
/// sidecars follow it, because a document published without the evidence that
/// describes it is a different artifact from the one that was verified.
fn publish_build(staging: &str, output: &str) -> Result<(), String> {
    std::fs::rename(staging, output).map_err(|error| {
        format!("{output} could not be replaced with the verified build: {error}")
    })?;
    for suffix in SIDECARS {
        let from = format!("{staging}{suffix}");
        if !std::path::Path::new(&from).is_file() {
            continue;
        }
        std::fs::rename(&from, format!("{output}{suffix}")).map_err(|error| {
            format!("{output} was written, but its {suffix} could not be: {error}")
        })?;
    }
    Ok(())
}

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
    // Build into a staging path BESIDE the target, and publish it only once the
    // verification agrees. Writing straight to `output` and verifying afterwards
    // leaves the bad package exactly where the caller asked for it: on a real
    // patent, six saves reported `output-not-verified` and each had already put a
    // malformed DOCX on disk, because a build can only be judged after it ran.
    // The rename is the last step, so a save that fails changes nothing.
    let staging = format!("{output}.building-{operation_id}");
    let built = session.build_docx(
        &json!({ "output": staging, "operation_id": operation_id }),
        build_commit,
    );
    let Some(envelope) = built.get("structuredContent") else {
        discard_build(&staging);
        return built;
    };
    if envelope.get("outcome").and_then(Value::as_str) != Some("success") {
        // The build refused, so nothing was written — or what was written is not
        // claimed. It already carries the reason and the recovery payload.
        discard_build(&staging);
        return built;
    }

    // Verify what is on disk against the workdir. A disagreement is not a
    // warning: the file is not what the caller asked for, so the save did not
    // succeed.
    let verified = session.verify_output(&json!({ "output": staging }), build_commit);
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
        discard_build(&staging);
        return session.failure(
            "document_save",
            "output-not-verified",
            &format!(
                "{output} was not written: the package this workdir builds does not match it, so \
                 there is nothing to deliver — {detail} (the rejected build was discarded)"
            ),
            build_commit,
        );
    }

    // The last step, and the only one the caller sees.
    if let Err(error) = publish_build(&staging, output) {
        discard_build(&staging);
        return session.failure(
            "document_save",
            "output-not-published",
            &format!(
                "the build verified, but it could not be delivered: {error}. Close the file in \
                 Word, or save to another path"
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory that removes itself, panics included.
    struct Scratch(std::path::PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn scratch(tag: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("save-publish-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        Scratch(dir)
    }

    /// The publish is the only step the caller sees, so it has to behave like a
    /// replacement: the file that was there goes, the package and the sidecars
    /// that describe it arrive together, and nothing of the build stays behind.
    #[test]
    fn a_publish_replaces_the_file_and_brings_its_sidecars() {
        let dir = scratch("replace");
        let staging = dir.0.join("out.docx.building-x");
        let output = dir.0.join("out.docx");
        std::fs::write(&staging, b"verified package").expect("staging");
        std::fs::write(format!("{}.evidence.json", staging.display()), b"evidence")
            .expect("evidence");
        std::fs::write(&output, b"what was there before").expect("existing");

        publish_build(
            staging.to_str().expect("path"),
            output.to_str().expect("path"),
        )
        .expect("publish");

        assert_eq!(
            std::fs::read(&output).expect("published"),
            b"verified package"
        );
        assert_eq!(
            std::fs::read(format!("{}.evidence.json", output.display())).expect("evidence moved"),
            b"evidence"
        );
        assert!(!staging.exists(), "the staging file is still there");
    }

    /// A save that fails must leave nothing at the path the caller named, and
    /// nothing of the build beside it either.
    #[test]
    fn a_discarded_build_leaves_no_file_behind() {
        let dir = scratch("discard");
        let staging = dir.0.join("out.docx.building-y");
        std::fs::write(&staging, b"half a package").expect("staging");
        for suffix in SIDECARS {
            std::fs::write(format!("{}{suffix}", staging.display()), b"{}").expect("sidecar");
        }

        discard_build(staging.to_str().expect("path"));

        for suffix in SIDECARS {
            let path = dir.0.join(format!("out.docx.building-y{suffix}"));
            assert!(!path.exists(), "{} survived the discard", path.display());
        }
        assert!(!staging.exists(), "the discarded package is still there");
    }
}
