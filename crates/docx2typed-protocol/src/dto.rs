//! Versioned wire DTOs: `docx2typed-diagnostic-1` and `docx2typed-result-1`,
//! mirroring `scripts/protocol.py` (`diagnostic`/`domain_diagnostic`/
//! `result_envelope`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::descriptor::{engine_descriptor, EngineDescriptor};
use super::evidence::RunEvidence;
use crate::{DIAGNOSTIC_SCHEMA, RESULT_SCHEMA};

/// Frozen registry entry for one diagnostic code (values copied from
/// `scripts/protocol_schema_bundle.json` `diagnostics` for the codes this
/// slice emits).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiagnosticSpec {
    pub severity: &'static str,
    pub category: &'static str,
    pub retriable: bool,
}

pub const DEFAULT_SPEC: DiagnosticSpec = DiagnosticSpec {
    severity: "error",
    category: "domain",
    retriable: false,
};

/// Registered spec for a code; unknown codes fall back to the stable domain
/// default (mirroring Python `domain_diagnostic`), so a failure envelope
/// never depends on registry membership.
///
/// The declared spec for `code`, or `None` when the engine never declared it.
///
/// `diagnostic_spec` is this plus a default. Both come from ONE match, so a
/// caller that needs to know whether a code is real cannot drift from the
/// caller that needs its severity.
pub fn diagnostic_spec_opt(code: &str) -> Option<DiagnosticSpec> {
    match code {
        "input-not-found" => Some(DiagnosticSpec {
            severity: "error",
            category: "input",
            retriable: false,
        }),
        "workdir-not-found" => Some(DiagnosticSpec {
            severity: "error",
            category: "workdir",
            retriable: false,
        }),
        "workdir-invalid" => Some(DiagnosticSpec {
            severity: "error",
            category: "workdir",
            retriable: false,
        }),
        "workdir-unreadable" => Some(DiagnosticSpec {
            severity: "error",
            category: "workdir",
            retriable: true,
        }),
        "workdir-already-open" => Some(DiagnosticSpec {
            severity: "error",
            category: "workdir",
            retriable: false,
        }),
        "contract-incompatible" => Some(DiagnosticSpec {
            severity: "error",
            category: "contract",
            retriable: false,
        }),
        "required-feature-unsupported" => Some(DiagnosticSpec {
            severity: "error",
            category: "contract",
            retriable: false,
        }),
        "evidence-publish-failed" => Some(DiagnosticSpec {
            severity: "error",
            category: "evidence",
            retriable: true,
        }),
        "resource-limit-exceeded" => Some(DiagnosticSpec {
            severity: "error",
            category: "input",
            retriable: false,
        }),
        "asset-closure" => Some(DiagnosticSpec {
            severity: "error",
            category: "workdir",
            retriable: false,
        }),
        "schema-incompatible" => Some(DiagnosticSpec {
            severity: "error",
            category: "workdir",
            retriable: false,
        }),
        "source-drift" => Some(DiagnosticSpec {
            severity: "error",
            category: "workdir",
            retriable: false,
        }),
        "symlink-detected" => Some(DiagnosticSpec {
            severity: "error",
            category: "workdir",
            retriable: false,
        }),
        "target-already-exists" => Some(DiagnosticSpec {
            severity: "error",
            category: "output",
            retriable: false,
        }),
        "operation-id-required" => Some(DiagnosticSpec {
            severity: "error",
            category: "invocation",
            retriable: false,
        }),
        "migrate-verification-failed" => Some(DiagnosticSpec {
            severity: "error",
            category: "workdir",
            retriable: false,
        }),
        "edit-dirty" => Some(DiagnosticSpec {
            severity: "error",
            category: "edit",
            retriable: true,
        }),
        "edit-stale" => Some(DiagnosticSpec {
            severity: "error",
            category: "edit",
            retriable: true,
        }),
        "edit-conflict" => Some(DiagnosticSpec {
            severity: "error",
            category: "edit",
            retriable: true,
        }),
        "edit-state-missing" => Some(DiagnosticSpec {
            severity: "error",
            category: "edit",
            retriable: false,
        }),
        "edit-state-incompatible" => Some(DiagnosticSpec {
            severity: "error",
            category: "edit",
            retriable: false,
        }),
        "edit-binding-mismatch" => Some(DiagnosticSpec {
            severity: "error",
            category: "edit",
            retriable: false,
        }),
        "edit-header-tampered" => Some(DiagnosticSpec {
            severity: "error",
            category: "edit",
            retriable: false,
        }),
        "edit-grammar-invalid" => Some(DiagnosticSpec {
            severity: "error",
            category: "edit",
            retriable: false,
        }),
        "edit-header-missing" => Some(DiagnosticSpec {
            severity: "error",
            category: "edit",
            retriable: false,
        }),
        "invalid-arguments" => Some(DiagnosticSpec {
            severity: "error",
            category: "invocation",
            retriable: false,
        }),
        // Syncing a source into a workdir that carries an uncommitted draft
        // would discard the draft. Not retriable as-is: the caller must commit
        // (or discard) first, which is a different request.
        "workdir-has-pending-edit" => Some(DiagnosticSpec {
            severity: "error",
            category: "workdir",
            retriable: false,
        }),
        // Syncing over a workdir that is not on its baseline would make the
        // incoming file the current state, leaving the applied edits only in
        // history. Refused rather than retried: the caller must decide.
        "workdir-not-at-baseline" => Some(DiagnosticSpec {
            severity: "error",
            category: "workdir",
            retriable: false,
        }),
        // The rest of the codes the engine emits. These were declared only in
        // the app's own `is_registered_code` list, so `diagnostic_spec` answered
        // DEFAULT_SPEC for half of the engine's real refusals — wrong severity,
        // wrong category, and `retriable: false` even for the transient
        // concurrency and space conditions an agent should retry. One registry
        // now, so membership and spec cannot disagree.
        "comment-not-found" => Some(DiagnosticSpec {
            severity: "error",
            category: "workdir",
            retriable: false,
        }),
        "decided-output-already-exists" => Some(DiagnosticSpec {
            severity: "error",
            category: "output",
            retriable: false,
        }),
        "decided-workdir-already-exists" => Some(DiagnosticSpec {
            severity: "error",
            category: "workdir",
            retriable: false,
        }),
        "generation-conflict" => Some(DiagnosticSpec {
            severity: "error",
            category: "workdir",
            retriable: true,
        }),
        "invalid-action" => Some(DiagnosticSpec {
            severity: "error",
            category: "invocation",
            retriable: false,
        }),
        "invalid-edit" => Some(DiagnosticSpec {
            severity: "error",
            category: "edit",
            retriable: false,
        }),
        "invalid-table-reference" => Some(DiagnosticSpec {
            severity: "error",
            category: "edit",
            retriable: false,
        }),
        "malformed-revision-key" => Some(DiagnosticSpec {
            severity: "error",
            category: "edit",
            retriable: false,
        }),
        "merge-would-discard-content" => Some(DiagnosticSpec {
            severity: "error",
            category: "edit",
            retriable: false,
        }),
        "needs-recovery" => Some(DiagnosticSpec {
            severity: "error",
            category: "workdir",
            retriable: false,
        }),
        "opaque-paragraph-mutated" => Some(DiagnosticSpec {
            severity: "error",
            category: "edit",
            retriable: false,
        }),
        "operation-id-reused" => Some(DiagnosticSpec {
            severity: "error",
            category: "invocation",
            retriable: false,
        }),
        "operation-journal-conflict" => Some(DiagnosticSpec {
            severity: "error",
            category: "workdir",
            retriable: true,
        }),
        "prose-edit-ambiguous" => Some(DiagnosticSpec {
            severity: "error",
            category: "edit",
            retriable: false,
        }),
        "prose-edit-unsupported" => Some(DiagnosticSpec {
            severity: "error",
            category: "edit",
            retriable: false,
        }),
        "prose-xml-invalid" => Some(DiagnosticSpec {
            severity: "error",
            category: "edit",
            retriable: false,
        }),
        "replace-cross-leaf-matches" => Some(DiagnosticSpec {
            severity: "error",
            category: "edit",
            retriable: false,
        }),
        "replace-unrecorded-match" => Some(DiagnosticSpec {
            severity: "error",
            category: "edit",
            retriable: false,
        }),
        "reserve-depleted" => Some(DiagnosticSpec {
            severity: "error",
            category: "workdir",
            retriable: true,
        }),
        "revision-fingerprint-mismatch" => Some(DiagnosticSpec {
            severity: "error",
            category: "edit",
            retriable: false,
        }),
        "revision-not-found" => Some(DiagnosticSpec {
            severity: "error",
            category: "edit",
            retriable: false,
        }),
        "revision-outside-editable-surface" => Some(DiagnosticSpec {
            severity: "error",
            category: "edit",
            retriable: false,
        }),
        "revision-text-fingerprint-mismatch" => Some(DiagnosticSpec {
            severity: "error",
            category: "edit",
            retriable: false,
        }),
        "store-invalid" => Some(DiagnosticSpec {
            severity: "error",
            category: "workdir",
            retriable: false,
        }),
        "unsupported-by-design" => Some(DiagnosticSpec {
            severity: "error",
            category: "domain",
            retriable: false,
        }),
        "writer-busy" => Some(DiagnosticSpec {
            severity: "error",
            category: "workdir",
            retriable: true,
        }),
        "writer-timeout" => Some(DiagnosticSpec {
            severity: "error",
            category: "workdir",
            retriable: true,
        }),
        _ => None,
    }
}

/// The spec for `code`, with the default for anything undeclared.
pub fn diagnostic_spec(code: &str) -> DiagnosticSpec {
    diagnostic_spec_opt(code).unwrap_or(DEFAULT_SPEC)
}

/// `docx2typed-diagnostic-1`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Diagnostic {
    pub schema: &'static str,
    pub code: String,
    pub severity: &'static str,
    pub category: &'static str,
    pub retriable: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_actions: Option<Vec<String>>,
}

impl Diagnostic {
    pub fn new(code: &str, message: String) -> Self {
        Self::with_details(code, message, None, None)
    }

    pub fn with_details(
        code: &str,
        message: String,
        details: Option<Value>,
        next_actions: Option<Vec<String>>,
    ) -> Self {
        let spec = diagnostic_spec(code);
        Diagnostic {
            schema: DIAGNOSTIC_SCHEMA,
            code: code.to_string(),
            severity: spec.severity,
            category: spec.category,
            retriable: spec.retriable,
            message,
            details,
            next_actions,
        }
    }
}

/// `docx2typed-result-1`.
#[derive(Clone, Debug, Serialize)]
pub struct ResultEnvelope {
    pub schema: &'static str,
    pub operation: String,
    pub outcome: String,
    pub data: Value,
    pub diagnostics: Vec<Diagnostic>,
    pub evidence: Vec<RunEvidence>,
    pub engine: EngineDescriptor,
}

impl ResultEnvelope {
    /// Mirror of `result_envelope(operation, outcome, ...)` in
    /// `scripts/protocol.py`; `engine` is the live descriptor with the
    /// build commit observed at call time.
    pub fn new(
        operation: &str,
        outcome: &str,
        data: Value,
        diagnostics: Vec<Diagnostic>,
        evidence: Vec<RunEvidence>,
        build_commit: &str,
    ) -> Self {
        ResultEnvelope {
            schema: RESULT_SCHEMA,
            operation: operation.to_string(),
            outcome: outcome.to_string(),
            data,
            diagnostics,
            evidence,
            engine: engine_descriptor(build_commit),
        }
    }
}
