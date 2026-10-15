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
fn extract_beside(
    session: &McpSession,
    source: &Path,
    build_commit: &str,
) -> Result<PathBuf, Value> {
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
        // A document. If this document has ALREADY been opened, its workdir is
        // beside it, and extracting again is not a no-op: it rebuilds the
        // baseline (measured: `baseline.json` epoch 1 -> 2 on a second open of
        // the same DOCX), and once the workdir carries committed edits the
        // rebuild is refused as `workdir-not-at-baseline`. The observable
        // failure was "the document opened, I edited it, and now it will not
        // open again" — so the workdir is found first and extraction happens
        // only when there is nothing to open.
        let existing = workdir_for(&source);
        if is_workdir(&existing) {
            existing
        } else {
            // Extracting creates a family, which needs no proof of continuity;
            // the binding question only arises for a document already in a
            // family, and `workspace_adopt` answers it.
            match extract_beside(session, &source, build_commit) {
                Ok(workdir) => workdir,
                Err(failure) => return failure,
            }
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
    // Any internal failure is passed through, unconditionally. Skipping this on
    // a re-open hid it: a workdir whose metadata had gone bad would report a
    // success with no session behind it.
    if opened.get("isError").and_then(Value::as_bool) == Some(true) {
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
    // Whether this workdir can be built at all — answered HERE, not only when
    // `document_save` finally refuses. A document carrying committed typed edits
    // with no record in the edit sidecars cannot be rebuilt (the replay would
    // drop them), and learning that after a dozen edits is a trap: the work
    // looks fine right up to the one call that produces a file. Measured on a
    // real patent, an agent ground through fourteen `document_edit` attempts and
    // never reached the save that held the answer.
    if !workdir.join("islands.json").is_file()
        && !workdir
            .join(docx2typed_core::prose::STRUCTURE_FILE)
            .is_file()
        && docx2typed_review::draft::has_unrecorded_typed_edits(&workdir).unwrap_or(false)
    {
        data["cannot_build"] = json!({
            "code": "edits-not-implemented",
            "why": "this workdir holds committed typed edits that are not recorded in the edit \
                    sidecars (islands.json / structure.json); a build would replay the template \
                    and drop them",
            "next": "build with the Python reference engine — no edit made here can fix it",
        });
    }
    session.success("document_open", data, build_commit)
}
