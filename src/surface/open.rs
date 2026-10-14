//! `document_open` — see `docs/specs/08_agent_surface.md`.
//!
//! Absorbs: extract, identity minting, workdir_open, session resume and rebase, and preflight resolution.
//! `source` accepts either a DOCX or an existing workdir — the engine decides
//! which it was given, because a caller should not have to know.
//!
//! NOT IMPLEMENTED YET. It refuses rather than pretending: a stub that returned
//! success would be the one failure this surface exists to remove.

use crate::mcp::McpSession;
use serde_json::Value;

pub fn document_open(session: &mut McpSession, _args: &Value, build_commit: &str) -> Value {
    session.failure(
        "document_open",
        "not-implemented",
        "document_open is not implemented yet; no change was made",
        build_commit,
    )
}
