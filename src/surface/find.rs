//! `document_find` — see `docs/specs/08_agent_surface.md`.
//!
//! Absorbs: document_search, list_paragraphs, get_paragraph, and THE editability
//! verdict. A search must never promise `patchable_as_single_hunk: true` for a
//! span the edit path refuses as `cross-leaf-edit`.
//!
//! NOT IMPLEMENTED YET. It refuses rather than pretending: a stub that returned
//! success would be the one failure this surface exists to remove.

use crate::mcp::McpSession;
use serde_json::Value;

pub fn document_find(session: &mut McpSession, _args: &Value, build_commit: &str) -> Value {
    session.failure(
        "document_find",
        "not-implemented",
        "document_find is not implemented yet; no change was made",
        build_commit,
    )
}
