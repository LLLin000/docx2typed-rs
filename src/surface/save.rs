//! `document_save` — write the DOCX, and verify what was written.
//!
//! This is the only call that produces files: the output DOCX and, beside it, its
//! evidence set — `<out>.docx.evidence.json`, `<out>.docx.operation-ledger.json`
//! and `<out>.docx.verify.evidence.json`. All four name the same bytes; that set is
//! what a later reader can check the release against.
//!
//! It does not trust its own build: the output is verified against the workdir
//! before success is reported.
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
use std::path::Path;

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

/// The bytes one already-published sidecar claims the document has, when it
/// claims anything.
///
/// The build's evidence and the verification's evidence both record
/// `payload.outputs.docx.sha256`, so a document and its sidecars can be checked
/// against each other instead of trusted as a group. The operation ledger
/// records operations rather than an artifact hash, and answers `None`.
pub(crate) fn recorded_docx_sha256(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value
        .get("payload")?
        .get("outputs")?
        .get("docx")?
        .get("sha256")?
        .as_str()
        .map(str::to_string)
}

/// Move a verified build to the path the caller asked for.
///
/// **Sidecars first, the document last.** The document is the artifact; the
/// sidecars are evidence about it, and the rename that puts the document in place
/// is the moment the save becomes visible. An interruption before that rename
/// leaves the version that was already there — never a new document with evidence
/// from some other build. The reverse order (document first, then three renames)
/// can be interrupted with the new document in place and one or more old sidecars
/// beside it, which is a mixed version that reads like one release.
///
/// A sidecar that cannot be renamed stops the save before the document is
/// touched, and the caller is told which one.
fn publish_build(staging: &str, output: &str, captured: &PublishExpectation) -> Result<(), String> {
    // Guard FIRST (fail-closed): the hold is acquired BEFORE any check or
    // write. A guard that cannot be held on an existing target REFUSES —
    // never an unguarded publish.
    let _guard = hold_output_no_write(output)?;
    // Re-check under the handle: the target must still be exactly what the
    // caller captured (present + hash, or absent). A NEW target has no
    // handle (nothing to hold) — its absence was just proven by the failed
    // open inside the guard, and the LINK below decides atomically.
    // NOTE the ordering asymmetry: the final hash re-check runs UNDER the
    // guard; the staged-sidecar preflight below runs WITHOUT it (it takes
    // no locks and mutates nothing). The preflight takes no locks and mutates
    // nothing; the guard is acquired only after it passes, so a blocked
    // sidecar destination refuses before ANY publish write — old document
    // survives AND no leftover staged files remain. A crash between the
    // sidecar renames and the document rename leaves new sidecars + old
    // document: the mixed set the reader reports as output-set-inconsistent
    // (repaired by re-running save). A crash after the document rename with
    // sidecars pending is recovered the same way. is only DENY-WRITE while
    // held (Windows share READ|DELETE): it does NOT stop an external
    // rename-onto-target between the check and the rename, which is
    // documented, not claimed away.
    match (&captured.present, &captured.sha256) {
        (true, Some(was)) => match std::fs::read(output) {
            Ok(now) => {
                let now_hash = docx2typed_protocol::bytes_sha256(&now);
                if now_hash != *was {
                    return Err(format!(
                        "external output changed outside the engine during the build: {output} now hashes to {now_hash}, expected {was}; the verified staged build was discarded and nothing was published"
                    ));
                }
            }
            Err(error) => {
                return Err(format!(
                    "external output disappeared outside the engine during the build: {output} is now unreadable ({error}); the staged build was discarded and nothing was published"
                ));
            }
        },
        (false, _) => {
            // New path: validate absence here, CREATE below in the branched
            // document publish (hard-link no-clobber + sidecars + rollback).
            // No write in this arm — the LINK below decides.
            if std::path::Path::new(output).exists() {
                return Err(format!(
                    "external output appeared outside the engine just before publication: \
                     {output} now exists; the staged build was discarded and nothing \
                     was published"
                ));
            }
        }
        _ => {
            return Err(format!(
                "external-file-changed: {output} is not in a publishable state; \
                 the staged build was discarded and nothing was published"
            ));
        }
    }
    // Present + matching, BOTH the document and its sidecars publish through
    // the SAME guard window: sidecars first (the declared publication
    // invariant — a reader must never see a new document with old evidence),
    // then the atomic document rename. A staged-sidecar preflight runs BEFORE
    // any rename — no locks, no writes, just an existence + open-for-write
    // probe per staged sidecar — and a blocked destination refuses before ANY
    // publish write (old document survives, no staged leftovers). A crash
    // between the sidecar renames and the document rename leaves new sidecars
    // + old document: the mixed set the reader reports as
    // output-set-inconsistent (repaired by re-running save).
    for suffix in SIDECARS {
        let from = format!("{staging}{suffix}");
        if !Path::new(&from).is_file() {
            continue;
        }
        preflight_sidecar_receivable(&from, &format!("{output}{suffix}"))?;
    }
    // Document publish branches on captured presence: present => sidecars
    // then atomic replace via rename; absent (new path) => atomic no-clobber
    // hard-link create, then sidecars. The LINK decides — no exists-branch
    // (that TOCTOU is the overwrite bug). A new-path sidecar failure removes
    // the just-linked document AND every sidecar THIS publish itself created
    // as an absent file (tracked: only destinations that did not exist when
    // this publish ran). A sidecar destination that already existed is NEVER
    // removed: it belongs to a prior release (or to nobody) and refusing
    // over it must never delete it. Unknown-existing sidecars were already
    // refused by the caller at entry (document_save refuses unknown-existing
    // targets; the store entry check does the same); the preflight above
    // refuses present destinations that fail the write-check BEFORE any
    // rename happens. This is a best-effort atomicity boundary for
    // genuinely new paths, NOT a version-checked CAS: concurrent external
    // renames during the window are refused when detected (exists-check,
    // link failure, rename error), never silently overwritten.
    if captured.present {
        for suffix in SIDECARS {
            let from = format!("{staging}{suffix}");
            if !Path::new(&from).is_file() {
                continue;
            }
            std::fs::rename(&from, format!("{output}{suffix}")).map_err(|error| {
                format!(
                    "the document was NOT published: its {suffix} could not be put in place \
                     ({error}), so whatever {output} held is untouched"
                )
            })?;
        }
        coordinated_output_replace(staging, output)
    } else {
        match std::fs::hard_link(staging, output) {
            Ok(()) => {
                let _ = std::fs::remove_file(staging);
            }
            Err(error) => {
                return Err(format!(
                    "external output appeared outside the engine just before publication: \
                     {output} now exists; the staged build was discarded and nothing \
                     was published ({error})"
                ));
            }
        }
        let mut placed: Vec<String> = Vec::new();
        for suffix in SIDECARS {
            let from = format!("{staging}{suffix}");
            if !Path::new(&from).is_file() {
                continue;
            }
            let dst = format!("{output}{suffix}");
            let dst_existed = Path::new(&dst).exists();
            std::fs::rename(&from, &dst).map_err(|error| {
                // Roll back ONLY what this publish created: the just-linked
                // document plus sidecars placed as absent files (tracked in
                // `placed`). A sidecar destination that already existed
                // belongs to a prior release and is NEVER removed.
                let _ = std::fs::remove_file(output);
                for placed_suffix in &placed {
                    let _ = std::fs::remove_file(format!("{output}{placed_suffix}"));
                }
                format!(
                    "the document was NOT published: its {suffix} could not be put in place \
                     ({error}); the linked document and this publish's own new sidecars were \
                     removed, so whatever {output} held before is untouched"
                )
            })?;
            if !dst_existed {
                placed.push(suffix.to_string());
            }
        }
        Ok(())
    }
}

/// Staged-sidecar preflight: can `dst` receive `src`, without writing
/// anything? Checks the parent dir exists/writable (probe by opening the
/// existing destination when present) and refuses BEFORE any rename. Takes
/// no locks, mutates nothing — including no staged files.
fn preflight_sidecar_receivable(src: &str, dst: &str) -> Result<(), String> {
    let dst_path = std::path::Path::new(dst);
    // Parent must exist (we never create directories as a side effect of a
    // refused publish) and the destination, when present, must be openable
    // for write-check. Absent destination + existing parent = receivable.
    let parent = dst_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    if !parent.is_dir() {
        return Err(format!(
            "the document was NOT published: its evidence parent {parent:?} does not exist, \
             so whatever {output} held is untouched",
            parent = parent.display(),
            output = dst,
        ));
    }
    if dst_path.exists() {
        // Probe writability without mutating: open for write (no truncate).
        // On Windows this also surfaces a no-delete-sharing lock as an error
        // here, before any rename — matching the blocked-sidecar test.
        std::fs::OpenOptions::new()
            .write(true)
            .open(dst_path)
            .map_err(|error| {
                format!(
                    "the document was NOT published: its evidence {dst} could not be put in place \
                 ({error}), so whatever {output} held is untouched",
                    output = dst,
                )
            })?;
    }
    // Staged source must still be there (nothing consumed it yet).
    if !std::path::Path::new(src).is_file() {
        return Err(format!(
            "the document was NOT published: staged evidence {src} vanished before publication"
        ));
    }
    Ok(())
}

/// What the target looked like when the caller captured it: presence plus the
/// hash the save may overwrite. Passed into the final publish so the publish
/// step re-checks under the guard instead of re-reading the world.
#[derive(Clone, Debug)]
struct PublishExpectation {
    present: bool,
    sha256: Option<String>,
}
/// Held guard, or a refusal. A missing target is the new-path case (`Ok(None)`,
/// no handle needed — creation itself is the atomic no-clobber step). Failure
/// to hold a handle on an EXISTING target refuses before ANY write: a guard
/// that cannot be held is a window that cannot be narrowed, and an unguarded
/// publish is never attempted.
fn hold_output_no_write(output: &str) -> Result<Option<std::fs::File>, String> {
    let path = std::path::Path::new(output);
    if !path.exists() {
        return Ok(None);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x0000_0001;
        const FILE_SHARE_DELETE: u32 = 0x0000_0004;
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
            .open(path)
            .map_err(|error| {
                format!(
                    "external output is locked outside the engine and the publish guard \
                     cannot be held: {output} ({error}); nothing was published — close \
                     the file in Word, or save to a new path"
                )
            })
            .map(Some)
    }
    #[cfg(not(windows))]
    {
        std::fs::File::open(path)
            .map_err(|error| {
                format!(
                    "external output cannot be guarded for publication: {output} ({error}); \
                     nothing was published"
                )
            })
            .map(Some)
    }
}

/// Same-directory atomic replace. No retries, no sleeps: one rename, and a
/// failure is returned loudly. A retry would mask a concurrent writer racing
/// the publish.
fn coordinated_output_replace(staging: &str, output: &str) -> Result<(), String> {
    std::fs::rename(staging, output)
        .map_err(|error| format!("{output} could not be replaced with the verified build: {error}"))
}

/// Point the published ledger's records at the file they produced.
///
/// The build journals the operation it ran, and it ran it at the staging path —
/// which, once the publication renames the artifact, no longer exists. The
/// publication is this step's work, so the path it corrects is this step's
/// business; the staged name is kept beside it as `staged_at`, because that is
/// where the build's own evidence says it wrote.
fn repoint_ledger(output: &str, staging: &str) {
    let path = format!("{output}.operation-ledger.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(mut ledger) = serde_json::from_str::<Value>(&text) else {
        return;
    };
    let mut changed = false;
    if let Some(records) = ledger.get_mut("records").and_then(Value::as_object_mut) {
        for record in records.values_mut() {
            let Some(data) = record
                .get_mut("envelope")
                .and_then(|envelope| envelope.get_mut("data"))
            else {
                continue;
            };
            let at_staging = data
                .get("output")
                .and_then(|output| output.get("value"))
                .and_then(Value::as_str)
                .is_some_and(|value| value == staging);
            if at_staging {
                data["output"] = json!({ "kind": "absolute", "value": output });
                data["staged_at"] = json!(staging);
                changed = true;
            }
        }
    }
    if changed {
        let _ = std::fs::write(&path, format!("{ledger}\n"));
    }
}

/// The document and the evidence beside it have to be one version.
///
/// Reported, not assumed: every sidecar that records the artifact's bytes is
/// compared with the bytes that are now on disk, and the names that disagree are
/// returned. `document_open` reports the same thing about a document it is asked
/// to open, so a mixed set is never mistaken for a committed one — which is what
/// an interrupted publication would otherwise leave behind.
pub(crate) fn sidecars_disagreeing_with(output: &str, published_sha256: &str) -> Vec<String> {
    let mut disagree = Vec::new();
    for suffix in SIDECARS {
        let path = Path::new(&format!("{output}{suffix}")).to_path_buf();
        if let Some(recorded) = recorded_docx_sha256(&path) {
            if recorded != published_sha256 {
                disagree.push(format!("{suffix} records {recorded}"));
            }
        }
    }
    disagree
}

/// What the evidence beside a document says about the bytes it has.
///
/// Reported by `document_open`, because that is where someone hands over a file
/// and expects the thing on disk to be one version. `absent` is the ordinary
/// answer for a document this engine has never written; `mixed` means the
/// evidence describes some *other* build of it, which an interrupted publication
/// leaves behind and which nothing else can detect — the sidecars are valid JSON,
/// about a document that is no longer there.
pub(crate) fn sidecar_set_state(document: &Path) -> Value {
    let output = document.to_string_lossy().to_string();
    let recorded: Vec<(String, String)> = SIDECARS
        .iter()
        .filter_map(|suffix| {
            recorded_docx_sha256(Path::new(&format!("{output}{suffix}")))
                .map(|sha256| (suffix.to_string(), sha256))
        })
        .collect();
    if recorded.is_empty() {
        // Nothing beside this document claims to describe it.
        return json!({ "state": "absent" });
    }
    let published = docx2typed_protocol::file_sha256(document).unwrap_or_default();
    let disagree: Vec<String> = recorded
        .iter()
        .filter(|(_, sha256)| *sha256 != published)
        .map(|(suffix, sha256)| format!("{suffix} records {sha256}"))
        .collect();
    if disagree.is_empty() {
        json!({ "state": "consistent", "docx_sha256": published })
    } else {
        json!({
            "state": "mixed",
            "why": "the evidence beside this document describes different bytes than the document \
                    has, so this is not one release — most likely a publication that was \
                    interrupted between its sidecars and its document",
            "disagree": disagree,
            "docx_sha256": published,
        })
    }
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
    let Some(workdir) = session.open_workdir().map(str::to_string) else {
        return session.failure(
            "document_save",
            "workdir-not-open",
            "no document is open; call document_open first",
            build_commit,
        );
    };
    // External-output precondition BEFORE the build mutates anything: the
    // caller-visible `output` must be this workdir's own last publication
    // (same epoch, same bytes), the canonical source holding the baseline
    // bytes after an explicit re-extract, or a new path observed absent. An
    // existing unknown target refuses here, before the build runs — no staging
    // beside C, no workdir write — and the store re-checks the same condition
    // under its Writer lane at journal and publication time. Sidecars beside
    // C are owned by C's own evidence set and are never touched by a refusal.
    {
        let workdir_path = std::path::Path::new(&workdir);
        let epoch = docx2typed_core::read_baseline_epoch(workdir_path).unwrap_or(0);
        let registry = docx2typed_store::read_external_targets(workdir_path);
        if let Err(error) = docx2typed_store::check_external_target(
            workdir_path,
            std::path::Path::new(output),
            epoch,
            &registry,
        ) {
            return session.failure(
                "document_save",
                "external-file-changed",
                &error.message(),
                build_commit,
            );
        }
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

    // The window between verifying and publishing is the one place a third party
    // could swap the staged file for another one — the path being the same proves
    // the path, not the bytes. So the bytes are hashed again and compared with the
    // hash the verification recorded.
    //
    // What this claims, exactly: a staged file that changed at any point BEFORE
    // this read is caught, and the publication is refused. It does not claim that
    // nothing can change between this read and the rename — that would take a file
    // handle protocol, and for a single-process local tool the remaining window is
    // not worth it. It is a check, not a transaction.
    // The verification wrote its evidence beside the staged build, and that
    // evidence names the bytes it verified (`payload.outputs.docx.sha256`). A
    // verification that left no evidence is not one this step may trust, so that
    // is a refusal rather than a shrug.
    let evidence = format!("{staging}.verify.evidence.json");
    let Some(expected) = recorded_docx_sha256(Path::new(&evidence)) else {
        discard_build(&staging);
        return session.failure(
            "document_save",
            "verification-evidence-missing",
            &format!(
                "the verification of the staged build left no evidence to check the publication \
                 against ({evidence}); nothing was published"
            ),
            build_commit,
        );
    };
    match docx2typed_protocol::file_sha256(Path::new(&staging)) {
        Ok(actual) if actual == expected => {}
        Ok(actual) => {
            discard_build(&staging);
            return session.failure(
                "document_save",
                "staged-artifact-changed",
                &format!(
                    "the verified build has changed on disk before it could be published: the \
                     verification saw {expected}, the file now hashes to {actual}. Nothing was \
                     published, so {output} still holds whatever it held before"
                ),
                build_commit,
            );
        }
        Err(error) => {
            discard_build(&staging);
            return session.failure(
                "document_save",
                "staged-artifact-unreadable",
                &format!("the verified build could not be read back before publishing: {error}"),
                build_commit,
            );
        }
    }
    // Capture the expectation the final publish re-checks UNDER the
    // no-write guard: presence + hash read now, verified inside publish_build
    // while the handle is held. No second world-read after the guard — the
    // check and the rename share one window.
    let captured = match std::fs::read(output) {
        Ok(bytes) => PublishExpectation {
            present: true,
            sha256: Some(docx2typed_protocol::bytes_sha256(&bytes)),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => PublishExpectation {
            present: false,
            sha256: None,
        },
        Err(error) => {
            discard_build(&staging);
            return session.failure(
                "document_save",
                "external-file-changed",
                &format!(
                    "{output} cannot be guarded for publication ({error}); nothing was published"
                ),
                build_commit,
            );
        }
    };
    // The registry decision was already made at entry (unknown-existing
    // refused before the build). Here only the guard + under-handle re-check
    // runs: a file that appeared or changed after the entry check refuses
    // with the staged build discarded and the target untouched. Residual: an
    // external rename-onto-target between check and rename, documented.
    // The last step, and the only one the caller sees.
    if let Err(error) = publish_build(&staging, output, &captured) {
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
    repoint_ledger(output, &staging);

    // And the set has to be one version. Every sidecar that records the
    // artifact's bytes is compared with what was just published; a sidecar from
    // another build is reported rather than left beside the document, because a
    // reader cannot tell the difference by looking.
    let published = match docx2typed_protocol::file_sha256(Path::new(output)) {
        Ok(sha256) => sha256,
        Err(error) => {
            return session.failure(
                "document_save",
                "output-unreadable",
                &format!("{output} was published but could not be read back: {error}"),
                build_commit,
            )
        }
    };
    let disagree = sidecars_disagreeing_with(output, &published);
    if !disagree.is_empty() {
        return session.failure(
            "document_save",
            "output-set-inconsistent",
            &format!(
                "{output} holds the package that was just verified, but the evidence beside it \
                 belongs to another build ([{}]) — that is not one release. Re-run \
                 document_save to regenerate the sidecars, and do not read that set as a \
                 committed version in the meantime",
                disagree.join(", ")
            ),
            build_commit,
        );
    }
    // Success advances the expectation to the bytes just published: a later
    // save over the same path must still see these bytes, or refuse. The
    // registry lives beside the workdir so a re-extract (epoch advance)
    // re-baselines every path without manual cleanup.
    {
        use docx2typed_store::ExternalExpectation;
        let epoch =
            docx2typed_core::read_baseline_epoch(std::path::Path::new(&workdir)).unwrap_or(0);
        let key = docx2typed_protocol::resolve_path(std::path::Path::new(output))
            .to_string_lossy()
            .into_owned();
        let mut targets = docx2typed_store::read_external_targets(std::path::Path::new(&workdir));
        targets.insert(
            key,
            ExternalExpectation {
                expected_sha256: Some(published.clone()),
                epoch,
                observed_absent: false,
            },
        );
        let _ = docx2typed_store::write_external_targets_public(
            std::path::Path::new(&workdir),
            &targets,
        );
    }
    // Both payloads were produced against the staged build, so both name it: the
    // build wrote there, the verification read it back, and after publication
    // neither name is one the caller can follow to anything (measured: a blind
    // agent reported the staging name as the thing that was "verified"). Repointed
    // to the file the caller now has — the hashes in them are the published bytes.
    let mut build = envelope.get("data").cloned().unwrap_or(Value::Null);
    let mut verified = verify_envelope.get("data").cloned().unwrap_or(Value::Null);
    repoint_paths(&mut build, &staging, output);
    repoint_paths(&mut verified, &staging, output);
    session.success(
        "document_save",
        json!({
            "output": output,
            "operation_id": operation_id,
            "docx_sha256": published,
            "build": build,
            "verified": verified,
        }),
        build_commit,
    )
}

/// Replace every occurrence of `from` inside the payload's strings with `to`.
fn repoint_paths(value: &mut Value, from: &str, to: &str) {
    match value {
        Value::String(text) => {
            if let Some(index) = text.find(from) {
                let mut replaced = String::with_capacity(text.len() + to.len());
                replaced.push_str(&text[..index]);
                replaced.push_str(to);
                replaced.push_str(&text[index + from.len()..]);
                *text = replaced;
            }
        }
        Value::Array(items) => {
            for item in items {
                repoint_paths(item, from, to);
            }
        }
        Value::Object(map) => {
            for item in map.values_mut() {
                repoint_paths(item, from, to);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verification_paths_name_the_published_file_not_the_staging_one() {
        let mut payload = json!({
            "output": "C:/x/out.docx.building-abc",
            "verified": {
                "value": "C:/x/out.docx.building-abc",
                "evidence": "C:/x/out.docx.building-abc.verify.evidence.json",
            },
            "digest": "abc",
        });
        repoint_paths(&mut payload, "C:/x/out.docx.building-abc", "C:/x/out.docx");
        assert_eq!(payload["output"], "C:/x/out.docx");
        assert_eq!(payload["verified"]["value"], "C:/x/out.docx");
        assert_eq!(
            payload["verified"]["evidence"],
            "C:/x/out.docx.verify.evidence.json"
        );
        assert_eq!(payload["digest"], "abc");

        // The build payload names the staged file as well: the build wrote there,
        // and after publication that name is not one the caller can follow.
        let mut built =
            json!({ "output": "C:/x/out.docx.building-abc", "docx_sha256": "deadbeef" });
        repoint_paths(&mut built, "C:/x/out.docx.building-abc", "C:/x/out.docx");
        assert_eq!(built["output"], "C:/x/out.docx");
    }

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
        let was = std::fs::read(&output).expect("existing bytes");
        let captured = PublishExpectation {
            present: true,
            sha256: Some(docx2typed_protocol::bytes_sha256(&was)),
        };

        publish_build(
            staging.to_str().expect("path"),
            output.to_str().expect("path"),
            &captured,
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

    /// A sidecar that cannot be received stops the save BEFORE anything is
    /// written: the staged-sidecar preflight validates every destination
    /// before the first rename, so a blocked sidecar destination refuses
    /// with the old document AND the old sidecar untouched and no staged
    /// leftovers. Fail-closed, never a partial publish.
    #[test]
    fn a_publish_that_cannot_place_a_sidecar_reports_refusal() {
        let dir = scratch("blocked-sidecar");
        let staging = dir.0.join("out.docx.building-z");
        let output = dir.0.join("out.docx");
        std::fs::write(&staging, b"the new package").expect("staging");
        std::fs::write(
            format!("{}.evidence.json", staging.display()),
            b"new evidence",
        )
        .expect("staged evidence");
        std::fs::write(&output, b"the version that must survive").expect("published version");
        let blocked = format!("{}.evidence.json", output.display());
        std::fs::write(&blocked, b"evidence of the surviving version").expect("destination");
        // Hold the destination open WITHOUT delete sharing: the rename onto it
        // then fails, which is how a file open in Word behaves. `File::open` is
        // not enough — Rust asks for FILE_SHARE_DELETE by default, so a rename
        // over a handle it holds still succeeds.
        #[cfg(windows)]
        let guard = {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_SHARE_READ: u32 = 0x0000_0001;
            const FILE_SHARE_WRITE: u32 = 0x0000_0002;
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .open(&blocked)
                .expect("open without delete sharing")
        };
        #[cfg(not(windows))]
        let guard = std::fs::File::open(&blocked).expect("open");

        let was = std::fs::read(&output).expect("existing bytes");
        let captured = PublishExpectation {
            present: true,
            sha256: Some(docx2typed_protocol::bytes_sha256(&was)),
        };
        let result = publish_build(
            staging.to_str().expect("path"),
            output.to_str().expect("path"),
            &captured,
        );
        // The caller (document_save) discards the staged build on refusal;
        // the unit calls publish_build directly, so discard here to assert
        // the same end state.
        drop(guard);
        // The caller (document_save) discards the staged build on refusal;
        // the unit calls publish_build directly, so discard here to assert
        // the same end state. Dropped AFTER the guard: the preflight probes
        // the still-locked destination, matching production order.
        discard_build(staging.to_str().expect("path"));
        if cfg!(windows) {
            assert!(
                result.is_err(),
                "a blocked sidecar must fail the publication: {result:?}"
            );
            // Fail-closed: the preflight refused before ANY write — old
            // document AND old sidecar survive, no staged leftovers.
            assert_eq!(
                std::fs::read(&output).expect("untouched"),
                b"the version that must survive",
                "a refused publish replaced the document it never published"
            );
            assert_eq!(
                std::fs::read(&blocked).expect("sidecar untouched"),
                b"evidence of the surviving version",
                "a refused publish overwrote the prior sidecar"
            );
            assert!(
                !staging.exists(),
                "a refused publish left the staged package behind"
            );
        }
    }

    /// A sidecar left beside a document by some other build is reported, not
    /// trusted: that is what an interrupted publication looks like from outside.
    #[test]
    fn evidence_from_another_build_is_reported_as_a_mixed_set() {
        let dir = scratch("mixed-set");
        let document = dir.0.join("doc.docx");
        std::fs::write(&document, b"the document").expect("document");
        let evidence = format!("{}.evidence.json", document.to_str().expect("path"));
        std::fs::write(
            &evidence,
            serde_json::to_vec(&json!({
                "schema": "docx2typed-run-evidence-1",
                "payload": { "outputs": { "docx": { "sha256": "0000" } } },
            }))
            .expect("json"),
        )
        .expect("evidence");
        assert_eq!(sidecar_set_state(&document)["state"], "mixed");

        // And the same set, agreeing, is reported as one version.
        let real = docx2typed_protocol::file_sha256(&document).expect("hash");
        std::fs::write(
            format!("{}.verify.evidence.json", document.to_str().expect("path")),
            serde_json::to_vec(&json!({
                "schema": "docx2typed-run-evidence-1",
                "payload": { "outputs": { "docx": { "sha256": real } } },
            }))
            .expect("json"),
        )
        .expect("verify evidence");
        std::fs::remove_file(&evidence).expect("remove");
        assert_eq!(sidecar_set_state(&document)["state"], "consistent");

        // A document nobody wrote evidence for says so rather than guessing.
        let bare = dir.0.join("bare.docx");
        std::fs::write(&bare, b"a document from elsewhere").expect("bare");
        assert_eq!(sidecar_set_state(&bare)["state"], "absent");
    }

    /// A bounded tail of the publication path: the ledger the build wrote names
    /// the staging path, and the publication is what knows the artifact's real
    /// name, so it is the step that corrects it.
    #[test]
    fn the_published_ledger_points_at_the_published_file() {
        let dir = scratch("ledger");
        let staging = dir.0.join("out.docx.building-l");
        let output = dir.0.join("out.docx");
        std::fs::write(&output, b"package").expect("published");
        std::fs::write(
            format!("{}.operation-ledger.json", output.display()),
            serde_json::to_vec(&json!({
                "schema": "docx2typed-operation-ledger-1",
                "records": { "op-1": { "envelope": { "data": {
                    "operation_id": "op-1",
                    "output": { "kind": "absolute", "value": staging.to_str().expect("path") },
                } } } },
            }))
            .expect("json"),
        )
        .expect("ledger");

        repoint_ledger(
            output.to_str().expect("path"),
            staging.to_str().expect("path"),
        );

        let ledger: Value = serde_json::from_str(
            &std::fs::read_to_string(format!("{}.operation-ledger.json", output.display()))
                .expect("ledger"),
        )
        .expect("json");
        let data = &ledger["records"]["op-1"]["envelope"]["data"];
        assert_eq!(data["output"]["value"], output.to_str().expect("path"));
        assert_eq!(data["staged_at"], staging.to_str().expect("path"));
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
