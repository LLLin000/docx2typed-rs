//! Installed `docx2typed` binary: concrete CLI and MCP(stdout) adapters
//! over the one synchronous Engine (issue #55 slice). Adapters parse and
//! validate transport DTOs, invoke the Engine, and serialize the common
//! `docx2typed-result-1` envelope; they never reassemble Engine order.

mod cli;
mod mcp;

use docx2typed_app::Operation;

fn main() {
    // Windows' default main-thread stack is 1 MiB; the extraction pipeline
    // (zip member reads + hashing) needs more headroom in debug builds, so
    // run the real entry point on a spawned thread with an explicit stack.
    let handle = std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(run_main)
        .expect("spawn entry thread");
    let exit_code = handle.join().unwrap_or(1);
    std::process::exit(exit_code);
}

fn run_main() -> i32 {
    arm_faults();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    // `--json` may appear anywhere in the invocation (Python semantics).
    let json_mode = argv.iter().any(|arg| arg == "--json");
    let argv: Vec<String> = argv.into_iter().filter(|arg| arg != "--json").collect();
    let build_commit = std::env::var("DOCX2TYPED_BUILD_COMMIT").unwrap_or_default();
    dispatch(&argv, json_mode, &build_commit)
}

fn dispatch(argv: &[String], json_mode: bool, build_commit: &str) -> i32 {
    if argv.is_empty() {
        if json_mode {
            cli::print_invocation_failure("", "no command given", argv);
        } else {
            eprintln!("usage: docx2typed <command> [--json] [options]");
        }
        return 1;
    }
    if argv == ["--version"] {
        let descriptor = docx2typed_protocol::engine_descriptor(build_commit);
        if json_mode {
            let mut value = serde_json::to_value(&descriptor).expect("descriptor serializes");
            // Issue #61: the self-contained binary reports the exact
            // embedded asset identities so consumers can pin the artifact.
            value["embedded_assets"] = docx2typed_app::embedded::table_value();
            println!(
                "{}",
                serde_json::to_string(&value).expect("descriptor serializes")
            );
        } else {
            println!(
                "{} {} ({})",
                descriptor.name, descriptor.version, descriptor.build_commit
            );
        }
        return 0;
    }
    match argv[0].as_str() {
        "extract" => cli::run(Operation::Extract, &argv[1..], json_mode, build_commit),
        "build" => cli::run(Operation::Build, &argv[1..], json_mode, build_commit),
        "verify" => cli::run(Operation::Verify, &argv[1..], json_mode, build_commit),
        "inspect" => cli::run(Operation::Inspect, &argv[1..], json_mode, build_commit),
        "migrate" => cli::run(Operation::Migrate, &argv[1..], json_mode, build_commit),
        "edit" => cli::run(Operation::Edit, &argv[1..], json_mode, build_commit),
        "store-state" => cli::run(Operation::StoreState, &argv[1..], json_mode, build_commit),
        "enumerate" => cli::run(Operation::Enumerate, &argv[1..], json_mode, build_commit),
        "revisions" => cli::run(Operation::Revisions, &argv[1..], json_mode, build_commit),
        "decide" => cli::run(Operation::Decide, &argv[1..], json_mode, build_commit),
        "comment" => cli::run(Operation::Comment, &argv[1..], json_mode, build_commit),
        "audit" => cli::run(Operation::Audit, &argv[1..], json_mode, build_commit),
        "project-view" => cli::run(Operation::ProjectView, &argv[1..], json_mode, build_commit),
        "mcp" => mcp::run(build_commit),
        "review" => review(build_commit),
        other => {
            if json_mode {
                cli::print_invocation_failure(
                    other,
                    &format!("no Protocol-major-1 --json contract for command: {other}"),
                    argv,
                );
            } else {
                eprintln!("Unknown command: {other}");
            }
            1
        }
    }
}

/// `docx2typed review <workdir> [--host H] [--port N]` — start the
/// single-session secured review HTTP server (issue #60). Blocks until the
/// process is interrupted.
fn review(build_commit: &str) -> i32 {
    let _ = build_commit;
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut workdir: Option<String> = None;
    let mut host = "127.0.0.1".to_string();
    let mut port: u16 = 8876;
    let mut index = 1;
    while index < argv.len() {
        match argv[index].as_str() {
            "--host" => {
                index += 1;
                if index < argv.len() {
                    host = argv[index].clone();
                }
            }
            "--port" => {
                index += 1;
                if index < argv.len() {
                    port = argv[index].parse().unwrap_or(8876);
                }
            }
            arg if workdir.is_none() && !arg.starts_with("--") => workdir = Some(arg.to_string()),
            _ => {}
        }
        index += 1;
    }
    let Some(workdir) = workdir else {
        eprintln!("usage: docx2typed review <workdir> [--host H] [--port N]");
        return 1;
    };
    match docx2typed_review::server::serve(std::path::Path::new(&workdir), &host, port) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("review server error: {error}");
            1
        }
    }
}

/// Deterministic fault-injection seam for the qualification gates and the
/// real-process-kill tests: `DOCX2TYPED_FAULT=kill:<cut>` etc. arm the
/// Store's cut points before any command runs.
fn arm_faults() {
    if let Ok(spec) = std::env::var("DOCX2TYPED_FAULT") {
        docx2typed_store::store::arm_faults_from_env(&spec);
    }
}
