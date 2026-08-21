//! The atomic review frame (`docx2typed-review-frame-1`): one store pin
//! produces one consistent read model — the Core document projection, the
//! queue snapshot, the collaboration state, and the history trail are all
//! read from the same immutable generation path. The Review layer never
//! parses OOXML itself; the document surface is exactly the Core
//! projection's canonical ids and fingerprints (no fabricated paragraph /
//! revision / comment ids, no raw `rPr`).
//!
//! A legacy non-store workdir is served as a non-atomic frame
//! (`backed: false`, null identity) and never masquerades as atomic: the
//! frame does not claim a generation it cannot pin. `?history=<history_id>`
//! reconstructs the frame from the generation a history record references
//! (a live reference; GC is out of scope for this slice).

use docx2typed_store::store::Store;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::history;
use crate::server::MutationError;

pub const FRAME_SCHEMA: &str = "docx2typed-review-frame-1";
/// Position contract for every offset the frame exposes: patch offsets and
/// projection region offsets are Unicode-scalar (char) offsets.
pub const POSITION_CONTRACT: &str = "unicode-scalar-1";

fn mutation_from_store(error: &docx2typed_store::StoreError) -> MutationError {
    crate::server::mutation_from_store(error)
}

/// The single Review-side seam onto the Core document projection (Core API
/// boundary: no Store types — a plain `&Path` in, a serializable
/// `DocumentProjection` out). Both the frame's `document` surface and the
/// patch fingerprint gate (`collab::verify_patch_fingerprints`) consume
/// this; the Review layer never parses OOXML itself.
///
/// Contract (Core agent, assignment-frozen): `project_workdir(&Path)`
/// locates `_template.docx` and delegates; `DocumentProjection` and every
/// nested type implement `serde::Serialize` (parts/blocks/paragraphs/
/// tables/styles/revisions/comments, canonical ids, Unicode-scalar segment
/// offsets). `DocumentProjection` carries no precomputed fingerprints —
/// they are recomputed by the Core free functions `paragraph_fingerprint` /
/// `region_fingerprint` over a `CanonicalParagraph`.
pub(crate) fn project_document_json(workdir: &Path) -> Result<Value, String> {
    let template = workdir.join("_template.docx");
    let islands =
        docx2typed_core::prose::load_islands(workdir).map_err(|error| error.to_string())?;
    if islands.is_empty() {
        let projection = docx2typed_core::document_projection::project_workdir(workdir)
            .map_err(|error| error.to_string())?;
        return serde_json::to_value(&projection).map_err(|error| error.to_string());
    }
    // Review must show the committed MCP edits, not only the immutable
    // template. Project a short-lived patched template; the source workdir
    // remains untouched and the generated path never enters the frame.
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    let temp_dir: PathBuf =
        std::env::temp_dir().join(format!("docx2typed-review-{stamp}-{}", std::process::id()));
    fs::create_dir_all(&temp_dir).map_err(|error| error.to_string())?;
    let result = (|| {
        let patched = docx2typed_core::prose::apply_edits(&template, &islands)
            .map_err(|error| error.to_string())?;
        fs::write(temp_dir.join("_template.docx"), patched).map_err(|error| error.to_string())?;
        let projection = docx2typed_core::document_projection::project_workdir(&temp_dir)
            .map_err(|error| error.to_string())?;
        serde_json::to_value(&projection).map_err(|error| error.to_string())
    })();
    let _ = fs::remove_dir_all(&temp_dir);
    result
}

/// A fail-closed Core fingerprint check on one patch target: recompute the
/// paragraph identity, the Unicode-scalar range over the canonical text
/// (`join(segment.text)`), the expected text, and — when claimed — the
/// paragraph and region fingerprints via the Core fingerprint functions.
/// The covering style segment is the projection's segment whose range
/// covers `[start, end)`; there are no stable segment ids in the contract,
/// so coverage is proven by range + fingerprint, never by id.
pub(crate) fn verify_document_fingerprints(
    workdir: &Path,
    paragraph_id: &str,
    start: usize,
    end: usize,
    expected_text: &str,
    claimed_paragraph_fingerprint: &str,
    claimed_region_fingerprint: &str,
) -> Result<(), (String, String)> {
    use docx2typed_core::document_projection::{
        paragraph_fingerprint, project_workdir, region_fingerprint, CanonicalParagraph,
    };
    let projection = project_workdir(workdir)
        .map_err(|error| ("workdir-invalid".to_string(), error.to_string()))?;
    let paragraph = projection
        .paragraphs
        .iter()
        .find(|paragraph| paragraph.paragraph_id == paragraph_id)
        .ok_or_else(|| {
            (
                "paragraph-not-found".to_string(),
                format!("paragraph {paragraph_id} is not in the canonical projection"),
            )
        })?;
    let text: String = paragraph
        .segments
        .iter()
        .map(|segment| segment.text.as_str())
        .collect();
    let scalar_len = text.chars().count();
    if start > end || end > scalar_len {
        return Err((
            "patch-range".to_string(),
            format!("range [{start}, {end}) is outside the {scalar_len}-scalar paragraph text"),
        ));
    }
    let actual: String = text.chars().skip(start).take(end - start).collect();
    if actual != expected_text {
        return Err((
            "patch-precondition".to_string(),
            format!(
                "target text does not match the canonical projection (expected {expected_text:?}, projection has {actual:?})"
            ),
        ));
    }
    let canonical = CanonicalParagraph::from(paragraph);
    let paragraph_fp = paragraph_fingerprint(&canonical);
    if !claimed_paragraph_fingerprint.is_empty() && claimed_paragraph_fingerprint != paragraph_fp {
        return Err((
            "paragraph-fingerprint-mismatch".to_string(),
            format!(
                "paragraph_fingerprint {claimed_paragraph_fingerprint} does not match the canonical {paragraph_fp}"
            ),
        ));
    }
    if !claimed_region_fingerprint.is_empty() {
        let covered = paragraph
            .segments
            .iter()
            .any(|segment| segment.start <= start && segment.end >= end);
        if !covered {
            return Err((
                "region-not-found".to_string(),
                format!("no style region covers [{start}, {end}) in paragraph {paragraph_id}"),
            ));
        }
        let region_fp = region_fingerprint(&canonical, start, end);
        if claimed_region_fingerprint != region_fp {
            return Err((
                "region-fingerprint-mismatch".to_string(),
                format!(
                    "region_fingerprint {claimed_region_fingerprint} does not match the canonical {region_fp}"
                ),
            ));
        }
    }
    Ok(())
}

/// Build the atomic review frame. `history_id` (opaque, from
/// `?history=`) pins the generation that history record references instead
/// of the current generation; the frame identity then reports that
/// generation so the frontend never sees a generation path.
pub fn review_frame(workdir: &Path, history_id: Option<&str>) -> Result<Value, MutationError> {
    let backed = docx2typed_store::has_store(workdir);
    let mut diagnostics: Vec<Value> = Vec::new();
    let target: std::path::PathBuf;
    let mut identity_generation: Option<String> = None;
    let mut identity_manifest: Option<String> = None;
    let store: Option<Store>;
    let mut frame_history_id: Option<String> = None;

    if backed {
        let opened = Store::open(workdir).map_err(|error| mutation_from_store(&error))?;
        match history_id {
            None => {
                let pin = opened.pin().map_err(|error| mutation_from_store(&error))?;
                identity_generation = Some(pin.generation.clone());
                identity_manifest = pin.manifest_sha256.clone();
                target = pin.path;
            }
            Some(id) => {
                // The current generation's trail holds every record (the
                // append-only file is copied forward); its generation field
                // is the live reference this frame pins.
                let pin = opened.pin().map_err(|error| mutation_from_store(&error))?;
                let record = history::read(&pin.path, id, &|generation| {
                    opened.generation_manifest_sha256(generation)
                })
                .ok_or_else(|| {
                    MutationError::new("history-not-found", "no history record with this id")
                })?;
                let generation = record
                    .get("generation")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        MutationError::new(
                            "history-generation-unavailable",
                            "the history record predates generation binding and cannot be reconstructed as an atomic frame",
                        )
                    })?;
                let gen_dir = opened.generations_dir.join(generation);
                if !gen_dir.is_dir() {
                    return Err(MutationError::new(
                        "history-generation-unavailable",
                        "the referenced generation is no longer materialized",
                    ));
                }
                identity_generation = Some(generation.to_string());
                identity_manifest = opened.generation_manifest_sha256(generation);
                frame_history_id = Some(id.to_string());
                target = gen_dir;
            }
        }
        store = Some(opened);
    } else {
        store = None;
        target = workdir.to_path_buf();
    }

    // Document: the Core projection of the same pinned path. A projection
    // failure degrades the document to null but never the frame identity —
    // the client keeps its generation token and can still merge.
    let document = match project_document_json(&target) {
        Ok(value) => value,
        Err(message) => {
            diagnostics.push(json!({
                "code": "document-projection-failed",
                "message": message,
            }));
            Value::Null
        }
    };

    let review = crate::queue::snapshot_readonly(&target);
    let state = crate::collab::document_state_readonly(&target);
    let resolve_manifest = |generation: &str| -> Option<String> {
        store
            .as_ref()
            .and_then(|store| store.generation_manifest_sha256(generation))
    };
    let history_list = history::list(&target, &resolve_manifest);

    for warning in docx2typed_store::pending_recovery(workdir) {
        diagnostics.push(json!({ "code": "pending-recovery", "message": warning }));
    }

    Ok(json!({
        "schema": FRAME_SCHEMA,
        "backed": backed,
        "identity": {
            "generation": identity_generation,
            "generation_manifest_sha256": identity_manifest,
        },
        "generation": identity_generation,
        "generation_manifest_sha256": identity_manifest,
        "history_id": frame_history_id,
        "current_snapshot": state.get("current_snapshot").cloned(),
        "review_base": state.get("review_base").cloned(),
        "staged_snapshot": state.get("staged_snapshot").cloned(),
        "position_contract": POSITION_CONTRACT,
        "document": document,
        "review": review,
        "state": state,
        "history": history_list,
        "diagnostics": diagnostics,
    }))
}
