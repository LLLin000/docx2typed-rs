//! Deep module: package profiling and declarative planning (issue #55
//! slice). Consumes immutable bytes and validated domain values and returns
//! a declarative `ChangeSet`/`BuildPlan`; never publishes workdir files or
//! external artifacts itself.
//!
//! The no-op contract (frozen PRD decision 13): extract stores the source
//! package bytes verbatim as `_template.docx`; a pristine (unedited) build
//! replays those bytes via byte copy — never recompression. Python's
//! reference no-op output is byte-identical to the source package, so
//! replay reproduces the Python Reference output exactly.

pub mod document_projection;
pub mod document_search;
pub mod document_view;
pub mod edit_state;
pub mod govern;
pub mod inspect;
pub mod prose;
pub mod token_table;
pub mod workspace_identity;
pub mod xml_walker;

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use docx2typed_protocol::{bytes_sha256, file_sha256, resolve_path};
use zip::ZipArchive;

pub const FORMAT_SCHEMA: &str = "typed-format-1";
pub const STYLES_SCHEMA: &str = "typed-styles-1";
pub const MODEL_VERSION: i64 = 1;
pub const CANONICALIZER_VERSION: i64 = 1;

/// One workdir asset to materialize.
#[derive(Clone, Debug)]
pub enum Asset {
    /// path, bytes to write
    Bytes(String, Vec<u8>),
    /// path, source file to byte-copy verbatim
    CopySource { path: String, source: PathBuf },
    /// path to remove. Extraction is a BASELINE CONSTRUCTOR: when it replaces a
    /// workdir's baseline it has to clear what was applied on top of the old
    /// one, and removing it in the SAME published set as the writes is what
    /// keeps the workdir from being left half-reset.
    Remove(String),
}

/// The workdir's baseline generation record. Extraction increments it and a
/// reset does NOT clear it — which is the whole point: a reset that restores the
/// IDENTICAL document leaves nothing else behind to distinguish it, because the
/// template is byte-identical by construction and every other file it clears is
/// gone. A session anchors to this number as well as to the template, so
/// "same content, new lifecycle" is detectable.
pub const BASELINE_FILE: &str = "baseline.json";
pub const BASELINE_SCHEMA: &str = "docx2typed-baseline-1";

/// The baseline generation this workdir is on, and the source it was built from.
pub fn read_baseline_epoch(workdir: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(workdir.join(BASELINE_FILE)).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    if value.get("schema").and_then(serde_json::Value::as_str) != Some(BASELINE_SCHEMA) {
        return None;
    }
    value.get("epoch").and_then(serde_json::Value::as_u64)
}

/// The workdir's version pointer. The Store owns its CONTENTS; the layout
/// constant lives here because extraction has to clear it when it replaces a
/// baseline, and a literal in two crates is a literal that drifts.
pub const POINTER_FILE: &str = "workdir.json";

/// Declarative extract result: the assets a Store commits into a workdir.
#[derive(Clone, Debug)]
pub struct ChangeSet {
    pub assets: Vec<Asset>,
}

#[derive(Clone, Debug)]
pub struct BuildPlan {
    /// Byte-copy source for the output package.
    pub template: PathBuf,
    /// true = pristine no-op replay; false = edited (not implemented in
    /// this slice; plan_build rejects edits instead).
    pub replay: bool,
    /// Issue #58: island edits to apply to the template bytes before
    /// publication (empty = pristine replay). The edits were validated
    /// against the template during planning.
    pub edits: Vec<crate::prose::IslandEdit>,
    /// PR 3: structural edits (new / deleted paragraphs) applied after the
    /// island edits. Validated against the template during planning.
    pub structure: crate::prose::StructureEdits,
    /// The format engine's plan for this workdir, read from `format_plan.json`.
    /// The build EXECUTES it and the verifier checks it; neither re-derives the
    /// rules (ADR 0050).
    pub format_plan: Option<serde_json::Value>,
}

/// Workdir metadata produced by validation, shared by build/verify/open.
#[derive(Clone, Debug)]
pub struct WorkdirMeta {
    pub root: PathBuf,
    pub template: PathBuf,
    pub template_sha256: String,
    pub source_sha256: String,
    pub typed_sha256: String,
    /// true when typed.md matches the pristine extract record.
    pub pristine: bool,
}

#[derive(Debug)]
pub enum CoreError {
    Io(std::io::Error),
    /// message carries the domain failure text (code prefix kebab or the
    /// `workdir-invalid` fallback, mirroring Python's domain diagnostics).
    Domain(String),
    /// message is a plain failure text (no registered code prefix).
    Message(String),
}

impl CoreError {
    pub fn io(error: std::io::Error) -> Self {
        CoreError::Io(error)
    }

    pub fn message(message: impl Into<String>) -> Self {
        CoreError::Message(message.into())
    }
}

impl std::fmt::Display for CoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CoreError::Io(error) => write!(formatter, "{error}"),
            CoreError::Domain(message) | CoreError::Message(message) => {
                formatter.write_str(message)
            }
        }
    }
}

impl std::error::Error for CoreError {}

/// `os.path.relpath`-style path: same-drive paths become relative (with
/// `..` segments), cross-drive paths fall back to the absolute form.
pub fn relative_source_path(source: &Path, output_dir: &Path) -> String {
    let source = resolve_path(source);
    let output = resolve_path(output_dir);
    let source_text = source.to_string_lossy();
    let output_text = output.to_string_lossy();
    let drive = |text: &str| -> Option<String> {
        let bytes = text.as_bytes();
        if bytes.len() >= 2 && bytes[1] == b':' {
            Some(text[..1].to_ascii_lowercase())
        } else {
            None
        }
    };
    match (drive(&source_text), drive(&output_text)) {
        (Some(source_drive), Some(output_drive)) if source_drive != output_drive => {
            return source_text.into_owned();
        }
        _ => {}
    }
    let source_parts: Vec<String> = source
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect();
    let output_parts: Vec<String> = output
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect();
    let mut common = 0usize;
    while common < source_parts.len()
        && common < output_parts.len()
        && source_parts[common].eq_ignore_ascii_case(&output_parts[common])
    {
        common += 1;
    }
    let mut segments: Vec<String> = Vec::new();
    for _ in common..output_parts.len() {
        segments.push("..".to_string());
    }
    segments.extend(source_parts[common..].iter().cloned());
    if segments.is_empty() {
        ".".to_string()
    } else {
        segments.join(if cfg!(windows) { "\\" } else { "/" })
    }
}

fn read_member<R: std::io::Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
    index: usize,
) -> Result<(String, Vec<u8>), CoreError> {
    let mut member = archive
        .by_index(index)
        .map_err(|error| CoreError::Message(format!("not a valid DOCX: {error}")))?;
    let name = member.name().to_string();
    let mut buf = Vec::new();
    member.read_to_end(&mut buf).map_err(CoreError::io)?;
    Ok((name, buf))
}

/// Profile a source DOCX: validates it is a readable zip with
/// `word/document.xml`, and computes the extract ChangeSet. `outdir` is the
/// target workdir the assets will be committed into (used for the relative
/// source path record).
/// The reference's `PART_KEYS_PATTERN`: headers, footers, footnotes, endnotes
/// and comments -- and deliberately NOT `document.xml`, whose digest lives in
/// `document_xml_sha256`. Getting this set wrong is a whole-key difference in
/// format.json, so it is spelled out rather than approximated by "every xml".
fn is_part_key(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("word/") else {
        return false;
    };
    let Some(stem) = rest.strip_suffix(".xml") else {
        return false;
    };
    if matches!(stem, "footnotes" | "endnotes" | "comments") {
        return true;
    }
    for prefix in ["header", "footer"] {
        if let Some(digits) = stem.strip_prefix(prefix) {
            if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
                return true;
            }
        }
    }
    false
}

/// `sha256` of each header/footer/note part's XML, keyed as the reference keys
/// them.
fn part_digests(package: &[u8]) -> Result<BTreeMap<String, String>, CoreError> {
    let mut out = BTreeMap::new();
    let reader = std::io::Cursor::new(package);
    let mut archive = ZipArchive::new(reader)
        .map_err(|error| CoreError::Message(format!("not a valid DOCX: {error}")))?;
    for index in 0..archive.len() {
        let (name, bytes) = read_member(&mut archive, index)?;
        if is_part_key(&name) {
            // The reference keys by PART KEY ("comments", "endnotes"), not by
            // the package path ("word/comments.xml").
            let key = name
                .strip_prefix("word/")
                .and_then(|rest| rest.strip_suffix(".xml"))
                .unwrap_or(&name)
                .to_string();
            out.insert(key, bytes_sha256(&bytes));
        }
    }
    Ok(out)
}

/// How many edits the workdir currently carries on top of its baseline.
///
/// `_template.docx` stays the file `extract` wrote — a commit does not fold
/// edits into it, and `build` does not touch the workdir at all — so the current
/// logical state differs from the baseline by exactly what the sidecars hold.
/// That is the condition for "this workdir has moved past its baseline", and
/// unlike a version counter it cannot be fooled by history: a workdir whose
/// edits were all undone reads as zero.
///
/// Measured on a real workdir: after `extract` → edit → `commit_sync`, the
/// template still hashed equal to the source while `islands.json` held the edit,
/// and `build_docx` left every one of those files alone.
fn applied_edit_count(workdir: &Path) -> usize {
    let islands = crate::prose::load_islands(workdir)
        .map(|edits| edits.len())
        .unwrap_or(0);
    let structure = crate::prose::load_structure(workdir)
        .map(|edits| edits.new_paragraphs.len() + edits.deleted_paragraphs.len())
        .unwrap_or(0);
    let format = std::fs::read_to_string(workdir.join("format_plan.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|plan| {
            plan.get("paragraphs")
                .and_then(|rows| rows.as_array())
                .map(Vec::len)
        })
        .unwrap_or(0);
    islands + structure + format
}

/// What `plan_extract` does when the target workdir has already moved past its
/// baseline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BaselineReset {
    /// Refuse. The default, because a workdir that carries work the source does
    /// not have is not a workdir the incoming file may silently replace.
    Refuse,
    /// Allow the reset: the caller has declared that the incoming file becomes
    /// the current state and that the applied edits stay only in history. A
    /// pending draft is still refused — discarding uncommitted work needs its
    /// own explicit act, not the same switch.
    AllowCommitted,
}

pub fn plan_extract(source: &Path, outdir: &Path) -> Result<ChangeSet, CoreError> {
    plan_extract_with(source, outdir, BaselineReset::Refuse)
}

pub fn plan_extract_with(
    source: &Path,
    outdir: &Path,
    reset: BaselineReset,
) -> Result<ChangeSet, CoreError> {
    // Taking in a document that changed outside the tool is `extract` over the
    // SAME workdir: a workdir is its document's permanent editing home and is
    // never replaced to follow a change (ADR 0048, CONTEXT.md "Baseline drift
    // policy").
    //
    // `extract` is a BASELINE CONSTRUCTOR, not a merge: it re-derives
    // `_template.docx` and the typed projection from the source. Running it over
    // a workdir that has moved past its baseline therefore resets that baseline,
    // and it does so whether or not the source changed — measured: a workdir
    // with one committed version, re-extracted with a byte-identical source,
    // lost the committed text from its current state.
    //
    // So the precondition is not "the source did not change", it is "the workdir
    // is AT its baseline". Two conditions say it is not, and each names a
    // different way out:
    if outdir.join(crate::edit_state::STATE_FILE).exists() {
        if let Ok(state) = crate::edit_state::classify_edit_state(outdir) {
            if state.state != "clean" {
                return Err(CoreError::Domain(format!(
                    "workdir-has-pending-edit: {} carries a `{}` draft; commit it first \
                     (`commit_sync`), then sync the source in — syncing now would discard the \
                     pending edit",
                    outdir.to_string_lossy(),
                    state.state
                )));
            }
        }
    }
    // An applied edit is work the source does not have. Comparing the CURRENT
    // state against the baseline — rather than counting history — is what keeps
    // a workdir whose edits were all undone from being refused: history keeps
    // the version either way, and "history still has it" is not the same claim
    // as "this is still the current version".
    if reset == BaselineReset::Refuse {
        let applied = applied_edit_count(outdir);
        if applied > 0 {
            return Err(CoreError::Domain(format!(
                "workdir-not-at-baseline: {} carries {applied} edit(s) on top of its baseline \
                 that the source does not have; exporting keeps them but does not put the \
                 workdir back on its baseline, so either reset the workdir to the source on \
                 purpose (`--discard-committed-versions`) or start the incoming file in a \
                 workdir of its own",
                outdir.to_string_lossy()
            )));
        }
    }
    let source_path = resolve_path(source);
    if !source_path.is_file() {
        return Err(CoreError::Domain(format!(
            "file not found: {}",
            source_path.to_string_lossy()
        )));
    }
    let file = File::open(&source_path).map_err(CoreError::io)?;
    let mut archive = ZipArchive::new(file)
        .map_err(|error| CoreError::Message(format!("not a valid DOCX: {error}")))?;
    let mut package_manifest: BTreeMap<String, String> = BTreeMap::new();
    let mut document_xml: Option<Vec<u8>> = None;
    for index in 0..archive.len() {
        let (name, bytes) = read_member(&mut archive, index)?;
        if name == "word/document.xml" {
            document_xml = Some(bytes.clone());
        }
        package_manifest.insert(name, bytes_sha256(&bytes));
    }
    let document_xml = document_xml.ok_or_else(|| {
        CoreError::Message("not a valid DOCX: missing word/document.xml".to_string())
    })?;
    if archive.is_empty() {
        return Err(CoreError::Message(
            "not a valid DOCX: empty package".to_string(),
        ));
    }

    let source_sha256 = file_sha256(&source_path).map_err(CoreError::io)?;
    let document_xml_sha256 = bytes_sha256(&document_xml);
    let template_name = "_template.docx".to_string();
    let source_name = source_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| source_path.to_string_lossy().into_owned());

    // The whole typed projection, not a header (#52): this file is what
    // `styles.json` is keyed against and what a version's tree is computed
    // over, so a header-only stub could never match the reference.
    let package_bytes = std::fs::read(&source_path).map_err(CoreError::io)?;
    let projection = crate::document_projection::project_document_bytes(&package_bytes)?;
    let trees = crate::token_table::paragraph_nodes(&package_bytes)?;
    let tokens = crate::token_table::token_table(&package_bytes)?;
    // The vertical-promotion predicate canonicalises a style's rPr minus one
    // `w:vertAlign`, so it needs the same style table `styles.json` is built
    // from.
    let catalog = crate::prose::StyleCatalog::build(&package_bytes)?;
    let (prepared, promoted) = crate::prose::prepare_paragraphs(&trees, &catalog);
    let typed_md = crate::prose::serialize_typed(&source_name, &projection, &prepared);

    // The registry, built from the document's own runs. This was a hardcoded
    // empty object (#48), which made the format lane inert: a restyle is
    // refused unless the style it derives is already registered here.
    let styles_value = crate::prose::build_style_registry(&catalog)?;
    // The reference writes every canonical JSON asset as
    // `json.dumps(..., ensure_ascii=False, indent=2, sort_keys=True) + "\n"`
    // (typed_docx.json_bytes). These bytes are hashed into the tree digest, so
    // the serialization is part of the contract, not a formatting preference.
    let styles_bytes = {
        let mut text = serde_json::to_string_pretty(&styles_value).expect("styles serialize");
        text.push('\n');
        text.into_bytes()
    };
    let styles_sha256 = bytes_sha256(&styles_bytes);

    let paragraphs = crate::prose::format_paragraphs(&projection, &prepared);

    let format = serde_json::json!({
        "schema": FORMAT_SCHEMA,
        "model_version": MODEL_VERSION,
        "canonicalizer_version": CANONICALIZER_VERSION,
        "source": source_name,
        "source_path": relative_source_path(&source_path, outdir),
        "source_sha256": source_sha256,
        "template": template_name,
        "template_sha256": source_sha256,
        "document_xml_sha256": document_xml_sha256,
        "package_manifest": package_manifest,
        "styles_sha256": styles_sha256,
        // Per-part digests, and the paragraph records (#52). Both were empty,
        // which is most of why format.json could not match the reference.
        "parts": part_digests(&package_bytes)?,
        "paragraphs": paragraphs,
        "tokens": crate::token_table::tokens_map(&tokens),
        "source_track_enabled": crate::token_table::source_track_enabled(&package_bytes),
        "uses_date_utc": false,
        "vertical_promotion": { "schema": "2", "promoted": promoted },
        // `typed_sha256` is deliberately absent: the reference pins typed.md
        // through the edit-state sidecar instead, and `validate_workdir`
        // already treats an undeclared fingerprint as authoritative.
    });
    // The reference writes every canonical JSON asset as
    // `json.dumps(..., ensure_ascii=False, indent=2, sort_keys=True) + "\n"`.
    // The trailing newline is part of the file, and these bytes are hashed into
    // the tree digest -- it was the last byte of the difference.
    let format_bytes = {
        let mut text = serde_json::to_string_pretty(&format).expect("format serializes");
        text.push('\n');
        text.into_bytes()
    };

    // The revision inventory, written as a sidecar because a consumer has to be
    // able to address a revision before touching it, and because the projection
    // work needs a record taken before the change to be judged against
    // afterwards. It is derived from the same walk the `revisions` operation
    // serves, so the file and the operation can never disagree.
    let revisions = crate::govern::scan_revisions_bytes(&package_bytes)?;
    let revisions_bytes = {
        let value = crate::govern::revisions_sidecar_value(&revisions);
        let mut text = serde_json::to_string_pretty(&value).expect("revisions serialize");
        text.push('\n');
        text.into_bytes()
    };

    let mut assets = vec![
        Asset::CopySource {
            path: template_name,
            source: source_path,
        },
        Asset::Bytes("format.json".to_string(), format_bytes),
        Asset::Bytes("styles.json".to_string(), styles_bytes),
        Asset::Bytes("typed.md".to_string(), typed_md.into_bytes()),
        Asset::Bytes("revisions.json".to_string(), revisions_bytes),
        Asset::Bytes(
            "revisions.md".to_string(),
            crate::govern::revisions_md(&revisions).into_bytes(),
        ),
    ];
    // The baseline generation, incremented here and never cleared by a reset.
    let epoch = read_baseline_epoch(outdir).unwrap_or(0) + 1;
    assets.push(Asset::Bytes(
        BASELINE_FILE.to_string(),
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema": BASELINE_SCHEMA,
            "epoch": epoch,
        }))
        .expect("serializes"),
    ));
    // Replacing an existing baseline clears what was applied on top of the old
    // one, in this same published set — an extract that writes the new baseline
    // and then deletes the stale sidecars in a second step is not atomic, and a
    // crash between the two leaves exactly the tear this removes.
    if replaces_existing_baseline(outdir) {
        assets.extend(stale_baseline_assets(outdir));
    }
    Ok(ChangeSet { assets })
}

/// Whether this extraction replaces an existing baseline rather than creating
/// one. A workdir that already has a template is being rebuilt, so whatever was
/// applied on top of the old baseline has to go with it.
pub fn replaces_existing_baseline(outdir: &Path) -> bool {
    outdir.join("_template.docx").is_file()
}

/// The files a baseline replacement has to clear, in the SAME published set as
/// the writes that replace the baseline.
///
/// Everything here was expressed against the OLD template: the edit sidecars
/// address its runs, and the projection pins its hash. Leaving them behind
/// produces a worked example of the tear — measured: after an allowed reset the
/// template was the incoming document while `islands.json` still held an edit
/// against the old one and `workdir.json` still pointed at the old version, and
/// the next build refused with `edit-stale` whose only offered escape was to
/// accept the tear.
fn stale_baseline_assets(outdir: &Path) -> Vec<Asset> {
    [
        crate::prose::ISLANDS_FILE,
        crate::prose::STRUCTURE_FILE,
        // The frozen editable baseline the diff and the unrecorded-edit check
        // compare against: it was rendered from the OLD template, so leaving
        // it behind makes every paragraph of the incoming document read as an
        // unrecorded user edit.
        "baseline.view",
        // External-output expectations are expressed against the old epoch,
        // so a baseline replacement clears them with the rest: the next
        // publication re-baselines every path it writes. Keyed by epoch, so
        // a stale file left behind would only be history — but history that
        // survives a reset is a file the inventory never declared.
        "external-targets.json",
        // The format engine's plan and requests are expressed against the old
        // template too, so a baseline replacement clears them with the rest.
        "format_plan.json",
        "format_intents.json",
        crate::edit_state::PROJECTION_FILE,
        crate::edit_state::STATE_FILE,
        POINTER_FILE,
    ]
    .into_iter()
    .filter(|name| outdir.join(name).is_file())
    .map(|name| Asset::Remove(name.to_string()))
    .collect()
}

/// Validate a typed workdir and report its metadata. Mirrors the
/// freshness invariants Python's `validate_workdir` enforces (template
/// fingerprint, package manifest, styles fingerprint, typed record) for the
/// subset of assets this slice produces.
pub fn validate_workdir(workdir: &Path) -> Result<WorkdirMeta, CoreError> {
    let root = resolve_path(workdir);
    if !root.is_dir() {
        return Err(CoreError::Domain(format!(
            "workdir not found: {}",
            root.to_string_lossy()
        )));
    }
    for name in ["typed.md", "format.json", "styles.json", "_template.docx"] {
        if !root.join(name).is_file() {
            return Err(CoreError::Domain(format!("workdir missing: {name}")));
        }
    }
    let format_bytes = std::fs::read(root.join("format.json")).map_err(CoreError::io)?;
    let format: serde_json::Value = serde_json::from_slice(&format_bytes)
        .map_err(|error| CoreError::Domain(format!("invalid workdir JSON: {error}")))?;
    if format.get("schema").and_then(|v| v.as_str()) != Some(FORMAT_SCHEMA)
        || format.get("model_version").and_then(|v| v.as_i64()) != Some(MODEL_VERSION)
        || format.get("canonicalizer_version").and_then(|v| v.as_i64())
            != Some(CANONICALIZER_VERSION)
    {
        return Err(CoreError::Domain(
            "incompatible typed workdir schema".to_string(),
        ));
    }
    let template = root.join("_template.docx");
    let template_sha256 = file_sha256(&template).map_err(CoreError::io)?;
    let template_recorded = format.get("template_sha256").and_then(|v| v.as_str());
    if template_recorded != Some(&template_sha256) {
        return Err(CoreError::Domain(
            "source-drift: template fingerprint changed after extract".to_string(),
        ));
    }
    let manifest_recorded = format.get("package_manifest").and_then(|v| v.as_object());
    let manifest_now = package_manifest(&template)?;
    let manifest_json = serde_json::to_value(&manifest_now).expect("manifest serializes");
    if manifest_recorded != manifest_json.as_object() {
        return Err(CoreError::Domain(
            "source-drift: template package manifest changed after extract".to_string(),
        ));
    }
    let styles_sha256 = file_sha256(&root.join("styles.json")).map_err(CoreError::io)?;
    if format.get("styles_sha256").and_then(|v| v.as_str()) != Some(&styles_sha256) {
        return Err(CoreError::Domain(
            "source-drift: styles.json changed after extract".to_string(),
        ));
    }
    let source_sha256 = format
        .get("source_sha256")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let typed_bytes = std::fs::read(root.join("typed.md")).map_err(CoreError::io)?;
    let typed_sha256 = bytes_sha256(&typed_bytes);
    // `pristine` is only meaningful when the extractor declared a typed
    // fingerprint: the Rust extractor writes `typed_sha256` into
    // format.json; the Python Reference does not (it pins typed.md through
    // the edit-state sidecar instead). An undeclared fingerprint means the
    // typed record is authoritative and cannot drift by this check — edit
    // freshness covers it (issue #56).
    let typed_recorded = format.get("typed_sha256").and_then(|v| v.as_str());
    let pristine = match typed_recorded {
        Some(recorded) => recorded == typed_sha256,
        None => {
            if root.join("edit.state.json").is_file() {
                true
            } else if let Ok(package_bytes) = std::fs::read(&template) {
                if let (Ok(projection), Ok(trees), Ok(catalog)) = (
                    crate::document_projection::project_document_bytes(&package_bytes),
                    crate::token_table::paragraph_nodes(&package_bytes),
                    crate::prose::StyleCatalog::build(&package_bytes),
                ) {
                    let source_name = format
                        .get("source")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    let (prepared, _) = crate::prose::prepare_paragraphs(&trees, &catalog);
                    let expected =
                        crate::prose::serialize_typed(source_name, &projection, &prepared);
                    bytes_sha256(expected.as_bytes()) == typed_sha256
                } else {
                    true
                }
            } else {
                true
            }
        }
    };
    Ok(WorkdirMeta {
        root,
        template,
        template_sha256,
        source_sha256,
        typed_sha256,
        pristine,
    })
}

pub fn plan_build(workdir: &Path) -> Result<BuildPlan, CoreError> {
    let meta = validate_workdir(workdir)?;
    // Island edits (issue #58) are the one supported edited-build path: when
    // the sidecar records edits, the pristine gate does not apply — the build
    // applies those edits to the template bytes instead of replaying them.
    let islands = crate::prose::load_islands(&meta.root)?;
    let structure = crate::prose::load_structure(&meta.root)?;
    // The format engine's plan, if this workdir has one. The build executes it;
    // it does not read the requests or re-derive the rules.
    let format_plan: Option<serde_json::Value> =
        std::fs::read_to_string(meta.root.join("format_plan.json"))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok());
    let is_store = meta.root.join(".docx2typed-store").is_dir();
    if !meta.pristine
        && !is_store
        && islands.is_empty()
        && structure.is_empty()
        && format_plan.is_none()
    {
        return Err(CoreError::Domain(
            "workdir-invalid: typed edits are not implemented in the protocol-major-1 Rust slice (issue #55; edits land in #56+)".to_string(),
        ));
    }
    if meta.root.join("edit.state.json").is_file() {
        {
            let state = crate::edit_state::classify_edit_state(&meta.root)?;
            match state.state.as_str() {
                "clean" => {}
                "dirty" => {
                    return Err(CoreError::Domain(
                    "edit-dirty: edit.md has unapplied changes; run `docx2typed edit sync` \
                     to apply them or `docx2typed edit refresh --discard` to replace the projection"
                        .to_string(),
                ));
                }
                "stale-clean" => {
                    return Err(CoreError::Domain(
                        "edit-stale: typed.md changed after the projection was generated; \
                     run `docx2typed edit refresh` first"
                            .to_string(),
                    ));
                }
                _ => {
                    return Err(CoreError::Domain(
                        "edit-conflict: typed.md and edit.md both changed; resolve explicitly \
                     before building"
                            .to_string(),
                    ));
                }
            }
        }
    }
    if !islands.is_empty() {
        // Global invariant gate before an edited build (issue #58):
        // package prevalidation + per-part XML well-formedness + opaque
        // containment + leaf provability; failure rejects the whole build
        // with no output.
        crate::prose::validate_islands(&meta.template, &islands)?;
    }
    if !structure.is_empty() {
        // PR 3: the same fail-closed gate for structural edits — the
        // inherit/deleted paragraph must exist, be plain prose, and carry
        // no protected structure; failure rejects the whole build.
        crate::prose::validate_structure(&meta.template, &structure)?;
    }
    Ok(BuildPlan {
        template: meta.template,
        replay: true,
        edits: islands,
        structure,
        format_plan,
    })
}

/// Per-part SHA-256 of a package, used for manifest identity.
pub fn package_manifest(path: &Path) -> Result<BTreeMap<String, String>, CoreError> {
    let file = File::open(path).map_err(CoreError::io)?;
    let mut archive = ZipArchive::new(file)
        .map_err(|error| CoreError::Message(format!("not a valid DOCX: {error}")))?;
    let mut manifest = BTreeMap::new();
    for index in 0..archive.len() {
        let (name, bytes) = read_member(&mut archive, index)?;
        manifest.insert(name, bytes_sha256(&bytes));
    }
    Ok(manifest)
}

/// Per-part SHA-256 of a package held in memory (issue #59: the governed
/// mutation closure regenerates `format.json` from a settled template
/// without a round-trip through disk).
pub fn package_manifest_bytes(package: &[u8]) -> Result<BTreeMap<String, String>, CoreError> {
    let file = std::io::Cursor::new(package.to_vec());
    let mut archive = ZipArchive::new(file)
        .map_err(|error| CoreError::Message(format!("not a valid DOCX: {error}")))?;
    let mut manifest = BTreeMap::new();
    for index in 0..archive.len() {
        let (name, bytes) = read_member(&mut archive, index)?;
        manifest.insert(name, bytes_sha256(&bytes));
    }
    Ok(manifest)
}

/// Rebuild the workdir `format.json` after a governed mutation replaced
/// `_template.docx` (issue #59): the template fingerprint, the document XML
/// hash, and the package manifest are recomputed; every other record keeps
/// its identity (the typed.md fingerprint stays authoritative).
pub fn regenerate_workdir_format(format: &serde_json::Value, template: &[u8]) -> serde_json::Value {
    use std::collections::BTreeMap;
    let mut format = format.clone();
    let template_sha256 = bytes_sha256(template);
    let document_xml_sha256 = {
        // Extract word/document.xml from the package bytes.
        let file = std::io::Cursor::new(template.to_vec());
        let mut archive = match zip::ZipArchive::new(file) {
            Ok(archive) => archive,
            Err(_) => return format,
        };
        let mut hash = String::new();
        for index in 0..archive.len() {
            let Ok(mut member) = archive.by_index(index) else {
                continue;
            };
            if member.name() != "word/document.xml" {
                continue;
            }
            let mut bytes = Vec::new();
            if member.read_to_end(&mut bytes).is_ok() {
                hash = bytes_sha256(&bytes);
            }
            break;
        }
        hash
    };
    let manifest: BTreeMap<String, String> = package_manifest_bytes(template).unwrap_or_default();
    let manifest_value = serde_json::to_value(manifest).unwrap_or_default();
    if let Some(object) = format.as_object_mut() {
        object.insert(
            "template_sha256".to_string(),
            serde_json::json!(template_sha256),
        );
        object.insert(
            "source_sha256".to_string(),
            serde_json::json!(template_sha256),
        );
        object.insert(
            "document_xml_sha256".to_string(),
            serde_json::json!(document_xml_sha256),
        );
        object.insert("package_manifest".to_string(), manifest_value);
    }
    format
}
