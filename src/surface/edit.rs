//! `document_edit` — see `docs/specs/08_agent_surface.md`.
//!
//! Absorbs: the four edit transports, diff_preview, commit_sync, and revert on
//! failure. One transaction: preflight -> resolve -> apply -> sync -> verify,
//! rolling back on failure. `edits` has exactly one shape, so there is no second
//! key spelling to guess, and the agent is never asked to sync between edits.
//!
//! NOT IMPLEMENTED YET. It refuses rather than pretending: a stub that returned
//! success would be the one failure this surface exists to remove.

use crate::mcp::McpSession;
use serde_json::Value;

pub fn document_edit(session: &mut McpSession, _args: &Value, build_commit: &str) -> Value {
    session.failure(
        "document_edit",
        "not-implemented",
        "document_edit is not implemented yet; no change was made",
        build_commit,
    )
}
