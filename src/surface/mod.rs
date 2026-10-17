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

pub(crate) use edit::{apply_and_sync, Edit};
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
                // One connection drives one workdir. Leaving it to open another
                // is a decision the caller makes explicitly, and this is where
                // it makes it — the refusal it replaces costs nothing to take,
                // because a workdir keeps its state on disk.
                "release_current": {"type": ["boolean", "null"], "default": null},
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
                // The engine pages by block and its own `next.call` names this
                // parameter when a page is short, so a schema without it made the
                // tool refuse the call it had just told the caller to make.
                // Measured: an agent following that instruction got
                // `unknown argument \`offset\`` and, on a real patent, never
                // reached the 15 of 35 blocks the first page left out.
                "offset": {"type": ["integer", "null"], "default": null},
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
                            // A closed set, not free text: an invented kind is
                            // refused naming the allowed ones. `replace` is the
                            // default, so the common case needs no `kind` at all.
                            // `format` names a style the document already carries
                            // for the characters the handle covers; its own key is
                            // `style_id`, and the ids are the ones document_find
                            // reports for the span.
                            "kind": {
                                "type": "string",
                                "enum": ["replace", "insert_after", "delete", "format"],
                                "default": "replace",
                            },
                            // Every kind addresses its target the same way: a
                            // handle from document_find. One addressing scheme,
                            // because two is how a caller picks the wrong one.
                            "handle": {"type": "string"},
                            "replace_with": {"type": ["string", "null"], "default": null},
                            "text": {"type": ["string", "null"], "default": null},
                            // A selector names the targets by QUERY instead of a handle:
                            // {"text": "...", "match": "all"|"first", "scope": ...,
                            //  "expected_count": N}. The surface enumerates every match
                            // (the caller never transcribes handles); `expected_count`
                            // that disagrees, an incomplete search, or no match at all
                            // are refusals.
                            "selector": {"type": ["object", "null"], "default": null},
                            // kind `format`: the style the covered characters take.
                            // Name it (`style_id`, from document_find's verdict) or
                            // name properties (`format: {"bold": true}`) and let the
                            // engine resolve the style this document already carries.
                            "style_id": {"type": ["string", "null"], "default": null},
                            "format": {"type": ["object", "null"], "default": null},
                            // How much of the anchor's formatting the new
                            // paragraph takes. `safe_inherit` is the default and
                            // copies only what may be copied — no revision
                            // record, no section break, no unknown property.
                            // `none` writes the paragraph with no inherited
                            // properties at all.
                            "format_policy": {
                                "type": ["string", "null"],
                                "enum": ["safe_inherit", "none", null],
                                "default": null,
                            },
                        },
                        // A target is named EITHER by a handle or by a selector:
                        // the skill says a selector may replace a handle, and a
                        // schema that demanded both was a refusal an agent could
                        // only work around (it did: handle + selector together).
                        "anyOf": [
                            {"required": ["handle"]},
                            {"required": ["selector"]},
                        ],
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

#[cfg(test)]
mod schema_forwarding_tests {
    /// Every key the surface forwards must be declared, because an undeclared
    /// key is refused before the forward ever happens — and the engine's own
    /// `next.call` names `offset` when a search page is short. A schema without
    /// it made `document_find` refuse the very call it had just recommended:
    /// measured on a real patent, the agent never reached the 15 of 35 blocks
    /// the first page left out.
    #[test]
    fn every_forwarded_key_is_declared() {
        for (tool, forwarded) in [
            ("document_find", &["scope", "limit", "offset"][..]),
            (
                "document_open",
                &["author", "date", "track", "release_current"][..],
            ),
            ("document_edit", &["edits"][..]),
            ("document_save", &["output"][..]),
        ] {
            let declared = crate::mcp::declared_parameters(tool).expect("schema");
            for key in forwarded {
                assert!(
                    declared.contains(*key),
                    "{tool} forwards `{key}` but does not declare it: {declared:?}"
                );
            }
        }
    }

    /// The same rule one level down. An edit object is validated against its own
    /// schema, and that schema is closed — so a per-edit key the code reads but
    /// the schema omits is refused before the read, exactly like a top-level one.
    /// `format_policy` was taught to `document_edit` and not declared here, and
    /// the tool rejected the parameter it had just been given a way to honour.
    #[test]
    fn every_per_edit_key_is_declared() {
        let schema = crate::surface::schema("document_edit").expect("schema");
        let item = &schema["properties"]["edits"]["items"];
        let declared = item["properties"].as_object().expect("edit properties");
        for key in [
            "kind",
            "handle",
            "replace_with",
            "text",
            "format_policy",
            "style_id",
        ] {
            assert!(
                declared.contains_key(key),
                "document_edit reads `{key}` off an edit but does not declare it: {:?}",
                declared.keys().collect::<Vec<_>>()
            );
        }
        assert!(
            item["anyOf"].as_array().is_some_and(|rows| rows.len() == 2),
            "an edit must be addressable by handle OR selector: {:?}",
            item["anyOf"]
        );
        assert_eq!(
            item["additionalProperties"],
            serde_json::json!(false),
            "an open edit schema would let a mistyped key reach the engine as a silent default"
        );
    }
}
