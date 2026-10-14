//! `document_save` — see `docs/specs/08_agent_surface.md`.
//!
//! Absorbs: build_docx, verify_output, and the evidence publication. The build is
//! verified before it is reported: a save that reports success while the file
//! kept the old text is the failure this whole line of work exists to remove.
//!
//! NOT IMPLEMENTED YET. It refuses rather than pretending: a stub that returned
//! success would be the one failure this surface exists to remove.

use crate::mcp::McpSession;
use serde_json::Value;

pub fn document_save(session: &mut McpSession, _args: &Value, build_commit: &str) -> Value {
    session.failure(
        "document_save",
        "not-implemented",
        "document_save is not implemented yet; no change was made",
        build_commit,
    )
}
