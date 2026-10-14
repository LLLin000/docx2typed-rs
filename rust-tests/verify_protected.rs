//! The four invariants a tracked edit must satisfy, and the tampers that prove
//! the verifier still enforces them.
//!
//! `corpus/release/complex.docx` P3 is the case that matters: a 20-run paragraph
//! carrying a bookmark, a comment range and a hyperlink, edited with a tracked
//! replace that crosses runs. The verifier used to reject it — it estimated byte
//! shifts from `xml_escape(new)`, which is wrong the moment a tracked edit wraps
//! the text in `w:del`/`w:ins` — and the fix must not be "reject less".
//!
//! So every case here is a REAL tamper of a real product, and each must be
//! refused by the check that owns that invariant. A tamper that is refused by a
//! generic hash comparison proves nothing about the structural checks.

use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_docx2typed")
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("corpus/release/{name}.docx"))
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("verify-protected-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// One MCP tool call over stdio, replaying the session first.
fn call(tool: &str, args: Value, session: &[Value]) -> Value {
    let mut child = Command::new(bin())
        .arg("mcp")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("the server starts");
    let last_id;
    {
        let mut stdin = child.stdin.take().expect("stdin");
        let mut id = 1;
        for earlier in session {
            writeln!(
                stdin,
                "{}",
                json!({
                    "jsonrpc": "2.0", "id": id, "method": "tools/call",
                    "params": { "name": earlier["name"], "arguments": earlier["arguments"] },
                })
            )
            .expect("write request");
            id += 1;
        }
        writeln!(
            stdin,
            "{}",
            json!({
                "jsonrpc": "2.0", "id": id, "method": "tools/call",
                "params": { "name": tool, "arguments": args },
            })
        )
        .expect("write request");
        last_id = id;
    }
    let output = child.wait_with_output().expect("the server answers");
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Ok(reply) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if reply["id"] == json!(last_id) {
            return reply["result"]["structuredContent"].clone();
        }
    }
    panic!("no reply for {tool}");
}

fn extract(source: &Path, workdir: &Path) {
    let out = Command::new(bin())
        .args([
            "extract",
            source.to_str().expect("utf8"),
            "-o",
            workdir.to_str().expect("utf8"),
            "--json",
        ])
        .output()
        .expect("extract runs");
    assert!(out.status.success(), "extract failed: {out:?}");
}

/// Rewrite one member of a package, leaving every other byte alone.
fn rewrite_document(package: &Path, target: &Path, change: impl Fn(&str) -> String) {
    let mut archive = zip::ZipArchive::new(Cursor::new(std::fs::read(package).expect("readable")))
        .expect("a zip");
    let mut members = Vec::new();
    for index in 0..archive.len() {
        let mut member = archive.by_index(index).expect("member");
        let name = member.name().to_string();
        let mut bytes = Vec::new();
        member.read_to_end(&mut bytes).expect("read member");
        members.push((name, bytes));
    }
    let mut out = zip::ZipWriter::new(std::fs::File::create(target).expect("create"));
    let options: zip::write::FileOptions<()> = zip::write::FileOptions::default();
    for (name, bytes) in members {
        out.start_file(name.clone(), options).expect("start");
        if name == "word/document.xml" {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            out.write_all(change(&text).as_bytes()).expect("write");
        } else {
            out.write_all(&bytes).expect("write");
        }
    }
    out.finish().expect("finish");
}

/// The verification message, which now names the failing check AND its reason.
fn verify(workdir: &Path, output: &Path) -> (String, String) {
    let result = Command::new(bin())
        .args([
            "verify",
            workdir.to_str().expect("utf8"),
            output.to_str().expect("utf8"),
            "--json",
        ])
        .output()
        .expect("verify runs");
    let text = String::from_utf8_lossy(&result.stdout).into_owned();
    let envelope: Value = serde_json::from_str(text.trim()).expect("verify json");
    let outcome = envelope["outcome"].as_str().unwrap_or("").to_string();
    let message = envelope["diagnostics"]
        .as_array()
        .and_then(|list| list.first())
        .and_then(|entry| entry["message"].as_str())
        .unwrap_or("")
        .to_string();
    (outcome, message)
}

/// Build the tracked cross-run edit on the protected paragraph, and return
/// `(workdir, output)`.
fn build_the_protected_edit(dir: &Path) -> (PathBuf, PathBuf) {
    let source = dir.join("source.docx");
    std::fs::copy(fixture("complex"), &source).expect("copy fixture");
    let workdir = dir.join("wd");
    extract(&source, &workdir);
    let output = dir.join("out.docx");

    let mut session = vec![json!({
        "name": "workdir_open",
        "arguments": { "workdir": workdir.to_str().expect("utf8"), "track": true, "author": "verify" },
    })];
    let hunks = json!([{ "paragraph_id": "P3", "old": "bold italic underline", "new": "BOLD ITALIC UNDERLINE" }]);
    let patch = call(
        "document_patch",
        json!({ "operation_id": "op_prot_patch", "hunks": hunks }),
        &session,
    );
    assert_eq!(patch["outcome"], "success", "the edit: {patch}");
    session.push(json!({
        "name": "document_patch",
        "arguments": { "operation_id": "op_prot_patch", "hunks": hunks },
    }));
    assert_eq!(
        call(
            "commit_sync",
            json!({ "operation_id": "op_prot_commit" }),
            &session
        )["outcome"],
        "success"
    );
    let build = call(
        "build_docx",
        json!({ "operation_id": "op_prot_build", "output": output.to_str().expect("utf8") }),
        &session,
    );
    assert_eq!(build["outcome"], "success", "the build: {build}");
    (workdir, output)
}

#[test]
fn a_tracked_edit_beside_protected_structure_verifies() {
    let dir = scratch("positive");
    let (workdir, output) = build_the_protected_edit(&dir);
    let (outcome, message) = verify(&workdir, &output);
    assert_eq!(
        outcome, "success",
        "a legal edit beside a bookmark, a comment range and a hyperlink must \
         verify — refusing it is the defect this replaced: {message}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Every tamper, and the check that owns the invariant it breaks.
const TAMPERS: &[(&str, &str)] = &[
    // Protected Structure: the anchor's identity.
    ("bookmark", "protected-structure-preserved"),
    ("comment-range", "protected-structure-preserved"),
    ("hyperlink-target", "protected-structure-preserved"),
    // Non-target Preservation: three paragraphs away from the edit.
    ("untouched-run-properties", "non-target-preserved"),
    // Revision Integrity: the revision's own identity and text.
    ("revision-author", "edited-part-exact"),
    ("revision-date", "edited-part-exact"),
    ("deleted-text", "edited-part-exact"),
];

#[test]
fn every_structural_tamper_is_refused_by_the_check_that_owns_it() {
    let dir = scratch("tampers");
    let (workdir, output) = build_the_protected_edit(&dir);
    let (baseline, _) = verify(&workdir, &output);
    assert_eq!(baseline, "success", "the baseline must verify first");

    for (name, owner) in TAMPERS {
        let target = dir.join(format!("tamper-{name}.docx"));
        rewrite_document(&output, &target, |xml| match *name {
            "bookmark" => xml.replacen("w:bookmarkStart w:id=\"", "w:bookmarkStart w:id=\"9", 1),
            "comment-range" => xml.replacen(
                "w:commentRangeStart w:id=\"",
                "w:commentRangeStart w:id=\"9",
                1,
            ),
            "hyperlink-target" => {
                // The first hyperlink's relationship target.
                match xml.find("<w:hyperlink") {
                    Some(at) => match xml[at..].find("r:id=\"") {
                        Some(rel) => {
                            let from = at + rel + "r:id=\"".len();
                            let mut patched = xml.to_string();
                            patched.replace_range(from..from + 1, "9");
                            patched
                        }
                        None => xml.to_string(),
                    },
                    None => xml.to_string(),
                }
            }
            "untouched-run-properties" => {
                // Scoped by paragraph: a document-wide replace would hit the
                // edited paragraph first, and then this case would be testing
                // the target instead of the untouched zone.
                let Some(at) = xml.find(">Element<") else {
                    return xml.to_string();
                };
                let start = xml[..at]
                    .rfind("<w:p ")
                    .into_iter()
                    .chain(xml[..at].rfind("<w:p>"))
                    .max()
                    .unwrap_or(0);
                let end = xml[at..]
                    .find("</w:p>")
                    .map(|o| at + o)
                    .unwrap_or(xml.len());
                let mut patched = xml.to_string();
                if let Some(rpr) = patched[start..end].find("<w:rPr>") {
                    patched.insert_str(start + rpr + "<w:rPr>".len(), "<w:u w:val=\"double\"/>");
                }
                patched
            }
            "revision-author" => xml.replacen("w:author=\"verify\"", "w:author=\"someone\"", 1),
            "revision-date" => match xml.find("w:date=\"") {
                Some(at) => {
                    let from = at + "w:date=\"".len();
                    let mut patched = xml.to_string();
                    patched.insert_str(from, "1999-01-01");
                    patched
                }
                None => xml.to_string(),
            },
            "deleted-text" => match xml.find("<w:delText") {
                Some(at) => match xml[at..].find('>') {
                    Some(gt) => {
                        let from = at + gt + 1;
                        let mut patched = xml.to_string();
                        patched.insert_str(from, "XX");
                        patched
                    }
                    None => xml.to_string(),
                },
                None => xml.to_string(),
            },
            other => panic!("no tamper named {other}"),
        });
        assert_ne!(
            std::fs::read(&target).expect("tampered"),
            std::fs::read(&output).expect("baseline"),
            "{name}: the tamper changed nothing, so it proves nothing"
        );

        let (outcome, message) = verify(&workdir, &target);
        assert_eq!(outcome, "failure", "{name} must be refused: {message}");
        assert!(
            message.contains(owner),
            "{name} must be refused by {owner}, not by a generic check: {message}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
