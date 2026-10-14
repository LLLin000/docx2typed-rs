//! `document_open` — the first call of every task.
//!
//! One decision the caller must not have to make: whether the path it holds is
//! a DOCX or a workdir. A real session passed a document to a tool documented
//! as accepting one and was told `workdir not found` about a path that plainly
//! existed — the engine knew the distinction and did not say it. Here the
//! engine resolves it.
//!
//! For a DOCX, the reference's rule is the one that holds: proof before
//! continuity (`_resolve_document_argument` in the Python reference). A DOCX
//! proven to belong to an existing workspace opens THAT workspace; an unproven
//! one is never guessed into somebody else's family. It gets a NEW family, which
//! needs no proof — extraction is what creates one.

use crate::mcp::McpSession;
use docx2typed_app::{ExtractArgs, Operation, OperationArgs, OperationContext};
use docx2typed_protocol::new_operation_id;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Where a fresh extract of `source` puts its workdir.
///
/// Deterministic and reported back, so a second call finds the same workdir and
/// the caller is never left guessing where it went. It sits beside the document
/// rather than in a temp directory: a workdir holds the revision history and the
/// evidence for a filing, and a temp directory is a place things are deleted
/// from.
fn workdir_for(source: &Path) -> PathBuf {
    let stem = source
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_else(|| "document".to_string());
    source
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{stem}.docx2typed"))
}

/// Is this a workdir the engine can open?
fn is_workdir(path: &Path) -> bool {
    path.is_dir() && path.join("typed.md").is_file()
}

/// Extract `source` into a workdir beside it, then report where it went.
fn extract_beside(session: &McpSession, source: &Path, build_commit: &str) -> Result<PathBuf, Value> {
    let outdir = workdir_for(source);
    let outcome = session
        .engine()
        .execute(
            Operation::Extract,
            OperationContext::new(new_operation_id()),
            OperationArgs::Extract(ExtractArgs {
                input: source.to_path_buf(),
                outdir: outdir.clone(),
                discard_committed_versions: false,
            }),
        )
        .map_err(|error| {
            session.failure(
                "document_open",
                "workdir-unreadable",
                &format!("could not extract {}: {}", source.display(), error.message),
                build_commit,
            )
        })?;
    if outcome.outcome.as_str() != "success" {
        let detail = outcome
            .diagnostics
            .first()
            .map(|diagnostic| format!("{}: {}", diagnostic.code, diagnostic.message))
            .unwrap_or_else(|| "extraction failed".to_string());
        return Err(session.failure("document_open", "workdir-unreadable", &detail, build_commit));
    }
    Ok(outdir)
}

/// `document_open` — open a DOCX or a workdir and get a session.
pub fn document_open(session: &mut McpSession, args: &Value, build_commit: &str) -> Value {
    let Some(source) = args.get("source").and_then(Value::as_str) else {
        return session.failure(
            "document_open",
            "invalid-arguments",
            "document_open requires `source`: a path to a DOCX or to a typed workdir",
            build_commit,
        );
    };
    let source = PathBuf::from(source);
    let workdir = if is_workdir(&source) {
        source
    } else if source.is_file() {
        // A document. Extracting it creates a family, which needs no proof of
        // continuity; the binding question only arises for a document that is
        // already in a family, and `workspace_adopt` answers it.
        match extract_beside(session, &source, build_commit) {
            Ok(workdir) => workdir,
            Err(failure) => return failure,
        }
    } else {
        return session.failure(
            "document_open",
            "workdir-invalid",
            &format!(
                "{} is neither a document nor a typed workdir; `source` takes a DOCX path or \
                 the directory a previous open or extract wrote",
                source.display()
            ),
            build_commit,
        );
    };

    // `workdir_open` itself treats a re-open of the same workdir as a success
    // and returns the current session, so there is nothing to special-case
    // here — a second open reports `reopened` so the caller can tell it was a
    // no-op without having to diff two payloads.
    let already_open = session
        .open_workdir()
        .is_some_and(|open| Path::new(open) == workdir.as_path());

    let mut open_args = json!({ "workdir": workdir.to_string_lossy() });
    for key in ["author", "date", "track"] {
        if let Some(value) = args.get(key) {
            if !value.is_null() {
                open_args[key] = value.clone();
            }
        }
    }
    let opened = session.workdir_open(&open_args, build_commit);
    if !already_open && opened.get("isError").and_then(Value::as_bool) == Some(true) {
        return opened;
    }
    // Report the workdir: it is the thing a later call addresses, and for a
    // freshly extracted document the caller has never seen this path before.
    let mut data = opened
        .get("structuredContent")
        .and_then(|envelope| envelope.get("data"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    data["workdir"] = json!(workdir.to_string_lossy());
    data["reopened"] = json!(already_open);
    session.success("document_open", data, build_commit)
}
