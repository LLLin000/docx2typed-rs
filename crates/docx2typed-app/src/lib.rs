//! Synchronous, stateless Engine use-case seam (issue #36 resolution,
//! issue #55 slice): `Engine::execute(Operation, OperationContext) ->
//! OperationOutcome`. The Engine centrally performs workdir validation,
//! Core planning, Store commit/publish, the required independent Verifier
//! check, and Result/Diagnostic/Evidence construction. Adapters only
//! translate transport DTOs and invoke the Engine — they cannot reassemble
//! or bypass this order.
//!
//! The slice implements `extract`, no-op `build`, and `verify`; edits,
//! review, decisions, and recovery land in #56+.

pub mod embedded;

use std::fs;
use std::path::{Path, PathBuf};

use docx2typed_core::{plan_build, plan_extract, validate_workdir, Asset, BuildPlan, ChangeSet};
use docx2typed_protocol::{
    base_evidence_payload, canonical_operation_input, file_sha256, resolve_path, run_evidence,
    typed_path_value, Diagnostic, ResultEnvelope, RunEvidence,
};
use docx2typed_store::{
    PinnedGeneration, RunOutcome, StoreError, StoreMutateRequest, Transaction, WorkdirStore,
};
use docx2typed_verify::{IndependentVerifier, VerificationEvidence, VerificationRequest};

/// Closed operation set for the slice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Extract,
    Build,
    Verify,
    Inspect,
    Migrate,
    /// Issue #57: the one real store mutation (commit the edit.md draft
    /// ingress into a new immutable generation).
    Edit,
    /// Issue #57: read-only store inspection (transactions/phases/actions).
    StoreState,
    /// Issue #58: read-only recursive-prose enumeration of a DOCX package.
    Enumerate,
    /// Issue #59: read-only tracked-revision inventory and accept/reject
    /// views (CLI `revisions list|view`).
    Revisions,
    /// Issue #59: revision settlement decisions and table structure ops
    /// (CLI `decide accept|reject|reinsert|table-*`).
    Decide,
    /// Issue #59: comment inventory and byte-surgery deletion (CLI
    /// `comment list|delete`).
    Comment,
    /// Issue #59: read-only governed Unicode normalization audit (CLI
    /// `audit`).
    Audit,
}

impl Operation {
    pub fn name(&self) -> &'static str {
        match self {
            Operation::Extract => "extract",
            Operation::Build => "build",
            Operation::Verify => "verify",
            Operation::Inspect => "inspect",
            Operation::Migrate => "migrate",
            Operation::Edit => "edit",
            Operation::StoreState => "store-state",
            Operation::Enumerate => "enumerate",
            Operation::Revisions => "revisions",
            Operation::Decide => "decide",
            Operation::Comment => "comment",
            Operation::Audit => "audit",
        }
    }

    /// The frozen finite-command set the CLI/MCP expose.
    pub const IMPLEMENTED_COMMANDS: [&str; 12] = [
        "extract",
        "build",
        "verify",
        "inspect",
        "migrate",
        "edit",
        "store-state",
        "enumerate",
        "revisions",
        "decide",
        "comment",
        "audit",
    ];
}

/// Typed, adapter-validated operation arguments (adapters parse and convert
/// wire DTOs; serde optional/default behavior never defines domain
/// invariants).
#[derive(Clone, Debug)]
pub enum OperationArgs {
    Extract(ExtractArgs),
    Build(BuildArgs),
    Verify(VerifyArgs),
    Inspect(InspectArgs),
    Migrate(MigrateArgs),
    Edit(EditArgs),
    StoreState(StoreStateArgs),
    Enumerate(EnumerateArgs),
    Revisions(RevisionsArgs),
    Decide(DecideArgs),
    Comment(CommentArgs),
    Audit(AuditArgs),
}

#[derive(Clone, Debug)]
pub struct ExtractArgs {
    pub input: PathBuf,
    pub outdir: PathBuf,
}

#[derive(Clone, Debug)]
pub struct BuildArgs {
    pub workdir: PathBuf,
    pub output: Option<PathBuf>,
    /// Bounded Writer lane wait (ms); 0 = fail immediately with writer-busy.
    pub lock_timeout_ms: u64,
}

#[derive(Clone, Debug)]
pub struct VerifyArgs {
    pub workdir: PathBuf,
    pub output: PathBuf,
}

#[derive(Clone, Debug)]
pub struct InspectArgs {
    pub source: PathBuf,
}

#[derive(Clone, Debug)]
pub struct MigrateArgs {
    pub source: PathBuf,
    pub target: PathBuf,
}

#[derive(Clone, Debug)]
pub struct EditArgs {
    pub workdir: PathBuf,
    /// Bounded Writer lane wait (ms); 0 = fail immediately with writer-busy.
    pub lock_timeout_ms: u64,
    /// Issue #58: when present, commit a real island text edit (instead of
    /// the #57 ingress sync commit).
    pub text: Option<TextEdit>,
}

#[derive(Clone, Debug)]
pub struct StoreStateArgs {
    pub source: PathBuf,
}

/// Issue #58: one island-local text edit (CLI `edit text`).
#[derive(Clone, Debug)]
pub struct TextEdit {
    /// `<paragraph-id>.<leaf-index>` leaf path (e.g. "T0.R1.C1.P0.0").
    pub leaf: String,
    pub old: String,
    pub new: String,
}

#[derive(Clone, Debug)]
pub struct EnumerateArgs {
    /// A DOCX package or a typed workdir (its `_template.docx` is used).
    pub source: PathBuf,
}

/// Issue #59: read-only revision inventory / view (`revisions list|view`).
#[derive(Clone, Debug)]
pub struct RevisionsArgs {
    /// A DOCX package or a typed workdir (its `_template.docx` is used).
    pub source: PathBuf,
    /// `None` = inventory; `Some("accept"|"reject")` = per-paragraph view.
    pub view: Option<String>,
}

/// Issue #59: one decision or table operation (`decide <action> ...`).
#[derive(Clone, Debug)]
pub struct DecideArgs {
    pub workdir: PathBuf,
    /// accept | reject | reinsert | table-insert-row | table-delete-row |
    /// table-insert-col | table-delete-col | table-merge-cells |
    /// table-split-cells.
    pub action: String,
    /// The revision key (`part|kind|w_id|fingerprint`) or table ref (`Tn`).
    pub revision_key: String,
    pub fingerprint: Option<String>,
    pub author: Option<String>,
    pub text: Option<String>,
    /// Space-separated numeric table-op arguments.
    pub args: Vec<usize>,
    pub discard_content: bool,
    /// New-artifact targets (table ops): decided docx + fresh workdir.
    pub output: Option<PathBuf>,
    pub workdir_out: Option<PathBuf>,
    /// Bounded Writer lane wait (ms); 0 = fail immediately with writer-busy.
    pub lock_timeout_ms: u64,
}

/// Issue #59: comment inventory / deletion (`comment list|delete`).
#[derive(Clone, Debug)]
pub struct CommentArgs {
    pub workdir: PathBuf,
    /// `None` = inventory; `Some(id)` = delete one comment.
    pub delete: Option<String>,
    pub lock_timeout_ms: u64,
}

/// Issue #59: read-only Unicode normalization audit (`audit <wd>`).
#[derive(Clone, Debug)]
pub struct AuditArgs {
    /// A DOCX package or a typed workdir (its `_template.docx` is used).
    pub source: PathBuf,
    /// Path of the pinned `unicode-vertical-catalog-1` JSON (the binary
    /// embeds the in-repo default; tests may override).
    pub catalog_path: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct OperationContext {
    pub operation_id: String,
    /// Check profile (S / L / X); recorded in evidence. Enforcement of
    /// wall/RSS budgets is a measurement gate in this slice (issue #38
    /// gates), not an in-engine fail-closed limit.
    pub profile: String,
    #[allow(dead_code)]
    pub deadline: Option<std::time::Instant>,
}

impl OperationContext {
    pub fn new(operation_id: String) -> Self {
        OperationContext {
            operation_id,
            profile: "S".to_string(),
            deadline: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Success,
    Failure,
    Partial,
}

impl Outcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Outcome::Success => "success",
            Outcome::Failure => "failure",
            Outcome::Partial => "partial",
        }
    }
}

/// One executed operation's Result/Diagnostic/Evidence construction.
#[derive(Clone, Debug)]
pub struct OperationOutcome {
    pub outcome: Outcome,
    pub data: serde_json::Value,
    pub diagnostics: Vec<Diagnostic>,
    pub evidence: Vec<RunEvidence>,
}

impl OperationOutcome {
    pub fn success(data: serde_json::Value, evidence: Vec<RunEvidence>) -> Self {
        OperationOutcome {
            outcome: Outcome::Success,
            data,
            diagnostics: Vec::new(),
            evidence,
        }
    }

    pub fn failure(diagnostics: Vec<Diagnostic>) -> Self {
        OperationOutcome {
            outcome: Outcome::Failure,
            data: serde_json::Value::Object(Default::default()),
            diagnostics,
            evidence: Vec::new(),
        }
    }

    /// Wrap into the `docx2typed-result-1` envelope with the live engine
    /// descriptor.
    pub fn into_envelope(self, operation: &str, build_commit: &str) -> ResultEnvelope {
        ResultEnvelope::new(
            operation,
            self.outcome.as_str(),
            self.data,
            self.diagnostics,
            self.evidence,
            build_commit,
        )
    }
}

/// Engine-level failure (a crash, not a domain failure): the adapter turns
/// this into a hard error. Domain failures are `OperationOutcome::failure`.
#[derive(Clone, Debug)]
pub struct EngineFailure {
    pub message: String,
}

// ---------------------------------------------------------------------------
// Ports (issue #36: app privately defines narrow StorePort/VerifierPort
// because each has two real adapters: the production concrete implementation
// and a deterministic in-memory test implementation).
// ---------------------------------------------------------------------------

pub trait StorePort {
    fn commit_workdir(&self, dir: &Path, change_set: &ChangeSet) -> Result<(), StoreError>;
    fn publish_build(&self, template: &Path, output: &Path) -> Result<PathBuf, StoreError>;
    /// Issue #58: publish an edited build's bytes (island surgery output).
    fn publish_bytes(&self, bytes: &[u8], output: &Path) -> Result<PathBuf, StoreError>;
    fn publish_run_evidence(&self, path: &Path, evidence: &RunEvidence) -> Result<(), StoreError>;
    fn derived_workdir_manifest(&self, dir: &Path) -> serde_json::Value;
    fn manifest_sha256(&self, dir: &Path) -> String;
    /// Byte-for-byte staged copy of a workdir (mtimes preserved).
    fn copy_workdir(&self, source: &Path, staging: &Path) -> Result<(), StoreError>;
    /// Write the versioned workdir manifest into the staging workdir.
    fn write_workdir_manifest(
        &self,
        staging: &Path,
        manifest: &serde_json::Value,
    ) -> Result<(), StoreError>;
    /// Atomically publish the staged workdir onto `target`.
    fn publish_workdir(&self, staging: &Path, target: &Path) -> Result<PathBuf, StoreError>;

    // -- issue #57: generation Store surface ---------------------------------
    /// True when `dir` is a store-backed workdir.
    fn has_store(&self, dir: &Path) -> bool;
    /// Open or birth (lazy upgrade of a pre-store workdir) the generation
    /// store. Recovery is never run here; `store_mutate` runs it at entry.
    fn store_ensure(
        &self,
        dir: &Path,
        operation_id: &str,
        input_sha256: &str,
    ) -> Result<(), StoreError>;
    /// Pin the current immutable generation (readers never mix generations).
    fn store_pin(&self, dir: &Path) -> Result<PinnedGeneration, StoreError>;
    /// Run one journaled mutation; returns the committed Result envelope.
    fn store_mutate(&self, request: StoreMutateRequest) -> Result<serde_json::Value, StoreError>;
    /// Read-only store diagnostics (backed/generation/pending transactions/
    /// reserve/qualification).
    fn store_state(&self, dir: &Path) -> Result<serde_json::Value, StoreError>;
}

pub trait VerifierPort {
    fn verify(&self, request: &VerificationRequest) -> VerificationEvidence;
}

impl StorePort for WorkdirStore {
    fn commit_workdir(&self, dir: &Path, change_set: &ChangeSet) -> Result<(), StoreError> {
        WorkdirStore::commit_workdir(self, dir, change_set)
    }

    fn publish_build(&self, template: &Path, output: &Path) -> Result<PathBuf, StoreError> {
        WorkdirStore::publish_build(self, template, output)
    }

    fn publish_bytes(&self, bytes: &[u8], output: &Path) -> Result<PathBuf, StoreError> {
        WorkdirStore::publish_bytes(self, bytes, output)
    }

    fn publish_run_evidence(&self, path: &Path, evidence: &RunEvidence) -> Result<(), StoreError> {
        WorkdirStore::publish_run_evidence(self, path, evidence)
    }

    fn derived_workdir_manifest(&self, dir: &Path) -> serde_json::Value {
        WorkdirStore::derived_workdir_manifest(self, dir)
    }

    fn manifest_sha256(&self, dir: &Path) -> String {
        WorkdirStore::manifest_sha256(self, dir)
    }

    fn copy_workdir(&self, source: &Path, staging: &Path) -> Result<(), StoreError> {
        WorkdirStore::copy_workdir(self, source, staging)
    }

    fn write_workdir_manifest(
        &self,
        staging: &Path,
        manifest: &serde_json::Value,
    ) -> Result<(), StoreError> {
        WorkdirStore::write_workdir_manifest(self, staging, manifest)
    }

    fn publish_workdir(&self, staging: &Path, target: &Path) -> Result<PathBuf, StoreError> {
        WorkdirStore::publish_workdir(self, staging, target)
    }

    fn has_store(&self, dir: &Path) -> bool {
        docx2typed_store::has_store(dir)
    }

    fn store_ensure(
        &self,
        dir: &Path,
        operation_id: &str,
        input_sha256: &str,
    ) -> Result<(), StoreError> {
        docx2typed_store::Store::ensure(dir, operation_id, input_sha256).map(|_| ())
    }

    fn store_pin(&self, dir: &Path) -> Result<PinnedGeneration, StoreError> {
        docx2typed_store::Store::open(dir)?.pin()
    }

    fn store_mutate(&self, request: StoreMutateRequest) -> Result<serde_json::Value, StoreError> {
        docx2typed_store::Store::open(&request.workdir)?.mutate(request)
    }

    fn store_state(&self, dir: &Path) -> Result<serde_json::Value, StoreError> {
        Ok(docx2typed_store::state(dir))
    }
}

impl VerifierPort for IndependentVerifier {
    fn verify(&self, request: &VerificationRequest) -> VerificationEvidence {
        IndependentVerifier::verify(self, request)
    }
}

/// Deterministic in-memory store for Engine tests.
#[derive(Default)]
pub struct MemoryStore {
    /// workdir path -> asset path -> bytes
    pub workdirs: std::cell::RefCell<
        std::collections::HashMap<PathBuf, std::collections::HashMap<String, Vec<u8>>>,
    >,
    pub builds: std::cell::RefCell<std::collections::HashMap<PathBuf, Vec<u8>>>,
    pub evidence: std::cell::RefCell<Vec<(PathBuf, RunEvidence)>>,
}

impl MemoryStore {
    fn workdir_dir(&self, dir: &Path) -> Option<std::collections::HashMap<String, Vec<u8>>> {
        self.workdirs.borrow().get(&resolve_path(dir)).cloned()
    }

    fn missing(message: &str) -> StoreError {
        StoreError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, message))
    }
}

impl StorePort for MemoryStore {
    fn commit_workdir(&self, dir: &Path, change_set: &ChangeSet) -> Result<(), StoreError> {
        let mut assets = std::collections::HashMap::new();
        for asset in &change_set.assets {
            match asset {
                Asset::Bytes(path, bytes) => {
                    assets.insert(path.clone(), bytes.clone());
                }
                Asset::CopySource { path, source } => {
                    let bytes = std::fs::read(source).map_err(StoreError::Io)?;
                    assets.insert(path.clone(), bytes);
                }
            }
        }
        self.workdirs.borrow_mut().insert(resolve_path(dir), assets);
        Ok(())
    }

    fn publish_build(&self, template: &Path, output: &Path) -> Result<PathBuf, StoreError> {
        let template_dir = template.parent().unwrap_or_else(|| Path::new("."));
        let dir = self
            .workdir_dir(template_dir)
            .ok_or_else(|| MemoryStore::missing("memory store: missing workdir"))?;
        let bytes = dir
            .get("_template.docx")
            .ok_or_else(|| MemoryStore::missing("memory store: missing _template.docx"))?
            .clone();
        self.builds.borrow_mut().insert(resolve_path(output), bytes);
        Ok(resolve_path(output))
    }

    fn publish_bytes(&self, bytes: &[u8], output: &Path) -> Result<PathBuf, StoreError> {
        self.builds
            .borrow_mut()
            .insert(resolve_path(output), bytes.to_vec());
        Ok(resolve_path(output))
    }

    fn publish_run_evidence(&self, path: &Path, evidence: &RunEvidence) -> Result<(), StoreError> {
        self.evidence
            .borrow_mut()
            .push((resolve_path(path), evidence.clone()));
        Ok(())
    }

    fn derived_workdir_manifest(&self, dir: &Path) -> serde_json::Value {
        let mut assets = Vec::new();
        if let Some(workdir) = self.workdir_dir(dir) {
            for (name, bytes) in &workdir {
                assets.push(serde_json::json!({
                    "path": name,
                    "bytes": bytes.len(),
                    "sha256": docx2typed_protocol::bytes_sha256(bytes),
                }));
            }
        }
        serde_json::json!({
            "schema": "docx2typed-derived-workdir-manifest-1",
            "assets": assets,
        })
    }

    fn manifest_sha256(&self, dir: &Path) -> String {
        docx2typed_protocol::semantic_sha256(&self.derived_workdir_manifest(dir))
    }

    // Migration is filesystem-only: the Engine's inspect/verify steps read
    // the real source directory, which the memory store never materializes.
    // These fail closed so a memory-backed Engine can never claim a migrate
    // that did not happen on disk.
    fn copy_workdir(&self, _source: &Path, _staging: &Path) -> Result<(), StoreError> {
        Err(StoreError::Io(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "memory store: migrate is filesystem-only",
        )))
    }

    fn write_workdir_manifest(
        &self,
        staging: &Path,
        manifest: &serde_json::Value,
    ) -> Result<(), StoreError> {
        let mut workdirs = self.workdirs.borrow_mut();
        let workdir = workdirs.entry(resolve_path(staging)).or_default();
        workdir.insert(
            "workdir.manifest.json".to_string(),
            serde_json::to_vec(manifest).expect("manifest serializes"),
        );
        Ok(())
    }

    fn publish_workdir(&self, staging: &Path, target: &Path) -> Result<PathBuf, StoreError> {
        let mut dirs = self.workdirs.borrow_mut();
        let staged = dirs
            .remove(&resolve_path(staging))
            .ok_or_else(|| MemoryStore::missing("memory store: missing staging workdir"))?;
        dirs.insert(resolve_path(target), staged);
        Ok(resolve_path(target))
    }

    // Issue #57: the generation Store is filesystem-only (probe, advisory
    // lane, durable journals, real process-kill recovery). The memory store
    // never materializes a store dir, so these fail closed — a memory-backed
    // Engine can never claim a store mutation that did not happen on disk.
    fn has_store(&self, _dir: &Path) -> bool {
        false
    }

    fn store_ensure(
        &self,
        _dir: &Path,
        _operation_id: &str,
        _input_sha256: &str,
    ) -> Result<(), StoreError> {
        Err(MemoryStore::unsupported())
    }

    fn store_pin(&self, _dir: &Path) -> Result<PinnedGeneration, StoreError> {
        Err(MemoryStore::unsupported())
    }

    fn store_mutate(&self, _request: StoreMutateRequest) -> Result<serde_json::Value, StoreError> {
        Err(MemoryStore::unsupported())
    }

    fn store_state(&self, _dir: &Path) -> Result<serde_json::Value, StoreError> {
        Ok(serde_json::json!({ "schema": "docx2typed-store-state-1", "backed": false }))
    }
}

impl MemoryStore {
    fn unsupported() -> StoreError {
        StoreError::store(
            "unsupported-by-design",
            "memory store: generation-store mutations are filesystem-only",
        )
    }
}

/// Deterministic in-memory verifier for Engine tests: mirrors the
/// production checks against the memory store's bytes.
pub struct MemoryVerifier {
    pub store: std::rc::Rc<std::cell::RefCell<MemoryStore>>,
}

impl MemoryVerifier {
    pub fn new(store: std::rc::Rc<std::cell::RefCell<MemoryStore>>) -> Self {
        MemoryVerifier { store }
    }
}

impl VerifierPort for MemoryVerifier {
    fn verify(&self, request: &VerificationRequest) -> VerificationEvidence {
        let store = self.store.borrow();
        let template_bytes = store
            .workdirs
            .borrow()
            .get(&resolve_path(&request.workdir))
            .and_then(|dir| dir.get("_template.docx").cloned())
            .unwrap_or_default();
        let output_bytes = store
            .builds
            .borrow()
            .get(&resolve_path(&request.output))
            .cloned()
            .unwrap_or_default();
        let identical = !template_bytes.is_empty() && template_bytes == output_bytes;
        VerificationEvidence {
            verdict: if identical { "pass" } else { "fail" }.to_string(),
            checks: vec![docx2typed_verify::VerificationCheck {
                name: "parts-match-template".to_string(),
                status: if identical { "pass" } else { "fail" }.to_string(),
                detail: None,
            }],
            output_sha256: docx2typed_protocol::bytes_sha256(&output_bytes),
            template_sha256: docx2typed_protocol::bytes_sha256(&template_bytes),
            parts_identical: identical,
            profile: request.profile.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

pub struct Engine {
    store: Box<dyn StorePort>,
    verifier: Box<dyn VerifierPort>,
}

impl Engine {
    pub fn new() -> Self {
        Engine::with_ports(
            Box::new(WorkdirStore::new()),
            Box::new(IndependentVerifier::new()),
        )
    }

    pub fn with_ports(store: Box<dyn StorePort>, verifier: Box<dyn VerifierPort>) -> Self {
        Engine { store, verifier }
    }

    /// Execute one operation synchronously. Domain failures are
    /// `OperationOutcome::failure` carrying frozen Diagnostics; only
    /// unrecoverable engine faults return `Err(EngineFailure)`.
    pub fn execute(
        &self,
        operation: Operation,
        context: OperationContext,
        args: OperationArgs,
    ) -> Result<OperationOutcome, EngineFailure> {
        match operation {
            Operation::Extract => self.extract(&context, args),
            Operation::Build => self.build(&context, args),
            Operation::Verify => self.verify(&context, args),
            Operation::Inspect => self.inspect(&context, args),
            Operation::Migrate => self.migrate(&context, args),
            Operation::Edit => self.edit(&context, args),
            Operation::StoreState => self.store_state_op(&context, args),
            Operation::Enumerate => self.enumerate(&context, args),
            Operation::Revisions => self.revisions_op(&context, args),
            Operation::Decide => self.decide_op(&context, args),
            Operation::Comment => self.comment_op(&context, args),
            Operation::Audit => self.audit_op(&context, args),
        }
    }

    fn extract(
        &self,
        context: &OperationContext,
        args: OperationArgs,
    ) -> Result<OperationOutcome, EngineFailure> {
        let OperationArgs::Extract(args) = args else {
            return Err(EngineFailure {
                message: "operation/args mismatch".to_string(),
            });
        };
        let source = resolve_path(&args.input);
        if !source.is_file() {
            return Ok(OperationOutcome::failure(vec![Diagnostic::new(
                "input-not-found",
                format!("source file not found: {}", source.to_string_lossy()),
            )]));
        }
        let source_sha256 = match file_sha256(&source) {
            Ok(hash) => hash,
            Err(error) => {
                return Ok(OperationOutcome::failure(vec![Diagnostic::new(
                    "workdir-unreadable",
                    error.to_string(),
                )]))
            }
        };
        let change_set = match plan_extract(&source, &args.outdir) {
            Ok(change_set) => change_set,
            Err(error) => return Ok(self.domain_failure("extract", &error.to_string())),
        };
        if let Err(error) = self.store.commit_workdir(&args.outdir, &change_set) {
            return Ok(self.domain_failure("extract", &error.to_string()));
        }
        let workdir = resolve_path(&args.outdir);
        let manifest_sha256 = self.store.manifest_sha256(&workdir);
        let payload = serde_json::json!({
            "engine": base_evidence_payload().get("engine"),
            "contracts": base_evidence_payload().get("contracts"),
            "inputs": {"source": {"sha256": source_sha256}},
            "outputs": {"workdir": {"manifest_sha256": manifest_sha256}},
            "checks": [{"name": "workdir-extracted", "status": "pass"}],
        });
        let evidence = run_evidence(
            "extract",
            "success",
            "mutation",
            &context.operation_id,
            payload,
        );
        let evidence_path = workdir.join("run.evidence.json");
        if let Err(error) = self.store.publish_run_evidence(&evidence_path, &evidence) {
            return Ok(OperationOutcome::failure(vec![Diagnostic::with_details(
                "evidence-publish-failed",
                format!("required run evidence could not be published: {error}"),
                None,
                None,
            )]));
        }
        Ok(OperationOutcome::success(
            serde_json::json!({ "workdir": typed_path_value(&workdir) }),
            vec![evidence],
        ))
    }

    fn build(
        &self,
        context: &OperationContext,
        args: OperationArgs,
    ) -> Result<OperationOutcome, EngineFailure> {
        let OperationArgs::Build(args) = args else {
            return Err(EngineFailure {
                message: "operation/args mismatch".to_string(),
            });
        };
        let workdir = resolve_path(&args.workdir);
        if !workdir.is_dir() {
            return Ok(OperationOutcome::failure(vec![Diagnostic::new(
                "workdir-not-found",
                format!("typed workdir not found: {}", workdir.to_string_lossy()),
            )]));
        }
        let manifest_before = self.store.manifest_sha256(&workdir);
        let plan: BuildPlan = match plan_build(&workdir) {
            Ok(plan) => plan,
            Err(error) => return Ok(self.domain_failure("build", &error.to_string())),
        };
        let output = match &args.output {
            Some(path) => resolve_path(path),
            None => {
                let name = workdir
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "workdir".to_string());
                workdir
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(format!("{name}.docx"))
            }
        };
        if !plan.replay {
            return Ok(OperationOutcome::failure(vec![Diagnostic::new(
                "workdir-invalid",
                "no-op slice cannot build edited workdirs".to_string(),
            )]));
        }
        if self.store.has_store(&workdir) {
            // Issue #57: external build publication through the generation
            // Store (two-phase: staged bytes -> journaled atomic publish
            // with verified backup; recovery rolls forward/back).
            return self.build_store_backed(context, &workdir, &plan, &output, &manifest_before);
        }
        let published = if plan.edits.is_empty() {
            match self.store.publish_build(&plan.template, &output) {
                Ok(path) => path,
                Err(error) => return Ok(self.domain_failure("build", &error.to_string())),
            }
        } else {
            // Issue #58: island-edit build — apply the recorded text
            // surgery to the template bytes (revalidated here; plan_build
            // already gated the whole build) and publish the result.
            let bytes = match docx2typed_core::prose::apply_edits(&plan.template, &plan.edits) {
                Ok(bytes) => bytes,
                Err(error) => return Ok(self.domain_failure("build", &error.to_string())),
            };
            match self.store.publish_bytes(&bytes, &output) {
                Ok(path) => path,
                Err(error) => return Ok(self.domain_failure("build", &error.to_string())),
            }
        };
        let output_sha256 = match file_sha256(&published) {
            Ok(hash) => hash,
            Err(error) => return Ok(self.domain_failure("build", &error.to_string())),
        };
        let output_bytes = std::fs::metadata(&published)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let payload = serde_json::json!({
            "engine": base_evidence_payload().get("engine"),
            "contracts": base_evidence_payload().get("contracts"),
            "inputs": {"workdir": {"manifest_sha256": manifest_before}},
            "outputs": {"docx": {"sha256": output_sha256, "bytes": output_bytes}},
            "checks": [{"name": "build", "status": "pass"}],
        });
        let evidence = run_evidence("build", "success", "build", &context.operation_id, payload);
        let evidence_path = PathBuf::from(format!("{}.evidence.json", published.to_string_lossy()));
        if let Err(error) = self.store.publish_run_evidence(&evidence_path, &evidence) {
            return Ok(OperationOutcome::failure(vec![Diagnostic::with_details(
                "evidence-publish-failed",
                format!("required run evidence could not be published: {error}"),
                None,
                None,
            )]));
        }
        Ok(OperationOutcome::success(
            serde_json::json!({ "output": typed_path_value(&published) }),
            vec![evidence],
        ))
    }

    /// Issue #57 real mutation: commit the workdir's current draft ingress
    /// (typed.md/edit.md) into a new immutable generation. Pre-store workdirs
    /// (e.g. #56 migrated targets) are lazily upgraded: birth generation 0
    /// snapshots the root, then the ingress commit advances the pointer.
    /// Success reports only after journal + ledger + evidence are durable.
    fn edit(
        &self,
        context: &OperationContext,
        args: OperationArgs,
    ) -> Result<OperationOutcome, EngineFailure> {
        let OperationArgs::Edit(args) = args else {
            return Err(EngineFailure {
                message: "operation/args mismatch".to_string(),
            });
        };
        let workdir = resolve_path(&args.workdir);
        if !workdir.is_dir() {
            return Ok(OperationOutcome::failure(vec![Diagnostic::new(
                "workdir-not-found",
                format!("typed workdir not found: {}", workdir.to_string_lossy()),
            )]));
        }
        if let Some(text) = &args.text {
            return self.edit_text(context, &workdir, &args.lock_timeout_ms, text);
        }
        let canonical = canonical_operation_input(
            "edit",
            &serde_json::json!({ "workdir": workdir.to_string_lossy() }),
        );
        // Lazy upgrade of a pre-store workdir (mirror of Python
        // `_run_json_operation` `Store.ensure`): birth generation 0.
        if let Err(error) = self
            .store
            .store_ensure(&workdir, &context.operation_id, &canonical)
        {
            return Ok(self.store_failure("edit", &error));
        }
        let pin = match self.store.store_pin(&workdir) {
            Ok(pin) => pin,
            Err(error) => return Ok(self.store_failure("edit", &error)),
        };
        let manifest_sha = pin.manifest_sha256.clone().unwrap_or_default();
        let expected_generation = pin.generation.clone();
        // The closure owns its copies; the request still needs the originals.
        let closure_manifest = manifest_sha.clone();
        let closure_generation = expected_generation.clone();
        let run = Box::new(move |_target: &Path, _tx: &mut Transaction| {
            let payload = serde_json::json!({
                "engine": base_evidence_payload().get("engine"),
                "contracts": base_evidence_payload().get("contracts"),
                "inputs": {
                    "workdir": {
                        "manifest_sha256": closure_manifest.clone(),
                        "generation": closure_generation.clone(),
                    }
                },
                "outputs": {"generation": "committed"},
                "checks": [{"name": "edit-commit", "status": "pass"}],
            });
            Ok(RunOutcome {
                outcome: "success".to_string(),
                data: serde_json::json!({ "changed": [], "generation": closure_generation.clone() }),
                kind: "edit-commit".to_string(),
                payload,
                diagnostics: vec![],
            })
        });
        let request = StoreMutateRequest {
            workdir: workdir.clone(),
            operation: "edit".to_string(),
            operation_id: context.operation_id.clone(),
            canonical,
            input_sha256: manifest_sha,
            expected_generation,
            generation: true,
            ledger_anchor: None,
            ledger_directory: true,
            evidence_path: None,
            kind: "edit-commit".to_string(),
            lock_timeout_ms: args.lock_timeout_ms,
            run,
        };
        match self.store.store_mutate(request) {
            Ok(envelope) => Ok(envelope_into_outcome(envelope)),
            Err(error) => Ok(self.store_failure("edit", &error)),
        }
    }

    /// Issue #58 real island text edit: resolve the leaf path against the
    /// current generation's template, prove the old text, commit a new
    /// generation carrying the updated `islands.json` sidecar. Cross-island
    /// or unprovable edits fail closed with a frozen diagnostic and no
    /// generation is committed (the Store rolls the mutation back).
    fn edit_text(
        &self,
        context: &OperationContext,
        workdir: &Path,
        lock_timeout_ms: &u64,
        text: &TextEdit,
    ) -> Result<OperationOutcome, EngineFailure> {
        let (paragraph_id, leaf_index) = match docx2typed_core::prose::parse_leaf_path(&text.leaf) {
            Some(parsed) => parsed,
            None => {
                return Ok(OperationOutcome::failure(vec![Diagnostic::new(
                    "invalid-edit",
                    format!("invalid leaf path: {}", text.leaf),
                )]))
            }
        };
        let Some(part) = docx2typed_core::prose::part_for_paragraph_id(&paragraph_id) else {
            return Ok(OperationOutcome::failure(vec![Diagnostic::new(
                "invalid-edit",
                format!("unknown paragraph id: {paragraph_id}"),
            )]));
        };
        let edit = docx2typed_core::prose::IslandEdit {
            part,
            paragraph_id,
            leaf_index,
            old: text.old.clone(),
            new: text.new.clone(),
            tracked: false,
            author: String::new(),
            date: String::new(),
        };
        let canonical = canonical_operation_input(
            "edit",
            &serde_json::json!({
                "workdir": workdir.to_string_lossy(),
                "leaf": text.leaf,
                "old": text.old,
                "new": text.new,
            }),
        );
        if let Err(error) = self
            .store
            .store_ensure(workdir, &context.operation_id, &canonical)
        {
            return Ok(self.store_failure("edit", &error));
        }
        let pin = match self.store.store_pin(workdir) {
            Ok(pin) => pin,
            Err(error) => return Ok(self.store_failure("edit", &error)),
        };
        let manifest_sha = pin.manifest_sha256.clone().unwrap_or_default();
        let expected_generation = pin.generation.clone();
        let closure_manifest = manifest_sha.clone();
        let closure_generation = expected_generation.clone();
        let closure_edit = edit.clone();
        let run = Box::new(move |target: &Path, _tx: &mut Transaction| {
            let template = target.join("_template.docx");
            // Invariant gate + leaf proof (fail closed; the Store rolls the
            // mutation back on Err — no partial generation is ever
            // committed).
            docx2typed_core::prose::validate_islands(
                &template,
                std::slice::from_ref(&closure_edit),
            )
            .map_err(|error| StoreError::store(edit_code(&error), error.to_string()))?;
            let mut islands = docx2typed_core::prose::load_islands(target)
                .map_err(|error| StoreError::store("workdir-invalid", error.to_string()))?;
            islands.push(closure_edit.clone());
            docx2typed_core::prose::save_islands(target, &islands).map_err(core_error_to_store)?;
            let payload = serde_json::json!({
                "engine": base_evidence_payload().get("engine"),
                "contracts": base_evidence_payload().get("contracts"),
                "inputs": {
                    "workdir": {
                        "manifest_sha256": closure_manifest.clone(),
                        "generation": closure_generation.clone(),
                    },
                    "leaf": closure_edit.leaf_index,
                    "paragraph": closure_edit.paragraph_id,
                    "part": closure_edit.part,
                },
                "outputs": {
                    "changed": [format!("{}.{}", closure_edit.paragraph_id, closure_edit.leaf_index)],
                    "generation": "committed",
                },
                "checks": [{"name": "island-edit-commit", "status": "pass"}],
            });
            Ok(RunOutcome {
                outcome: "success".to_string(),
                data: serde_json::json!({
                    "changed": [format!("{}.{}", closure_edit.paragraph_id, closure_edit.leaf_index)],
                    "generation": closure_generation.clone(),
                }),
                kind: "island-edit-commit".to_string(),
                payload,
                diagnostics: vec![],
            })
        });
        let request = StoreMutateRequest {
            workdir: workdir.to_path_buf(),
            operation: "edit".to_string(),
            operation_id: context.operation_id.clone(),
            canonical,
            input_sha256: manifest_sha,
            expected_generation,
            generation: true,
            ledger_anchor: None,
            ledger_directory: true,
            evidence_path: None,
            kind: "island-edit-commit".to_string(),
            lock_timeout_ms: *lock_timeout_ms,
            run,
        };
        match self.store.store_mutate(request) {
            Ok(envelope) => Ok(envelope_into_outcome(envelope)),
            Err(error) => Ok(self.store_failure("edit", &error)),
        }
    }

    /// Issue #58 read-only recursive prose enumeration (CLI `enumerate`);
    /// accepts a DOCX package or a typed workdir (uses `_template.docx`).
    /// The payload is the `docx2typed-prose-inventory-1` view used by the
    /// differential gate and the focused tests.
    fn enumerate(
        &self,
        _context: &OperationContext,
        args: OperationArgs,
    ) -> Result<OperationOutcome, EngineFailure> {
        let OperationArgs::Enumerate(args) = args else {
            return Err(EngineFailure {
                message: "operation/args mismatch".to_string(),
            });
        };
        let source = resolve_path(&args.source);
        let package = if source.is_dir() {
            source.join("_template.docx")
        } else {
            source.clone()
        };
        if !package.is_file() {
            return Ok(OperationOutcome::failure(vec![Diagnostic::new(
                "input-not-found",
                format!(
                    "package not found: {} (pass a .docx or a typed workdir)",
                    package.to_string_lossy()
                ),
            )]));
        }
        let inventory = match docx2typed_core::prose::enumerate_package(&package) {
            Ok(inventory) => inventory,
            Err(error) => return Ok(self.domain_failure("enumerate", &error.to_string())),
        };
        let package_sha256 = match docx2typed_protocol::file_sha256(&package) {
            Ok(hash) => hash,
            Err(error) => return Ok(self.domain_failure("enumerate", &error.to_string())),
        };
        let paragraphs: Vec<serde_json::Value> = inventory
            .paragraphs
            .iter()
            .map(|paragraph| {
                serde_json::json!({
                    "id": paragraph.paragraph_id,
                    "part": paragraph.part_key,
                    "editable": paragraph.editable,
                    "leaf_count": paragraph.leaf_count,
                    "opaque_count": paragraph.opaque_count,
                    "visible_text": paragraph.visible_text,
                })
            })
            .collect();
        let leaves: Vec<serde_json::Value> = inventory
            .leaves
            .iter()
            .map(|leaf| {
                serde_json::json!({
                    "path": format!("{}.{}", leaf.paragraph_id, leaf.leaf_index),
                    "part": leaf.part_key,
                    "paragraph": leaf.paragraph_id,
                    "leaf_index": leaf.leaf_index,
                    "text": leaf.text,
                    "editable": leaf.editable,
                    "style_sha256": leaf.style_sha256,
                })
            })
            .collect();
        let opaques: Vec<serde_json::Value> = inventory
            .opaques
            .iter()
            .map(|opaque| {
                serde_json::json!({
                    "part": opaque.part_key,
                    "paragraph": opaque.paragraph_id,
                    "tag": opaque.tag,
                    "start": opaque.start,
                    "end": opaque.end,
                })
            })
            .collect();
        Ok(OperationOutcome::success(
            serde_json::json!({
                "schema": "docx2typed-prose-inventory-1",
                "package": {"sha256": package_sha256},
                "parts": inventory.part_keys,
                "paragraphs": paragraphs,
                "leaves": leaves,
                "opaques": opaques,
            }),
            Vec::new(),
        ))
    }

    /// Resolve a source argument (package or workdir) to the package path.
    fn package_for(source: &Path) -> PathBuf {
        let source = resolve_path(source);
        if source.is_dir() {
            source.join("_template.docx")
        } else {
            source
        }
    }

    /// Issue #59 read-only revision inventory / views (CLI `revisions
    /// list|view`). Accepts a DOCX package or a typed workdir; never
    /// mutates anything.
    fn revisions_op(
        &self,
        _context: &OperationContext,
        args: OperationArgs,
    ) -> Result<OperationOutcome, EngineFailure> {
        let OperationArgs::Revisions(args) = args else {
            return Err(EngineFailure {
                message: "operation/args mismatch".to_string(),
            });
        };
        let package = Self::package_for(&args.source);
        if !package.is_file() {
            return Ok(OperationOutcome::failure(vec![Diagnostic::new(
                "input-not-found",
                format!(
                    "package not found: {} (pass a .docx or a typed workdir)",
                    package.to_string_lossy()
                ),
            )]));
        }
        let package_sha256 = match docx2typed_protocol::file_sha256(&package) {
            Ok(hash) => hash,
            Err(error) => return Ok(self.domain_failure("revisions", &error.to_string())),
        };
        if let Some(action) = &args.view {
            let views = match docx2typed_core::govern::revision_views(&package, action) {
                Ok(views) => views,
                Err(error) => return Ok(self.domain_failure("revisions", &error.to_string())),
            };
            let paragraphs: Vec<serde_json::Value> = views
                .iter()
                .map(|view| {
                    serde_json::json!({
                        "part": view.part,
                        "id": view.id,
                        "text": view.text,
                    })
                })
                .collect();
            return Ok(OperationOutcome::success(
                serde_json::json!({
                    "schema": docx2typed_core::govern::VIEW_SCHEMA,
                    "action": action,
                    "package": {"sha256": package_sha256},
                    "paragraphs": paragraphs,
                }),
                Vec::new(),
            ));
        }
        let entries = match docx2typed_core::govern::scan_revisions(&package) {
            Ok(entries) => entries,
            Err(error) => return Ok(self.domain_failure("revisions", &error.to_string())),
        };
        let revisions: Vec<serde_json::Value> = entries
            .iter()
            .map(|entry| {
                serde_json::json!({
                    "part": entry.part,
                    "kind": entry.kind,
                    "w_id": entry.w_id,
                    "author": entry.author,
                    "date": entry.date,
                    "text": entry.text,
                    "paragraph_id": entry.paragraph_id,
                    "editable": entry.editable,
                    "reason": entry.reason,
                    "scope": entry.scope,
                    "fingerprint": docx2typed_core::govern::revision_fingerprint(&entry.text),
                    "revision_key": entry.revision_key(),
                })
            })
            .collect();
        Ok(OperationOutcome::success(
            serde_json::json!({
                "schema": docx2typed_core::govern::REVISIONS_SCHEMA,
                "package": {"sha256": package_sha256},
                "revisions": revisions,
            }),
            Vec::new(),
        ))
    }

    /// Issue #59 comment inventory / deletion (CLI `comment list|delete`).
    /// Deletion is a store generation commit: the new generation carries
    /// the surgically cleaned `_template.docx` (comment entry + anchors +
    /// references removed, every other byte verbatim), a regenerated
    /// `format.json`, and a `decisions.json` sidecar. Failures (unknown
    /// comment id, store faults) roll the mutation back with no side
    /// effect.
    fn comment_op(
        &self,
        context: &OperationContext,
        args: OperationArgs,
    ) -> Result<OperationOutcome, EngineFailure> {
        let OperationArgs::Comment(args) = args else {
            return Err(EngineFailure {
                message: "operation/args mismatch".to_string(),
            });
        };
        let workdir = resolve_path(&args.workdir);
        if !workdir.is_dir() {
            return Ok(OperationOutcome::failure(vec![Diagnostic::new(
                "workdir-not-found",
                format!("typed workdir not found: {}", workdir.to_string_lossy()),
            )]));
        }
        let Some(comment_id) = &args.delete else {
            // Read-only inventory over the current template.
            let package = workdir.join("_template.docx");
            if !package.is_file() {
                return Ok(OperationOutcome::failure(vec![Diagnostic::new(
                    "workdir-invalid",
                    "typed workdir has no _template.docx".to_string(),
                )]));
            }
            let comments = match docx2typed_core::govern::scan_comments(&package) {
                Ok(comments) => comments,
                Err(error) => return Ok(self.domain_failure("comment", &error.to_string())),
            };
            let payload: Vec<serde_json::Value> = comments
                .iter()
                .map(|comment| {
                    serde_json::json!({
                        "id": comment.id,
                        "author": comment.author,
                        "date": comment.date,
                        "text": comment.text,
                        "anchors": comment.anchors,
                    })
                })
                .collect();
            return Ok(OperationOutcome::success(
                serde_json::json!({
                    "schema": docx2typed_core::govern::COMMENT_SCHEMA,
                    "comments": payload,
                }),
                Vec::new(),
            ));
        };
        let canonical = canonical_operation_input(
            "comment",
            &serde_json::json!({
                "workdir": workdir.to_string_lossy(),
                "comment_id": comment_id,
                "action": "delete",
            }),
        );
        if let Err(error) = self
            .store
            .store_ensure(&workdir, &context.operation_id, &canonical)
        {
            return Ok(self.store_failure("comment", &error));
        }
        let pin = match self.store.store_pin(&workdir) {
            Ok(pin) => pin,
            Err(error) => return Ok(self.store_failure("comment", &error)),
        };
        let manifest_sha = pin.manifest_sha256.clone().unwrap_or_default();
        let expected_generation = pin.generation.clone();
        let closure_manifest = manifest_sha.clone();
        let closure_generation = expected_generation.clone();
        let closure_id = comment_id.clone();
        let run = Box::new(move |target: &Path, _tx: &mut Transaction| {
            let template_path = target.join("_template.docx");
            let package = std::fs::read(&template_path).map_err(StoreError::Io)?;
            let cleaned = docx2typed_core::govern::delete_comment_bytes(&package, &closure_id)
                .map_err(|error| StoreError::store(comment_code(&error), error.to_string()))?;
            // Independent internal verification: the comment is gone.
            let remaining = docx2typed_core::govern::scan_comments_bytes(&cleaned)
                .map_err(|error| StoreError::store("workdir-invalid", error.to_string()))?;
            if remaining.iter().any(|comment| comment.id == closure_id) {
                return Err(StoreError::store(
                    "workdir-invalid",
                    "comment-delete verification failed: entry still present".to_string(),
                ));
            }
            std::fs::write(&template_path, &cleaned).map_err(StoreError::Io)?;
            let format_path = target.join("format.json");
            let format: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&format_path).map_err(StoreError::Io)?)
                    .map_err(|error| StoreError::store("workdir-invalid", error.to_string()))?;
            let regenerated = docx2typed_core::regenerate_workdir_format(&format, &cleaned);
            let mut format_bytes =
                serde_json::to_vec_pretty(&regenerated).expect("format serializes");
            format_bytes.push(b'\n');
            std::fs::write(&format_path, format_bytes).map_err(StoreError::Io)?;
            let decisions = serde_json::json!({
                "schema": "typed-decisions-1",
                "action": "comment-delete",
                "comment_id": closure_id,
                "decisions": [],
            });
            let mut decisions_bytes =
                serde_json::to_vec_pretty(&decisions).expect("decisions serializes");
            decisions_bytes.push(b'\n');
            std::fs::write(target.join("decisions.json"), decisions_bytes)
                .map_err(StoreError::Io)?;
            let payload = serde_json::json!({
                "engine": base_evidence_payload().get("engine"),
                "contracts": base_evidence_payload().get("contracts"),
                "inputs": {
                    "workdir": {
                        "manifest_sha256": closure_manifest.clone(),
                        "generation": closure_generation.clone(),
                    },
                    "comment_id": closure_id,
                },
                "outputs": {"generation": "committed"},
                "checks": [{"name": "comment-delete-commit", "status": "pass"}],
            });
            Ok(RunOutcome {
                outcome: "success".to_string(),
                data: serde_json::json!({
                    "decision": {"action": "comment-delete", "comment_id": closure_id},
                    "state": "clean",
                }),
                kind: "comment-delete-commit".to_string(),
                payload,
                diagnostics: vec![],
            })
        });
        let request = StoreMutateRequest {
            workdir: workdir.clone(),
            operation: "comment".to_string(),
            operation_id: context.operation_id.clone(),
            canonical,
            input_sha256: manifest_sha,
            expected_generation,
            generation: true,
            ledger_anchor: None,
            ledger_directory: true,
            evidence_path: None,
            kind: "comment-delete-commit".to_string(),
            lock_timeout_ms: args.lock_timeout_ms,
            run,
        };
        match self.store.store_mutate(request) {
            Ok(envelope) => Ok(envelope_into_outcome(envelope)),
            Err(error) => Ok(self.store_failure("comment", &error)),
        }
    }

    /// Issue #59 revision settlement decisions and table structure
    /// operations (CLI `decide <action> ...`).
    ///
    /// Single decisions (accept/reject/reinsert) run as store generation
    /// commits: the guards (confirmation-vs-key fingerprint, key-vs-actual
    /// fingerprint, kind, editable surface) fail closed with frozen
    /// diagnostics and no mutation; on success the new generation carries
    /// the settled `_template.docx`, a regenerated `format.json`, and a
    /// `decisions.json` sidecar. Table operations build a new DOCX and
    /// re-extract a fresh workdir (new-baseline semantics, issue #49);
    /// the source workdir is never mutated.
    fn decide_op(
        &self,
        context: &OperationContext,
        args: OperationArgs,
    ) -> Result<OperationOutcome, EngineFailure> {
        let OperationArgs::Decide(args) = args else {
            return Err(EngineFailure {
                message: "operation/args mismatch".to_string(),
            });
        };
        let workdir = resolve_path(&args.workdir);
        if !workdir.is_dir() {
            return Ok(OperationOutcome::failure(vec![Diagnostic::new(
                "workdir-not-found",
                format!("typed workdir not found: {}", workdir.to_string_lossy()),
            )]));
        }
        if args.action.starts_with("table-") {
            return self.table_op(context, &workdir, &args);
        }
        // -- single revision decisions -------------------------------------
        let action = args.action.as_str();
        if !matches!(action, "accept" | "reject" | "reinsert") {
            return Ok(OperationOutcome::failure(vec![Diagnostic::new(
                "invalid-action",
                format!("unknown decision action: {action}"),
            )]));
        }
        // Key shape: <part>|<kind>|<w_id>|<fingerprint>.
        let parts: Vec<&str> = args.revision_key.split('|').collect();
        if parts.len() != 4 {
            return Ok(OperationOutcome::failure(vec![Diagnostic::new(
                "malformed-revision-key",
                format!("malformed revision key: {}", args.revision_key),
            )]));
        }
        let (part, kind, w_id, key_fingerprint) = (parts[0], parts[1], parts[2], parts[3]);
        // Guard 1: the confirmation fingerprint must equal the key's.
        if let Some(expected) = &args.fingerprint {
            if expected != key_fingerprint {
                return Ok(OperationOutcome::failure(vec![Diagnostic::new(
                    "revision-fingerprint-mismatch",
                    format!(
                        "revision-fingerprint-mismatch: key says {key_fingerprint}, confirmation says {expected}"
                    ),
                )]));
            }
        }
        // Guard 2: only direct-body document revisions are decidable.
        if part != "word/document.xml" {
            return Ok(OperationOutcome::failure(vec![Diagnostic::new(
                "revision-outside-editable-surface",
                format!("revision-outside-editable-surface: {part} revisions can only be viewed"),
            )]));
        }
        let canonical = canonical_operation_input(
            "decide",
            &serde_json::json!({
                "workdir": workdir.to_string_lossy(),
                "action": action,
                "revision_key": args.revision_key,
                "fingerprint": args.fingerprint,
            }),
        );
        if let Err(error) = self
            .store
            .store_ensure(&workdir, &context.operation_id, &canonical)
        {
            return Ok(self.store_failure("decide", &error));
        }
        let pin = match self.store.store_pin(&workdir) {
            Ok(pin) => pin,
            Err(error) => return Ok(self.store_failure("decide", &error)),
        };
        let manifest_sha = pin.manifest_sha256.clone().unwrap_or_default();
        let expected_generation = pin.generation.clone();
        let closure_manifest = manifest_sha.clone();
        let closure_generation = expected_generation.clone();
        let closure_key = args.revision_key.clone();
        let closure_kind = kind.to_string();
        let closure_w_id = w_id.to_string();
        let closure_action = action.to_string();
        let closure_fingerprint = key_fingerprint.to_string();
        let closure_author = args.author.clone();
        let closure_text = args.text.clone();
        let run = Box::new(move |target: &Path, _tx: &mut Transaction| {
            let template_path = target.join("_template.docx");
            let package = std::fs::read(&template_path).map_err(StoreError::Io)?;
            let document_xml = docx2typed_core::govern::document_xml_bytes(&package)
                .map_err(|error| StoreError::store("workdir-invalid", error.to_string()))?;
            let (settled, decision) = if closure_action == "reinsert" {
                let author = closure_author.clone().unwrap_or_else(|| {
                    std::env::var("DOCX2TYPED_AUTHOR").unwrap_or_else(|_| "Unknown".to_string())
                });
                let date = docx2typed_protocol::utc_now_iso();
                let (out, record) = docx2typed_core::govern::reinsert_deleted_text(
                    &document_xml,
                    &package,
                    &closure_w_id,
                    &author,
                    &date,
                    closure_text.as_deref(),
                )
                .map_err(|(code, message)| StoreError::store(&code, message))?;
                // Kind + fingerprint guards on the target deletion.
                if record.kind != "delete" && record.kind != "move_from" {
                    return Err(StoreError::store(
                        "workdir-invalid",
                        format!("reinsert target is not a deletion: {}", record.kind),
                    ));
                }
                if record.fingerprint != closure_fingerprint {
                    return Err(StoreError::store(
                        "revision-text-fingerprint-mismatch",
                        format!(
                            "revision-text-fingerprint-mismatch: expected {closure_fingerprint}, got {}",
                            record.fingerprint
                        ),
                    ));
                }
                let decision = serde_json::json!({
                    "w_id": record.w_id,
                    "kind": record.kind,
                    "action": "reinsert",
                    "fingerprint": record.fingerprint,
                    "paragraph_id": record.paragraph_id,
                    "operation": record.operation,
                    "new_w_id": record.new_w_id,
                });
                (out, decision)
            } else {
                let settled = docx2typed_core::govern::settle_one_revision(
                    &document_xml,
                    &closure_w_id,
                    &closure_action,
                )
                .map_err(|(code, message)| StoreError::store(&code, message))?;
                // Guard 3: the key's kind must match the element's kind.
                if settled.revision.kind != closure_kind {
                    return Err(StoreError::store(
                        "workdir-invalid",
                        format!(
                            "revision kind mismatch: key says {closure_kind}, node is {}",
                            settled.revision.kind
                        ),
                    ));
                }
                // Guard 4: the key's fingerprint must match the live text.
                if settled.revision.fingerprint() != closure_fingerprint {
                    return Err(StoreError::store(
                        "revision-text-fingerprint-mismatch",
                        format!(
                            "revision-text-fingerprint-mismatch: expected {closure_fingerprint}, got {}",
                            settled.revision.fingerprint()
                        ),
                    ));
                }
                let decision = serde_json::json!({
                    "w_id": settled.revision.w_id,
                    "kind": settled.revision.kind,
                    "action": closure_action,
                    "fingerprint": settled.revision.fingerprint(),
                    "paragraph_id": settled.revision.paragraph_id,
                    "operation": if (closure_action == "accept"
                        && (settled.revision.kind == "insert" || settled.revision.kind == "move_to"))
                        || (closure_action == "reject"
                            && (settled.revision.kind == "delete" || settled.revision.kind == "move_from"))
                    {
                        "unwrap"
                    } else {
                        "remove"
                    },
                });
                (settled.part_xml, decision)
            };
            // Independent internal verification: the decided revision is
            // gone (or, for reinsert, the new insertion exists).
            let new_package = docx2typed_core::govern::patch_document_xml(&package, &settled)
                .map_err(|error| StoreError::store("workdir-invalid", error.to_string()))?;
            let verified = verify_decision_outcome(&new_package, &closure_w_id, &closure_action)
                .map_err(|message| StoreError::store("workdir-invalid", message))?;
            if !verified {
                return Err(StoreError::store(
                    "workdir-invalid",
                    format!(
                        "decision verification failed: {closure_action} w:id {closure_w_id} did not settle"
                    ),
                ));
            }
            std::fs::write(&template_path, &new_package).map_err(StoreError::Io)?;
            let format_path = target.join("format.json");
            let format: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&format_path).map_err(StoreError::Io)?)
                    .map_err(|error| StoreError::store("workdir-invalid", error.to_string()))?;
            let regenerated = docx2typed_core::regenerate_workdir_format(&format, &new_package);
            let mut format_bytes =
                serde_json::to_vec_pretty(&regenerated).expect("format serializes");
            format_bytes.push(b'\n');
            std::fs::write(&format_path, format_bytes).map_err(StoreError::Io)?;
            let decisions = serde_json::json!({
                "schema": "typed-decisions-1",
                "action": closure_action,
                "revision_key": closure_key,
                "decisions": [decision.clone()],
            });
            let mut decisions_bytes =
                serde_json::to_vec_pretty(&decisions).expect("decisions serializes");
            decisions_bytes.push(b'\n');
            std::fs::write(target.join("decisions.json"), decisions_bytes)
                .map_err(StoreError::Io)?;
            let payload = serde_json::json!({
                "engine": base_evidence_payload().get("engine"),
                "contracts": base_evidence_payload().get("contracts"),
                "inputs": {
                    "workdir": {
                        "manifest_sha256": closure_manifest.clone(),
                        "generation": closure_generation.clone(),
                    },
                    "revision_key": closure_key,
                },
                "outputs": {"generation": "committed"},
                "decision": decision,
                "checks": [{"name": "decision-published", "status": "pass"}],
            });
            Ok(RunOutcome {
                outcome: "success".to_string(),
                data: serde_json::json!({
                    "decision": decision,
                    "state": "clean",
                }),
                kind: "decision-published".to_string(),
                payload,
                diagnostics: vec![],
            })
        });
        let request = StoreMutateRequest {
            workdir: workdir.clone(),
            operation: "decide".to_string(),
            operation_id: context.operation_id.clone(),
            canonical,
            input_sha256: manifest_sha,
            expected_generation,
            generation: true,
            ledger_anchor: None,
            ledger_directory: true,
            evidence_path: None,
            kind: "decision-published".to_string(),
            lock_timeout_ms: args.lock_timeout_ms,
            run,
        };
        match self.store.store_mutate(request) {
            Ok(envelope) => Ok(envelope_into_outcome(envelope)),
            Err(error) => Ok(self.store_failure("decide", &error)),
        }
    }

    /// Issue #59 table structure ops: new DOCX + fresh workdir baseline
    /// (mirror of Python `_apply_table_op`). The source workdir is never
    /// mutated; the decided artifact is published through the store's
    /// journaled external lane.
    fn table_op(
        &self,
        context: &OperationContext,
        workdir: &Path,
        args: &DecideArgs,
    ) -> Result<OperationOutcome, EngineFailure> {
        let action = args.action.trim_start_matches("table-").to_string();
        let output = match &args.output {
            Some(path) => resolve_path(path),
            None => {
                return Ok(OperationOutcome::failure(vec![Diagnostic::new(
                    "invalid-arguments",
                    "table ops need --output and --workdir-out".to_string(),
                )]))
            }
        };
        let new_workdir = match &args.workdir_out {
            Some(path) => resolve_path(path),
            None => {
                return Ok(OperationOutcome::failure(vec![Diagnostic::new(
                    "invalid-arguments",
                    "table ops need --output and --workdir-out".to_string(),
                )]))
            }
        };
        if output.exists() {
            return Ok(OperationOutcome::failure(vec![Diagnostic::new(
                "decided-output-already-exists",
                format!(
                    "decided output already exists: {}",
                    output.to_string_lossy()
                ),
            )]));
        }
        if new_workdir.exists() {
            return Ok(OperationOutcome::failure(vec![Diagnostic::new(
                "decided-workdir-already-exists",
                format!(
                    "decided workdir already exists: {}",
                    new_workdir.to_string_lossy()
                ),
            )]));
        }
        let canonical = canonical_operation_input(
            "decide",
            &serde_json::json!({
                "workdir": workdir.to_string_lossy(),
                "action": args.action,
                "table": args.revision_key,
                "args": args.args,
                "discard_content": args.discard_content,
            }),
        );
        if let Err(error) = self
            .store
            .store_ensure(workdir, &context.operation_id, &canonical)
        {
            return Ok(self.store_failure("decide", &error));
        }
        let pin = match self.store.store_pin(workdir) {
            Ok(pin) => pin,
            Err(error) => return Ok(self.store_failure("decide", &error)),
        };
        let manifest_sha = pin.manifest_sha256.clone().unwrap_or_default();
        let expected_generation = pin.generation.clone();
        let closure_manifest = manifest_sha.clone();
        let table_ref = args.revision_key.clone();
        let table_args = args.args.clone();
        let discard = args.discard_content;
        let closure_action = action.clone();
        let out = output.clone();
        let new_wd = new_workdir.clone();
        let operation_id = context.operation_id.clone();
        let evidence_path = new_workdir.join("run.evidence.json");
        let run = Box::new(move |target: &Path, tx: &mut Transaction| {
            let template_path = target.join("_template.docx");
            let package = std::fs::read(&template_path).map_err(StoreError::Io)?;
            let document_xml = docx2typed_core::govern::document_xml_bytes(&package)
                .map_err(|error| StoreError::store("workdir-invalid", error.to_string()))?;
            let table_index = parse_table_ref(&table_ref).ok_or_else(|| {
                StoreError::store(
                    "invalid-table-reference",
                    format!("invalid table reference: {table_ref}"),
                )
            })?;
            let patched = docx2typed_core::govern::apply_table_operation(
                &document_xml,
                table_index,
                &closure_action,
                &table_args,
                discard,
            )
            .map_err(|error| StoreError::store(table_code(&error), error.to_string()))?;
            let new_package = docx2typed_core::govern::patch_document_xml(&package, &patched)
                .map_err(|error| StoreError::store("workdir-invalid", error.to_string()))?;
            // Stage the decided docx for journaled external publication.
            let staged = tx.staging("decided.docx");
            std::fs::write(&staged, &new_package).map_err(StoreError::Io)?;
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&staged)
                .map_err(StoreError::Io)?
                .sync_all()
                .map_err(StoreError::Io)?;
            tx.stage_external(&out, &staged, "create")?;
            // Fresh workdir baseline re-extracted from the decided docx.
            if let Some(parent) = new_wd.parent() {
                fs::create_dir_all(parent).map_err(StoreError::Io)?;
            }
            let change_set = docx2typed_core::plan_extract(&staged, &new_wd)
                .map_err(|error| StoreError::store("workdir-invalid", error.to_string()))?;
            WorkdirStore::commit_workdir(&WorkdirStore::new(), &new_wd, &change_set)
                .map_err(|error| StoreError::store("workdir-invalid", error.to_string()))?;
            let decisions = serde_json::json!({
                "schema": "typed-decisions-1",
                "action": format!("table-{closure_action}"),
                "table": table_ref,
                "args": table_args,
                "decisions": [],
            });
            let mut decisions_bytes =
                serde_json::to_vec_pretty(&decisions).expect("decisions serializes");
            decisions_bytes.push(b'\n');
            std::fs::write(new_wd.join("decisions.json"), decisions_bytes)
                .map_err(StoreError::Io)?;
            let output_sha256 =
                docx2typed_protocol::file_sha256(&staged).map_err(StoreError::Io)?;
            let payload = serde_json::json!({
                "engine": base_evidence_payload().get("engine"),
                "contracts": base_evidence_payload().get("contracts"),
                "inputs": {"workdir": {"manifest_sha256": closure_manifest}},
                "outputs": {
                    "docx": {"sha256": output_sha256},
                    "workdir": {"manifest_sha256": "committed"},
                },
                "action": format!("table-{closure_action}"),
                "table": table_ref,
                "checks": [{"name": "table-op", "status": "pass"}],
            });
            Ok(RunOutcome {
                outcome: "success".to_string(),
                data: serde_json::json!({
                    "operation": format!("table-{closure_action}"),
                    "table": table_ref,
                    "workdir": typed_path_value(&new_wd),
                    "output": typed_path_value(&out),
                }),
                kind: "mutation".to_string(),
                payload,
                diagnostics: vec![],
            })
        });
        let request = StoreMutateRequest {
            workdir: workdir.to_path_buf(),
            operation: "decide".to_string(),
            operation_id,
            canonical,
            input_sha256: manifest_sha,
            expected_generation,
            generation: false,
            ledger_anchor: Some(new_workdir.clone()),
            ledger_directory: true,
            evidence_path: Some(evidence_path),
            kind: "mutation".to_string(),
            lock_timeout_ms: args.lock_timeout_ms,
            run,
        };
        match self.store.store_mutate(request) {
            Ok(envelope) => Ok(envelope_into_outcome(envelope)),
            Err(error) => Ok(self.store_failure("decide", &error)),
        }
    }

    /// Issue #59 read-only governed Unicode normalization audit (CLI
    /// `audit`): reports vertical-catalog candidates as data; no mutation,
    /// no standalone normalize surface. The catalog payload is validated
    /// (schema + pinned hash) before scanning.
    fn audit_op(
        &self,
        _context: &OperationContext,
        args: OperationArgs,
    ) -> Result<OperationOutcome, EngineFailure> {
        let OperationArgs::Audit(args) = args else {
            return Err(EngineFailure {
                message: "operation/args mismatch".to_string(),
            });
        };
        let package = Self::package_for(&args.source);
        if !package.is_file() {
            return Ok(OperationOutcome::failure(vec![Diagnostic::new(
                "input-not-found",
                format!(
                    "package not found: {} (pass a .docx or a typed workdir)",
                    package.to_string_lossy()
                ),
            )]));
        }
        let package_bytes = match std::fs::read(&package) {
            Ok(bytes) => bytes,
            Err(error) => return Ok(self.domain_failure("audit", &error.to_string())),
        };
        let package_sha256 = docx2typed_protocol::bytes_sha256(&package_bytes);
        // Issue #61: the release binary embeds the pinned catalog; an
        // explicit `--catalog` override is honored for gates/tests only.
        let (catalog_path, catalog_bytes) = if let Some(path) = &args.catalog_path {
            let resolved = resolve_path(path);
            match std::fs::read(&resolved) {
                Ok(bytes) => (resolved, bytes),
                Err(error) => {
                    return Ok(OperationOutcome::failure(vec![Diagnostic::new(
                        "workdir-invalid",
                        format!("cannot read pinned Unicode catalog: {error}"),
                    )]))
                }
            }
        } else {
            (
                PathBuf::from("embedded"),
                embedded::canonical_asset(embedded::UNICODE_VERTICAL_CATALOG_JSON)
                    .into_owned()
                    .into_bytes(),
            )
        };
        let catalog: serde_json::Value = match serde_json::from_slice(&catalog_bytes) {
            Ok(value) => value,
            Err(error) => {
                return Ok(OperationOutcome::failure(vec![Diagnostic::new(
                    "workdir-invalid",
                    format!("cannot read pinned Unicode catalog: {error}"),
                )]))
            }
        };
        let candidates = match docx2typed_core::govern::scan_normalization_candidates(
            &package_bytes,
            &catalog,
        ) {
            Ok(candidates) => candidates,
            Err(error) => return Ok(self.domain_failure("audit", &error.to_string())),
        };
        let payload: Vec<serde_json::Value> = candidates
            .iter()
            .map(|candidate| {
                serde_json::json!({
                    "candidate_id": candidate.candidate_id,
                    "occurrence_id": candidate.occurrence_id,
                    "paragraph_id": candidate.paragraph_id,
                    "codepoint": candidate.codepoint,
                    "source": candidate.source,
                    "name": candidate.name,
                    "category": candidate.category,
                    "vertical": candidate.vertical,
                    "proposed_target": candidate.proposed_target,
                    "reversible": candidate.reversible,
                    "context": candidate.context,
                })
            })
            .collect();
        Ok(OperationOutcome::success(
            serde_json::json!({
                "schema": docx2typed_core::govern::AUDIT_SCHEMA,
                "package": {"sha256": package_sha256},
                "catalog": {
                    "path": catalog_path.to_string_lossy(),
                    "unicode_version": catalog.get("unicode_version"),
                    "catalog_hash": catalog.get("catalog_hash"),
                },
                "candidates": payload,
                "checks": [{"name": "unicode-audit-scan", "status": "pass"}],
            }),
            Vec::new(),
        ))
    }

    /// Issue #57 external build publication through the Store: the run
    /// stages the byte-copy build output, the Store journals `prepared`,
    /// publishes the external output with a verified backup, writes the
    /// ledger beside the artifact, and only then reports success.
    fn build_store_backed(
        &self,
        context: &OperationContext,
        workdir: &Path,
        plan: &BuildPlan,
        output: &Path,
        manifest_before: &str,
    ) -> Result<OperationOutcome, EngineFailure> {
        let pin = match self.store.store_pin(workdir) {
            Ok(pin) => pin,
            Err(error) => return Ok(self.store_failure("build", &error)),
        };
        let canonical = canonical_operation_input(
            "build",
            &serde_json::json!({
                "workdir": workdir.to_string_lossy(),
                "output": output.to_string_lossy(),
            }),
        );
        let evidence_path = PathBuf::from(format!("{}.evidence.json", output.to_string_lossy()));
        let template = plan.template.clone();
        let edits = plan.edits.clone();
        let out = output.to_path_buf();
        let manifest_before = manifest_before.to_string();
        let run = Box::new(move |_target: &Path, tx: &mut Transaction| {
            let staged = tx.staging("out.docx");
            if edits.is_empty() {
                fs::copy(&template, &staged).map_err(StoreError::Io)?;
            } else {
                // Issue #58: island-edit build through the store: apply the
                // recorded surgery to the template bytes (revalidated) and
                // stage the result.
                let bytes = docx2typed_core::prose::apply_edits(&template, &edits)
                    .map_err(|error| StoreError::store("workdir-invalid", error.to_string()))?;
                fs::write(&staged, bytes).map_err(StoreError::Io)?;
            }
            // Flush the staged bytes before publication: the atomic rename
            // must never outrun its content.
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&staged)
                .map_err(StoreError::Io)?
                .sync_all()
                .map_err(StoreError::Io)?;
            tx.stage_external(&out, &staged, "replace")?;
            let output_sha256 = file_sha256(&staged).map_err(StoreError::Io)?;
            Ok(RunOutcome {
                outcome: "success".to_string(),
                data: serde_json::json!({ "output": typed_path_value(&out) }),
                kind: "build".to_string(),
                payload: serde_json::json!({
                    "engine": base_evidence_payload().get("engine"),
                    "contracts": base_evidence_payload().get("contracts"),
                    "inputs": {"workdir": {"manifest_sha256": manifest_before}},
                    "outputs": {"docx": {"sha256": output_sha256}},
                    "checks": [{"name": "build", "status": "pass"}],
                }),
                diagnostics: vec![],
            })
        });
        let request = StoreMutateRequest {
            workdir: workdir.to_path_buf(),
            operation: "build".to_string(),
            operation_id: context.operation_id.clone(),
            canonical,
            input_sha256: pin.manifest_sha256.clone().unwrap_or_default(),
            expected_generation: pin.generation.clone(),
            generation: false,
            ledger_anchor: Some(output.to_path_buf()),
            ledger_directory: false,
            evidence_path: Some(evidence_path),
            kind: "build".to_string(),
            lock_timeout_ms: 0,
            run,
        };
        match self.store.store_mutate(request) {
            Ok(envelope) => Ok(envelope_into_outcome(envelope)),
            Err(error) => Ok(self.store_failure("build", &error)),
        }
    }

    /// Read-only store inspection: the `docx2typed-store-state-1` payload
    /// (backed, generation, pending transactions with phases, recovery
    /// warnings, reserve state, filesystem qualification). No recover
    /// command exists — startup recovery runs at mutation entry points.
    fn store_state_op(
        &self,
        _context: &OperationContext,
        args: OperationArgs,
    ) -> Result<OperationOutcome, EngineFailure> {
        let OperationArgs::StoreState(args) = args else {
            return Err(EngineFailure {
                message: "operation/args mismatch".to_string(),
            });
        };
        let source = resolve_path(&args.source);
        if !source.exists() {
            return Ok(OperationOutcome::failure(vec![Diagnostic::new(
                "workdir-not-found",
                format!("source workdir not found: {}", source.to_string_lossy()),
            )]));
        }
        match self.store.store_state(&source) {
            Ok(state) => Ok(OperationOutcome::success(state, Vec::new())),
            Err(error) => Ok(self.store_failure("store-state", &error)),
        }
    }

    fn verify(
        &self,
        context: &OperationContext,
        args: OperationArgs,
    ) -> Result<OperationOutcome, EngineFailure> {
        let OperationArgs::Verify(args) = args else {
            return Err(EngineFailure {
                message: "operation/args mismatch".to_string(),
            });
        };
        let workdir = resolve_path(&args.workdir);
        if !workdir.is_dir() {
            return Ok(OperationOutcome::failure(vec![Diagnostic::new(
                "workdir-not-found",
                format!("typed workdir not found: {}", workdir.to_string_lossy()),
            )]));
        }
        let output = resolve_path(&args.output);
        if !output.is_file() {
            return Ok(OperationOutcome::failure(vec![Diagnostic::new(
                "input-not-found",
                format!("output DOCX not found: {}", output.to_string_lossy()),
            )]));
        }
        let manifest = self.store.manifest_sha256(&workdir);
        let verification = self.verifier.verify(&VerificationRequest {
            workdir: workdir.clone(),
            output: output.clone(),
            profile: context.profile.clone(),
        });
        if verification.verdict != "pass" {
            let detail = verification
                .checks
                .iter()
                .find(|check| check.status != "pass")
                .map(|check| check.name.clone())
                .unwrap_or_else(|| "verification".to_string());
            return Ok(OperationOutcome::failure(vec![Diagnostic::new(
                "workdir-invalid",
                format!("independent verification failed: {detail}"),
            )]));
        }
        let output_sha256 = verification.output_sha256.clone();
        let detail_checks: Vec<serde_json::Value> = verification
            .checks
            .iter()
            .map(|check| {
                serde_json::json!({
                    "name": check.name,
                    "status": check.status,
                    "detail": check.detail,
                })
            })
            .collect();
        let payload = serde_json::json!({
            "engine": base_evidence_payload().get("engine"),
            "contracts": base_evidence_payload().get("contracts"),
            "inputs": {"workdir": {"manifest_sha256": manifest}},
            "outputs": {"docx": {"sha256": output_sha256}},
            "verdict": "pass",
            "checks": [{"name": "independent-verification", "status": "pass"}],
            "verifier_checks": detail_checks,
        });
        let evidence = run_evidence(
            "verify",
            "success",
            "verify",
            &context.operation_id,
            payload,
        );
        let evidence_path =
            PathBuf::from(format!("{}.verify.evidence.json", output.to_string_lossy()));
        if let Err(error) = self.store.publish_run_evidence(&evidence_path, &evidence) {
            return Ok(OperationOutcome::failure(vec![Diagnostic::with_details(
                "evidence-publish-failed",
                format!("required run evidence could not be published: {error}"),
                None,
                None,
            )]));
        }
        Ok(OperationOutcome::success(
            serde_json::json!({ "verified": typed_path_value(&output) }),
            vec![evidence],
        ))
    }

    /// Read-only readiness classification of one schema-1 workdir
    /// (mirroring Python's `_inspect_json`): the classification payload is
    /// the Result data even when readiness is `blocked`.
    fn inspect(
        &self,
        _context: &OperationContext,
        args: OperationArgs,
    ) -> Result<OperationOutcome, EngineFailure> {
        let OperationArgs::Inspect(args) = args else {
            return Err(EngineFailure {
                message: "operation/args mismatch".to_string(),
            });
        };
        let source = resolve_path(&args.source);
        if !source.exists() {
            return Ok(OperationOutcome::failure(vec![Diagnostic::new(
                "workdir-not-found",
                format!("source workdir not found: {}", source.to_string_lossy()),
            )]));
        }
        if !source.is_dir() {
            return Ok(OperationOutcome::failure(vec![Diagnostic::new(
                "workdir-invalid",
                format!("source is not a directory: {}", source.to_string_lossy()),
            )]));
        }
        let inspection = match docx2typed_core::inspect::inspect_workdir(&source) {
            Ok(inspection) => inspection,
            Err(error) => {
                return Ok(OperationOutcome::failure(vec![Diagnostic::new(
                    "workdir-unreadable",
                    error.to_string(),
                )]))
            }
        };
        Ok(OperationOutcome::success(inspection.payload, Vec::new()))
    }

    /// Lossless schema-1 -> manifest-backed migration (mirroring Python's
    /// `_migrate_json` + `migrate_workdir`): inspect, stage byte-for-byte
    /// in a sibling temp dir, verify asset closure + byte/semantic
    /// equivalence + typed validation + observable behavior, write the
    /// versioned manifest, publish the evidence sidecar, then atomically
    /// rename staging onto TARGET. Any failure removes staging and the
    /// sidecar and leaves no normal TARGET. The source is never modified.
    fn migrate(
        &self,
        context: &OperationContext,
        args: OperationArgs,
    ) -> Result<OperationOutcome, EngineFailure> {
        let OperationArgs::Migrate(args) = args else {
            return Err(EngineFailure {
                message: "operation/args mismatch".to_string(),
            });
        };
        let source = resolve_path(&args.source);
        let target = resolve_path(&args.target);
        if !source.exists() {
            return Ok(OperationOutcome::failure(vec![Diagnostic::new(
                "workdir-not-found",
                format!("source workdir not found: {}", source.to_string_lossy()),
            )]));
        }
        if !source.is_dir() {
            return Ok(OperationOutcome::failure(vec![Diagnostic::new(
                "workdir-invalid",
                format!("source is not a directory: {}", source.to_string_lossy()),
            )]));
        }
        let inspection = match docx2typed_core::inspect::inspect_workdir(&source) {
            Ok(inspection) => inspection,
            Err(error) => {
                return Ok(OperationOutcome::failure(vec![Diagnostic::new(
                    "workdir-unreadable",
                    error.to_string(),
                )]))
            }
        };
        if inspection.readiness != "ready" {
            let reason = docx2typed_core::inspect::blocking_reason(&inspection.reason_codes);
            return Ok(OperationOutcome::failure(vec![Diagnostic::new(
                reason,
                format!(
                    "source workdir is not migratable: {}",
                    inspection.reason_codes.join(", ")
                ),
            )]));
        }
        if target.exists() {
            return Ok(OperationOutcome::failure(vec![Diagnostic::new(
                "target-already-exists",
                format!("target already exists: {}", target.to_string_lossy()),
            )]));
        }
        if let Some(parent) = target.parent() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                return Ok(OperationOutcome::failure(vec![Diagnostic::new(
                    "workdir-unreadable",
                    error.to_string(),
                )]));
            }
        }
        let target_name = target
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "workdir".to_string());
        let staging = target
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(format!(
                ".{}.migrate-{}.tmp",
                target_name,
                docx2typed_protocol::new_operation_id()
            ));
        let evidence_path = PathBuf::from(format!(
            "{}.migrate.evidence.json",
            target.to_string_lossy()
        ));
        let result = self.migrate_attempt(
            &source,
            &target,
            &staging,
            &evidence_path,
            &inspection,
            &context.operation_id,
        );
        match result {
            Ok((data, evidence)) => Ok(OperationOutcome::success(data, vec![evidence])),
            Err((code, message)) => {
                // Failed publication leaves no normal TARGET: staging and
                // the evidence sidecar are removed.
                let _ = std::fs::remove_dir_all(&staging);
                let _ = std::fs::remove_file(&evidence_path);
                Ok(OperationOutcome::failure(vec![Diagnostic::new(
                    &code, message,
                )]))
            }
        }
    }

    /// The staged migration pipeline: copy -> verify -> manifest ->
    /// evidence -> atomic publish. Returns `(data, evidence)` on success or
    /// `(diagnostic code, message)` on any failure (the caller cleans up).
    fn migrate_attempt(
        &self,
        source: &Path,
        target: &Path,
        staging: &Path,
        evidence_path: &Path,
        inspection: &docx2typed_core::inspect::Inspection,
        operation_id: &str,
    ) -> Result<(serde_json::Value, RunEvidence), (String, String)> {
        let fail = |code: &str, message: String| Err((code.to_string(), message));
        if let Err(error) = std::fs::create_dir(staging) {
            return fail("workdir-unreadable", error.to_string());
        }
        if let Err(error) = self.store.copy_workdir(source, staging) {
            return fail("workdir-unreadable", error.to_string());
        }
        let checks = match verify_staged(self.store.as_ref(), source, staging, inspection) {
            Ok(checks) => checks,
            Err(message) => return fail("migrate-verification-failed", message),
        };
        let manifest = match build_manifest(
            self.store.as_ref(),
            source,
            staging,
            inspection,
            operation_id,
            &checks,
        ) {
            Ok(manifest) => manifest,
            Err(message) => return fail("migrate-verification-failed", message),
        };
        if let Err(error) = self.store.write_workdir_manifest(staging, &manifest) {
            return fail("workdir-unreadable", error.to_string());
        }
        let payload = evidence_payload(&manifest, &checks, inspection);
        let evidence = run_evidence("migrate", "success", "mutation", operation_id, payload);
        if let Err(error) = self.store.publish_run_evidence(evidence_path, &evidence) {
            return fail(
                "evidence-publish-failed",
                format!("required run evidence could not be published: {error}"),
            );
        }
        if let Err(error) = self.store.publish_workdir(staging, target) {
            return fail("workdir-unreadable", error.to_string());
        }
        let data = serde_json::json!({
            "operation_id": operation_id,
            "workdir": typed_path_value(target),
            "manifest": typed_path_value(&target.join("workdir.manifest.json")),
        });
        Ok((data, evidence))
    }

    /// Validate a workdir and produce the `docx2typed-session-descriptor-1`
    /// payload (MCP `workdir_open` support). Domain errors return the
    /// failure text; the adapter maps it to a frozen Diagnostic.
    pub fn open_workdir_session(
        &self,
        workdir: &Path,
        author: Option<&str>,
    ) -> Result<serde_json::Value, String> {
        let meta = validate_workdir(workdir).map_err(|error| error.to_string())?;
        let manifest_sha256 = self.store.manifest_sha256(&meta.root);
        Ok(serde_json::json!({
            "schema": "docx2typed-session-descriptor-1",
            "workdir": typed_path_value(&meta.root),
            "workdir_manifest_sha256": manifest_sha256,
            "freshness": if meta.pristine { "clean" } else { "dirty" },
            "effective_mode": "direct",
            "author": author,
            "paragraphs": 0,
            "snapshot": {"current": "clean", "staged": "clean"},
            "cas": {"current_matches_filesystem": true},
            "supported_tools": docx2typed_protocol::PROTOCOL_TOOLS.to_vec(),
        }))
    }

    /// Map a Core/Store failure into a frozen Diagnostic. The message's
    /// kebab prefix is used as the code when registered; otherwise the
    /// stable `workdir-invalid` domain default applies (mirroring Python's
    /// `domain_code_from_message`).
    fn domain_failure(&self, _operation: &str, message: &str) -> OperationOutcome {
        let candidate = message
            .split(':')
            .next()
            .unwrap_or("")
            .trim()
            .to_lowercase()
            .replace(' ', "-");
        let code = match candidate.as_str() {
            "file not found" | "source file not found" => "input-not-found",
            "workdir not found" => "workdir-not-found",
            "not a valid docx"
            | "incompatible typed workdir schema"
            | "workdir missing"
            | "invalid workdir json" => "workdir-invalid",
            // source-drift is a registered code (Python emits it directly).
            "source-drift" => "source-drift",
            // Issue #58 prose failures carry their kebab code in the
            // message prefix (invalid-edit, opaque-paragraph-mutated,
            // prose-edit-ambiguous, prose-edit-unsupported, prose-xml-invalid).
            other if is_registered_code(other) => other,
            _ => "workdir-invalid",
        };
        OperationOutcome::failure(vec![Diagnostic::new(code, message.to_string())])
    }

    /// Map a Store-contract failure to its frozen diagnostic code.
    fn store_failure(&self, _operation: &str, error: &StoreError) -> OperationOutcome {
        let code = match error.code() {
            Some(code) if is_registered_code(code) => code,
            _ => "workdir-invalid",
        };
        OperationOutcome::failure(vec![Diagnostic::new(code, error.message().to_string())])
    }
}

/// The frozen diagnostic code carried by a Core govern failure (the
/// message's kebab prefix when registered, else `workdir-invalid`).
fn govern_code(error: &docx2typed_core::CoreError) -> String {
    let message = error.to_string();
    let candidate = message.split(':').next().unwrap_or("").trim().to_string();
    if is_registered_code(&candidate) {
        candidate
    } else {
        "workdir-invalid".to_string()
    }
}

/// Comment-deletion failure codes (comment-not-found is the only govern
/// code the surgery emits).
fn comment_code(error: &docx2typed_core::CoreError) -> String {
    let candidate = error.to_string();
    if candidate.starts_with("comment-not-found") {
        "comment-not-found".to_string()
    } else {
        govern_code(error)
    }
}

/// Table-op failure codes (invalid-table-reference and
/// merge-would-discard-content are registered; range errors fall back to
/// the Python reference's workdir-invalid).
fn table_code(error: &docx2typed_core::CoreError) -> String {
    let message = error.to_string();
    let candidate = message.split(':').next().unwrap_or("").trim().to_string();
    match candidate.as_str() {
        "invalid-table-reference" | "merge-would-discard-content" => candidate,
        _ => "workdir-invalid".to_string(),
    }
}

/// `Tn` -> n (Python's table reference check).
fn parse_table_ref(reference: &str) -> Option<usize> {
    let digits = reference.strip_prefix('T')?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Independent internal verification of one decision outcome: the decided
/// revision wrapper must be gone from the settled package (or, for
/// reinsert, a fresh insertion with a new w:id exists after the deletion).
fn verify_decision_outcome(package: &[u8], w_id: &str, action: &str) -> Result<bool, String> {
    let entries = docx2typed_core::govern::scan_revisions_bytes(package)
        .map_err(|error| error.to_string())?;
    if action == "reinsert" {
        // The original deletion may remain; the new insertion must exist.
        return Ok(entries
            .iter()
            .any(|entry| entry.kind == "insert" && entry.w_id != w_id && entry.reason.is_none()));
    }
    // accept/reject: the target revision is settled (gone or unwrapped —
    // either way no revision wrapper with that w:id remains on the
    // editable surface).
    Ok(!entries
        .iter()
        .any(|entry| entry.w_id == w_id && entry.reason.is_none() && entry.scope.is_none()))
}

/// Convert a Core prose failure into a Store-contract failure for the
/// journal abort path.
fn core_error_to_store(error: docx2typed_core::CoreError) -> StoreError {
    match error {
        docx2typed_core::CoreError::Io(io) => StoreError::Io(io),
        docx2typed_core::CoreError::Domain(message)
        | docx2typed_core::CoreError::Message(message) => {
            StoreError::store("workdir-invalid", message)
        }
    }
}

/// The frozen diagnostic code carried by a Core prose failure (the message
/// is `"<kebab-code>: <detail>"`); falls back to `workdir-invalid` for
/// non-edit failures.
fn edit_code(error: &docx2typed_core::CoreError) -> &'static str {
    let message = error.to_string();
    let candidate = message.split(':').next().unwrap_or("").trim().to_string();
    if is_registered_code(&candidate) {
        match candidate.as_str() {
            "invalid-edit" => "invalid-edit",
            "opaque-paragraph-mutated" => "opaque-paragraph-mutated",
            "prose-edit-ambiguous" => "prose-edit-ambiguous",
            "prose-edit-unsupported" => "prose-edit-unsupported",
            "prose-xml-invalid" => "prose-xml-invalid",
            _ => "workdir-invalid",
        }
    } else {
        "workdir-invalid"
    }
}

/// Rebuild the `OperationOutcome` carried by a committed Store envelope (the
/// journal's envelope is authoritative for replay parity).
fn envelope_into_outcome(envelope: serde_json::Value) -> OperationOutcome {
    let outcome = match envelope.get("outcome").and_then(serde_json::Value::as_str) {
        Some("success") => Outcome::Success,
        Some("partial") => Outcome::Partial,
        _ => Outcome::Failure,
    };
    let data = envelope
        .get("data")
        .cloned()
        .unwrap_or_else(|| serde_json::Value::Object(Default::default()));
    let diagnostics = envelope
        .get("diagnostics")
        .and_then(serde_json::Value::as_array)
        .map(|list| list.iter().map(diagnostic_from_value).collect())
        .unwrap_or_default();
    let evidence = envelope
        .get("evidence")
        .and_then(serde_json::Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|value| serde_json::from_value(value.clone()).ok())
                .collect()
        })
        .unwrap_or_default();
    OperationOutcome {
        outcome,
        data,
        diagnostics,
        evidence,
    }
}

fn diagnostic_from_value(value: &serde_json::Value) -> Diagnostic {
    Diagnostic::with_details(
        value
            .get("code")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("workdir-invalid"),
        value
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        value.get("details").cloned(),
        value
            .get("next_actions")
            .and_then(serde_json::Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_string)
                    .collect()
            }),
    )
}

fn is_registered_code(code: &str) -> bool {
    matches!(
        code,
        "input-not-found"
            | "workdir-not-found"
            | "workdir-invalid"
            | "workdir-unreadable"
            | "contract-incompatible"
            | "required-feature-unsupported"
            | "evidence-publish-failed"
            | "resource-limit-exceeded"
            | "invalid-arguments"
            | "asset-closure"
            | "schema-incompatible"
            | "source-drift"
            | "symlink-detected"
            | "target-already-exists"
            | "operation-id-required"
            | "migrate-verification-failed"
            | "edit-dirty"
            | "edit-stale"
            | "edit-conflict"
            | "edit-state-missing"
            | "edit-state-incompatible"
            | "edit-binding-mismatch"
            | "edit-header-tampered"
            | "edit-grammar-invalid"
            | "edit-header-missing"
            // Issue #57 store diagnostics (frozen bundle registry).
            | "writer-busy"
            | "writer-timeout"
            | "generation-conflict"
            | "needs-recovery"
            | "reserve-depleted"
            | "unsupported-by-design"
            | "store-invalid"
            | "operation-id-reused"
            | "operation-journal-conflict"
            // Issue #58 prose diagnostics (frozen bundle registry).
            | "invalid-edit"
            | "opaque-paragraph-mutated"
            | "prose-edit-ambiguous"
            | "prose-edit-unsupported"
            | "prose-xml-invalid"
            // Issue #59 governed-workflow diagnostics (frozen bundle
            // registry: revisions/decisions/comments/tables/audit).
            | "invalid-action"
            | "malformed-revision-key"
            | "revision-fingerprint-mismatch"
            | "revision-not-found"
            | "revision-outside-editable-surface"
            | "revision-text-fingerprint-mismatch"
            | "invalid-table-reference"
            | "merge-would-discard-content"
            | "comment-not-found"
            | "decided-output-already-exists"
            | "decided-workdir-already-exists"
    )
}

// ---------------------------------------------------------------------------
// Migration helpers (mirroring scripts/inspect_migrate.py
// `_verify_staged` / `_build_manifest` / `_evidence_payload`)
// ---------------------------------------------------------------------------

/// Present (path -> entry) map over an inventory table.
fn present_assets(
    assets: &[docx2typed_core::inspect::AssetEntry],
) -> std::collections::BTreeMap<&str, &docx2typed_core::inspect::AssetEntry> {
    assets
        .iter()
        .filter(|asset| asset.presence == "present")
        .map(|asset| (asset.path.as_str(), asset))
        .collect()
}

/// Verify the staged copy before publication: asset closure, byte
/// equivalence, semantic equivalence (derived workdir manifest), typed
/// validation, and observable behavior (clean workdirs build byte-identical
/// output — the Rust mirror replays the template bytes).
fn verify_staged(
    store: &dyn StorePort,
    source: &Path,
    staging: &Path,
    inspection: &docx2typed_core::inspect::Inspection,
) -> Result<Vec<serde_json::Value>, String> {
    let mut checks = Vec::new();
    let source_assets = match docx2typed_core::inspect::inventory_assets(source) {
        Ok(assets) => assets,
        Err(error) => return Err(error.to_string()),
    };
    let staged_assets = match docx2typed_core::inspect::inventory_assets(staging) {
        Ok(assets) => assets,
        Err(error) => return Err(error.to_string()),
    };
    let source_present = present_assets(&source_assets);
    let staged_present = present_assets(&staged_assets);
    let missing: Vec<&str> = source_present
        .keys()
        .filter(|path| !staged_present.contains_key(*path))
        .cloned()
        .collect();
    let extra: Vec<&str> = staged_present
        .keys()
        .filter(|path| !source_present.contains_key(*path))
        .cloned()
        .collect();
    if !missing.is_empty() || !extra.is_empty() {
        return Err(format!(
            "asset closure mismatch after copy; missing={missing:?} extra={extra:?}"
        ));
    }
    let mismatched: Vec<&str> = source_present
        .iter()
        .filter(|(path, asset)| {
            staged_present
                .get(*path)
                .map(|staged| staged.sha256 != asset.sha256)
                .unwrap_or(true)
        })
        .map(|(path, _)| *path)
        .collect();
    if !mismatched.is_empty() {
        return Err(format!("asset bytes differ after copy: {mismatched:?}"));
    }
    checks.push(serde_json::json!({ "name": "asset-closure", "status": "pass" }));
    checks.push(serde_json::json!({ "name": "byte-equivalence", "status": "pass" }));

    if store.derived_workdir_manifest(source) != store.derived_workdir_manifest(staging) {
        return Err("semantic workdir manifest differs after copy".to_string());
    }
    checks.push(serde_json::json!({ "name": "semantic-equivalence", "status": "pass" }));

    if let Err(error) = docx2typed_core::validate_workdir(staging) {
        return Err(format!(
            "typed validation of the staged workdir failed: {error}"
        ));
    }
    checks.push(serde_json::json!({ "name": "typed-validation", "status": "pass" }));

    // Observable behavior equivalence: a clean workdir must build
    // byte-identical output from source and from the staged copy (the no-op
    // contract replays the template bytes). Non-clean semantic state is
    // preserved (never flattened) and recorded in the manifest instead.
    let edit_state = inspection
        .edit_state
        .get("state")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if edit_state == "clean" {
        let source_hash = docx2typed_protocol::file_sha256(&source.join("_template.docx"))
            .map_err(|error| error.to_string())?;
        let staged_hash = docx2typed_protocol::file_sha256(&staging.join("_template.docx"))
            .map_err(|error| error.to_string())?;
        if source_hash != staged_hash {
            return Err(
                "observable build output differs between source and staged workdir".to_string(),
            );
        }
        checks.push(serde_json::json!({ "name": "behavior-equivalence", "status": "pass" }));
    } else {
        checks.push(serde_json::json!({ "name": "behavior-equivalence", "status": "skipped" }));
    }
    Ok(checks)
}

/// The versioned workdir manifest (`docx2typed-workdir-manifest-1`),
/// mirroring `_build_manifest`: present assets minus the manifest file plus
/// the single generated self-entry (null self-hash).
fn build_manifest(
    store: &dyn StorePort,
    source: &Path,
    staging: &Path,
    inspection: &docx2typed_core::inspect::Inspection,
    operation_id: &str,
    checks: &[serde_json::Value],
) -> Result<serde_json::Value, String> {
    use docx2typed_core::inspect::MANIFEST_FILE;
    let format_data = docx2typed_core::inspect::format_data(source).unwrap_or_default();
    let mut assets = Vec::new();
    for asset in &inspection.assets {
        if asset.presence != "present" || asset.path == MANIFEST_FILE {
            continue;
        }
        assets.push(asset.to_json());
    }
    assets.push(serde_json::json!({
        "path": MANIFEST_FILE,
        "kind": "generated",
        "required": true,
        "read_only": true,
        "role": "workdir-manifest",
        "presence": "present",
        "bytes": serde_json::Value::Null,
        "sha256": serde_json::Value::Null,
        "mtime_ns": serde_json::Value::Null,
    }));
    let identity =
        docx2typed_core::inspect::inventory_sha256(source).map_err(|error| error.to_string())?;
    Ok(serde_json::json!({
        "schema": docx2typed_core::inspect::WORKDIR_MANIFEST_SCHEMA,
        "manifest_version": docx2typed_core::inspect::MANIFEST_VERSION,
        "workdir_schema": format_data.get("schema").and_then(serde_json::Value::as_str).unwrap_or("typed-format-1"),
        "model_version": format_data.get("model_version").and_then(serde_json::Value::as_i64).unwrap_or(1),
        "canonicalizer_version": format_data.get("canonicalizer_version").and_then(serde_json::Value::as_i64).unwrap_or(1),
        "producer": {
            "engine": docx2typed_protocol::ENGINE_NAME,
            "version": docx2typed_protocol::PACKAGE_VERSION,
            "operation": "migrate",
            "operation_id": operation_id,
        },
        "source": {
            "identity": identity,
            "semantic_manifest_sha256": store.manifest_sha256(source),
        },
        "baseline": {
            "template": format_data.get("template"),
            "template_sha256": format_data.get("template_sha256"),
            "package_manifest": format_data.get("package_manifest"),
            "source": format_data.get("source"),
            "source_sha256": format_data.get("source_sha256"),
            "styles_sha256": format_data.get("styles_sha256"),
            "document_xml_sha256": format_data.get("document_xml_sha256"),
            "source_track_enabled": format_data.get("source_track_enabled"),
            "uses_date_utc": format_data.get("uses_date_utc"),
        },
        "features": {
            "supported": docx2typed_protocol::FEATURES,
            "required": docx2typed_protocol::REQUIRED_FEATURES,
        },
        "state": {
            "readiness": inspection.readiness,
            "edit": inspection.edit_state,
            "revision_count": inspection.revision_count,
            "comment_count": inspection.comment_count,
            "reason_codes": inspection.reason_codes,
            "semantic_manifest_sha256": store.manifest_sha256(staging),
        },
        "checks": checks,
        "assets": assets,
    }))
}

/// The semantic part of the migrate run-evidence record, mirroring
/// `_evidence_payload` (producer engine identity comes from the live
/// `base_evidence_payload`).
fn evidence_payload(
    manifest: &serde_json::Value,
    checks: &[serde_json::Value],
    inspection: &docx2typed_core::inspect::Inspection,
) -> serde_json::Value {
    let base = base_evidence_payload();
    let opaque = inspection
        .assets
        .iter()
        .filter(|asset| asset.kind == "opaque" && asset.presence == "present")
        .count();
    serde_json::json!({
        "engine": base.get("engine"),
        "contracts": base.get("contracts"),
        "inputs": {
            "source": {
                "inventory_sha256": manifest.get("source").and_then(|v| v.get("identity")),
                "semantic_manifest_sha256": manifest.get("source").and_then(|v| v.get("semantic_manifest_sha256")),
            }
        },
        "outputs": {
            "target": {
                "manifest_sha256": docx2typed_protocol::semantic_sha256(manifest),
                "semantic_manifest_sha256": manifest.get("state").and_then(|v| v.get("semantic_manifest_sha256")),
                "assets": manifest.get("assets").and_then(serde_json::Value::as_array).map(|list| list.len()).unwrap_or(0),
                "opaque_assets": opaque,
            }
        },
        "checks": checks,
    })
}

impl Default for Engine {
    fn default() -> Self {
        Engine::new()
    }
}
