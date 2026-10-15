//! The agent surface: five tools, not fifty-three.
//!
//! The engine serves fifty-three MCP tools. That number is the problem, not the
//! individual tools. Measured against real use — one session, 94 calls, 16
//! errors — the failures were an agent guessing a calling convention it had no
//! way to know, and two engine gates disagreeing about one edit. Both are
//! symptoms of asking the caller to decide things the engine already knows.
//!
//! The published evidence says the count dominates: all-tools exposure reaches
//! 32.1% task success (ToolMenuBench, arXiv 2606.15508); a four-tool interface
//! is the measured optimum, +16.4% completion and argument accuracy 0.20 -> 0.40
//! over fine-grained primitives (MCP-GRANITE, arXiv 2609.24161); 89.1% of
//! tool-interface failures are tool MISSELECTION (ToolRobustBench,
//! arXiv 2608.23635). OpenAI, Google, and Anthropic all publish the same
//! direction: keep the exposed set small, merge tools that always run together.
//!
//! So: five tools are advertised. The other forty-eight stay implemented and
//! callable by name — the CLI, the test suites, and the qualification suite
//! depend on them — but they are not the agent's interface. Where a workflow
//! step has exactly one correct answer, the engine takes it. `commit_sync` after
//! an edit has one correct answer. Resolving a review preflight before a write
//! has one correct answer. Choosing between four edit transports has one correct
//! answer the engine already knows. None of those belong in an agent's context.
//!
//! See `docs/specs/08_agent_surface.md`.

mod edit;

pub(crate) use edit::apply_and_sync;
mod find;
mod open;
mod save;
mod status;

use crate::mcp::McpSession;
use serde_json::{json, Value};

/// The five tools an agent sees, in the order `tools/list` serves them.
///
/// Owned by the protocol crate because the descriptor and a session's
/// `supported_tools` must report the same list: one question, one answer.
pub const AGENT_TOOLS: [&str; 5] = docx2typed_protocol::AGENT_TOOLS;

pub fn is_agent_tool(name: &str) -> bool {
    AGENT_TOOLS.contains(&name)
}

/// The input schema for one of the five.
///
/// These are new tools, so they are not in the frozen schema bundle: that
/// bundle is a published, hashed contract and extending it would change a
/// digest other things check. `additionalProperties: false` everywhere, and a
/// single shape per argument, so a wrong key is refused naming the right one
/// rather than silently ignored.
pub fn schema(name: &str) -> Option<Value> {
    let value = match name {
        "document_open" => json!({
            "type": "object",
            "properties": {
                "source": {
                    "type": "string",
                    "description": "Path to a DOCX, or to an existing typed workdir. Both are accepted: the engine decides which one it was given, because the caller should not have to know.",
                },
                "author": {"type": ["string", "null"], "default": null},
                "date": {"type": ["string", "null"], "default": null},
                "track": {"type": ["boolean", "null"], "default": null},
            },
            "required": ["source"],
            "additionalProperties": false,
        }),
        "document_find" => json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "scope": {"type": ["string", "null"], "default": null},
                "limit": {"type": ["integer", "null"], "default": null},
            },
            "required": ["query"],
            "additionalProperties": false,
        }),
        "document_edit" => json!({
            "type": "object",
            "properties": {
                "edits": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "handle": {"type": "string"},
                            "replace_with": {"type": "string"},
                        },
                        "required": ["handle", "replace_with"],
                        "additionalProperties": false,
                    },
                },
                "dry_run": {"type": ["boolean", "null"], "default": null},
                "operation_id": {"type": ["string", "null"], "default": null},
            },
            "required": ["edits"],
            "additionalProperties": false,
        }),
        "document_save" => json!({
            "type": "object",
            "properties": {
                "output": {"type": "string"},
            },
            "required": ["output"],
            "additionalProperties": false,
        }),
        "document_status" => json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false,
        }),
        _ => return None,
    };
    Some(value)
}

/// One handler signature for all five.
pub fn dispatch(session: &mut McpSession, tool: &str, args: &Value, build_commit: &str) -> Value {
    match tool {
        "document_open" => open::document_open(session, args, build_commit),
        "document_find" => find::document_find(session, args, build_commit),
        "document_edit" => edit::document_edit(session, args, build_commit),
        "document_save" => save::document_save(session, args, build_commit),
        "document_status" => status::document_status(session, args, build_commit),
        other => session.failure(
            other,
            "invalid-arguments",
            &format!("{other} is not part of the agent surface"),
            build_commit,
        ),
    }
}
