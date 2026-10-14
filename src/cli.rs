//! CLI adapter: hand-rolled argv parsing for the five finite commands of
//! this slice (mirroring Python argparse behavior), Engine invocation, and
//! `docx2typed-result-1` envelope printing (compact, key-sorted JSON on
//! stdout — the Python `_print_json` format).

use std::path::PathBuf;

use docx2typed_app::{
    AuditArgs, BuildArgs, CommentArgs, DecideArgs, EditArgs, Engine, EnumerateArgs, ExtractArgs,
    InspectArgs, MigrateArgs, Operation, OperationArgs, OperationContext, ProjectViewArgs,
    RevisionsArgs, StoreStateArgs, TextEdit, VerifyArgs,
};
use docx2typed_protocol::{Diagnostic, ResultEnvelope};

pub fn run(operation: Operation, argv: &[String], json_mode: bool, build_commit: &str) -> i32 {
    let operation_name = operation.name();
    // The Protocol JSON contract requires an explicit retry identity for
    // migrate (mirroring Python's `_migrate_json`); human mode generates one.
    if json_mode
        && operation == Operation::Migrate
        && !argv.iter().any(|arg| arg == "--operation-id")
    {
        print_envelope(
            &ResultEnvelope::new(
                operation_name,
                "failure",
                serde_json::Value::Object(Default::default()),
                vec![Diagnostic::new(
                    "operation-id-required",
                    "migrate requires --operation-id in the Protocol JSON contract".to_string(),
                )],
                vec![],
                build_commit,
            ),
            json_mode,
        );
        return 1;
    }
    let parsed = match parse(operation, argv) {
        Ok(parsed) => parsed,
        Err(message) => {
            if json_mode {
                print_invocation_failure(operation_name, &message, argv);
            } else {
                eprintln!("docx2typed {operation_name}: {message}");
            }
            return 1;
        }
    };
    let engine = Engine::new();
    let context = OperationContext::new(parsed.operation_id);
    let outcome = match engine.execute(operation, context, parsed.args) {
        Ok(outcome) => outcome,
        Err(failure) => {
            let envelope = ResultEnvelope::new(
                operation_name,
                "failure",
                serde_json::Value::Object(Default::default()),
                vec![Diagnostic::new("workdir-invalid", failure.message)],
                vec![],
                build_commit,
            );
            print_envelope(&envelope, json_mode);
            return 1;
        }
    };
    let envelope = outcome.into_envelope(operation_name, build_commit);
    let failed = envelope.outcome != "success";
    print_envelope(&envelope, json_mode);
    if failed {
        1
    } else {
        0
    }
}

fn print_envelope(envelope: &ResultEnvelope, json_mode: bool) {
    let json = serde_json::to_string(envelope).expect("envelope serializes");
    if json_mode {
        println!("{json}");
    } else {
        match envelope.outcome.as_str() {
            "success" => {
                for (key, value) in envelope.data.as_object().expect("data is an object") {
                    println!(
                        "{key}: {}",
                        value
                            .get("value")
                            .and_then(|v| v.as_str())
                            .unwrap_or(&serde_json::to_string(value).expect("value serializes"))
                    );
                }
            }
            _ => {
                for diagnostic in &envelope.diagnostics {
                    eprintln!("{}: {}", diagnostic.code, diagnostic.message);
                }
            }
        }
    }
}

pub fn print_invocation_failure(operation: &str, message: &str, argv: &[String]) {
    let envelope = ResultEnvelope::new(
        operation,
        "failure",
        serde_json::Value::Object(Default::default()),
        vec![Diagnostic::with_details(
            "invalid-arguments",
            message.to_string(),
            Some(serde_json::json!({
                "expected": ["see docx2typed --help"],
                "actual": argv,
            })),
            None,
        )],
        vec![],
        "",
    );
    println!(
        "{}",
        serde_json::to_string(&envelope).expect("envelope serializes")
    );
}

struct ParseResult {
    args: OperationArgs,
    operation_id: String,
}

fn parse(operation: Operation, argv: &[String]) -> Result<ParseResult, String> {
    match operation {
        Operation::Extract => {
            let mut positional: Vec<&String> = Vec::new();
            let mut outdir = ".".to_string();
            let mut operation_id: Option<String> = None;
            let mut index = 0usize;
            while index < argv.len() {
                match argv[index].as_str() {
                    "-o" | "--outdir" => {
                        index += 1;
                        outdir = argv
                            .get(index)
                            .ok_or("expected value for -o/--outdir")?
                            .clone();
                    }
                    // The explicit declaration that the incoming file becomes
                    // the current state even when the workdir has committed
                    // versions. A workdir at its baseline needs no switch and
                    // extracts without one.
                    "--discard-committed-versions" => {}
                    "--operation-id" => {
                        index += 1;
                        operation_id = Some(
                            argv.get(index)
                                .ok_or("expected value for --operation-id")?
                                .clone(),
                        );
                    }
                    flag if flag.starts_with('-') => {
                        return Err(format!("unrecognized argument: {flag}"));
                    }
                    _ => positional.push(&argv[index]),
                }
                index += 1;
            }
            let input: String = positional
                .first()
                .map(|value| (*value).clone())
                .ok_or("extract requires a source .docx")?;
            if positional.len() > 1 {
                return Err("extract accepts exactly one source .docx".to_string());
            }
            Ok(ParseResult {
                args: OperationArgs::Extract(ExtractArgs {
                    input: PathBuf::from(input),
                    outdir: PathBuf::from(outdir),
                    discard_committed_versions: argv
                        .iter()
                        .any(|a| a == "--discard-committed-versions"),
                }),
                operation_id: operation_id.unwrap_or_else(docx2typed_protocol::new_operation_id),
            })
        }
        Operation::Build => {
            let mut positional: Vec<&String> = Vec::new();
            let mut output: Option<String> = None;
            let mut operation_id: Option<String> = None;
            let mut lock_timeout_ms: u64 = 0;
            let mut index = 0usize;
            while index < argv.len() {
                match argv[index].as_str() {
                    "-o" | "--output" => {
                        index += 1;
                        output = Some(
                            argv.get(index)
                                .ok_or("expected value for -o/--output")?
                                .clone(),
                        );
                    }
                    "--operation-id" => {
                        index += 1;
                        operation_id = Some(
                            argv.get(index)
                                .ok_or("expected value for --operation-id")?
                                .clone(),
                        );
                    }
                    "--lock-timeout-ms" => {
                        index += 1;
                        lock_timeout_ms = argv
                            .get(index)
                            .ok_or("expected value for --lock-timeout-ms")?
                            .parse()
                            .map_err(|_| "--lock-timeout-ms must be a number".to_string())?;
                    }
                    flag if flag.starts_with('-') => {
                        return Err(format!("unrecognized argument: {flag}"));
                    }
                    _ => positional.push(&argv[index]),
                }
                index += 1;
            }
            let workdir: String = positional
                .first()
                .map(|value| (*value).clone())
                .ok_or("build requires a typed workdir")?;
            if positional.len() > 1 {
                return Err("build accepts exactly one workdir".to_string());
            }
            Ok(ParseResult {
                args: OperationArgs::Build(BuildArgs {
                    workdir: PathBuf::from(workdir),
                    output: output.map(PathBuf::from),
                    lock_timeout_ms,
                }),
                operation_id: operation_id.unwrap_or_else(docx2typed_protocol::new_operation_id),
            })
        }
        Operation::Verify => {
            let mut positional: Vec<&String> = Vec::new();
            let mut index = 0usize;
            while index < argv.len() {
                let flag = argv[index].as_str();
                if flag.starts_with('-') {
                    return Err(format!("unrecognized argument: {flag}"));
                }
                positional.push(&argv[index]);
                index += 1;
            }
            if positional.len() != 2 {
                return Err("verify requires a typed workdir and an output .docx".to_string());
            }
            Ok(ParseResult {
                args: OperationArgs::Verify(VerifyArgs {
                    workdir: PathBuf::from(positional[0]),
                    output: PathBuf::from(positional[1]),
                }),
                operation_id: docx2typed_protocol::new_operation_id(),
            })
        }
        Operation::Inspect => {
            let mut positional: Vec<&String> = Vec::new();
            let mut index = 0usize;
            while index < argv.len() {
                let flag = argv[index].as_str();
                if flag.starts_with('-') {
                    return Err(format!("unrecognized argument: {flag}"));
                }
                positional.push(&argv[index]);
                index += 1;
            }
            let source: String = positional
                .first()
                .map(|value| (*value).clone())
                .ok_or("inspect requires a typed workdir")?;
            if positional.len() > 1 {
                return Err("inspect accepts exactly one workdir".to_string());
            }
            Ok(ParseResult {
                args: OperationArgs::Inspect(InspectArgs {
                    source: PathBuf::from(source),
                }),
                operation_id: docx2typed_protocol::new_operation_id(),
            })
        }
        Operation::Migrate => {
            let mut positional: Vec<&String> = Vec::new();
            let mut out: Option<String> = None;
            let mut operation_id: Option<String> = None;
            let mut index = 0usize;
            while index < argv.len() {
                match argv[index].as_str() {
                    "--out" => {
                        index += 1;
                        out = Some(argv.get(index).ok_or("expected value for --out")?.clone());
                    }
                    "--operation-id" => {
                        index += 1;
                        operation_id = Some(
                            argv.get(index)
                                .ok_or("expected value for --operation-id")?
                                .clone(),
                        );
                    }
                    flag if flag.starts_with('-') => {
                        return Err(format!("unrecognized argument: {flag}"));
                    }
                    _ => positional.push(&argv[index]),
                }
                index += 1;
            }
            let source: String = positional
                .first()
                .map(|value| (*value).clone())
                .ok_or("migrate requires a source workdir and --out TARGET")?;
            if positional.len() > 1 {
                return Err("migrate accepts exactly one source workdir".to_string());
            }
            let out = out.ok_or("migrate requires --out TARGET")?;
            Ok(ParseResult {
                args: OperationArgs::Migrate(MigrateArgs {
                    source: PathBuf::from(source),
                    target: PathBuf::from(out),
                }),
                operation_id: operation_id.unwrap_or_else(docx2typed_protocol::new_operation_id),
            })
        }
        Operation::Edit => {
            let mut positional: Vec<&String> = Vec::new();
            let mut operation_id: Option<String> = None;
            let mut lock_timeout_ms: u64 = 0;
            let mut index = 0usize;
            while index < argv.len() {
                match argv[index].as_str() {
                    "--operation-id" => {
                        index += 1;
                        operation_id = Some(
                            argv.get(index)
                                .ok_or("expected value for --operation-id")?
                                .clone(),
                        );
                    }
                    "--lock-timeout-ms" => {
                        index += 1;
                        lock_timeout_ms = argv
                            .get(index)
                            .ok_or("expected value for --lock-timeout-ms")?
                            .parse()
                            .map_err(|_| "--lock-timeout-ms must be a number".to_string())?;
                    }
                    flag if flag.starts_with('-') => {
                        return Err(format!("unrecognized argument: {flag}"));
                    }
                    _ => positional.push(&argv[index]),
                }
                index += 1;
            }
            // Optional Python-style subcommand token (`sync`); the tracer
            // edit IS the sync commit, so it is accepted and ignored.
            if positional
                .first()
                .is_some_and(|token| token.as_str() == "sync")
            {
                positional.remove(0);
            }
            // Issue #58: `edit text <workdir> <leaf-path> <old> <new>` —
            // one real island-local text edit (committed as a generation).
            if positional
                .first()
                .is_some_and(|token| token.as_str() == "text")
            {
                positional.remove(0);
                let workdir: String = positional
                    .first()
                    .map(|value| (*value).clone())
                    .ok_or("edit text requires a typed workdir, a leaf path, old, and new")?;
                let leaf: String = positional
                    .get(1)
                    .map(|value| (*value).clone())
                    .ok_or("edit text requires a leaf path, old, and new")?;
                let old_text: String = positional
                    .get(2)
                    .map(|value| (*value).clone())
                    .ok_or("edit text requires old and new text")?;
                let new_text: String = positional
                    .get(3)
                    .map(|value| (*value).clone())
                    .ok_or("edit text requires new text")?;
                if positional.len() > 4 {
                    return Err(
                        "edit text accepts exactly workdir, leaf path, old, new".to_string()
                    );
                }
                return Ok(ParseResult {
                    args: OperationArgs::Edit(EditArgs {
                        workdir: PathBuf::from(workdir),
                        lock_timeout_ms,
                        text: Some(TextEdit {
                            leaf,
                            old: old_text,
                            new: new_text,
                        }),
                    }),
                    operation_id: operation_id
                        .unwrap_or_else(docx2typed_protocol::new_operation_id),
                });
            }
            let workdir: String = positional
                .first()
                .map(|value| (*value).clone())
                .ok_or("edit requires a typed workdir")?;
            if positional.len() > 1 {
                return Err("edit accepts exactly one workdir".to_string());
            }
            Ok(ParseResult {
                args: OperationArgs::Edit(EditArgs {
                    workdir: PathBuf::from(workdir),
                    lock_timeout_ms,
                    text: None,
                }),
                operation_id: operation_id.unwrap_or_else(docx2typed_protocol::new_operation_id),
            })
        }
        Operation::Enumerate => {
            let mut positional: Vec<&String> = Vec::new();
            let mut index = 0usize;
            while index < argv.len() {
                let flag = argv[index].as_str();
                if flag.starts_with('-') {
                    return Err(format!("unrecognized argument: {flag}"));
                }
                positional.push(&argv[index]);
                index += 1;
            }
            let source: String = positional
                .first()
                .map(|value| (*value).clone())
                .ok_or("enumerate requires a .docx or typed workdir")?;
            if positional.len() > 1 {
                return Err("enumerate accepts exactly one source".to_string());
            }
            Ok(ParseResult {
                args: OperationArgs::Enumerate(EnumerateArgs {
                    source: PathBuf::from(source),
                }),
                operation_id: docx2typed_protocol::new_operation_id(),
            })
        }
        Operation::StoreState => {
            let mut positional: Vec<&String> = Vec::new();
            let mut index = 0usize;
            while index < argv.len() {
                let flag = argv[index].as_str();
                if flag.starts_with('-') {
                    return Err(format!("unrecognized argument: {flag}"));
                }
                positional.push(&argv[index]);
                index += 1;
            }
            let source: String = positional
                .first()
                .map(|value| (*value).clone())
                .ok_or("store-state requires a typed workdir")?;
            if positional.len() > 1 {
                return Err("store-state accepts exactly one workdir".to_string());
            }
            Ok(ParseResult {
                args: OperationArgs::StoreState(StoreStateArgs {
                    source: PathBuf::from(source),
                }),
                operation_id: docx2typed_protocol::new_operation_id(),
            })
        }
        Operation::Revisions => {
            let mut positional: Vec<&String> = Vec::new();
            let mut index = 0usize;
            while index < argv.len() {
                let flag = argv[index].as_str();
                if flag.starts_with('-') {
                    return Err(format!("unrecognized argument: {flag}"));
                }
                positional.push(&argv[index]);
                index += 1;
            }
            let subcommand = positional
                .first()
                .map(|value| value.as_str())
                .unwrap_or("list");
            // `revisions list <source>` | `revisions view <source> <action>`.
            let view = match subcommand {
                "list" => None,
                "view" => {
                    let action = positional
                        .get(2)
                        .map(|value| value.as_str())
                        .ok_or("revisions view requires an action (accept|reject)")?;
                    if action != "accept" && action != "reject" {
                        return Err(format!(
                            "revisions view action must be accept or reject, got {action}"
                        ));
                    }
                    Some(action.to_string())
                }
                _ => return Err(format!("unknown revisions subcommand: {subcommand}")),
            };
            let source = match subcommand {
                "list" => positional
                    .get(1)
                    .map(|value| (*value).clone())
                    .ok_or("revisions list requires a .docx or typed workdir")?,
                _ => positional
                    .get(1)
                    .map(|value| (*value).clone())
                    .ok_or("revisions view requires a .docx or typed workdir")?,
            };
            Ok(ParseResult {
                args: OperationArgs::Revisions(RevisionsArgs {
                    source: PathBuf::from(source),
                    view,
                }),
                operation_id: docx2typed_protocol::new_operation_id(),
            })
        }
        Operation::Decide => {
            let mut positional: Vec<&String> = Vec::new();
            let mut workdir: Option<String> = None;
            let mut fingerprint: Option<String> = None;
            let mut author: Option<String> = None;
            let mut text: Option<String> = None;
            let mut raw_args: Vec<String> = Vec::new();
            let mut output: Option<String> = None;
            let mut workdir_out: Option<String> = None;
            let mut discard_content = false;
            let mut operation_id: Option<String> = None;
            let mut lock_timeout_ms: u64 = 0;
            let mut index = 0usize;
            while index < argv.len() {
                match argv[index].as_str() {
                    "--workdir" => {
                        index += 1;
                        workdir = Some(
                            argv.get(index)
                                .ok_or("expected value for --workdir")?
                                .clone(),
                        );
                    }
                    "--fingerprint" => {
                        index += 1;
                        fingerprint = Some(
                            argv.get(index)
                                .ok_or("expected value for --fingerprint")?
                                .clone(),
                        );
                    }
                    "--author" => {
                        index += 1;
                        author = Some(
                            argv.get(index)
                                .ok_or("expected value for --author")?
                                .clone(),
                        );
                    }
                    "--text" => {
                        index += 1;
                        text = Some(argv.get(index).ok_or("expected value for --text")?.clone());
                    }
                    "--args" => {
                        index += 1;
                        raw_args = argv
                            .get(index)
                            .ok_or("expected value for --args")?
                            .split_whitespace()
                            .map(str::to_string)
                            .collect();
                    }
                    "--output" => {
                        index += 1;
                        output = Some(
                            argv.get(index)
                                .ok_or("expected value for --output")?
                                .clone(),
                        );
                    }
                    "--workdir-out" => {
                        index += 1;
                        workdir_out = Some(
                            argv.get(index)
                                .ok_or("expected value for --workdir-out")?
                                .clone(),
                        );
                    }
                    "--discard-content" => discard_content = true,
                    "--operation-id" => {
                        index += 1;
                        operation_id = Some(
                            argv.get(index)
                                .ok_or("expected value for --operation-id")?
                                .clone(),
                        );
                    }
                    "--lock-timeout-ms" => {
                        index += 1;
                        lock_timeout_ms = argv
                            .get(index)
                            .ok_or("expected value for --lock-timeout-ms")?
                            .parse()
                            .map_err(|_| "--lock-timeout-ms must be a number".to_string())?;
                    }
                    flag if flag.starts_with('-') => {
                        return Err(format!("unrecognized argument: {flag}"));
                    }
                    _ => positional.push(&argv[index]),
                }
                index += 1;
            }
            let action: String = positional
                .first()
                .map(|value| (*value).clone())
                .ok_or("decide requires an action")?;
            let revision_key: String = positional
                .get(1)
                .map(|value| (*value).clone())
                .ok_or("decide requires a revision key or table reference")?;
            if positional.len() > 2 {
                return Err("decide accepts exactly one revision key".to_string());
            }
            let workdir = workdir.ok_or("decide requires --workdir")?;
            let args = match raw_args
                .iter()
                .map(|token| token.parse::<usize>())
                .collect::<Result<Vec<_>, _>>()
            {
                Ok(parsed) => parsed,
                Err(_) => return Err("--args must be space-separated numbers".to_string()),
            };
            Ok(ParseResult {
                args: OperationArgs::Decide(DecideArgs {
                    workdir: PathBuf::from(workdir),
                    action,
                    revision_key,
                    fingerprint,
                    author,
                    text,
                    args,
                    discard_content,
                    output: output.map(PathBuf::from),
                    workdir_out: workdir_out.map(PathBuf::from),
                    lock_timeout_ms,
                }),
                operation_id: operation_id.unwrap_or_else(docx2typed_protocol::new_operation_id),
            })
        }
        Operation::Comment => {
            let mut positional: Vec<&String> = Vec::new();
            let mut operation_id: Option<String> = None;
            let mut lock_timeout_ms: u64 = 0;
            let mut index = 0usize;
            while index < argv.len() {
                match argv[index].as_str() {
                    "--operation-id" => {
                        index += 1;
                        operation_id = Some(
                            argv.get(index)
                                .ok_or("expected value for --operation-id")?
                                .clone(),
                        );
                    }
                    "--lock-timeout-ms" => {
                        index += 1;
                        lock_timeout_ms = argv
                            .get(index)
                            .ok_or("expected value for --lock-timeout-ms")?
                            .parse()
                            .map_err(|_| "--lock-timeout-ms must be a number".to_string())?;
                    }
                    flag if flag.starts_with('-') => {
                        return Err(format!("unrecognized argument: {flag}"));
                    }
                    _ => positional.push(&argv[index]),
                }
                index += 1;
            }
            let subcommand = positional
                .first()
                .map(|value| value.as_str())
                .ok_or("comment requires list or delete")?;
            // `comment list <wd>` | `comment delete <wd> <id>`.
            let delete = match subcommand {
                "list" => None,
                "delete" => Some(
                    positional
                        .get(2)
                        .map(|value| (*value).clone())
                        .ok_or("comment delete requires a comment id")?,
                ),
                _ => return Err(format!("unknown comment subcommand: {subcommand}")),
            };
            let source = match subcommand {
                "list" => positional
                    .get(1)
                    .map(|value| (*value).clone())
                    .ok_or("comment list requires a typed workdir")?,
                _ => positional
                    .get(1)
                    .map(|value| (*value).clone())
                    .ok_or("comment delete requires a typed workdir")?,
            };
            Ok(ParseResult {
                args: OperationArgs::Comment(CommentArgs {
                    workdir: PathBuf::from(source),
                    delete,
                    lock_timeout_ms,
                }),
                operation_id: operation_id.unwrap_or_else(docx2typed_protocol::new_operation_id),
            })
        }
        Operation::Audit => {
            let mut positional: Vec<&String> = Vec::new();
            let mut catalog: Option<String> = None;
            let mut index = 0usize;
            while index < argv.len() {
                match argv[index].as_str() {
                    "--catalog" => {
                        index += 1;
                        catalog = Some(
                            argv.get(index)
                                .ok_or("expected value for --catalog")?
                                .clone(),
                        );
                    }
                    flag if flag.starts_with('-') => {
                        return Err(format!("unrecognized argument: {flag}"));
                    }
                    _ => positional.push(&argv[index]),
                }
                index += 1;
            }
            let source: String = positional
                .first()
                .map(|value| (*value).clone())
                .ok_or("audit requires a .docx or typed workdir")?;
            if positional.len() > 1 {
                return Err("audit accepts exactly one source".to_string());
            }
            let catalog_path = catalog.map(PathBuf::from);
            Ok(ParseResult {
                args: OperationArgs::Audit(AuditArgs {
                    source: PathBuf::from(source),
                    catalog_path,
                }),
                operation_id: docx2typed_protocol::new_operation_id(),
            })
        }
        Operation::ProjectView => {
            let positional: Vec<&String> = argv.iter().filter(|a| !a.starts_with('-')).collect();
            let input: String = positional
                .first()
                .map(|value| (*value).clone())
                .ok_or("project-view requires a .docx")?;
            if positional.len() > 1 {
                return Err("project-view accepts exactly one source".to_string());
            }
            Ok(ParseResult {
                args: OperationArgs::ProjectView(ProjectViewArgs {
                    input: PathBuf::from(input),
                }),
                operation_id: docx2typed_protocol::new_operation_id(),
            })
        }
    }
}
