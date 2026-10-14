//! Tool-level recovery mapping (port of `scripts/mcp_server.py::_recovery_for`).
//!
//! One source: every failure payload's `data["recovery"]` and diagnostic
//! `next_actions` derive from this table. `tools_list_payload` (mcp.rs)
//! must NOT duplicate this mapping.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Recovery {
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub choices: Option<Vec<String>>,
}

impl Recovery {
    /// Tool names the agent should call next (reference `next_actions` base).
    pub fn tool_list(&self) -> Vec<String> {
        if let Some(tools) = &self.tools {
            return tools.clone();
        }
        self.tool.clone().into_iter().collect()
    }

    pub fn next_actions(&self) -> Vec<String> {
        self.tool_list()
            .into_iter()
            .map(|t| format!("call {}", t))
            .collect()
    }
}

fn r(action: &str, tools: &[&str]) -> Recovery {
    Recovery {
        action: action.to_string(),
        message: None,
        tools: Some(tools.iter().map(|s| s.to_string()).collect()),
        tool: None,
        choices: None,
    }
}

fn rm(action: &str, message: &str, tools: &[&str]) -> Recovery {
    Recovery {
        action: action.to_string(),
        message: Some(message.to_string()),
        tools: Some(tools.iter().map(|s| s.to_string()).collect()),
        tool: None,
        choices: None,
    }
}

fn rt(action: &str, tool: &str, choices: &[&str]) -> Recovery {
    Recovery {
        action: action.to_string(),
        message: None,
        tools: None,
        tool: Some(tool.to_string()),
        choices: Some(choices.iter().map(|s| s.to_string()).collect()),
    }
}

/// Port of `_recovery_for(operation, code)`. First matching branch wins
/// (Python `if/return` semantics).
pub fn recovery_for(operation: &str, code: &str) -> Recovery {
    if code == "operation-id-reused" {
        // First occurrence in the reference; unreachable second branch
        // (`new-operation-id`, tools:[operation]) is never reached in Python
        // because this one returns first.
        return rm(
            "retry-with-fresh-operation-id",
            "pass a NEW unique operation_id (or omit it); never reuse one from an earlier call, success or failure",
            &[],
        );
    }
    if code == "workdir-not-open" {
        return r("open-workdir", &["workdir_open"]);
    }
    if code == "agent-preflight-required" {
        return r(
            "resolve-review",
            &[
                "review_preflight",
                "review_inbox",
                "review_apply_patch",
                "review_apply_batch",
                "review_settlement_plan",
                "review_settle",
            ],
        );
    }
    if code == "edit-mode-ambiguous" {
        return rt(
            "choose-edit-mode",
            "workdir_open",
            &["track=true", "track=false"],
        );
    }
    if code == "generation-conflict"
        || code == "current-parent-mismatch"
        || code == "current-snapshot-drift"
    {
        return r("refresh-state", &["workdir_status", "review_state"]);
    }
    if code == "edit-dirty" || code == "edit-stale" || code == "edit-conflict" {
        return r("reconcile-edit", &["diff_preview", "commit_sync", "revert"]);
    }
    if code == "stale-document-view" || code == "patch-context-mismatch" || code == "patch-empty" {
        return r("re-read-document", &["document_read", "document_search"]);
    }
    if code == "patch-invalid"
        || code == "patch-structure-immutable"
        || code == "patch-block-inserted"
        || code == "patch-block-deleted"
        || code == "document-patch-hunks-overlap"
        || code == "document-patch-paragraph-repeated"
        || code == "invalid-arguments"
    {
        return r("reformulate-patch", &["document_read", "document_patch"]);
    }
    if code == "source-modified-outside-engine" {
        return r("re-extract-from-trusted-source", &["workdir_open"]);
    }
    if code == "comment-text-requires-opt-in" {
        return r(
            "confirm-comment-edit-intent",
            &["document_read", "delete_comment"],
        );
    }
    if code == "format-style-unavailable" {
        return r("reuse-existing-variant", &["document_read", "format_span"]);
    }
    if code == "format-noop" {
        return r("none-required", &[]);
    }
    if code == "text-inside-tracked-deletion" {
        return r("ask-the-user-about-the-revision", &["document_read"]);
    }
    if code == "patch-hunks-invalid" {
        // First reachable branch; unreachable second (`fix-listed-hunks`) dropped.
        return r("apply-listed-fixes", &["document_patch"]);
    }
    if code == "ambiguous-alignment" {
        return r(
            "disambiguate-insert-anchor",
            &["get_paragraph", "batch_edit"],
        );
    }
    if code == "edit-span-crosses-revision-boundary" {
        return r(
            "replan-within-revision-regions",
            &["document_read", "document_search"],
        );
    }
    if code == "edit-span-crosses-protected-marker" {
        return r(
            "replan-within-one-span",
            &["document_read", "document_search"],
        );
    }
    if code == "placeholder-in-edit-span" {
        return r(
            "choose-editable-span",
            &["document_read", "document_search"],
        );
    }
    if code == "text-not-found" || code == "text-ambiguous" {
        return r("re-read-document", &["document_search", "document_read"]);
    }
    if code == "cross-region-text" {
        return rm(
            "use-document_patch",
            "replace_text needs a single style region; document_patch accepts \
             cross-region spans and assigns style ownership itself — do not split \
             the edit at style boundaries",
            &["document_patch"],
        );
    }
    if code == "paragraph-not-found"
        || code == "draft-invalid"
        || code == "invalid-edit"
        || code == "region-out-of-range"
    {
        return r("refresh-edit", &["document_read", "diff_preview"]);
    }
    // second occurrence dropped (unreachable in Python)
    if code == "evidence-publish-failed" {
        return Recovery {
            action: "retry-same-operation".to_string(),
            message: None,
            tools: Some(vec![operation.to_string()]),
            tool: None,
            choices: None,
        };
    }
    if code == "writer-busy" || code == "writer-timeout" {
        return Recovery {
            action: "retry-same-operation".to_string(),
            message: None,
            tools: Some(vec![operation.to_string()]),
            tool: None,
            choices: None,
        };
    }
    if code == "output-docx-not-found" || code == "workdir-missing" {
        return r("build-output", &["build_docx"]);
    }
    // fallback
    r("inspect-diagnostic", &["workdir_status", "review_state"])
}

/// Port of `_profile_recovery`.
pub fn profile_recovery(entry: Recovery, active: Option<&[String]>) -> Recovery {
    let tools = match entry.tools.clone() {
        Some(t) => t,
        None => return entry,
    };
    let active = match active {
        Some(a) => a,
        None => return entry,
    };
    if tools.is_empty() && entry.tool.is_none() {
        // still filter empty list; keep same
    }
    let kept: Vec<String> = tools
        .into_iter()
        .filter(|n| active.iter().any(|a| a == n))
        .collect();
    if kept == entry.tools.clone().unwrap_or_default() {
        return entry;
    }
    let mut entry = entry;
    entry.tools = Some(kept.clone());
    if !kept.is_empty() {
        entry.message = Some(
            "in this tool profile only the listed tools are available; \
             run them in order, then retry the call that failed"
                .to_string(),
        );
    }
    entry
}

pub fn filtered_recovery_for(operation: &str, code: &str, active: Option<&[String]>) -> Recovery {
    let recovery = profile_recovery(recovery_for(operation, code), active);
    // Translate the engine's vocabulary into the agent's before it is sent.
    // `recovery_for` keeps the engine names — the CLI and the tests read it —
    // but a payload that names `workdir_open` tells an agent to call a tool that
    // is not mounted on its surface.
    let engine_tools = recovery.tool_list();
    if engine_tools.is_empty() {
        return recovery;
    }
    let mut translated = recovery.clone();
    let surface = agent_surface_tools(&engine_tools);
    if surface != engine_tools {
        translated.message = Some(match recovery.message {
            Some(message) => format!("{message} (engine steps: {})", engine_tools.join(", ")),
            None => format!("engine steps: {}", engine_tools.join(", ")),
        });
    }
    translated.tools = Some(surface);
    translated.tool = recovery.tool.as_deref().map(|tool| to_agent_surface(tool).to_string());
    translated
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::PROTOCOL_TOOLS;

    #[test]
    fn representative_failures() {
        let r = recovery_for("document_patch", "agent-preflight-required");
        assert_eq!(r.action, "resolve-review");
        assert!(r
            .tools
            .as_ref()
            .unwrap()
            .contains(&"review_preflight".to_string()));

        let r = recovery_for("workdir_open", "workdir-not-open");
        assert_eq!(r.action, "open-workdir");
        assert_eq!(r.tools, Some(vec!["workdir_open".to_string()]));

        let r = recovery_for("document_patch", "workdir-invalid");
        // unknown code -> default tail
        assert_eq!(r.action, "inspect-diagnostic");

        let r = recovery_for("document_patch", "patch-hunks-invalid");
        assert_eq!(r.action, "apply-listed-fixes");
        assert_eq!(r.tools, Some(vec!["document_patch".to_string()]));
    }

    #[test]
    fn unknown_code_gets_default() {
        let r = recovery_for("document_patch", "no-such-code");
        assert_eq!(r.action, "inspect-diagnostic");
        assert_eq!(
            r.tools,
            Some(vec![
                "workdir_status".to_string(),
                "review_state".to_string()
            ])
        );
    }

    #[test]
    fn all_table_tool_names_are_protocol_tools() {
        let codes = [
            "operation-id-reused",
            "workdir-not-open",
            "agent-preflight-required",
            "edit-mode-ambiguous",
            "generation-conflict",
            "edit-dirty",
            "stale-document-view",
            "patch-invalid",
            "source-modified-outside-engine",
            "comment-text-requires-opt-in",
            "format-style-unavailable",
            "format-noop",
            "text-inside-tracked-deletion",
            "patch-hunks-invalid",
            "ambiguous-alignment",
            "edit-span-crosses-revision-boundary",
            "edit-span-crosses-protected-marker",
            "placeholder-in-edit-span",
            "text-not-found",
            "cross-region-text",
            "paragraph-not-found",
            "evidence-publish-failed",
            "writer-busy",
            "output-docx-not-found",
            "bogus",
        ];
        for code in codes {
            for tool in recovery_for("document_patch", code).tool_list() {
                assert!(
                    PROTOCOL_TOOLS.contains(&tool.as_str()),
                    "unknown tool {} for code {}",
                    tool,
                    code
                );
            }
        }
    }
}

/// The agent-surface tool that carries out what a legacy tool did.
///
/// The recovery table is written in the engine's vocabulary — it predates the
/// five-tool surface and is still what the CLI and the tests read. An agent,
/// though, can only call the five, so naming `workdir_open` in a recovery
/// payload tells it to call a tool that is not mounted. Measured: every refusal
/// carried a `tools` list of names the agent could not use.
///
/// Where a legacy tool has no surface equivalent — `format_span`, the review
/// console, `revert`, the table operations — the honest answer is
/// `document_status`: it reports the state and names the next step, and it is
/// the one call that is always available. The legacy name stays in the payload's
/// `detail` field for a human or the CLI, rather than being dropped.
pub fn to_agent_surface(tool: &str) -> &'static str {
    match tool {
        "workdir_open" => "document_open",
        "workdir_status" => "document_status",
        "document_read" | "document_search" | "get_paragraph" => "document_find",
        "document_patch" | "batch_edit" | "diff_preview" => "document_edit",
        // The sync is absorbed: `document_edit` takes it, so a recovery that
        // told the caller to run it would be naming a step that no longer
        // exists on the surface.
        "commit_sync" => "document_edit",
        "build_docx" => "document_save",
        _ => "document_status",
    }
}

/// The same list, in the agent's vocabulary, with duplicates removed.
///
/// Order is preserved: the reference lists its recovery tools in the order they
/// should be tried, and that ordering is the useful part.
pub fn agent_surface_tools(tools: &[String]) -> Vec<String> {
    let mut mapped: Vec<String> = Vec::new();
    for tool in tools {
        let surface = to_agent_surface(tool).to_string();
        if !mapped.contains(&surface) {
            mapped.push(surface);
        }
    }
    mapped
}
