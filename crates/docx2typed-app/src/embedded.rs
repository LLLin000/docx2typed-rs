//! Embedded immutable assets (issue #61): the release binary carries the
//! pinned Protocol-1 schema bundle, the capability manifest, and the Unicode
//! vertical catalog inside the executable, so no repo checkout is needed at
//! runtime and the exact bytes are hash-bound at build time.
//!
//! The pinned identities are the frozen Python Reference bundle records
//! (`reference/bundle-1/freeze.json` identities: `contract.schema_bundle`
//! and `capability`; the catalog pins its own `catalog_hash` + `unicode_version`
//! inside the JSON). `verify()` recomputes every hash from the embedded bytes
//! and fails loudly on any drift — in the file, the embedding, or the record.

use std::borrow::Cow;

use serde_json::Value;

use docx2typed_protocol::{
    bytes_sha256, semantic_sha256, CAPABILITY_MANIFEST_SHA256, SCHEMA_BUNDLE_SHA256,
};

/// Embedded `scripts/protocol_schema_bundle.json` (Protocol-major-1 tool
/// schema registry; pinned by `docx2typed_protocol::SCHEMA_BUNDLE_SHA256`).
pub const SCHEMA_BUNDLE_JSON: &str = include_str!("../assets/protocol_schema_bundle.json");

/// Embedded `capabilities/manifest.json` (capability manifest; pinned by
/// `docx2typed_protocol::CAPABILITY_MANIFEST_SHA256`).
pub const CAPABILITY_MANIFEST_JSON: &str = include_str!("../assets/capability_manifest.json");

/// Embedded `scripts/unicode_vertical_catalog.json` (issue #59 Unicode audit
/// catalog; pins its own `catalog_hash` / `unicode_version`).
pub const UNICODE_VERTICAL_CATALOG_JSON: &str =
    include_str!("../assets/unicode_vertical_catalog.json");

/// Embedded frozen MCP input schemas. This is the sole runtime source for
/// `tools/list`; the checked-in file is the frozen MCP contract artifact.
pub const MCP_SCHEMAS_JSON: &str = include_str!("../assets/mcp_schemas.json");
 
/// Embedded flat tool-name -> description map. This is the sole runtime
/// source for the MCP `description` field in `tools/list`.
pub const TOOL_DESCRIPTIONS_JSON: &str = include_str!("../assets/tool_descriptions.json");
/// The catalog's self-pinned hash (the `catalog_hash` field inside the JSON).
pub const UNICODE_CATALOG_HASH: &str =
    "460a8f5581375b6f16e2cd3025d591785e2b1cde941a9d33eb43ba09cdacde58";
/// The catalog's pinned Unicode version (the `unicode_version` field).
pub const UNICODE_CATALOG_VERSION: &str = "16.0.0";

/// One embedded asset with its recomputed raw hash and (where the bundle
/// pins a semantic identity) its recomputed semantic hash.
pub struct EmbeddedAsset {
    pub name: &'static str,
    pub bytes: usize,
    pub raw_sha256: String,
    pub semantic_sha256: Option<String>,
    pub pinned_sha256: &'static str,
    pub match_pinned: bool,
}

/// Normalize source checkout line endings to the CRLF bytes frozen by the
/// Reference bundle. Git checkouts differ by host; release identities must not.
pub fn canonical_asset(raw: &str) -> Cow<'_, str> {
    let lf = raw.replace("\r\n", "\n");
    Cow::Owned(lf.replace('\n', "\r\n"))
}

/// Every embedded asset, with hashes recomputed from canonical frozen bytes.
pub fn assets() -> Vec<EmbeddedAsset> {
    let schema = canonical_asset(SCHEMA_BUNDLE_JSON);
    let capability = canonical_asset(CAPABILITY_MANIFEST_JSON);
    let catalog = canonical_asset(UNICODE_VERTICAL_CATALOG_JSON);
    let semantic = |json: &str| -> Option<String> {
        serde_json::from_str::<Value>(json)
            .ok()
            .map(|value| semantic_sha256(&value))
    };
    let catalog_ok = catalog_self_hash_matches();
    vec![
        EmbeddedAsset {
            name: "scripts/protocol_schema_bundle.json",
            bytes: schema.len(),
            raw_sha256: bytes_sha256(schema.as_bytes()),
            semantic_sha256: semantic(&schema),
            pinned_sha256: SCHEMA_BUNDLE_SHA256,
            match_pinned: semantic(&schema).as_deref() == Some(SCHEMA_BUNDLE_SHA256),
        },
        EmbeddedAsset {
            name: "capabilities/manifest.json",
            bytes: capability.len(),
            raw_sha256: bytes_sha256(capability.as_bytes()),
            semantic_sha256: semantic(&capability),
            pinned_sha256: CAPABILITY_MANIFEST_SHA256,
            match_pinned: semantic(&capability).as_deref() == Some(CAPABILITY_MANIFEST_SHA256),
        },
        EmbeddedAsset {
            name: "scripts/unicode_vertical_catalog.json",
            bytes: catalog.len(),
            raw_sha256: bytes_sha256(catalog.as_bytes()),
            semantic_sha256: None,
            pinned_sha256: UNICODE_CATALOG_HASH,
            match_pinned: catalog_ok,
        },
    ]
}

/// The catalog's self-pinned `catalog_hash` and `unicode_version` fields
/// must equal the frozen constants (the bundle records them inside the JSON;
/// the Reference freeze pins the file identity).
pub fn catalog_self_hash_matches() -> bool {
    let catalog = canonical_asset(UNICODE_VERTICAL_CATALOG_JSON);
    match serde_json::from_str::<Value>(&catalog) {
        Ok(value) => {
            value.get("catalog_hash").and_then(Value::as_str) == Some(UNICODE_CATALOG_HASH)
                && value.get("unicode_version").and_then(Value::as_str)
                    == Some(UNICODE_CATALOG_VERSION)
        }
        Err(_) => false,
    }
}

/// Verify every embedded asset against the frozen bundle records.
/// `Ok` carries the per-asset table; `Err` names the first failing asset.
pub fn verify() -> Result<Vec<EmbeddedAsset>, String> {
    let table = assets();
    for asset in &table {
        if !asset.match_pinned {
            return Err(format!(
                "embedded asset {} does not match its pinned hash (pinned {}, got semantic {:?})",
                asset.name, asset.pinned_sha256, asset.semantic_sha256
            ));
        }
    }
    Ok(table)
}

/// JSON table for `--version --json` / `engine_info` enrichment.
pub fn table_value() -> Value {
    let rows: Vec<Value> = assets()
        .into_iter()
        .map(|asset| {
            serde_json::json!({
                "name": asset.name,
                "bytes": asset.bytes,
                "raw_sha256": asset.raw_sha256,
                "semantic_sha256": asset.semantic_sha256,
                "pinned_sha256": asset.pinned_sha256,
                "match_pinned": asset.match_pinned,
            })
        })
        .collect();
    Value::Array(rows)
}
