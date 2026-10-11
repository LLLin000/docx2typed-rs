//! Workspace identity: the permanent Family and Workspace ids, mirroring
//! `scripts/workspace_identity.py`.
//!
//! The identity model is deliberately not "DOCX → workspace": `family_id` is
//! the logical document's lineage and never changes, while `workspace_id`
//! names one managed workdir for that family on this machine. Both live in
//! `workspace.json` at the workdir root — engine-owned, written by the
//! engine, and *not* part of the store pointer (`workdir.json` is rebuilt by
//! the store lane on every commit and would drop foreign keys). Losing the
//! global resolver registry must never damage this file: the registry is a
//! cache, this file is authoritative.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::CoreError;

/// File name of the identity record inside a workdir root.
pub const IDENTITY_FILE: &str = "workspace.json";
/// Payload schema tag; a file carrying anything else is not an identity.
pub const IDENTITY_SCHEMA: &str = "docx2typed-workspace-1";

/// The permanent ids a workspace carries. `schema` is never stored here: it
/// is written as a constant by [`write_identity`] and only checked on read.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Identity {
    pub family_id: String,
    pub workspace_id: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub origin_family: Option<String>,
    #[serde(default)]
    pub origin_version: Option<String>,
    /// DOCX file instances a human bound to this family. Python records the
    /// same fact as an `adopted` observation in its resolver cache; Rust has
    /// no registry, so the evidence lives in the engine-owned file — the
    /// remaining resolver reads it, and an empty list serializes nothing (a
    /// Python-written `workspace.json` stays byte-identical on rewrite).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub adopted: Vec<Adoption>,
}

/// One verified adoption: the evidence a lineage question was answered about.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Adoption {
    /// The opaque proof handle the question was asked with. Redundant with
    /// the fields below, kept so a caller can quote the exact evidence.
    #[serde(default)]
    pub lineage_token: String,
    pub sha256: String,
    /// Python's `f"{stat.st_dev:#x}"` (Windows: volume serial number).
    pub volume: String,
    /// Python's `f"{stat.st_ino:#x}"` (Windows: file index).
    pub file_id: String,
    pub path: String,
    #[serde(default)]
    pub adopted_at: String,
}

/// An unreadable adoption token: not base64url, not JSON, or no `sha256`.
pub const ADOPTION_TOKEN_INVALID: &str = "adoption-token-invalid";
/// The file changed while the lineage question was open.
pub const ADOPTION_TOKEN_STALE: &str = "adoption-token-stale";
/// The answer names a family this workspace is not.
pub const ADOPTION_REFUSED: &str = "adoption-refused";

/// Audit-only provenance of a forked workspace (`origin_family` + the
/// version name it was forked from); never used for resolution.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Origin {
    #[serde(default)]
    pub family_id: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
}

/// New family id: `f_` + UUID v4 hex.
pub fn new_family_id() -> String {
    format!("f_{}", uuid::Uuid::new_v4().simple())
}

/// New workspace id: `ws_` + UUID v4 hex.
pub fn new_workspace_id() -> String {
    format!("ws_{}", uuid::Uuid::new_v4().simple())
}

/// The workdir's identity, or `None` when it has none yet (legacy workdir),
/// or when the file is unreadable, unparsable, carries a foreign schema tag,
/// or lacks usable ids.
pub fn read_identity(workdir: &Path) -> Option<Identity> {
    let text = std::fs::read_to_string(workdir.join(IDENTITY_FILE)).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    if value.get("schema").and_then(serde_json::Value::as_str) != Some(IDENTITY_SCHEMA) {
        return None;
    }
    let identity: Identity = serde_json::from_value(value).ok()?;
    if identity.family_id.is_empty() || identity.workspace_id.is_empty() {
        return None;
    }
    Some(identity)
}

/// Atomically publish one identity file (temp + rename, LF newlines, sorted
/// keys, 2-space indent) and return the file it landed on.
pub fn write_identity(workdir: &Path, identity: &Identity) -> Result<PathBuf, CoreError> {
    let target = workdir.join(IDENTITY_FILE);
    // A BTreeMap (not serde_json's Map) pins key order independent of the
    // `preserve_order` feature unification elsewhere in the workspace.
    let mut payload: BTreeMap<String, serde_json::Value> = match serde_json::to_value(identity) {
        Ok(serde_json::Value::Object(fields)) => fields.into_iter().collect(),
        Ok(_) => {
            return Err(CoreError::message(
                "identity does not serialize to an object",
            ))
        }
        Err(error) => return Err(CoreError::message(error.to_string())),
    };
    payload.insert(
        "schema".to_string(),
        serde_json::Value::String(IDENTITY_SCHEMA.to_string()),
    );
    let mut text = serde_json::to_string_pretty(&payload)
        .map_err(|error| CoreError::message(error.to_string()))?;
    text.push('\n');

    let temp = temp_path(workdir);
    if let Err(error) = std::fs::write(&temp, text.as_bytes()) {
        let _ = std::fs::remove_file(&temp);
        return Err(CoreError::io(error));
    }
    if let Err(error) = std::fs::rename(&temp, &target) {
        let _ = std::fs::remove_file(&temp);
        return Err(CoreError::io(error));
    }
    Ok(target)
}

fn temp_path(workdir: &Path) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    workdir.join(format!(
        ".{IDENTITY_FILE}.{}-{nanos}.tmp",
        std::process::id()
    ))
}

/// `(identity, created)`. Mints ids for a workdir that has none — the one
/// idempotent write an old workspace receives, and the reason its history
/// stays addressable without a migration. Refuses (with Python's
/// `not a typed workdir:` text) when the workdir is not typed at all.
pub fn ensure_identity(
    workdir: &Path,
    origin: Option<Origin>,
) -> Result<(Identity, bool), CoreError> {
    if let Some(existing) = read_identity(workdir) {
        return Ok((existing, false));
    }
    if !workdir.join("typed.md").is_file() {
        return Err(CoreError::Domain(format!(
            "not a typed workdir: {}",
            workdir.display()
        )));
    }
    let origin = origin.unwrap_or_default();
    let identity = Identity {
        family_id: new_family_id(),
        workspace_id: new_workspace_id(),
        created_at: docx2typed_protocol::utc_now_iso(),
        origin_family: origin.family_id,
        origin_version: origin.version,
        adopted: Vec::new(),
    };
    write_identity(workdir, &identity)?;
    Ok((read_identity(workdir).unwrap_or(identity), true))
}

/// The evidence one open lineage question is about: a content hash plus the
/// local file object, mirroring Python's `adoption_token` payload. Opaque to
/// the caller — it is minted by the refusal that asked the question and handed
/// straight back to [`workspace_adopt`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AdoptionToken {
    pub sha256: String,
    #[serde(default)]
    pub volume: Option<String>,
    #[serde(default)]
    pub file_id: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
}

/// Mint the evidence handle for one file instance: content hash + volume/file
/// id + path, base64url (padded, as `base64.urlsafe_b64encode` writes it).
pub fn adoption_token(docx: &Path) -> Result<String, CoreError> {
    let (volume, file_id) = file_identity(docx);
    let token = AdoptionToken {
        sha256: docx2typed_protocol::file_sha256(docx).map_err(CoreError::io)?,
        volume: Some(volume),
        file_id: Some(file_id),
        path: Some(docx.to_string_lossy().into_owned()),
    };
    let payload =
        serde_json::to_string(&token).map_err(|error| CoreError::message(error.to_string()))?;
    Ok(base64url_encode(payload.as_bytes()))
}

/// Decode an adoption token. Anything unreadable, and anything without a
/// usable `sha256`, is [`ADOPTION_TOKEN_INVALID`]: a guess here would bind the
/// wrong document to a family.
pub fn decode_adoption_token(token: &str) -> Result<AdoptionToken, CoreError> {
    let invalid = || {
        CoreError::Domain(format!(
            "{ADOPTION_TOKEN_INVALID}: the adoption token is not readable"
        ))
    };
    let raw = base64url_decode(token).ok_or_else(invalid)?;
    let text = String::from_utf8(raw).map_err(|_| invalid())?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|_| invalid())?;
    if !value.is_object() {
        return Err(invalid());
    }
    let token: AdoptionToken = serde_json::from_value(value).map_err(|_| invalid())?;
    if token.sha256.is_empty() {
        return Err(invalid());
    }
    Ok(token)
}

/// `(volume, file_id)` for one file — stable across rename and in-place saves
/// on the same volume, and *only* that (a copy is a new object). The same two
/// figures Python's `file_identity` reports, so a token minted by either
/// engine verifies in the other; an unreadable path yields `("", "")`.
pub fn file_identity(path: &Path) -> (String, String) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match std::fs::metadata(path) {
            Ok(metadata) => (
                format!("{:#x}", metadata.dev()),
                format!("{:#x}", metadata.ino()),
            ),
            Err(_) => (String::new(), String::new()),
        }
    }
    #[cfg(windows)]
    {
        windows_file_identity(path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        (String::new(), String::new())
    }
}

/// CPython reads the pair from `GetFileInformationByHandleEx(FileIdInfo)`:
/// `st_dev` is `VolumeSerialNumber` and `st_ino` the low half of `FileId`.
/// `std`'s `volume_serial_number`/`file_index` are still unstable, so the one
/// call is made here; a filesystem without file ids yields `("", "")`, the
/// same shape Python's `OSError` path produces.
#[cfg(windows)]
fn windows_file_identity(path: &Path) -> (String, String) {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;

    #[repr(C)]
    struct FileIdInfo {
        volume_serial_number: u64,
        file_id: [u8; 16],
    }
    const FILE_ID_INFO_CLASS: i32 = 0x12;
    extern "system" {
        fn GetFileInformationByHandleEx(
            file: *mut core::ffi::c_void,
            class: i32,
            info: *mut core::ffi::c_void,
            size: u32,
        ) -> i32;
    }

    let Ok(file) = std::fs::File::open(path) else {
        return (String::new(), String::new());
    };
    let mut info = MaybeUninit::<FileIdInfo>::uninit();
    // SAFETY: a live handle borrowed from `file`, and an out buffer of the
    // size and layout the class requires; the call either fills it or fails.
    let filled = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FILE_ID_INFO_CLASS,
            info.as_mut_ptr().cast(),
            std::mem::size_of::<FileIdInfo>() as u32,
        )
    };
    if filled == 0 {
        return (String::new(), String::new());
    }
    // SAFETY: success means the call initialized the whole buffer.
    let info = unsafe { info.assume_init() };
    let index = u64::from_le_bytes(info.file_id[..8].try_into().expect("8 bytes"));
    (
        format!("{:#x}", info.volume_serial_number),
        format!("{index:#x}"),
    )
}

/// Bind one DOCX to a family once a human answered the lineage question.
///
/// `root` is that family's workdir — the workspace the refusal listed as a
/// candidate — because the family's own `workspace.json` is the only family
/// proof this engine owns (Python's resolver cache is the equivalent, and the
/// remaining Rust resolver reads the `adopted` records written here).
///
/// The token pins both the content hash and the local file object, so a file
/// edited while the question was on screen is refused
/// ([`ADOPTION_TOKEN_STALE`]) instead of adopted by accident. Repeating the
/// same confirmation is a no-op: the record is keyed by hash + path.
pub fn workspace_adopt(
    root: &Path,
    family_id: &str,
    adoption_token: &str,
) -> Result<serde_json::Value, CoreError> {
    let token = decode_adoption_token(adoption_token)?;
    let text = token.path.unwrap_or_default();
    let path = Path::new(&text);
    if !path.is_file() {
        return Err(CoreError::Domain(format!(
            "{ADOPTION_REFUSED}: file not found: {}",
            path.display()
        )));
    }
    let sha256 = docx2typed_protocol::file_sha256(path).map_err(CoreError::io)?;
    let (volume, file_id) = file_identity(path);
    if token.sha256 != sha256
        || token.volume.as_deref() != Some(volume.as_str())
        || token.file_id.as_deref() != Some(file_id.as_str())
    {
        return Err(CoreError::Domain(format!(
            "{ADOPTION_TOKEN_STALE}: the file changed while the question was open; re-run workdir_open on it"
        )));
    }
    let Some(mut identity) = read_identity(root) else {
        return Err(CoreError::Domain(format!(
            "{ADOPTION_REFUSED}: no workspace identity at {}; the family's workspace is missing",
            root.display()
        )));
    };
    if identity.family_id != family_id {
        return Err(CoreError::Domain(format!(
            "{ADOPTION_REFUSED}: {} holds family {}, not {family_id}",
            root.display(),
            identity.family_id
        )));
    }
    if !identity
        .adopted
        .iter()
        .any(|record| record.sha256 == sha256 && record.path == text)
    {
        identity.adopted.push(Adoption {
            lineage_token: adoption_token.to_string(),
            sha256: sha256.clone(),
            volume,
            file_id,
            path: text,
            adopted_at: docx2typed_protocol::utc_now_iso(),
        });
        write_identity(root, &identity)?;
    }
    Ok(serde_json::json!({
        "status": "resolved",
        "reason": "adopted",
        "family_id": family_id,
        "workspace_id": identity.workspace_id,
        "workspace": root.to_string_lossy(),
        "next": "call workdir_open on that workspace",
    }))
}

/// URL-safe base64 with padding (`base64.urlsafe_b64encode`).
fn base64url_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let second = *chunk.get(1).unwrap_or(&0);
        let third = *chunk.get(2).unwrap_or(&0);
        let packed = (u32::from(chunk[0]) << 16) | (u32::from(second) << 8) | u32::from(third);
        for (slot, shift) in [(0usize, 18u32), (1, 12), (2, 6), (3, 0)] {
            if slot <= chunk.len() {
                out.push(ALPHABET[((packed >> shift) & 63) as usize] as char);
            }
        }
    }
    for _ in 0..(3 - bytes.len() % 3) % 3 {
        out.push('=');
    }
    out
}

/// Inverse of [`base64url_encode`]; padding is optional, as Python re-pads
/// before decoding.
fn base64url_decode(text: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut lookup = [255u8; 256];
    for (index, byte) in ALPHABET.iter().enumerate() {
        lookup[*byte as usize] = index as u8;
    }
    let mut out = Vec::with_capacity(text.len() * 3 / 4 + 3);
    let mut packed: u32 = 0;
    let mut bits = 0u32;
    for byte in text.bytes() {
        if byte == b'=' {
            continue;
        }
        let value = lookup[byte as usize];
        if value == 255 {
            return None;
        }
        packed = (packed << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((packed >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scratch workdir under the OS temp dir, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "docx2typed-identity-{name}-{}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("scratch dir");
            Scratch(path)
        }

        fn typed(&self) -> &Path {
            std::fs::write(self.0.join("typed.md"), b"# typed\n").expect("typed.md");
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// `<prefix>_<32 hex>`, exactly what Python's `uuid4().hex` produces.
    fn shaped(prefix: &str, value: &str) -> bool {
        value
            .strip_prefix(prefix)
            .is_some_and(|hex| hex.len() == 32 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
    }

    #[test]
    fn minted_ids_carry_the_python_prefix_and_hex_shape() {
        let family = new_family_id();
        let workspace = new_workspace_id();
        assert!(shaped("f_", &family), "{family}");
        assert!(shaped("ws_", &workspace), "{workspace}");
        assert_ne!(family, new_family_id());
        assert_ne!(workspace, new_workspace_id());
        assert!(!shaped("ws_", &family));
        assert!(!shaped("f_", &workspace));
    }

    #[test]
    fn mint_new_identity_on_a_typed_workdir() {
        let scratch = Scratch::new("mint");
        let workdir = scratch.typed();
        let (identity, created) = ensure_identity(workdir, None).expect("ensure");
        assert!(created);
        assert!(shaped("f_", &identity.family_id));
        assert!(shaped("ws_", &identity.workspace_id));
        // Python: datetime.now(timezone.utc).isoformat(timespec="seconds").
        assert_eq!(identity.created_at.len(), 25, "{}", identity.created_at);
        assert!(identity.created_at.ends_with("+00:00"));
        assert_eq!(identity.created_at.as_bytes()[10], b'T');
        assert_eq!(identity.origin_family, None);
        assert_eq!(identity.origin_version, None);

        // On disk the file is the engine-owned record, schema-tagged, keys
        // sorted, 2-space indented, LF, newline-terminated.
        let text = std::fs::read_to_string(workdir.join(IDENTITY_FILE)).expect("read file");
        assert!(text.ends_with("}\n"), "{text:?}");
        assert!(!text.contains('\r'), "LF newlines only: {text:?}");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "{");
        assert!(
            lines[1].starts_with("  \"created_at\": \""),
            "2-space indent, sorted keys: {text:?}"
        );
        let keys: Vec<&str> = lines
            .iter()
            .filter_map(|line| line.split(": ").next())
            .filter(|line| *line != "{")
            .collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_eq!(keys, sorted, "sorted keys: {text:?}");
        assert!(text.contains("\"schema\": \"docx2typed-workspace-1\""));
    }

    #[test]
    fn write_then_read_roundtrips_every_field() {
        let scratch = Scratch::new("roundtrip");
        let identity = Identity {
            family_id: new_family_id(),
            workspace_id: new_workspace_id(),
            created_at: docx2typed_protocol::utc_now_iso(),
            origin_family: Some("f_0123456789abcdef0123456789abcdef".to_string()),
            origin_version: Some("V3".to_string()),
            adopted: Vec::new(),
        };
        let target = write_identity(&scratch.0, &identity).expect("write");
        assert_eq!(target, scratch.0.join(IDENTITY_FILE));
        assert_eq!(read_identity(&scratch.0), Some(identity));
        // No temp file survives publication.
        let leftovers: Vec<String> = std::fs::read_dir(&scratch.0)
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn missing_file_reads_as_none() {
        let scratch = Scratch::new("missing");
        assert_eq!(read_identity(&scratch.0), None);
        assert_eq!(read_identity(&scratch.0.join("nope")), None);
    }

    #[test]
    fn malformed_files_are_rejected() {
        let scratch = Scratch::new("malformed");
        let path = scratch.0.join(IDENTITY_FILE);
        let cases = [
            ("not json at all", ""),
            ("[]", ""),
            ("{}", ""),
            (
                "no workspace_id",
                "{\n  \"schema\": \"docx2typed-workspace-1\",\n  \"family_id\": \"f_0123456789abcdef0123456789abcdef\"\n}",
            ),
            (
                "no family_id",
                "{\n  \"schema\": \"docx2typed-workspace-1\",\n  \"workspace_id\": \"ws_0123456789abcdef0123456789abcdef\"\n}",
            ),
            (
                "empty family_id",
                "{\n  \"schema\": \"docx2typed-workspace-1\",\n  \"family_id\": \"\",\n  \"workspace_id\": \"ws_0123456789abcdef0123456789abcdef\"\n}",
            ),
            (
                "empty workspace_id",
                "{\n  \"schema\": \"docx2typed-workspace-1\",\n  \"family_id\": \"f_0123456789abcdef0123456789abcdef\",\n  \"workspace_id\": \"\"\n}",
            ),
        ];
        for (label, body) in cases {
            std::fs::write(&path, body).expect("write case");
            assert_eq!(read_identity(&scratch.0), None, "case: {label}");
        }
    }

    #[test]
    fn wrong_schema_is_rejected() {
        let scratch = Scratch::new("schema");
        std::fs::write(
            scratch.0.join(IDENTITY_FILE),
            "{\n  \"schema\": \"docx2typed-workspace-2\",\n  \"family_id\": \"f_0123456789abcdef0123456789abcdef\",\n  \"workspace_id\": \"ws_0123456789abcdef0123456789abcdef\"\n}\n",
        )
        .expect("write");
        assert_eq!(read_identity(&scratch.0), None);
    }

    #[test]
    fn ensure_is_idempotent_and_creates_only_once() {
        let scratch = Scratch::new("idempotent");
        let workdir = scratch.typed();
        let (first, created) = ensure_identity(workdir, None).expect("first");
        assert!(created);
        let (second, created) = ensure_identity(
            workdir,
            Some(Origin {
                family_id: Some("f_0123456789abcdef0123456789abcdef".to_string()),
                version: Some("V9".to_string()),
            }),
        )
        .expect("second");
        assert!(!created);
        assert_eq!(first, second);
        assert_eq!(second.origin_family, None, "origin is mint-time only");
    }

    #[test]
    fn ensure_records_origin_when_minting_a_fork() {
        let scratch = Scratch::new("origin");
        let workdir = scratch.typed();
        let (identity, created) = ensure_identity(
            workdir,
            Some(Origin {
                family_id: Some("f_0123456789abcdef0123456789abcdef".to_string()),
                version: Some("V3".to_string()),
            }),
        )
        .expect("ensure");
        assert!(created);
        assert_eq!(
            identity.origin_family.as_deref(),
            Some("f_0123456789abcdef0123456789abcdef")
        );
        assert_eq!(identity.origin_version.as_deref(), Some("V3"));
        assert_eq!(read_identity(workdir).expect("read-back"), identity);
    }

    #[test]
    fn ensure_refuses_a_non_typed_workdir() {
        let scratch = Scratch::new("untyped");
        let error = ensure_identity(&scratch.0, None).expect_err("must refuse");
        assert!(
            matches!(&error, CoreError::Domain(message) if message.starts_with("not a typed workdir: ")),
            "{error:?}"
        );
        assert_eq!(read_identity(&scratch.0), None);
    }

    #[test]
    fn reads_the_python_reference_payload() {
        // Exact bytes `json.dumps(..., ensure_ascii=False, indent=2,
        // sort_keys=True) + "\n"` produces in the Python Reference.
        let scratch = Scratch::new("python");
        let workdir = scratch.typed();
        let reference = "{\n  \"created_at\": \"2026-09-14T08:15:42+00:00\",\n  \"family_id\": \"f_0f1e2d3c4b5a69788796a5b4c3d2e1f0\",\n  \"origin_family\": null,\n  \"origin_version\": null,\n  \"schema\": \"docx2typed-workspace-1\",\n  \"workspace_id\": \"ws_102f3e4d5c6b7a8998a7b6c5d4e3f201\"\n}\n";
        std::fs::write(workdir.join(IDENTITY_FILE), reference).expect("write");
        let identity = read_identity(workdir).expect("identity");
        assert_eq!(identity.created_at, "2026-09-14T08:15:42+00:00");
        assert_eq!(identity.family_id, "f_0f1e2d3c4b5a69788796a5b4c3d2e1f0");
        assert_eq!(identity.workspace_id, "ws_102f3e4d5c6b7a8998a7b6c5d4e3f201");
        // Re-publishing it is byte-identical: write_identity is canonical.
        write_identity(workdir, &identity).expect("rewrite");
        let text = std::fs::read_to_string(workdir.join(IDENTITY_FILE)).expect("read");
        assert_eq!(text, reference);
    }

    /// A DOCX stand-in: adoption only ever hashes bytes and stats the file.
    fn source_file(scratch: &Scratch, name: &str, body: &str) -> PathBuf {
        let path = scratch.0.join(name);
        std::fs::write(&path, body).expect("source file");
        path
    }

    /// A minted token carries the three facts `workspace_adopt` re-checks, in
    /// the base64url form Python's `urlsafe_b64decode` accepts.
    #[test]
    fn adoption_token_roundtrips_the_evidence() {
        let scratch = Scratch::new("token");
        let docx = source_file(&scratch, "a.docx", "bytes");
        let token = adoption_token(&docx).expect("mint");
        let (volume, file_id) = file_identity(&docx);
        #[cfg(unix)]
        assert!(
            !volume.is_empty() && !file_id.is_empty(),
            "{volume} {file_id}"
        );
        let decoded = decode_adoption_token(&token).expect("decode");
        assert_eq!(
            decoded.sha256,
            docx2typed_protocol::file_sha256(&docx).unwrap()
        );
        assert_eq!(decoded.volume.as_deref(), Some(volume.as_str()));
        assert_eq!(decoded.file_id.as_deref(), Some(file_id.as_str()));
        assert_eq!(
            decoded.path.as_deref(),
            Some(docx.to_string_lossy().as_ref())
        );
        // Padded, URL-safe alphabet: the reference's decoder needs the padding.
        assert!(
            token.contains('=') || token.len().is_multiple_of(4),
            "{token}"
        );
        assert!(!token.contains('+') && !token.contains('/'), "{token}");
        // The reference emits its payload base64url-encoded too, so a token
        // built the Python way decodes here.
        let python_style = base64url_encode(
            format!(
                "{{\"sha256\": {:?}, \"volume\": {:?}, \"file_id\": {:?}, \"path\": {:?}}}",
                decoded.sha256,
                decoded.volume.clone().unwrap_or_default(),
                decoded.file_id.clone().unwrap_or_default(),
                decoded.path.clone().unwrap_or_default()
            )
            .as_bytes(),
        );
        assert_eq!(
            decode_adoption_token(&python_style).expect("python token"),
            decoded
        );
    }

    #[test]
    fn adoption_token_rejects_unreadable_payloads() {
        for token in ["", "!!!!", "bm90IGpzb24=", "eyJ2b2x1bWUiOiIifQ=="] {
            let error = decode_adoption_token(token).expect_err(token);
            assert!(
                matches!(&error, CoreError::Domain(message) if message.starts_with(ADOPTION_TOKEN_INVALID)),
                "{token}: {error:?}"
            );
        }
    }

    #[test]
    fn adopt_binds_the_source_to_the_family() {
        let scratch = Scratch::new("adopt");
        let workdir = scratch.typed();
        let (identity, _) = ensure_identity(workdir, None).expect("ensure");
        let docx = source_file(&scratch, "a.docx", "the document");
        let token = adoption_token(&docx).expect("mint");

        let result = workspace_adopt(workdir, &identity.family_id, &token).expect("adopt");
        assert_eq!(result["status"], "resolved");
        assert_eq!(result["reason"], "adopted");
        assert_eq!(result["family_id"], identity.family_id);
        assert_eq!(result["workspace_id"], identity.workspace_id);
        assert_eq!(result["workspace"], workdir.to_string_lossy().as_ref());

        // The evidence is durable, and a repeated confirmation does not
        // duplicate it (`workspace_adopt` is idempotent).
        let recorded = read_identity(workdir).expect("identity").adopted;
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].lineage_token, token);
        assert_eq!(recorded[0].path, docx.to_string_lossy());
        assert_eq!(
            recorded[0].sha256,
            docx2typed_protocol::file_sha256(&docx).unwrap()
        );
        assert_eq!(recorded[0].adopted_at.len(), 25);
        workspace_adopt(workdir, &identity.family_id, &token).expect("repeat");
        assert_eq!(read_identity(workdir).expect("identity").adopted.len(), 1);
    }

    #[test]
    fn adopt_refuses_a_stale_token_or_a_foreign_family() {
        let scratch = Scratch::new("stale");
        let workdir = scratch.typed();
        let (identity, _) = ensure_identity(workdir, None).expect("ensure");
        let docx = source_file(&scratch, "a.docx", "the document");
        let token = adoption_token(&docx).expect("mint");

        // A file rewritten while the question was on screen is refused rather
        // than adopted by accident, and nothing is recorded.
        std::fs::write(&docx, "another document entirely").expect("rewrite");
        let error = workspace_adopt(workdir, &identity.family_id, &token).expect_err("stale");
        assert!(
            matches!(&error, CoreError::Domain(message) if message.starts_with(ADOPTION_TOKEN_STALE)),
            "{error:?}"
        );
        assert!(read_identity(workdir).expect("identity").adopted.is_empty());

        // An answer that names a family this workspace is not.
        let token = adoption_token(&docx).expect("re-mint");
        let error = workspace_adopt(workdir, "f_0123456789abcdef0123456789abcdef", &token)
            .expect_err("foreign family");
        assert!(
            matches!(&error, CoreError::Domain(message) if message.starts_with(ADOPTION_REFUSED)),
            "{error:?}"
        );
        // A workdir with no identity is not a family at all.
        let untyped = Scratch::new("adopt-untyped");
        let error = workspace_adopt(&untyped.0, &identity.family_id, &token).expect_err("untyped");
        assert!(
            matches!(&error, CoreError::Domain(message) if message.starts_with(ADOPTION_REFUSED)),
            "{error:?}"
        );
        // An invalid token never reaches the filesystem.
        let error = workspace_adopt(workdir, &identity.family_id, "nonsense").expect_err("invalid");
        assert!(
            matches!(&error, CoreError::Domain(message) if message.starts_with(ADOPTION_TOKEN_INVALID)),
            "{error:?}"
        );
    }

    /// The adoption record survives a rewrite by the Python Reference: its
    /// readers only require `schema`/`family_id`/`workspace_id`, so an engine
    /// that never heard of `adopted` still round-trips the file.
    #[test]
    fn adopted_records_survive_a_reference_rewrite() {
        let scratch = Scratch::new("adopted-parity");
        let workdir = scratch.typed();
        let (identity, _) = ensure_identity(workdir, None).expect("ensure");
        let docx = source_file(&scratch, "a.docx", "the document");
        let token = adoption_token(&docx).expect("mint");
        workspace_adopt(workdir, &identity.family_id, &token).expect("adopt");

        let text = std::fs::read_to_string(workdir.join(IDENTITY_FILE)).expect("read");
        assert!(text.contains("\"adopted\": ["), "{text}");
        assert!(text.contains("\"lineage_token\""), "{text}");
        // Python's `read_identity` returns the decoded dict and requires only
        // the three fields, so this file is still an identity to it.
        let value: serde_json::Value = serde_json::from_str(&text).expect("json");
        assert_eq!(value["schema"], IDENTITY_SCHEMA);
        assert_eq!(value["family_id"], identity.family_id);
        assert_eq!(value["workspace_id"], identity.workspace_id);
        assert_eq!(read_identity(workdir).expect("read-back").adopted.len(), 1);
    }
}
