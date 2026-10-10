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
}

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
        Ok(_) => return Err(CoreError::message("identity does not serialize to an object")),
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
    };
    write_identity(workdir, &identity)?;
    Ok((read_identity(workdir).unwrap_or(identity), true))
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
        value.strip_prefix(prefix).is_some_and(|hex| {
            hex.len() == 32 && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
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
}
