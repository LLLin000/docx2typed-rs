//! Bake the commit into the binary, so a RELEASED artifact can say what it was
//! built from.
//!
//! `$DOCX2TYPED_BUILD_COMMIT` at runtime is an override — useful for CI and for
//! tests that want a fixed string — and it stays the highest priority. But it is
//! absent for every real user, which left `engine_info` reporting
//! `build_commit: "unknown"` and made a released binary untraceable to a
//! revision. That is the one field a bug report needs.
//!
//! A checkout that is not a git tree (a source tarball, a vendored build) gets
//! "unknown", which is honest rather than invented.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-env-changed=DOCX2TYPED_BUILD_COMMIT");

    // An explicit override at BUILD time wins over what git says.
    if let Ok(explicit) = std::env::var("DOCX2TYPED_BUILD_COMMIT") {
        if !explicit.is_empty() {
            println!("cargo:rustc-env=DOCX2TYPED_BUILD_COMMIT_COMPILED={explicit}");
            return;
        }
    }

    let commit = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    // A dirty tree is worth knowing about: the commit alone would not identify
    // these bytes.
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .is_some_and(|out| !out.stdout.is_empty());
    let commit = if dirty && commit != "unknown" {
        format!("{commit}-dirty")
    } else {
        commit
    };

    println!("cargo:rustc-env=DOCX2TYPED_BUILD_COMMIT_COMPILED={commit}");
}
