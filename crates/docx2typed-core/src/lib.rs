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
}

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
pub fn plan_extract(source: &Path, outdir: &Path) -> Result<ChangeSet, CoreError> {
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

    let typed_md = format!(
        "<!--@typed schema=\"1\" format=\"format.json\" styles=\"styles.json\" template=\"_template.docx\" source=\"{}\"-->\n",
        source_name
    );
    let typed_sha256 = bytes_sha256(typed_md.as_bytes());

    // The registry, built from the document's own runs. This was a hardcoded
    // empty object (#48), which made the format lane inert: a restyle is
    // refused unless the style it derives is already registered here.
    let styles_value =
        crate::prose::build_style_registry(&std::fs::read(&source_path).map_err(CoreError::io)?)?;
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

    let document_xml_text = String::from_utf8_lossy(&document_xml);
    let source_track_enabled = document_xml_text.contains("w:trackChanges");

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
        "parts": {},
        "paragraphs": [],
        "tokens": {},
        "source_track_enabled": source_track_enabled,
        "uses_date_utc": false,
        "typed_sha256": typed_sha256,
    });
    let format_bytes = serde_json::to_vec_pretty(&format).expect("format serializes");

    Ok(ChangeSet {
        assets: vec![
            Asset::CopySource {
                path: template_name,
                source: source_path,
            },
            Asset::Bytes("format.json".to_string(), format_bytes),
            Asset::Bytes("styles.json".to_string(), styles_bytes),
            Asset::Bytes("typed.md".to_string(), typed_md.into_bytes()),
        ],
    })
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
    let pristine = typed_recorded.is_none_or(|recorded| recorded == typed_sha256);
    Ok(WorkdirMeta {
        root,
        template,
        template_sha256,
        source_sha256,
        typed_sha256,
        pristine,
    })
}

/// Plan a build: a pristine workdir replays the template bytes (no-op);
/// an edited workdir is rejected in this slice (edits land in #56+).
///
/// Issue #56: workdirs with an edit projection (edit.state.json present)
/// additionally enforce Python's `require_clean_edit` freshness gate — a
/// non-clean state blocks the build with the same diagnostic code Python
/// emits. Workdirs without an edit projection (e.g. Rust-extracted no-op
/// workdirs, which have no edit.md/edit.state.json) keep the #55
/// pristine-replay behavior; Python refuses those with edit-state-missing,
/// and the divergence is deliberate to preserve the frozen no-op contract.
pub fn plan_build(workdir: &Path) -> Result<BuildPlan, CoreError> {
    let meta = validate_workdir(workdir)?;
    // Island edits (issue #58) are the one supported edited-build path: when
    // the sidecar records edits, the pristine gate does not apply — the build
    // applies those edits to the template bytes instead of replaying them.
    let islands = crate::prose::load_islands(&meta.root)?;
    let structure = crate::prose::load_structure(&meta.root)?;
    if !meta.pristine && islands.is_empty() && structure.is_empty() {
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
