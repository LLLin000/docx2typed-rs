//! Protocol-major-1 leaf crate: versioned DTOs, canonical JSON, hashing,
//! engine descriptor, and contract/feature negotiation.
//!
//! Mirrors `scripts/protocol.py` from the Python Reference (issue #55 slice:
//! negotiation + result/diagnostic/evidence shapes only). No DOCX AST, no
//! store, no orchestration, no transport lives here.

pub mod canonical;
pub mod descriptor;
pub mod dto;
pub mod evidence;
pub mod negotiate;

pub use canonical::{
    bytes_sha256, canonical_json, canonical_operation_input, file_sha256, new_operation_id,
    resolve_path, semantic_sha256, typed_path_value, utc_now_iso,
};
pub use descriptor::{
    engine_descriptor, EngineDescriptor, CONTRACT_RANGES, FEATURES, PACKAGE_VERSION,
    PROTOCOL_COMMANDS, PROTOCOL_TOOLS, REQUIRED_FEATURES,
};
pub use dto::{diagnostic_spec, diagnostic_spec_opt, Diagnostic, DiagnosticSpec, ResultEnvelope};
pub use evidence::{base_evidence_payload, run_evidence, RunEvidence};
pub use negotiate::{negotiate, NegotiationError};

/// Frozen identity of the Protocol-major-1 schema bundle (semantic SHA-256 of
/// `scripts/protocol_schema_bundle.json`, pinned by reference/bundle-1
/// plan.identities.contract.schema_bundle_sha256).
pub const SCHEMA_BUNDLE_SCHEMA: &str = "docx2typed-tool-schema-bundle-1";
pub const SCHEMA_BUNDLE_SHA256: &str =
    "8d1e8dbf2778e31cc6aa838e1ccae642c0481ff03719a996f439cf939e378d84";

/// Frozen identity of the capability manifest (semantic SHA-256 of
/// `capabilities/manifest.json`, pinned by reference/bundle-1
/// plan.identities.capability.sha256).
pub const CAPABILITY_MANIFEST_SCHEMA: &str = "docx2typed-capability-manifest-1";
pub const CAPABILITY_MANIFEST_SHA256: &str =
    "d911b616e2238ccc2dcd8b4dd7e170f3feff01ac2fb412ce3752bc7ec77baf37";

pub const RESULT_SCHEMA: &str = "docx2typed-result-1";
pub const DIAGNOSTIC_SCHEMA: &str = "docx2typed-diagnostic-1";
pub const EVIDENCE_SCHEMA: &str = "docx2typed-run-evidence-1";
pub const ENGINE_DESCRIPTOR_SCHEMA: &str = "docx2typed-engine-descriptor-1";

/// The engine name declared by this implementation (Python Reference
/// declares `docx2typed-python`).
pub const ENGINE_NAME: &str = "docx2typed-rust";
