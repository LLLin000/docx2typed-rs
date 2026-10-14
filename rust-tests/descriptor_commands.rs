//! Every command `engine_info` advertises must be one the binary accepts.
//!
//! `commands.finite` is what a consumer reads to decide what it may run, so it
//! has to describe the binary answering. It did not: it mirrored the Python
//! reference's frozen list, which includes `validate` — a command the Rust
//! dispatcher has never had — while omitting six the dispatcher accepts
//! (`store-state`, `enumerate`, `revisions`, `comment`, `audit`,
//! `project-view`).
//!
//! This drives the real binary for every advertised command. A command that
//! exits `Unknown command` fails the test, so the advertised set and the
//! dispatcher cannot drift apart again — whichever of the two is changed.

use std::path::Path;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_docx2typed")
}

/// The command list as the binary itself publishes it.
fn advertised_commands() -> Vec<String> {
    let out = Command::new(bin())
        .args(["--version", "--json"])
        .output()
        .expect("the binary runs");
    assert!(out.status.success(), "--version --json must succeed");
    let blob: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("--version --json is JSON");
    blob["commands"]["finite"]
        .as_array()
        .expect("commands.finite is an array")
        .iter()
        .map(|v| v.as_str().expect("a command name").to_string())
        .collect()
}

#[test]
fn every_advertised_command_is_dispatched() {
    let commands = advertised_commands();
    assert!(
        !commands.is_empty(),
        "commands.finite is empty, so the descriptor says nothing"
    );

    let mut unknown = Vec::new();
    for command in &commands {
        // No arguments: a real command fails with its own usage message, an
        // unknown one says exactly "Unknown command".
        let out = Command::new(bin())
            .arg(command)
            .output()
            .unwrap_or_else(|e| panic!("could not run {command}: {e}"));
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        if text.contains("Unknown command") {
            unknown.push(command.clone());
        }
    }
    assert!(
        unknown.is_empty(),
        "engine_info advertises commands the binary rejects: {unknown:?}. \
         Either implement them or take them out of PROTOCOL_COMMANDS — a \
         consumer acts on this list."
    );
}

#[test]
fn a_command_that_does_not_exist_is_rejected() {
    // The other direction: the dispatcher must not accept names the descriptor
    // never published. `validate` is the one that used to be advertised.
    for absent in ["validate", "definitely-not-a-command"] {
        let out = Command::new(bin())
            .arg(absent)
            .output()
            .expect("the binary runs");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            text.contains("Unknown command"),
            "the dispatcher accepted {absent:?}, which the descriptor does not publish"
        );
    }
}

#[test]
fn the_descriptor_names_the_rust_engine() {
    // A consumer picks an engine by these two fields.
    let out = Command::new(bin())
        .args(["--version", "--json"])
        .output()
        .expect("the binary runs");
    let blob: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON");
    assert_eq!(blob["name"], "docx2typed-rust");
    assert!(
        blob["target"].as_str().is_some_and(|t| t.contains("rust")),
        "target must name the rust build, got {:?}",
        blob["target"]
    );
    let _ = Path::new(bin());
}

/// A released binary must be able to say what it was built from.
///
/// `build_commit` came only from a runtime env var, which no real user sets, so
/// every released binary reported "unknown" — the one field a bug report needs.
/// The build now bakes the commit in, and the runtime variable stays as an
/// override for CI and for tests that want a fixed string.
#[test]
fn the_binary_knows_its_build_commit() {
    let read = |env: Option<&str>| -> String {
        let mut cmd = Command::new(bin());
        cmd.args(["--version", "--json"]);
        match env {
            Some(value) => cmd.env("DOCX2TYPED_BUILD_COMMIT", value),
            None => cmd.env_remove("DOCX2TYPED_BUILD_COMMIT"),
        };
        let out = cmd.output().expect("the binary runs");
        let blob: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON");
        blob["build_commit"]
            .as_str()
            .expect("build_commit is a string")
            .to_string()
    };

    let compiled = read(None);
    assert_ne!(
        compiled, "unknown",
        "a binary built from a git checkout must name its commit; only a source \
         tarball or a vendored build may say unknown"
    );
    assert!(
        compiled.len() >= 7,
        "build_commit looks too short to identify a revision: {compiled:?}"
    );

    // The runtime override wins, so CI and tests can pin a value.
    assert_eq!(read(Some("deadbeef")), "deadbeef");

    // An EMPTY override is not an override: it falls through to the compiled
    // value rather than blanking the field.
    assert_eq!(
        read(Some("")),
        compiled,
        "an empty override must not erase the compiled commit"
    );
}

/// `--help` is the first thing anyone tries on an unfamiliar CLI.
///
/// It used to fall through to the unknown-command arm: `Unknown command:
/// --help`, exit 1, no command list. An agent that probes a CLI that way reads
/// exit 1 as "this tool is broken" and goes looking for the answer in the
/// source instead — which is exactly what happened in a real session, and it
/// spent six probes getting a usage string the binary already had.
#[test]
fn help_prints_the_command_list_and_succeeds() {
    for invocation in [vec!["--help"], vec!["-h"], vec!["help"]] {
        let out = Command::new(bin())
            .args(&invocation)
            .output()
            .expect("the binary runs");
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            out.status.success(),
            "{invocation:?} must succeed, got {:?}",
            out.status.code()
        );
        assert!(
            !text.is_empty(),
            "{invocation:?} writes usage to stdout, not stderr"
        );
        // Every dispatched command is named, so the list cannot go stale
        // against the dispatcher.
        for command in advertised_commands() {
            assert!(
                text.contains(&command),
                "{invocation:?} does not mention {command:?}:\n{text}"
            );
        }
    }
}

/// A command that does not exist still fails, and now says where to look.
#[test]
fn an_unknown_command_points_at_help() {
    let out = Command::new(bin())
        .arg("definitely-not-a-command")
        .output()
        .expect("the binary runs");
    assert!(!out.status.success(), "an unknown command must fail");
    let text = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(text.contains("Unknown command"), "got: {text}");
    assert!(
        text.contains("--help"),
        "the refusal must name the way out: {text}"
    );
}
