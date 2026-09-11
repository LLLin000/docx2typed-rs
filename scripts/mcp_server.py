"""docx2typed-mcp — MCP server exposing the typed-workdir engine as span-free tools.

The agent-facing surface is visible plain text plus a per-paragraph style
region map. Style ownership is decided by the engine with zero guessing:

- unchanged characters keep their exact style;
- rewritten text inherits the style of the baseline text it replaces when
  that range covers a single style region (region-exact);
- a cross-region rewrite is styled by the explicit ``proportional-preserve``
  policy (boundary-proportional assignment, recorded + warned); protected
  boundaries never move (``protected-boundary-crossing``);
- insertions follow the caret context (left neighbor, paragraph-start right
  neighbor, ``insertion_style`` for empty paragraphs).

Workflow:

    workdir_open -> get_paragraph (style regions are shown by default)
    -> replace_text / insert_paragraph / delete_paragraph (write the draft)
    -> diff_preview (per-hunk style ownership, read-only)
    -> commit_sync (apply the draft, publish canonical state)
    -> build_docx -> verify_output

``revert`` discards the uncommitted draft.

Run as stdio MCP server:

    python -m docx2typed.mcp_server
"""
from __future__ import annotations

import difflib
import json
import os
import shutil
import sys
import tempfile
from difflib import SequenceMatcher
import re
import threading
import zipfile
from pathlib import Path
from typing import Any, Callable, Iterable

try:
    from .edit import (
        PROJECTION_FILE,
        classify_edit_state,
        refresh_edit_projection,
        sync_edit_projection,
        atomic_write_text,
    )
    from .edit_sync import (
        _validate_escaped_prose,
        flatten_paragraph,
        plan_sync,
        render_regions_md,
    )
    from .typed_core import (
        InlineNode,
        OpaqueNode,
        RangeNode,
        RevisionNode,
        Style,
        StyleRegistry,
        TextNode,
        TypedError,
        parse_typed,
    )
    from .typed_docx import (
        ValidationError,
        build_workdir,
        _build_workdir_to_staging,
        validate_output_path,
        validate_workdir,
        verify_workdir,
    )
    from .review_collab import (
        CollaborationError,
        document_state,
        document_state_readonly,
        external_write_guard,
        preflight,
        publish_current,
        settle_decisions,
        settlement_plan,
    )
    from .review_queue import (
        acknowledge as acknowledge_review,
        snapshot as review_snapshot,
        snapshot_readonly as review_snapshot_readonly,
        update_event as update_review_event,
    )
    from .protocol import (
        ProtocolMismatch,
        base_evidence_payload,
        canonical_operation_input,
        derived_workdir_manifest,
        diagnostic,
        domain_code_from_message,
        domain_diagnostic,
        engine_descriptor,
        file_sha256,
        mcp_result,
        negotiate,
        new_operation_id,
        operation_ledger,
        operation_ledger_path,
        publish_run_evidence,
        result_envelope,
        run_evidence,
        schema_bundle,
        semantic_sha256,
        typed_path,
    )
    from .store import (
        CANONICAL_ASSETS,
        Store,
        StoreError,
        canonical_tree_digest,
        find_version,
        has_store,
        head_version as store_head_version,
        history_gc as store_history_gc,
        history_list as store_history_list,
        trimmed_versions,
        history_verify as store_history_verify,
        read_root,
        store_dir_path,
    )
except ImportError:  # direct script execution has no package context.
    # Running ``python scripts/mcp_server.py`` directly must work for debugging:
    # put this directory on sys.path so the flat sibling modules import.
    import sys as _sys

    _here = str(Path(__file__).resolve().parent)
    if _here not in _sys.path:
        _sys.path.insert(0, _here)
    from edit import (
        PROJECTION_FILE,
        classify_edit_state,
        refresh_edit_projection,
        sync_edit_projection,
        atomic_write_text,
    )
    from edit_sync import (
        _validate_escaped_prose,
        flatten_paragraph,
        plan_sync,
        render_regions_md,
    )
    from typed_core import (
        InlineNode,
        OpaqueNode,
        RangeNode,
        RevisionNode,
        Style,
        StyleRegistry,
        TextNode,
        TypedError,
        parse_typed,
    )
    from typed_docx import (
        ValidationError,
        build_workdir,
        _build_workdir_to_staging,
        validate_output_path,
        validate_workdir,
        verify_workdir,
    )
    from review_collab import (  # type: ignore[no-redef]
        CollaborationError,
        document_state,
        document_state_readonly,
        external_write_guard,
        preflight,
        publish_current,
        settle_decisions,
        settlement_plan,
    )
    from review_queue import (  # type: ignore[no-redef]
        acknowledge as acknowledge_review,
        snapshot as review_snapshot,
        snapshot_readonly as review_snapshot_readonly,
        update_event as update_review_event,
    )
    from protocol import (
        ProtocolMismatch,
        base_evidence_payload,
        canonical_operation_input,
        derived_workdir_manifest,
        diagnostic,
        domain_code_from_message,
        domain_diagnostic,
        engine_descriptor,
        file_sha256,
        mcp_result,
        negotiate,
        new_operation_id,
        operation_ledger,
        operation_ledger_path,
        publish_run_evidence,
        result_envelope,
        run_evidence,
        schema_bundle,
        semantic_sha256,
        typed_path,
    )
    from store import Store, StoreError, has_store, read_root  # type: ignore[no-redef]
from mcp.server.fastmcp import FastMCP
from mcp.types import CallToolResult


class ToolError(TypedError):
    """Structured tool failure: ``code: message``; code is a stable diagnostic.

    ``details`` travels into the Result diagnostic payload so a refusal can
    carry the machinery the caller needs to recover (e.g. the paragraph span
    map behind a text-not-found / boundary refusal)."""

    def __init__(self, code: str, message: str, details: dict[str, Any] | None = None) -> None:
        super().__init__(f"{code}: {message}")
        self.code = code
        self.detail = message
        self.details = details


# --------------------------------------------------------------------------
# Session
# --------------------------------------------------------------------------

def _last_workdir_hint() -> str | None:
    """Most recently opened workdir, so a session that lost its server process
    can re-open in one call instead of searching the filesystem."""
    try:
        record = json.loads((Path.home() / ".docx2typed" / "last-workdir.json").read_text(encoding="utf-8"))
        path = str(record.get("workdir", ""))
        return path if path and Path(path).is_dir() else None
    except (OSError, json.JSONDecodeError, TypeError):
        return None


def _remember_workdir(workdir: Path) -> None:
    try:
        target = Path.home() / ".docx2typed"
        target.mkdir(parents=True, exist_ok=True)
        (target / "last-workdir.json").write_text(
            json.dumps({"workdir": str(workdir)}, ensure_ascii=False) + "\n",
            encoding="utf-8",
            newline="\n",
        )
    except OSError:
        pass


class WorkdirSession:
    def __init__(self) -> None:
        self.workdir: Path | None = None
        self.lock = threading.RLock()
        self.author: str | None = None
        self.track_override: bool | None = None
        self.mode: str | None = None
        self.last_build_output: Path | None = None

    def require(self) -> Path:
        if self.workdir is None:
            hint = _last_workdir_hint()
            message = "no workdir open; call workdir_open first"
            if hint:
                message += f" (last opened in an earlier session: {hint})"
            raise ToolError("workdir-not-open", message)
        return self.workdir


def _agent_preflight(
    workdir: Path, paragraph_ids: Iterable[str] | None = None
) -> dict[str, Any]:
    scope = (
        list(dict.fromkeys(str(item) for item in paragraph_ids))
        if paragraph_ids is not None
        else None
    )
    # readonly: a refused mutation must leave zero side effects (no
    # .review/inbox/ creation) — the gate reads the same state either way,
    # and workdir_open has already created the collaboration session.
    result = preflight(workdir, paragraph_ids=scope, readonly=True)
    if not result["ready"]:
        detail: dict[str, Any] = {
            "reasons": result["reasons"],
            "queued_events": result["queued_events"],
            "blocked_patches": result["blocked_patches"],
        }
        if scope is not None:
            detail["scope"] = scope
        raise ToolError(
            "agent-preflight-required",
            json.dumps(detail, ensure_ascii=False),
        )
    return result


def _filtered_recovery_for(operation: str, code: str) -> dict[str, Any]:
    return _profile_recovery(_recovery_for(operation, code))


def _domain_code(message: str) -> str:
    """Stable diagnostic code from a ValidationError message prefix
    (``kebab-code: detail``); falls back to ``workdir-invalid`` when the
    prefix is not a registered code. Shared with the CLI seam (issue #53)."""
    return domain_code_from_message(message)


_ACTIVE_TOOL_NAMES: set[str] | None = None  # None = full surface (no profile applied)


def _profile_recovery(entry: dict[str, Any]) -> dict[str, Any]:
    """Drop recovery tools the active profile hides — suggesting a tool the
    agent cannot call reads as "unrecoverable" and strands the session."""
    tools = entry.get("tools")
    if not tools or _ACTIVE_TOOL_NAMES is None:
        return entry
    kept = [name for name in tools if name in _ACTIVE_TOOL_NAMES]
    if kept == tools:
        return entry
    entry = {**entry, "tools": kept}
    if kept:
        entry["message"] = (
            "in this tool profile only the listed tools are available; "
            "run them in order, then retry the call that failed"
        )
    return entry


def _recovery_for(operation: str, code: str) -> dict[str, Any]:
    if code == "operation-id-reused":
        return {
            "action": "retry-with-fresh-operation-id",
            "message": "pass a NEW unique operation_id (or omit it); never reuse one from an earlier call, success or failure",
            "tools": [],
        }
    if code == "workdir-not-open":
        return {"action": "open-workdir", "tools": ["workdir_open"]}
    if code == "agent-preflight-required":
        return {
            "action": "resolve-review",
            "tools": [
                "review_preflight",
                "review_inbox",
                "review_apply_patch",
                "review_apply_batch",
                "review_settlement_plan",
                "review_settle",
            ],
        }
    if code == "edit-mode-ambiguous":
        return {
            "action": "choose-edit-mode",
            "tool": "workdir_open",
            "choices": ["track=true", "track=false"],
        }
    if code in {"generation-conflict", "current-parent-mismatch", "current-snapshot-drift"}:
        return {"action": "refresh-state", "tools": ["workdir_status", "review_state"]}
    if code in {"edit-dirty", "edit-stale", "edit-conflict"}:
        return {"action": "reconcile-edit", "tools": ["diff_preview", "commit_sync", "revert"]}
    # facade errors route back through the facade first; only genuine
    # region diagnosis drops to get_paragraph
    if code in {"stale-document-view", "patch-context-mismatch", "patch-empty"}:
        return {"action": "re-read-document", "tools": ["document_read", "document_search"]}
    if code in {"patch-invalid", "patch-structure-immutable", "patch-block-inserted", "patch-block-deleted", "document-patch-hunks-overlap", "document-patch-paragraph-repeated", "invalid-arguments"}:
        return {"action": "reformulate-patch", "tools": ["document_read", "document_patch"]}
    if code == "source-modified-outside-engine":
        return {"action": "re-extract-from-trusted-source", "tools": ["workdir_open"]}
    if code == "comment-text-requires-opt-in":
        return {"action": "confirm-comment-edit-intent", "tools": ["document_read", "delete_comment"]}
    if code == "format-style-unavailable":
        return {"action": "reuse-existing-variant", "tools": ["document_read", "format_span"]}
    if code == "format-noop":
        return {"action": "none-required", "tools": []}
    if code == "text-inside-tracked-deletion":
        return {"action": "settle-deletion-or-edit-replacement", "tools": ["document_read", "accept_revision"]}
    if code == "patch-hunks-invalid":
        return {"action": "apply-listed-fixes", "tools": ["document_patch"]}
        return {"action": "fix-listed-hunks", "tools": ["document_read", "document_patch"]}
    if code == "ambiguous-alignment":
        return {"action": "disambiguate-insert-anchor", "tools": ["get_paragraph", "batch_edit"]}
    if code == "edit-span-crosses-revision-boundary":
        return {"action": "replan-within-revision-regions", "tools": ["document_read", "document_search"]}
    if code == "edit-span-crosses-protected-marker":
        return {"action": "replan-within-one-span", "tools": ["document_read", "document_search"]}
    if code == "placeholder-in-edit-span":
        return {"action": "choose-editable-span", "tools": ["document_read", "document_search"]}
    if code in {"text-not-found", "text-ambiguous"}:
        return {"action": "re-read-document", "tools": ["document_search", "document_read"]}
    if code == "cross-region-text":
        return {
            "action": "use-document_patch",
            "message": "replace_text needs a single style region; document_patch accepts "
                       "cross-region spans and assigns style ownership itself — do not split "
                       "the edit at style boundaries",
            "tools": ["document_patch"],
        }
    if code in {"paragraph-not-found", "draft-invalid", "invalid-edit", "region-out-of-range"}:
        return {"action": "refresh-edit", "tools": ["document_read", "diff_preview"]}
    if code == "operation-id-reused":
        return {"action": "new-operation-id", "tools": [operation]}
    if code == "evidence-publish-failed":
        return {"action": "retry-same-operation", "tools": [operation]}
    if code in {"writer-busy", "writer-timeout"}:
        return {"action": "retry-same-operation", "tools": [operation]}
    if code in {"output-docx-not-found", "workdir-missing"}:
        return {"action": "build-output", "tools": ["build_docx"]}
    return {"action": "inspect-diagnostic", "tools": ["workdir_status", "review_state"]}


# --------------------------------------------------------------------------
# Formatting lane: change run properties of existing text (superscript etc.)
# --------------------------------------------------------------------------

_RPR_ORDER = (
    "rStyle", "rFonts", "b", "bCs", "i", "iCs", "caps", "smallCaps", "strike",
    "dstrike", "outline", "shadow", "emboss", "imprint", "noProof", "snapToGrid",
    "vanish", "webHidden", "color", "spacing", "w", "kern", "position", "sz",
    "szCs", "highlight", "u", "effect", "bdr", "shd", "fitText", "vertAlign",
    "rtl", "cs", "em", "lang", "eastAsianLayout", "specVanish", "oMath",
)


def _rpr_with_overrides(base_rpr: str, attributes: dict[str, Any]) -> str | None:
    """Return ``base_rpr`` with the requested run properties applied, or None
    when nothing changes. Elements are inserted in CT_RPr schema order so Word
    accepts the result."""
    from lxml import etree

    ns = "http://schemas.openxmlformats.org/wordprocessingml/2006/main"
    tag = f"{{{ns}}}"
    parser = etree.XMLParser(remove_blank_text=False)
    root = etree.fromstring((base_rpr or f'<w:rPr xmlns:w="{ns}"/>').encode("utf-8"), parser)
    changed = False
    for name, value in attributes.items():
        if name == "vertAlign":
            if value in (None, "", "baseline", "none"):
                existing = root.find(f"{tag}vertAlign")
                if existing is not None:
                    root.remove(existing)
                    changed = True
                continue
            if value not in ("superscript", "subscript"):
                raise ToolError("format-invalid-attribute", f"vertAlign must be superscript or subscript, got {value!r}")
            element = etree.Element(f"{tag}vertAlign")
            element.set(f"{tag}val", value)
        elif name in ("bold", "italic"):
            local = "b" if name == "bold" else "i"
            if not isinstance(value, bool):
                raise ToolError("format-invalid-attribute", f"{name} must be true or false")
            element = etree.Element(f"{tag}{local}")
            if not value:
                element.set(f"{tag}val", "0")
        else:
            raise ToolError(
                "format-invalid-attribute",
                f"unsupported attribute {name!r}; supported: vertAlign (superscript|subscript|baseline), bold, italic",
            )
        existing = root.find(f"{tag}{element.tag.split('}')[1]}")
        if existing is not None:
            if etree.tostring(existing) == etree.tostring(element):
                continue
            root.remove(existing)
        order = list(_RPR_ORDER).index(element.tag.split("}")[1])
        position = len(root)
        for index, child in enumerate(root):
            if child.tag.split("}")[1] in _RPR_ORDER and _RPR_ORDER.index(child.tag.split("}")[1]) > order:
                position = index
                break
        root.insert(position, element)
        changed = True
    if not changed:
        return None
    return etree.tostring(root, encoding="unicode")


def _visible_ranges(nodes: list[Any], start: int = 0, path: tuple[str, ...] = ()) -> tuple[list[tuple[int, int, Any, tuple[str, ...]]], int]:
    """Visible-coordinate ranges of every TextNode (deleted text is invisible).

    ``path`` records the enclosing revision/range containers so a span may be
    refused when it would cross a revision boundary."""
    ranges: list[tuple[int, int, Any, tuple[str, ...]]] = []
    offset = start
    for node in nodes:
        if isinstance(node, TextNode):
            ranges.append((offset, offset + len(node.text), node, path))
            offset += len(node.text)
        elif isinstance(node, (RevisionNode, RangeNode)):
            if isinstance(node, RevisionNode) and node.kind in ("delete", "move_from"):
                continue
            nested, offset = _visible_ranges(node.children, offset, path + (node.token_id,))
            ranges.extend(nested)
    return ranges, offset


def _split_text_nodes(nodes: list[Any], start: int, end: int) -> tuple[list[Any], list[Any], list[Any]]:
    """Slice a node list by a visible range into (before, middle, after),
    preserving styles; the range must live inside this list."""
    before: list[Any] = []
    middle: list[Any] = []
    after: list[Any] = []
    offset = 0
    for node in nodes:
        if isinstance(node, TextNode):
            node_start, node_end = offset, offset + len(node.text)
            offset = node_end
            if node_end <= start:
                before.append(node)
                continue
            if node_start >= end:
                after.append(node)
                continue
            local_start = max(start, node_start) - node_start
            local_end = min(end, node_end) - node_start
            if local_start:
                before.append(TextNode(node.style_id, node.text[:local_start]))
            middle.append(TextNode(node.style_id, node.text[local_start:local_end]))
            if local_end < len(node.text):
                after.append(TextNode(node.style_id, node.text[local_end:]))
            continue
        if isinstance(node, (RevisionNode, RangeNode)):
            nested, nested_end = _visible_ranges(node.children, offset)
            if isinstance(node, RevisionNode) and node.kind in ("delete", "move_from"):
                before.append(node)
                offset = nested_end
                continue
            if nested and nested[0][0] < end and nested[-1][1] > start:
                inner_before, inner_middle, inner_after = _split_text_nodes(node.children, start, end)
                if inner_before:
                    before.append(_clone_container(node, inner_before))
                middle.extend(inner_middle)
                if inner_after:
                    after.append(_clone_container(node, inner_after))
            else:
                (before if nested_end <= start else after).append(node)
            offset = nested_end
            continue
        (before if offset <= start else after).append(node)
    return before, middle, after


def _clone_container(node: Any, children: list[Any]) -> Any:
    import copy

    clone = copy.deepcopy(node)
    clone.children = children
    return clone


def _typed_style_regions(nodes: list[Any]) -> list[tuple[int, int]]:
    """Style regions of a paragraph as (start, end) visible offsets: maximal
    runs of equal style, i.e. the addresses reported in
    document_read(view="spans") -> span_map.style_regions."""
    regions: list[list[int]] = []
    style_of_region: list[str] = []

    def walk(items: list[Any], offset: int) -> int:
        for node in items:
            if isinstance(node, TextNode):
                if not node.text:
                    continue
                if regions and regions[-1][1] == offset and style_of_region[-1] == node.style_id:
                    regions[-1][1] = offset + len(node.text)
                else:
                    regions.append([offset, offset + len(node.text)])
                    style_of_region.append(node.style_id)
                offset += len(node.text)
                continue
            if isinstance(node, RevisionNode):
                if node.kind in ("delete", "move_from"):
                    continue
                offset = walk(node.children, offset)
                continue
            if isinstance(node, RangeNode):
                offset = walk(node.children, offset)
        return offset

    walk(nodes, 0)
    return [(a, b) for a, b in regions if b > a]


def _typed_spans(nodes: list[Any]) -> list[tuple[int, int]]:
    """Editable spans of a paragraph in typed-AST order: cuts at every
    revision container edge (insert/move-to start+end) and deletion gap,
    mirroring the read surface's map for a clean workdir."""
    cuts: set[int] = {0}
    offset = 0

    def walk(items: list[Any], offset: int) -> int:
        for node in items:
            if isinstance(node, TextNode):
                offset += len(node.text)
                continue
            if isinstance(node, RevisionNode):
                cuts.add(offset)
                if node.kind in ("delete", "move_from"):
                    pass  # invisible: no width, the gap itself is the cut
                else:
                    offset = walk(node.children, offset)
                cuts.add(offset)
                continue
            if isinstance(node, RangeNode):
                offset = walk(node.children, offset)
        return offset

    total = walk(nodes, offset)
    cuts.add(total)
    ordered = sorted({cut for cut in cuts if 0 <= cut <= total})
    return [(a, b) for a, b in zip(ordered, ordered[1:]) if b > a]


def _restyle_nodes(nodes: list[Any], start: int, end: int, new_style: str) -> list[Any]:
    """Split TextNodes so exactly [start, end) carries ``new_style``; the range
    is guaranteed to live inside this node list (caller checked containers)."""
    out: list[Any] = []
    offset = 0
    for node in nodes:
        if isinstance(node, TextNode):
            node_start, node_end = offset, offset + len(node.text)
            offset = node_end
            if node_end <= start or node_start >= end:
                out.append(node)
                continue
            local_start = max(start, node_start) - node_start
            local_end = min(end, node_end) - node_start
            if local_start:
                out.append(TextNode(node.style_id, node.text[:local_start]))
            out.append(TextNode(new_style, node.text[local_start:local_end]))
            if local_end < len(node.text):
                out.append(TextNode(node.style_id, node.text[local_end:]))
            continue
        if isinstance(node, (RevisionNode, RangeNode)):
            nested_ranges, _ = _visible_ranges(node.children, offset)
            if isinstance(node, RevisionNode) and node.kind in ("delete", "move_from"):
                out.append(node)
                continue
            if nested_ranges and nested_ranges[0][0] < end and nested_ranges[-1][1] > start:
                node.children = _restyle_nodes(node.children, start, end, new_style)
            out.append(node)
            _, offset = _visible_ranges(node.children, offset)
            continue
        out.append(node)
    return out


def _check_source_drift(workdir: Path) -> None:
    """Hard gate for commit_sync: workdir_open and build_docx already verify
    the source fingerprint via validate_workdir, but the commit path does
    not — without this check a session held open across an out-of-band
    source edit (raw OOXML escape) can still commit. Workdirs extracted
    before the field existed are exempt."""
    try:
        format_data = json.loads((workdir / "format.json").read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return
    recorded = str(format_data.get("source_sha256", "") or "")
    source_value = str(format_data.get("source_path", "") or "")
    if not recorded or not source_value:
        return
    source = Path(source_value)
    source = source if source.is_absolute() else workdir / source
    if not source.exists():
        return
    import hashlib
    if hashlib.sha256(source.read_bytes()).hexdigest() != recorded:
        raise ToolError(
            "source-modified-outside-engine",
            f"{source}: the source document changed after extract (recorded "
            "source_sha256 no longer matches). The edit session can no longer be "
            "trusted against the document on disk. Re-extract from a trusted copy "
            "into a fresh workdir; never patch the source DOCX directly.",
        )


def _require_comment_text_opt_in(paragraph_id: str, allow_comment_text: bool) -> None:
    """comments.P* paragraphs are annotation content, not document prose.
    Every text-mutation entry (document_patch / replace_text / batch_edit)
    routes through this gate: replaces there require the explicit
    allow_comment_text opt-in, which is only appropriate when the user
    explicitly asked to edit reviewer comment text. Removing a comment
    belongs to delete_comment (entry + anchors + references)."""
    if paragraph_id.startswith("comments.") and not allow_comment_text:
        raise ToolError(
            "comment-text-requires-opt-in",
            f"{paragraph_id}: comment text is annotation content, not document "
            "prose; pass allow_comment_text=true only when the user explicitly "
            "asked to edit reviewer comment text. Removing a comment belongs to "
            "delete_comment (entry + anchors + references).",
        )


def _failure_result(
    operation: str,
    code: str,
    message: str,
    *,
    operation_id: str | None = None,
    details: dict[str, Any] | None = None,
) -> CallToolResult:
    diagnostic_message = message
    if code == "agent-preflight-required":
        try:
            details = json.loads(message)
            diagnostic_message = "agent write blocked by review preflight"
        except json.JSONDecodeError:
            pass
    recovery = _filtered_recovery_for(operation, code)
    resolved_operation_id = str(operation_id).strip() if operation_id else ""
    resolved_operation_id = resolved_operation_id or new_operation_id()
    data: dict[str, Any] = {
        "operation_id": resolved_operation_id,
        "recovery": recovery,
    }
    envelope = result_envelope(
        operation,
        "failure",
        data=data,
        diagnostics=[
            domain_diagnostic(
                code,
                diagnostic_message,
                details=details,
                next_actions=[
                    f"call {tool}" for tool in recovery.get("tools", [recovery["tool"]] if "tool" in recovery else [])
                ],
            )
        ],
    )
    return mcp_result(envelope, is_error=True)


def _evidence_publish_failed(
    operation: str,
    operation_id: str,
    evidence_path: Path,
    exc: OSError,
    *,
    include_operation_id: bool = True,
) -> CallToolResult:
    """Structured ``evidence-publish-failed`` Result.

    The diagnostic detail names the exception class and the fixed evidence
    path — never the transient mkstemp temp filename embedded in
    ``str(exc)`` — so every independently-built attempt (first run or
    pending-repair retry) reports the byte-identical diagnostic."""
    data = {"operation_id": operation_id} if include_operation_id else {}
    next_actions = (
        [f"retry {operation} with operation_id {operation_id}"]
        if include_operation_id
        else [f"retry {operation} with the same call"]
    )
    envelope = result_envelope(
        operation,
        "failure",
        data=data,
        diagnostics=[
            domain_diagnostic(
                "evidence-publish-failed",
                f"required run evidence could not be published: {type(exc).__name__}: {evidence_path}",
                next_actions=next_actions,
            )
        ],
    )
    return mcp_result(envelope, is_error=True)

_DRAFT_MUTATION_OPERATIONS = frozenset(
    {
        "document_patch",
        "document_replace",
        "format_span",
        "replace_text",
        "batch_edit",
        "insert_paragraph",
        "delete_paragraph",
    }
)


def _adopt_requested_mode(track: bool | None) -> None:
    """Let a mutation carry the mode decision. Only sets the session when the
    caller states an intent, so an ambiguous-mode document no longer costs a
    refused call plus a reopen."""
    if track is None:
        return
    session.track_override = track
    session.mode = "track" if track else "direct"


_MODE_DEFAULT_NOTE: dict[str, str | None] = {"note": None}


def _mutation_tool(
    operation_id: str | None,
    operation: str,
    canonical_args: dict[str, Any],
    anchor: Path,
    *,
    directory: bool,
    evidence_path: Path,
    run: Callable[..., tuple[str, dict[str, Any], str, dict[str, Any], list[dict[str, Any]]]],
    store_workdir: Path | None = None,
    store_generation: bool = True,
    preflight_scope: Iterable[str] | None = None,
    require_agent_preflight: bool = False,
    include_operation_id_on_evidence_failure: bool = True,
    require_source_fresh: bool = True,
) -> CallToolResult:
    """Run one mutating tool under the Operation-ID/Evidence contract and
    return the common Result envelope as structuredContent.

    ``run`` returns (outcome, data, kind, payload, diagnostics); domain
    failures become ``isError`` Results carrying Diagnostics (no exception).
    Replay with the identical operation_id + canonical input returns the
    original envelope; changed input with a reused ID fails
    ``operation-id-reused``. With ``require_source_fresh`` (default) new
    operations verify the source fingerprint AFTER the ledger-replay lookup,
    so an exact retry replays the original result even if the source drifted
    afterwards, while any new operation fails closed with
    ``source-modified-outside-engine``. If operation_id is omitted, the server generates
    one and returns it in the Result data. With ``store_workdir`` the mutation
    runs through the immutable-generation store (Writer lane, CAS, durable
    journals, startup recovery, atomic external publication) and ``run``
    receives the fresh generation directory (or the pinned generation for
    external-only publication)."""
    op_id = str(operation_id).strip() if operation_id else ""
    op_id = op_id or new_operation_id()
    canonical = canonical_operation_input(operation, canonical_args)
    store = None
    if store_workdir is not None and has_store(store_workdir):
        try:
            store = Store.open(store_workdir)
        except (StoreError, OSError) as exc:
            return _failure_result(
                operation,
                getattr(exc, "code", None) or "workdir-unreadable",
                str(exc),
                operation_id=op_id,
            )
    ledger_anchor = anchor
    ledger_directory = directory
    if store is not None:
        # Replay lookup must hit the generation the record was written under:
        # the pointer may have advanced past the committing generation, so
        # search every generation, not just the current pin.
        record, corrupt_path = store.lookup_ledger(
            op_id, generation=store_generation, anchor=anchor, directory=directory
        )
    else:
        record = operation_ledger.lookup_persisted(op_id, ledger_anchor, directory=ledger_directory)
        corrupt_path = None
        if record is None:
            corrupt_path = operation_ledger.corrupt_persisted(
                op_id, ledger_anchor, directory=ledger_directory
            )
    if record is None and corrupt_path is not None:
        # Corrupt persisted row for this operation_id: the mutation may have
        # completed (e.g. a lost pending marker), so never rerun. Fail closed
        # with a structured Result naming the exact ledger file; the corrupt
        # row stays for inspection.
        return _failure_result(
            operation,
            "operation-ledger-invalid",
            f"ledger record for operation_id {op_id!r} is corrupt; "
            f"repair or remove {corrupt_path}",
            operation_id=op_id,
        )
    if record is not None:
        if record["input_sha256"] == canonical:
            envelope = record.get("envelope")
            if isinstance(envelope, dict) and envelope.get("outcome") in (
                "success",
                "failure",
                "partial",
            ):
                if record.get("pending") is not True:
                    return mcp_result(envelope, is_error=(envelope["outcome"] != "success"))
                # Pending record carrying the prepared exact envelope: the
                # effect already completed (run() returns only after the
                # effect landed), so never rerun. Repair the required
                # evidence sidecar from the envelope's sole evidence, then
                # upgrade the record without changing the envelope so every
                # replay stays byte-exact. A repair failure keeps the record
                # pending and reports evidence-publish-failed.
                stored_evidence = envelope.get("evidence") or []
                if len(stored_evidence) != 1:
                    return _failure_result(
                        operation,
                        "operation-ledger-invalid",
                        f"ledger record for operation_id {op_id!r} carries a prepared "
                        f"envelope without exactly one evidence record; repair or "
                        f"remove {operation_ledger_path(ledger_anchor, directory=ledger_directory)}",
                        operation_id=op_id,
                    )
                try:
                    candidate = json.loads(evidence_path.read_text(encoding="utf-8"))
                except (OSError, json.JSONDecodeError):
                    candidate = None
                if candidate != stored_evidence[0]:
                    try:
                        publish_run_evidence(evidence_path, stored_evidence[0])
                    except OSError as exc:
                        return _evidence_publish_failed(
                            operation,
                            op_id,
                            evidence_path,
                            exc,
                            include_operation_id=include_operation_id_on_evidence_failure,
                        )
                operation_ledger.record(op_id, canonical, envelope, ledger_anchor, directory=ledger_directory)
                return mcp_result(envelope, is_error=(envelope["outcome"] != "success"))
            # Missing/pending envelope: the operation never completed. Fall
            # through and rerun the idempotent operation (records are shape-
            # validated at read time, so a corrupt record can never replay).
        return _failure_result(
            operation,
            "operation-id-reused",
            f"operation_id {op_id!r} was already used with different input",
            operation_id=op_id,
        )
    if operation in _DRAFT_MUTATION_OPERATIONS and session.mode == "ambiguous":
        # Fail SAFE, not loud: a document that already carries revisions is
        # edited in track mode by default (never silently rewrite another
        # author's revision), with the choice reported so the caller can
        # override it in this same call via track=false.
        _adopt_requested_mode(True)
        _MODE_DEFAULT_NOTE["note"] = (
            "edit-mode-ambiguous-defaulted-to-track: the document carries pending revisions, "
            "so the edit was recorded as tracked revisions; pass track=false in the same call "
            "to edit directly instead"
        )
    else:
        _MODE_DEFAULT_NOTE["note"] = None
    if require_agent_preflight or preflight_scope is not None:
        try:
            _agent_preflight(
                store_workdir or anchor,
                preflight_scope,
            )
        except ToolError as exc:
            return _failure_result(operation, exc.code, exc.detail, operation_id=op_id, details=getattr(exc, "details", None))
        except CollaborationError as exc:
            return _failure_result(operation, exc.code, exc.detail, operation_id=op_id, details=getattr(exc, "details", None))
        except (TypedError, ValidationError) as exc:
            return _failure_result(operation, _domain_code(str(exc)), str(exc), operation_id=op_id)
        except OSError as exc:
            return _failure_result(operation, "workdir-unreadable", str(exc), operation_id=op_id)
        except (KeyError, ValueError) as exc:
            return _failure_result(operation, "workdir-invalid", str(exc), operation_id=op_id)
    if store_workdir is not None and store is None:
        try:
            Store.ensure(store_workdir, operation_id=op_id, input_sha256=canonical)
            store = Store.open(store_workdir)
        except (StoreError, OSError) as exc:
            return _failure_result(
                operation,
                getattr(exc, "code", None) or "workdir-unreadable",
                str(exc),
                operation_id=op_id,
            )
    if store is not None:
        base_run = run

        def run_checked(target: Path, tx: Any = None):
            # ledger-first: store.mutate replays exact retries without ever
            # calling run, so the freshness gate only fires for new operations
            if require_source_fresh:
                _check_source_drift(store_workdir)
            return base_run(target, tx)

        return _store_mutation_tool(
            operation,
            op_id,
            canonical,
            store,
            run_checked,
            evidence_path,
            generation=store_generation,
            anchor=anchor,
            directory=directory,
        )
    if require_source_fresh:
        try:
            _check_source_drift(Path(anchor))
        except ToolError as exc:
            return _failure_result(operation, exc.code, exc.detail, operation_id=op_id, details=getattr(exc, "details", None))
    try:
        outcome, data, kind, payload, diagnostics = run(Path(anchor))
    except ToolError as exc:
        return _failure_result(operation, exc.code, exc.detail, operation_id=op_id, details=getattr(exc, "details", None))
    except CollaborationError as exc:
        return _failure_result(operation, exc.code, exc.detail, operation_id=op_id, details=getattr(exc, "details", None))
    except (TypedError, ValidationError) as exc:
        return _failure_result(operation, _domain_code(str(exc)), str(exc), operation_id=op_id)
    except zipfile.BadZipFile as exc:
        return _failure_result(operation, "workdir-invalid", str(exc), operation_id=op_id)
    except OSError as exc:
        return _failure_result(operation, "workdir-unreadable", str(exc), operation_id=op_id)
    except (KeyError, ValueError) as exc:
        return _failure_result(operation, "workdir-invalid", str(exc), operation_id=op_id)
    evidence = run_evidence(
        operation, outcome, kind=kind, operation_id=op_id, payload=payload
    )
    data = {"operation_id": op_id, **data}
    envelope = result_envelope(
        operation,
        outcome,
        data=data,
        diagnostics=diagnostics,
        evidence=[evidence],
    )
    # Persist the exact prepared envelope as the pending record BEFORE the
    # sidecar publish: a crash between here and the completed upgrade leaves
    # a retryable record whose envelope is the byte-exact original Result.
    operation_ledger.record(
        op_id, canonical, envelope, ledger_anchor, directory=ledger_directory, pending=True
    )
    try:
        publish_run_evidence(evidence_path, evidence)
    except OSError as exc:
        # Keep the pending record carrying the prepared envelope unchanged: a
        # retry republishes the evidence and upgrades; the prepared envelope
        # is never replaced by this failure Result.
        return _evidence_publish_failed(
            operation,
            op_id,
            evidence_path,
            exc,
            include_operation_id=include_operation_id_on_evidence_failure,
        )
    operation_ledger.record(op_id, canonical, envelope, ledger_anchor, directory=ledger_directory)
    return mcp_result(envelope, is_error=(outcome != "success"))


def _store_mutation_tool(
    operation: str,
    op_id: str,
    canonical: str,
    store: "Store",
    run: Callable[..., tuple[str, dict[str, Any], str, dict[str, Any], list[dict[str, Any]]]],
    evidence_path: Path,
    *,
    generation: bool,
    anchor: Path,
    directory: bool,
) -> CallToolResult:
    """Run one mutating tool through the immutable-generation store. The store
    owns evidence, ledger, journal, pointer, and external publication
    durability; the caller only wraps the committed envelope."""
    try:
        pin = store.pin()
        expected_generation = pin["generation"]
        expected_manifest = pin["manifest_sha256"]

        def adapter(target: Path, tx: Any) -> tuple[Any, ...]:
            result = run(target, tx)
            outcome, data, kind, payload, diagnostics = result
            return outcome, data, kind, payload, diagnostics

        envelope = store.mutate(
            operation=operation,
            operation_id=op_id,
            canonical=canonical,
            input_sha256=expected_manifest or canonical,
            expected_generation=expected_generation,
            run=adapter,
            generation=generation,
            ledger_anchor=None if generation else anchor,
            ledger_directory=directory if not generation else True,
            evidence_path=None if generation else evidence_path,
        )
    except StoreError as exc:
        return _failure_result(operation, exc.code, str(exc), operation_id=op_id)
    except ToolError as exc:
        return _failure_result(operation, exc.code, exc.detail, operation_id=op_id, details=getattr(exc, "details", None))
    except CollaborationError as exc:
        return _failure_result(operation, exc.code, exc.detail, operation_id=op_id, details=getattr(exc, "details", None))
    except (TypedError, ValidationError) as exc:
        return _failure_result(operation, _domain_code(str(exc)), str(exc), operation_id=op_id)
    except zipfile.BadZipFile as exc:
        return _failure_result(operation, "workdir-invalid", str(exc), operation_id=op_id)
    except OSError as exc:
        return _failure_result(operation, "workdir-unreadable", str(exc), operation_id=op_id)
    except (KeyError, ValueError) as exc:
        return _failure_result(operation, "workdir-invalid", str(exc), operation_id=op_id)
    return mcp_result(envelope, is_error=(envelope["outcome"] != "success"))


def _workdir_manifest_sha256(workdir: Path) -> str:
    return semantic_sha256(derived_workdir_manifest(workdir))

session = WorkdirSession()
mcp = FastMCP("docx2typed")


def _fail_closed_on_unknown_arguments() -> None:
    """Refuse a call whose argument names are not in the tool's schema.

    The MCP layer drops unknown keys before the tool body runs, so a typo read
    as a successful call that quietly used DEFAULTS: build_docx(output_path=...)
    built to the default path and reported success. Fail closed instead, and
    name the closest accepted key — that is a one-attempt fix for the caller.
    """
    import difflib

    manager = mcp._tool_manager
    original = manager.call_tool

    async def guarded(name, arguments, context=None, convert_result=True):
        tool = manager._tools.get(name)
        if tool is not None and isinstance(arguments, dict):
            accepted = list((tool.parameters or {}).get("properties", {}))
            unknown = sorted(set(arguments) - set(accepted))
            if unknown:
                hints = []
                for key in unknown:
                    close = difflib.get_close_matches(key, accepted, n=1, cutoff=0.6)
                    hints.append(f"{key!r}" + (f" (did you mean {close[0]!r}?)" if close else ""))
                raise ValueError(
                    f"unknown argument(s) for {name}: " + ", ".join(hints)
                    + f"; accepted: {accepted}. Nothing was executed — these arguments would "
                    "otherwise be ignored and the call would run with defaults."
                )
        return await original(name, arguments, context=context, convert_result=convert_result)

    manager.call_tool = guarded


_fail_closed_on_unknown_arguments()

_ESC_LBRACKET = "\\u27E6"
_ESC_RBRACKET = "\\u27E7"
_ESC_BACKSLASH = "\\\\"


def _json(data: Any) -> str:
    return json.dumps(data, ensure_ascii=False, indent=2)


def _escape_prose(text: str) -> str:
    out: list[str] = []
    for char in text:
        if char == "\\":
            out.append(_ESC_BACKSLASH)
        elif char == "\u27e6":
            out.append(_ESC_LBRACKET)
        elif char == "\u27e7":
            out.append(_ESC_RBRACKET)
        else:
            out.append(char)
    return "".join(out)


def _split_chunks(body: str) -> list[tuple[str, str]]:
    """Split a draft body into ("text", raw-prose) and ("token", placeholder)
    chunks. Text chunks keep their escaped form; tokens are atomic."""
    chunks: list[tuple[str, str]] = []
    cursor = 0
    while True:
        start = body.find("\u27e6", cursor)
        if start < 0:
            if body[cursor:]:
                chunks.append(("text", body[cursor:]))
            return chunks
        if start > cursor:
            chunks.append(("text", body[cursor:start]))
        end = body.find("\u27e7", start + 1)
        if end < 0:
            raise ToolError("edit-grammar-invalid", "unclosed placeholder in draft")
        chunks.append(("token", body[start : end + 1]))
        cursor = end + 1


def _token_marker(kind: str) -> str:
    return {"tab": "\u21b9", "br": "\u21b5", "cr": "\u21b5"}.get(kind, f"\u27e6{kind}\u27e7")


def _visible_text(body: str) -> str:
    out: list[str] = []
    for kind, raw in _split_chunks(body):
        if kind == "token":
            match = re.match(r"\u27e6(token|range-start|range-end)(.*)", raw)
            if match:
                keyword, attrs = match.group(1), match.group(2)
                kind_match = re.search(r'kind="([^"]+)"', attrs)
                if keyword == "range-end":
                    out.append("\u27e7")
                elif kind_match:
                    out.append(_token_marker(kind_match.group(1)))
                else:
                    out.append("\u27e6?\u27e7")
            else:
                out.append("\u27e6?\u27e7")
        else:
            out.append(_validate_escaped_prose(raw))
    return "".join(out)


def _replace_in_body(
    body: str,
    old: str,
    new: str,
    paragraph_id: str,
    *,
    start_offset: int | None = None,
) -> str:
    """Replace one visible range, optionally anchored by paragraph offset."""
    if "\u27e6" in old or "\u27e7" in old:
        raise ToolError(
            "text-not-found",
            f"{paragraph_id}: old must be visible text without placeholder markers",
        )
    matches = 0
    cursor = 0
    out: list[str] = []
    for kind, raw in _split_chunks(body):
        if kind == "token":
            out.append(raw)
            continue
        visible = _validate_escaped_prose(raw)
        if start_offset is None:
            if old in visible:
                matches += 1
                out.append(_escape_prose(visible.replace(old, new, 1)))
            else:
                out.append(raw)
        else:
            local_start = start_offset - cursor
            anchored = (
                matches == 0
                and 0 <= local_start <= len(visible)
                and visible[local_start:local_start + len(old)] == old
            )
            if anchored:
                matches = 1
                out.append(_escape_prose(
                    visible[:local_start] + new + visible[local_start + len(old):]
                ))
            else:
                out.append(raw)
        cursor += len(visible)
    if matches == 0:
        err = _span_crosses_boundary(paragraph_id, body, old)
        if err is not None:
            raise err
        raise ToolError("text-not-found", f"{paragraph_id}: text {old!r} not found at the target offset")
    if start_offset is None and matches > 1:
        raise ToolError(
            "text-ambiguous",
            f"{paragraph_id}: text {old!r} appears {matches} times; provide a longer unique context",
        )
    return "".join(out)


def _paragraph_blocks(text: str) -> tuple[str, list[str]]:
    lines = text.splitlines()
    while lines and not lines[0].strip():
        lines.pop(0)
    if not lines or not lines[0].startswith("<!--@edit"):
        raise ToolError("edit-header-missing", "edit.md must start with an @edit header")
    header = lines[0]
    blocks: list[str] = []
    current: list[str] = []
    for line in lines[1:]:
        stripped = line.strip()
        if stripped.startswith("<!--@p") or stripped.startswith("<!--@new") or stripped.startswith("<!--@delete"):
            if current:
                blocks.append("\n".join(current))
                current = []
            current.append(line)
        elif stripped:
            current.append(line)
    if current:
        blocks.append("\n".join(current))
    return header, blocks


def _read_edit(workdir: Path) -> tuple[str, list[str]]:
    return _paragraph_blocks((workdir / PROJECTION_FILE).read_text(encoding="utf-8"))


def _hunks_still_applicable(workdir: Path, hunks: list[tuple[str, dict]]) -> bool:
    """True when every hunk still resolves in the CURRENT draft, i.e. a stale
    base_revision is harmless because the intervening change touched other
    paragraphs. Any hunk whose anchor moved, vanished, or became ambiguous
    returns False so the caller fails closed with the stale diagnostic."""
    try:
        _, blocks = _read_edit(workdir)
        for kind, hunk in hunks:
            paragraph_id = hunk.get("paragraph_id") or hunk.get("insert_after")
            _find_block(blocks, "p", str(paragraph_id))
            if kind == "replace":
                texts, _styles = _draft_paragraph_state(workdir, hunk["paragraph_id"], mode=session.mode)
                visible = "".join(texts)
                if hunk.get("offset") is not None:
                    if not visible.startswith(hunk["old"], int(hunk["offset"])):
                        return False
                elif visible.count(hunk["old"]) != 1:
                    return False
        return True
    except Exception:
        return False


def _write_edit(workdir: Path, header: str, blocks: list[str]) -> None:
    atomic_write_text(workdir / PROJECTION_FILE, header + "\n\n" + "\n\n".join(blocks) + "\n")


def _find_block(blocks: list[str], prefix: str, paragraph_id: str) -> int:
    for index, block in enumerate(blocks):
        if block.startswith(f'<!--@{prefix} id="{paragraph_id}"'):
            return index
    raise ToolError("paragraph-not-found", f"paragraph {paragraph_id} not found in the draft")


def _block_body(block: str) -> str:
    return "\n".join(block.splitlines()[1:]) if "\n" in block else ""


def _block_ident(block: str) -> tuple[str, str] | None:
    """(kind, id) of a draft block marker, e.g. ("p", "P3") for
    <!--@p id="P3"-->, ("new", temp) for <!--@new temp="...">, ("delete", id)
    for <!--@delete id="...">; None when the first line is not a marker."""
    marker = block.splitlines()[0].strip()
    match = re.match(r'<!--@(p|new|delete) (?:id|temp)="([^"]+)"', marker)
    return (match.group(1), match.group(2)) if match else None


def _draft_paragraph_state(workdir: Path, paragraph_id: str, mode: str | None = None) -> tuple[list[str], list[str]]:
    """Current (visible-unit texts, styles) of a draft paragraph.

    Uses the sync engine's dry-run so the regions reflect any uncommitted
    edits, not just the committed typed state. ``mode`` carries the session
    edit mode (track/direct); without it the engine re-infers from the
    source signals, which turns ambiguous for documents with pending
    revisions but trackChanges off and wrongly rejects dirty-draft edits.
    """
    typed = parse_typed((workdir / "typed.md").read_text(encoding="utf-8"))
    state = classify_edit_state(workdir)
    if state["state"] == "clean":
        paragraph = next((p for p in typed.paragraphs if p.paragraph_id == paragraph_id), None)
        if paragraph is None:
            raise ToolError("paragraph-not-found", f"paragraph {paragraph_id} not in typed.md")
        units = flatten_paragraph(paragraph)
    else:
        from .edit import parse_edit_projection

        projection = parse_edit_projection((workdir / PROJECTION_FILE).read_text(encoding="utf-8"))
        format_data = json.loads((workdir / "format.json").read_text(encoding="utf-8"))
        try:
            revision_ctx = None
            if mode == "track":
                from .edit import _build_revision_context

                revision_ctx = _build_revision_context(
                    typed, format_data, workdir,
                    mode="track", author=session.author or "Unknown", author_source="session",
                )
            plan = plan_sync(typed, projection, format_data, mode=mode, revision_ctx=revision_ctx)
        except ValidationError as exc:
            raise ToolError("draft-invalid", f"current draft cannot be applied: {exc}") from exc
        paragraph = next((p for p in plan.document.paragraphs if p.paragraph_id == paragraph_id), None)
        if paragraph is None:
            raise ToolError("paragraph-not-found", f"paragraph {paragraph_id} not in the draft")
        units = flatten_paragraph(paragraph)
    texts = [unit.value[1] for unit in units if not unit.token]
    styles = [unit.style for unit in units if not unit.token]
    return texts, styles


def _normalize_patch_hunks(hunks: list[dict]) -> list[dict]:
    """Normalize document_patch hunk dicts into ("replace"|"insert"|"delete", payload)"""
    normalized: list[tuple[str, dict]] = []
    for index, hunk in enumerate(hunks):
        if not isinstance(hunk, dict):
            raise ToolError("invalid-arguments", f"hunks[{index}] must be an object")
        known = {"paragraph_id", "old", "new", "match_ref", "insert_after", "text", "inherit", "delete"}
        unknown = sorted(set(hunk) - known)
        if unknown:
            suggestions = {
                key: difflib.get_close_matches(key, sorted(known), n=1)
                for key in unknown
            }
            hints = ", ".join(
                f"{key!r}" + (f" (did you mean {suggestions[key][0]!r}?)" if suggestions[key] else "")
                for key in unknown
            )
            raise ToolError(
                "invalid-arguments",
                f"hunks[{index}]: unknown key(s) {hints}; known keys are "
                + ", ".join(sorted(known))
                + " — an unrecognised key would be silently ignored, so nothing was applied",
            )
        if "match_ref" in hunk:
            if "new" not in hunk:
                raise ToolError(
                    "invalid-arguments",
                    f"hunks[{index}]: match_ref hunk needs 'new' (the replacement text); "
                    "without it the match would be treated as a deletion",
                )
            new = hunk["new"]
            if not isinstance(new, str) or not isinstance(hunk["match_ref"], str):
                raise ToolError("invalid-arguments", f"hunks[{index}]: match_ref hunk needs a string new")
            normalized.append(("match_ref", {"match_ref": hunk["match_ref"], "new": new}))
        elif "paragraph_id" in hunk:
            old = hunk.get("old")
            if "new" not in hunk:
                raise ToolError(
                    "invalid-arguments",
                    f"hunks[{index}]: replace hunk needs 'new' (use an empty string only when "
                    "you really mean to delete the text; use a delete hunk to drop a paragraph)",
                )
            new = hunk["new"]
            if not isinstance(old, str) or not old or not isinstance(new, str):
                raise ToolError(
                    "invalid-arguments",
                    f"hunks[{index}]: replace hunk needs string old (non-empty) and new",
                )
            normalized.append(("replace", {"paragraph_id": hunk["paragraph_id"], "old": old, "new": new}))
        elif "insert_after" in hunk:
            text = hunk.get("text")
            if not isinstance(text, str) or not text:
                raise ToolError("invalid-arguments", f"hunks[{index}]: insert hunk needs non-empty text")
            inherit = hunk.get("inherit")
            normalized.append(("insert", {"insert_after": hunk["insert_after"], "text": text, "inherit": inherit}))
        elif "delete" in hunk:
            normalized.append(("delete", {"paragraph_id": hunk["delete"]}))
        else:
            raise ToolError(
                "invalid-arguments",
                f"hunks[{index}]: needs one of paragraph_id (replace), insert_after, or delete",
            )
    return normalized


def _require_body_structure_mutable(paragraph_id: str) -> None:
    if paragraph_id.startswith(("T", "B")) or ("." in paragraph_id):
        raise ToolError(
            "table-structure-immutable",
            f"{paragraph_id}: paragraphs inside tables, text boxes, or parts "
            "cannot be patched from the body surface; use the container-specific tools",
        )


def _parse_unified_diff(diff_text: str) -> list[tuple[str, str]]:
    """Parse a unified diff into ordered ("="|"-"|"+", line) ops. Hunk
    headers are validated but their line numbers are NOT trusted as
    coordinates — blocks are located by marker id + exact body. Raises
    patch-invalid on malformed input; CRLF is normalized."""
    if "\r\n" in diff_text:
        diff_text = diff_text.replace("\r\n", "\n")
    diff_lines = diff_text.split("\n")
    if diff_lines and diff_lines[-1] == "":
        diff_lines.pop()  # artifact of a trailing newline, not a context line
    ops: list[tuple[str, str]] = []
    in_hunk = False
    for line in diff_lines:
        if line.startswith(("diff ", "index ", "--- ", "+++ ")):
            continue
        if line.startswith("@@"):
            if not re.match(r"@@ -\d+(?:,\d+)? \+\d+(?:,\d+)? @@", line):
                raise ToolError("patch-invalid", f"malformed hunk header: {line!r}")
            in_hunk = True
            continue
        if not in_hunk:
            if line.strip():
                raise ToolError("patch-invalid", f"content outside hunk headers: {line!r}")
            continue
        if line.startswith(" "):
            ops.append(("=", line[1:]))
        elif line.startswith("-"):
            if line[1:].lstrip().startswith("<!--@"):
                raise ToolError(
                    "patch-structure-immutable",
                    "a diff may not edit projection markers or the @edit header; "
                    "edit paragraph body text only",
                )
            ops.append(("-", line[1:]))
        elif line.startswith("+"):
            if line[1:].lstrip().startswith("<!--@"):
                raise ToolError(
                    "patch-structure-immutable",
                    "a diff may not edit projection markers or the @edit header; "
                    "edit paragraph body text only",
                )
            ops.append(("+", line[1:]))
        elif line == "":
            ops.append(("=", ""))
        elif line.startswith("\\"):
            continue  # "\\ No newline at end of file"
        else:
            raise ToolError("patch-invalid", f"malformed diff line: {line!r}")
    if not in_hunk:
        raise ToolError("patch-invalid", "no hunk headers in diff")
    return ops


def _marker_split(lines: list[str]) -> tuple[str, list[list[Any]]]:
    """Split projection-like lines into (header, blocks) where each block is
    [kind, id, body] and body is the concatenated non-empty lines after the
    marker. Used to align a diff's old/new sides by block identity."""
    header = ""
    blocks: list[list[Any]] = []
    current: list[Any] | None = None
    for line in lines:
        marker = re.match(r'<!--@(p|new|delete) (?:id|temp)="([^"]+)"', line.strip())
        if marker:
            if current is not None:
                blocks.append([current[0], current[1], "".join(current[2])])
            current = [marker.group(1), marker.group(2), []]
            continue
        if line.startswith("<!--@edit"):
            if not header:
                header = line
            elif header != line:
                raise ToolError(
                    "patch-structure-immutable",
                    "the diff changes the @edit header",
                )
            continue
        if current is not None and line.strip():
            current[2].append(line)
    if current is not None:
        blocks.append([current[0], current[1], "".join(current[2])])
    return header, blocks


def _anchored_insertion(old_body: str, pos: int, inserted: str) -> tuple[str, str]:
    """Represent a pure insertion at ``pos`` as a replacement of a minimal
    unique anchor span around it, so the hunk keeps replace semantics."""
    for radius in (2, 3, 5, 8, 13, 21, 34, 55):
        lo = max(0, pos - radius)
        hi = min(len(old_body), pos + radius)
        span = old_body[lo:hi]
        if span and old_body.count(span) == 1:
            return span, old_body[lo:pos] + inserted + old_body[pos:hi]
    raise ToolError(
        "text-ambiguous",
        "insertion anchor is not unique in the paragraph; include more "
        "unchanged context in the diff around the insertion point",
    )


def _span_diffs(old_body: str, new_body: str) -> list[tuple[str, str]]:
    """Minimal non-overlapping change spans between two block bodies. Pure
    insertions are rewritten as anchored replacements."""
    matcher = SequenceMatcher(None, old_body, new_body, autojunk=False)
    spans: list[tuple[str, str]] = []
    for tag, i1, i2, j1, j2 in matcher.get_opcodes():
        if tag == "equal":
            continue
        old_span = old_body[i1:i2]
        new_span = new_body[j1:j2]
        if not old_span:
            old_span, new_span = _anchored_insertion(old_body, i1, new_span)
        spans.append((old_span, new_span))
    return spans


def _hunks_from_projection_diff(old_lines: list[str], new_lines: list[str]) -> list[dict]:
    """Align the diff's old/new sides by block identity (kind + id + order)
    and derive one or more minimal replace hunks per changed body via
    SequenceMatcher. Structural changes (marker edits, reorders, whole-block
    insertions/deletions) fail closed; they belong to the hunks form."""
    old_header, old_blocks = _marker_split(old_lines)
    new_header, new_blocks = _marker_split(new_lines)
    if old_header and new_header and old_header != new_header:
        raise ToolError("patch-structure-immutable", "the diff changes the @edit header")
    old_ids = [(kind, pid) for kind, pid, _ in old_blocks]
    new_ids = [(kind, pid) for kind, pid, _ in new_blocks]
    if old_ids != new_ids:
        missing = [pid for kind, pid in new_ids if (kind, pid) not in old_ids]
        removed = [pid for kind, pid in old_ids if (kind, pid) not in new_ids]
        if missing:
            raise ToolError(
                "patch-block-inserted",
                f"{missing[0]}: whole-block insertions in a diff are not supported; "
                "use the hunks form with insert_after",
            )
        if removed:
            raise ToolError(
                "patch-block-deleted",
                f"{removed[0]}: whole-block deletions in a diff are not supported; "
                "use the hunks form with delete",
            )
        raise ToolError(
            "patch-structure-immutable",
            "the diff reorders projection blocks; block order is immutable in a diff",
        )
    hunks: list[dict] = []
    for (_, pid, old_body), (_, _, new_body) in zip(old_blocks, new_blocks):
        if old_body == new_body:
            continue
        for old_span, new_span in _span_diffs(old_body, new_body):
            hunks.append({"paragraph_id": pid, "old": old_span, "new": new_span})
    return hunks


def _ensure_diff_base_matches(
    real_header: str, real_blocks: list[str], old_lines: list[str]
) -> None:
    """The diff's old side is the agent's view (full projection or a window).
    Every block it shows must exist in the real projection with an identical
    body and kind — otherwise the diff was generated against a stale view."""
    _, view_blocks = _marker_split(old_lines)
    if not view_blocks:
        raise ToolError("patch-context-mismatch", "the diff shows no paragraph blocks")
    real: dict[str, str] = {}
    real_kind: dict[str, str] = {}
    for block in real_blocks:
        ident = _block_ident(block)
        if ident is None:
            continue
        real[ident[1]] = _block_body(block)
        real_kind[ident[1]] = ident[0]
    for kind, pid, body in view_blocks:
        if pid not in real or real_kind[pid] != kind:
            raise ToolError(
                "patch-context-mismatch",
                f"{pid}: the diff references a block the document does not have; "
                "re-read the document and regenerate the diff",
            )
        if real[pid] != body:
            raise ToolError(
                "patch-context-mismatch",
                f"{pid}: the diff's view of this paragraph does not match the "
                "current draft; re-read the document and regenerate the diff",
            )


def _plan_candidate(workdir: Path, candidate_text: str) -> tuple[Any, str]:
    """Run the sync engine's dry-run over an in-memory candidate projection:
    the Core decides what the facade may write (deterministic mixed-style
    mapping accepted with warnings; ambiguous/protected rewrites rejected)."""
    from .edit import _build_revision_context, parse_edit_projection
    from .edit_sync import _document_has_revisions, plan_sync
    from .typed_core import effective_edit_mode

    typed = parse_typed((workdir / "typed.md").read_text(encoding="utf-8"))
    format_data = json.loads((workdir / "format.json").read_text(encoding="utf-8"))
    mode = session.mode or effective_edit_mode(
        source_track_enabled=bool(format_data.get("source_track_enabled")),
        has_pending_revisions=_document_has_revisions(typed),
    )
    revision_ctx = (
        _build_revision_context(
            typed, format_data, workdir, mode=mode,
            author=session.author or "", author_source="session",
        )
        if mode == "track"
        else None
    )
    projection = parse_edit_projection(candidate_text)
    try:
        plan = plan_sync(typed, projection, format_data, mode=mode, revision_ctx=revision_ctx)
    except ValidationError as exc:
        raise ToolError(_domain_code(str(exc)), str(exc)) from exc
    return plan, mode


def _token_boundary_kind(chunk: str) -> str:
    """Every inline token is atomic for matching; name the flavour so a refusal
    can say WHAT blocks the span (revision control vs format history vs an
    anchor) instead of a bare text-not-found."""
    match = re.match(r"\u27e6(/?)(insert|move-to|move-from)\b", chunk)
    if match:
        return match.group(2) + ("-end" if match.group(1) else "-start")
    if chunk.startswith("\u27e6revision-gap"):
        return "revision-gap"
    kind_match = re.search(r'kind="([^"]+)"', chunk)
    if kind_match:
        return f"token:{kind_match.group(1)}"
    return "token:unknown"


def _body_boundaries(body: str) -> tuple[str, list[tuple[int, str]]]:
    """Flat visible text plus zero-width revision-control boundaries.

    Boundaries come from the same chunk stream ``_replace_in_body`` matches
    against: insert / move-to / move-from (start+end) and revision-gap
    markers all become atomic cut points in the visible text."""
    boundaries: list[tuple[int, str]] = []
    offset = 0
    for kind, chunk in _split_chunks(body):
        if kind == "token":
            label = _token_boundary_kind(chunk)
            boundaries.append((offset, label))
        else:
            offset += len(_validate_escaped_prose(chunk))
    flat = "".join(_validate_escaped_prose(chunk) for k, chunk in _split_chunks(body) if k == "text")
    return flat, boundaries


_REPEATED_JOIN = re.compile(r"(.{3,24}?)([-\u2013\u2014\u00b7\u3001,\uff0c]?)\1")


def _repeated_join(text: str) -> str | None:
    """The same phrase twice in a row (optionally one separator apart) is a
    join error, not style: an edit that leaves ``(PLBA)-(PLBA)`` or ``--``
    behind has duplicated existing text. Advisory only — legitimate repeats
    exist, so this warns instead of failing."""
    match = _REPEATED_JOIN.search(text)
    if match is None:
        return None
    return match.group(0)


def _projection_deleted_matches(workdir: Path, query: str, case_sensitive: bool) -> int:
    """Count query occurrences that exist ONLY in deleted tracked changes.

    The draft drops deleted revision text (it is not editable), while the
    projection keeps it so a human can still read what was removed — which is
    why a user counting in Word and an agent counting via document_search see
    different totals. Reporting this number explains the difference instead of
    leaving it a mystery."""
    needle = query if case_sensitive else query.lower()
    if not needle:
        return 0
    typed = workdir / "typed.md"
    if not typed.exists():
        return 0
    from .typed_core import RevisionNode, parse_typed, visible_text

    try:
        document = parse_typed(typed.read_text(encoding="utf-8", errors="replace"))
    except Exception:  # a projection the parser rejects cannot be counted
        return 0
    chunks: list[str] = []

    def walk(nodes: Any) -> None:
        for node in nodes or []:
            if isinstance(node, RevisionNode):
                if node.kind == "delete":
                    chunks.append(visible_text(node.children))
                walk(node.children)
            else:
                walk(getattr(node, "nodes", None))

    for paragraph in document.paragraphs:
        walk(paragraph.nodes)
    haystack = "".join(chunks)
    if not haystack:
        return 0
    return haystack.count(needle) if case_sensitive else haystack.lower().count(needle)


def _span_map_from(
    paragraph_id: str,
    body: str,
    texts: list[str] | None = None,
    styles: list[str] | None = None,
) -> dict[str, Any]:
    """The paragraph's editable-span map: the visible runs ``document_patch``
    can match, cut ONLY at revision-control boundaries.

    Style edges are NOT cut points (the facade legalises cross-region spans
    and assigns ownership itself); they are reported separately as advisory
    ``style_regions``. Each span carries ``unique``: when false the span text
    also occurs elsewhere in the paragraph, so the hunk needs the whole span
    (extending context across a boundary is refused)."""
    flat, boundaries = _body_boundaries(body)
    cuts = {0, len(flat)}
    for offset, _ in boundaries:
        if 0 < offset < len(flat):
            cuts.add(offset)
    ordered = sorted(cuts)

    style_regions: list[dict[str, Any]] = []
    if texts:
        position = 0
        for text, style in _merge_regions(texts, styles or []):
            style_regions.append(
                {"index": len(style_regions), "start": position, "end": position + len(text), "style_id": style, "text": text}
            )
            position += len(text)
        if position != len(flat):
            style_regions = []

    def region_at(offset: int) -> str:
        depth = 0
        for boundary_offset, kind in boundaries:
            if boundary_offset > offset:
                break
            if kind == "insert-start":
                depth += 1
            elif kind == "insert-end":
                depth = max(0, depth - 1)
        return "insert" if depth else "baseline"

    spans: list[dict[str, Any]] = []
    for start, end in zip(ordered, ordered[1:]):
        segment = flat[start:end]
        if not segment:
            continue
        spans.append(
            {
                "index": len(spans),
                "start": start,
                "end": end,
                "text": segment,
                "length": len(segment),
                "unique": flat.count(segment) == 1,
                "style_ids": sorted({r["style_id"] for r in style_regions if r["start"] < end and r["end"] > start}),
                "region": region_at(start),
            }
        )
    return {
        "paragraph_id": paragraph_id,
        "text": flat,
        "spans": spans,
        "style_regions": style_regions,
        "boundaries": [{"offset": offset, "kind": kind} for offset, kind in boundaries],
        "note": (
            "copy one span's text verbatim as old; a run crossing two spans crosses a "
            "revision-control boundary and is refused. unique=false means the span text "
            "recurs — use the whole span (context beyond it is not editable). Style edges "
            "are NOT edit boundaries: cross them freely (document_patch assigns style "
            "ownership; style_regions here are advisory only)"
        ),
    }


def _divergence_hint(span_map: dict[str, Any] | None, old: str) -> dict[str, Any] | None:
    """Where did the caller's ``old`` stop matching the document?

    Prefers a common-PREFIX anchor (the usual failure: a paraphrased or
    dropped middle), falling back to the longest common substring (the usual
    failure when only the tail differs). The payload names the exact wording
    the document has where the caller's text diverges."""
    if not span_map or not old:
        return None
    spans = [s for s in span_map.get("spans", []) if s.get("text", "").strip()]
    best_prefix = None
    for span in spans:
        text = span["text"]
        size = 0
        while size < min(len(old), len(text)) and old[size] == text[size]:
            size += 1
        if size >= 6 and (best_prefix is None or size > best_prefix["match_size"]):
            best_prefix = {
                "span": span["index"],
                "match_size": size,
                "matched_text": old[:size],
                "document_continues": text[size : size + 80],
                "span_text": text,
            }
    if best_prefix:
        best_prefix["anchor"] = "prefix"
        return best_prefix
    best_substring = None
    for span in spans:
        text = span["text"]
        match = SequenceMatcher(None, old, text).find_longest_match(0, len(old), 0, len(text))
        if match.size < 6 or (best_substring and match.size <= best_substring["match_size"]):
            continue
        best_substring = {
            "span": span["index"],
            "match_size": match.size,
            "matched_text": old[match.a : match.a + match.size],
            "document_before": text[max(0, match.b - 60) : match.b],
            "document_continues": text[match.b + match.size : match.b + match.size + 80],
        }
    if best_substring:
        best_substring["anchor"] = "substring"
    return best_substring


def _closest_spans(span_map: dict[str, Any] | None, old: str, limit: int = 3) -> list[dict[str, Any]]:
    """Top spans by similarity to a failed ``old`` string, so text-not-found
    becomes a "did you mean" instead of a dead end."""
    if not span_map or not old:
        return []
    scored = []
    for span in span_map.get("spans", []):
        text = span.get("text", "")
        if not text.strip():
            continue
        ratio = SequenceMatcher(None, old, text).ratio()
        if ratio >= 0.15:
            scored.append({"span": span["index"], "ratio": round(ratio, 3), "text": text})
    scored.sort(key=lambda item: item["ratio"], reverse=True)
    return scored[:limit]


def _span_map_for(workdir: Path, paragraph_id: str) -> dict[str, Any] | None:
    """Best-effort span map for refusal payloads (never raises)."""
    try:
        _, blocks = _read_edit(workdir)
        index = _find_block(blocks, "p", paragraph_id)
        body = _block_body(blocks[index])
        try:
            texts, styles = _draft_paragraph_state(workdir, paragraph_id, mode=session.mode)
        except Exception:
            texts, styles = None, None
        return _span_map_from(paragraph_id, body, texts, styles)
    except Exception:
        return None


# --------------------------------------------------------------------------
# Matching tolerance: fold width / punctuation / space variants before match
# --------------------------------------------------------------------------
# Every fold is character-local and length-preserving, so a normalized hit maps
# back to an exact document span. Folded characters are never written back —
# the resolved ORIGINAL text is what gets replaced.

_FOLD_PAIRS = {
    "，": ",", "。": ".", "、": ",", "；": ";", "：": ":", "？": "?", "！": "!",
    "（": "(", "）": ")", "【": "[", "】": "]", "《": "<", "》": ">", "“": '"',
    "”": '"', "‘": "'", "’": "'", "—": "-", "–": "-", "−": "-", "―": "-",
    "‒": "-", "～": "~", "〜": "~", "％": "%", "＋": "+", "－": "-", "／": "/",
    "＼": "\\", "＝": "=", "＜": "<", "＞": ">", "＃": "#", "＆": "&", "＊": "*",
    "＠": "@", "｜": "|", "＾": "^", "＿": "_", "｀": "`", "＂": '"', "＇": "'",
    "　": " ", "\u00a0": " ", "\u2002": " ", "\u2003": " ", "\u2004": " ",
    "\u2005": " ", "\u2006": " ", "\u2007": " ", "\u2008": " ", "\u2009": " ",
    "\u200a": " ", "\u202f": " ", "\u205f": " ", "\u3000": " ", "\u00b5": "\u03bc",
}


def _fold_text(text: str) -> str:
    """Length-preserving width/punctuation/space folding used ONLY for
    matching. Full-width ASCII folds to ASCII; CJK punctuation folds to its
    ASCII counterpart; exotic spaces fold to U+0020; micro sign folds to mu."""
    out: list[str] = []
    for char in text:
        if char in _FOLD_PAIRS:
            out.append(_FOLD_PAIRS[char])
            continue
        code = ord(char)
        if 0xFF01 <= code <= 0xFF5E:  # full-width ASCII block
            out.append(chr(code - 0xFEE0))
            continue
        out.append(char)
    return "".join(out)


def _fold_differences(original: str, folded_match: str) -> list[dict[str, str]]:
    """Character pairs that differ only by folding, e.g. [{'document': ',',
    'yours': '，'}] — the agent learns the exact convention it missed."""
    pairs: list[dict[str, str]] = []
    for a, b in zip(original, folded_match):
        if a != b:
            pairs.append({"document": a, "yours": b})
    return pairs


def _resolve_visible_match(visible: str, old: str) -> dict[str, Any] | None:
    """Locate ``old`` in ``visible``, tolerating width/punctuation/space
    variants. Returns the exact document span plus what was folded, or None
    when there is no match."""
    if not old:
        return None
    direct = visible.count(old)
    if direct == 1:
        start = visible.index(old)
        return {"start": start, "end": start + len(old), "matched_text": old, "normalized": False, "differences": []}
    if direct > 1:
        return None  # ambiguity is reported by the caller, not papered over
    folded_old = _fold_text(old)
    folded_visible = _fold_text(visible)
    if len(folded_visible) != len(visible):  # defensive: folds are 1:1
        return None
    offsets: list[int] = []
    cursor = folded_visible.find(folded_old)
    while cursor != -1:
        offsets.append(cursor)
        cursor = folded_visible.find(folded_old, cursor + 1)
    if len(offsets) != 1:
        return None
    start = offsets[0]
    end = start + len(old)
    matched_text = visible[start:end]
    return {
        "start": start,
        "end": end,
        "matched_text": matched_text,
        "normalized": True,
        "differences": _fold_differences(matched_text, old),
    }


def _deletion_containing(workdir: Path, paragraph_id: str, old: str) -> dict[str, Any] | None:
    """When ``old`` is invisible but present in the pre-revision text, name the
    tracked deletion that hides it instead of a bare not-found."""
    try:
        typed = parse_typed((workdir / "typed.md").read_text(encoding="utf-8"))
    except Exception:
        return None
    paragraph = next((p for p in typed.paragraphs if p.paragraph_id == paragraph_id), None)
    if paragraph is None:
        return None
    try:
        from .typed_core import RangeNode as _RangeNode
        from .typed_core import RevisionNode as _RevisionNode
        from .typed_core import visible_text_original
    except ImportError:
        return None
    if old not in visible_text_original(paragraph.nodes):
        return None

    def find_deletion(nodes: list[Any]) -> dict[str, Any] | None:
        for node in nodes:
            if isinstance(node, _RevisionNode) and node.kind in ("delete", "move_from"):
                inner = "".join(child.text for child in node.children if isinstance(child, TextNode))
                if old in inner:
                    return {"w_id": node.attrs.get("w:id"), "author": node.attrs.get("w:author"), "date": node.attrs.get("w:date")}
            if isinstance(node, (_RevisionNode, _RangeNode)):
                found = find_deletion(node.children)
                if found:
                    return found
        return None

    return find_deletion(paragraph.nodes)


def _region_at(boundaries: list[tuple[int, str]], offset: int) -> str:
    """baseline/insert for a visible offset (mirrors the span map)."""
    depth = 0
    for boundary_offset, kind in boundaries:
        if boundary_offset > offset:
            break
        if kind == "insert-start":
            depth += 1
        elif kind == "insert-end":
            depth = max(0, depth - 1)
    return "insert" if depth else "baseline"


def _span_crosses_boundary(paragraph_id: str, body: str, old: str):
    """Return a ToolError when old is contiguous in the token-stripped flat
    text but strictly spans a revision-control boundary marker (insert /
    move / revision-gap). Boundaries exactly at the span edges do not count:
    a span covering a whole revision body is one editable span."""
    if "\u27e6" in old or "\u27e7" in old:
        return None
    flat, boundaries = _body_boundaries(body)
    if not boundaries:
        return None
    if old not in flat:
        return None
    start = flat.index(old)
    end = start + len(old)
    crossed = sorted({name for off, name in boundaries if start < off < end})
    if not crossed:
        return None
    span_map = _span_map_from(paragraph_id, body)
    boundary_offset = min(off for off, name in boundaries if start < off < end)
    spans = span_map.get("spans", [])
    left_span = next((span["text"] for span in reversed(spans) if span["end"] <= boundary_offset and span["text"]), None)
    right_span = next((span["text"] for span in spans if span["start"] >= boundary_offset and span["text"]), None)
    recipe = None
    if left_span and right_span:
        recipe = {
            "action": "split-into-per-span-hunks",
            "blocked_at": boundary_offset,
            "hunks": [
                {"paragraph_id": paragraph_id, "old": left_span, "new": "<edit the left span>"},
                {"paragraph_id": paragraph_id, "old": right_span, "new": "<edit the right span>"},
            ],
        }
    # revision control, however the projection spells it: bare insert/move
    # labels, the gap marker, or a revision kind carried inside a range token
    # (⟦range-start kind="insert"⟧). Anchors and format history are NOT
    # revisions, and calling them one sends the agent hunting for a revision.
    revision_flavours = [
        name
        for name in crossed
        if not name.startswith("token:")
        or name.split(":", 1)[1] in {"insert", "delete", "move_from", "move_to"}
    ]
    if revision_flavours:
        why = (
            "the span crosses revision-control markers (" + ", ".join(revision_flavours) + "), "
            "so the engine cannot decide which revision owns the new text"
        )
    else:
        why = (
            "the span crosses protected inline markers (" + ", ".join(crossed) + ") — format "
            "history / anchors are atomic, so the visible text on the two sides is not one "
            "editable run"
        )
    return ToolError(
        # a span split by anchors/hyperlinks/fields has nothing to do with
        # revision control: naming it "revision boundary" sends the agent
        # looking for a revision that is not there (issue surfaced by the
        # cm-hyperlink-cross-boundary capability-matrix case).
        "edit-span-crosses-revision-boundary" if revision_flavours else "edit-span-crosses-protected-marker",
        f"{paragraph_id}: old is visible in the paragraph but {why}. Re-issue the edit as hunks "
        "that each copy ONE span's text from data.span_map (never a cross-boundary run); "
        "data.fix carries a skeleton for the split when both sides are plain text. If the new "
        "text cannot be partitioned without guessing, stop and ask the user.",
        details={
            "span_map": span_map,
            "crossed": crossed,
            "capability": "word.text.replace.cross-revision-boundary",
            **({"fix": recipe} if recipe else {}),
        },
    )


def _revision_span_diagnostic(target: Path, paragraph_id: str, old: str) -> None:
    """File-reading wrapper: locate the paragraph body then classify the span."""
    try:
        _, blocks = _read_edit(target)
        index = _find_block(blocks, "p", paragraph_id)
        body = _block_body(blocks[index])
    except ToolError:
        return
    err = _span_crosses_boundary(paragraph_id, body, old)
    if err is not None:
        raise err


def _check_single_region(
    workdir: Path,
    paragraph_id: str,
    old: str,
    texts: list[str],
    styles: list[str],
) -> tuple[int, int]:
    """Locate ``old`` in the paragraph's visible units and require it to cover
    exactly one style region. Returns the unit index range."""
    text = "".join(texts)
    tolerated = _resolve_visible_match(text, old)
    if tolerated is not None and tolerated["normalized"]:
        old = tolerated["matched_text"]
    count = text.count(old)
    if count == 0:
        _revision_span_diagnostic(workdir, paragraph_id, old)
        raise ToolError(
            "text-not-found",
            f"{paragraph_id}: text {old!r} not found in paragraph; copy one span "
            "verbatim from data.span_map",
            details={"span_map": _span_map_for(workdir, paragraph_id)},
        )
    if count > 1:
        raise ToolError(
            "text-ambiguous",
            f"{paragraph_id}: text {old!r} appears {count} times; provide a longer unique context",
        )
    start = text.index(old)
    end = start + len(old)
    offsets: list[int] = []
    cursor = 0
    for unit_text in texts:
        offsets.append(cursor)
        cursor += len(unit_text)
    offsets.append(cursor)
    i1 = max(i for i, offset in enumerate(offsets) if offset <= start)
    i2 = max(i for i, offset in enumerate(offsets) if offset < end)
    covered = set(styles[i1 : i2 + 1])
    if len(covered) > 1:
        regions = _region_labels(texts, styles)
        raise ToolError(
            "cross-region-text",
            f"{paragraph_id}: replace_text needs ONE style region but {old!r} covers "
            + " / ".join(regions)
            + " — this is a replace_text-only limit, not a document limit: resend the same "
            "edit through document_patch, which accepts cross-region spans and assigns "
            "style ownership itself (no hunk splitting at style boundaries).",
        )
    return i1, i2
def _fnv1a_utf16(text: str) -> str:
    """Match the browser's UTF-16 FNV-1a selection fingerprint."""
    digest = 2166136261
    encoded = text.encode("utf-16-le")
    for index in range(0, len(encoded), 2):
        digest ^= encoded[index] | (encoded[index + 1] << 8)
        digest = (digest * 16777619) & 0xFFFFFFFF
    return f"fnv1a-{digest:08x}"


def _validate_collab_patch_target(workdir: Path, event: dict[str, Any]) -> None:
    """Fail closed unless a semantic patch still addresses the exact text."""
    paragraph_id = str(event.get("paragraph_id") or event.get("target", {}).get("paragraph_id") or "")
    target = event.get("target")
    if not paragraph_id or not isinstance(target, dict):
        raise ToolError("patch-target", "semantic patch needs a paragraph and target")
    texts, styles = _draft_paragraph_state(workdir, paragraph_id, mode=session.mode)
    paragraph_text = "".join(texts)
    start = target.get("start_offset")
    end = target.get("end_offset")
    before = str(event.get("before", ""))
    expected = str(target.get("expected_text", ""))
    if not isinstance(start, int) or not isinstance(end, int) or start < 0 or end < start:
        raise ToolError("patch-range", "semantic patch offsets are invalid")
    if before != expected or paragraph_text[start:end] != before:
        raise ToolError("patch-precondition", f"{paragraph_id}: expected text no longer matches the current snapshot")
    if paragraph_text[max(0, start - 100):start] != str(target.get("left_context", "")):
        raise ToolError("patch-context-mismatch", f"{paragraph_id}: left context no longer matches the current snapshot")
    if paragraph_text[end:end + 100] != str(target.get("right_context", "")):
        raise ToolError("patch-context-mismatch", f"{paragraph_id}: right context no longer matches the current snapshot")
    paragraph_fingerprint = str(target.get("paragraph_fingerprint", ""))
    if paragraph_fingerprint and paragraph_fingerprint != _fnv1a_utf16(paragraph_text):
        raise ToolError("patch-fingerprint-mismatch", f"{paragraph_id}: paragraph fingerprint changed")
    region_fingerprint = str(target.get("region_fingerprint", ""))
    if region_fingerprint and region_fingerprint != _fnv1a_utf16(before):
        raise ToolError("patch-fingerprint-mismatch", f"{paragraph_id}: selected region fingerprint changed")
    style_region_ids = [str(item) for item in (target.get("style_region_ids") or [])]
    current_style_ids = list(dict.fromkeys(styles))
    if style_region_ids and style_region_ids != current_style_ids:
        raise ToolError("patch-style-mismatch", f"{paragraph_id}: style regions changed; re-read the paragraph")
    if before:
        offsets: list[int] = []
        cursor = 0
        for unit_text in texts:
            offsets.append(cursor)
            cursor += len(unit_text)
        offsets.append(cursor)
        i1 = max(i for i, offset in enumerate(offsets) if offset <= start)
        i2 = max(i for i, offset in enumerate(offsets) if offset < end)
        covered = set(styles[i1:i2 + 1])
        if len(covered) > 1:
            raise ToolError("cross-region-text", f"{paragraph_id}: patch covers multiple style regions")

def _apply_patch_to_draft(
    workdir: Path,
    event: dict[str, Any],
    *,
    validate: bool = True,
) -> None:
    if validate:
        _validate_collab_patch_target(workdir, event)
    paragraph_id = str(event["paragraph_id"])
    header, blocks = _read_edit(workdir)
    index = _find_block(blocks, "p", paragraph_id)
    marker = blocks[index].splitlines()[0]
    body = _block_body(blocks[index])
    target = event["target"]
    new_body = _replace_in_body(
        body,
        str(event["before"]),
        str(event["after"]),
        paragraph_id,
        start_offset=int(target["start_offset"]),
    )
    blocks[index] = marker + ("\n" + new_body if new_body else "")
    _write_edit(workdir, header, blocks)
    _refresh_regions(workdir)



def _style_info(workdir: Path, style_id: str) -> dict[str, Any]:
    registry = StyleRegistry.from_json(
        json.loads((workdir / "styles.json").read_text(encoding="utf-8"))
    )
    style = registry.styles.get(style_id)
    return {
        "style_id": style_id,
        "description": style.label if style else style_id,
        "rpr": style.rpr if style else None,
    }


def _merge_regions(texts: list[str], styles: list[str]) -> list[tuple[str, str]]:
    regions: list[tuple[str, str]] = []
    for text, style in zip(texts, styles):
        if regions and regions[-1][1] == style:
            regions[-1] = (regions[-1][0] + text, style)
        else:
            regions.append((text, style))
    return regions


def _resolve_region(edit: dict[str, Any], regions: list[tuple[str, str]], edit_no: int) -> int:
    if "region" in edit:
        index = edit["region"]
        if not isinstance(index, int) or index < 0 or index >= len(regions):
            raise ToolError(
                "region-out-of-range",
                f"edit {edit_no}: region {index} out of range (paragraph has {len(regions)} "
                "regions); re-read regions.md",
            )
        return index
    if "text" in edit:
        anchor = edit["text"]
        style_id = edit.get("style_id")
        matches = [
            index
            for index, region in enumerate(regions)
            if region[0] == anchor and (style_id is None or region[1] == style_id)
        ]
        if not matches:
            raise ToolError(
                "text-not-found",
                f"edit {edit_no}: region text {anchor!r} not found; re-read regions.md",
            )
        if len(matches) > 1:
            raise ToolError(
                "text-ambiguous",
                f"edit {edit_no}: region text {anchor!r} appears {len(matches)} times; "
                "add style_id to disambiguate",
            )
        return matches[0]
    raise ToolError("invalid-edit", f"edit {edit_no}: must specify 'region' index or 'text' anchor")


def _apply_batch_to_body(
    body: str,
    regions: list[tuple[str, str]],
    resolved: list[tuple[int, str | None, str]],
) -> str:
    """Apply region edits without rebuilding across protected tokens."""
    chunks = _split_chunks(body)
    text_chunks = [
        _validate_escaped_prose(raw) for kind, raw in chunks if kind == "text"
    ]
    if "".join(text_chunks) != "".join(region[0] for region in regions):
        raise ToolError(
            "draft-invalid",
            "draft text structure does not match the region view; re-read regions.md",
        )

    region_bounds: list[tuple[int, int]] = []
    cursor = 0
    for region_text, _ in regions:
        region_bounds.append((cursor, cursor + len(region_text)))
        cursor += len(region_text)

    chunk_bounds: list[tuple[int, int]] = []
    cursor = 0
    for chunk_text in text_chunks:
        chunk_bounds.append((cursor, cursor + len(chunk_text)))
        cursor += len(chunk_text)
    def map_boundary(text: str, replacement: str, position: int) -> int:
        for tag, i1, i2, j1, j2 in SequenceMatcher(
            None, text, replacement, autojunk=False
        ).get_opcodes():
            if i1 <= position <= i2:
                if tag == "equal":
                    return j1 + position - i1
                if position == i1:
                    return j1
                if position == i2:
                    return j2
                return j1 + (j2 - j1) * (position - i1) // max(1, i2 - i1)
        return len(replacement)


    pending: list[tuple[int, int, str, str]] = []
    for region_index, old, new in resolved:
        region_start, region_end = region_bounds[region_index]
        overlapping = [
            chunk_index
            for chunk_index, (chunk_start, chunk_end) in enumerate(chunk_bounds)
            if chunk_start < region_end and chunk_end > region_start
        ]
        if not overlapping:
            raise ToolError(
                "text-not-found",
                f"edit on region {region_index}: region has no editable text",
            )

        if old is None:
            if len(overlapping) == 1:
                chunk_index = overlapping[0]
                chunk_start, _ = chunk_bounds[chunk_index]
                pending.append(
                    (
                        chunk_index,
                        region_start - chunk_start,
                        text_chunks[chunk_index][
                            region_start - chunk_start : region_end - chunk_start
                        ],
                        new,
                    )
                )
                continue

            old_region = "".join(
                text_chunks[chunk_index][
                    max(region_start, chunk_bounds[chunk_index][0])
                    - chunk_bounds[chunk_index][0] : min(
                        region_end, chunk_bounds[chunk_index][1]
                    )
                    - chunk_bounds[chunk_index][0]
                ]
                for chunk_index in overlapping
            )
            for chunk_index in overlapping:
                chunk_start, chunk_end = chunk_bounds[chunk_index]
                old_start = max(region_start, chunk_start) - region_start
                old_end = min(region_end, chunk_end) - region_start
                new_start = map_boundary(old_region, new, old_start)
                new_end = map_boundary(old_region, new, old_end)
                local_start = max(region_start, chunk_start) - chunk_start
                old_text = text_chunks[chunk_index][
                    local_start : local_start + old_end - old_start
                ]
                pending.append(
                    (
                        chunk_index,
                        local_start,
                        old_text,
                        new[new_start:new_end],
                    )
                )
            continue

        matches: list[tuple[int, int]] = []
        for chunk_index in overlapping:
            chunk_start, chunk_end = chunk_bounds[chunk_index]
            allowed_start = max(region_start, chunk_start) - chunk_start
            allowed_end = min(region_end, chunk_end) - chunk_start
            cursor = allowed_start
            while True:
                found = text_chunks[chunk_index].find(old, cursor, allowed_end)
                if found < 0:
                    break
                matches.append((chunk_index, found))
                cursor = found + max(1, len(old))
        if len(matches) == 0:
            raise ToolError(
                "text-not-found",
                f"edit on region {region_index}: text {old!r} not found in that region",
            )
        if len(matches) > 1:
            raise ToolError(
                "text-ambiguous",
                f"edit on region {region_index}: text {old!r} appears "
                f"{len(matches)} times in that region; provide a longer context",
            )
        chunk_index, found = matches[0]
        pending.append((chunk_index, found, old, new))

    updated = list(text_chunks)
    for chunk_index, found, old, new in sorted(
        pending, key=lambda item: (item[0], item[1]), reverse=True
    ):
        updated[chunk_index] = (
            updated[chunk_index][:found]
            + new
            + updated[chunk_index][found + len(old) :]
        )

    out: list[str] = []
    text_index = 0
    for kind, raw in chunks:
        if kind == "token":
            out.append(raw)
        else:
            out.append(_escape_prose(updated[text_index]))
            text_index += 1
    return "".join(out)


def _refresh_regions(workdir: Path) -> None:
    """Best-effort rewrite of regions.md from the current draft (dry-run)."""
    try:
        typed = parse_typed((workdir / "typed.md").read_text(encoding="utf-8"))
        state = classify_edit_state(workdir)
        if state["state"] == "clean":
            document = typed
        else:
            from .edit import parse_edit_projection

            projection = parse_edit_projection((workdir / PROJECTION_FILE).read_text(encoding="utf-8"))
            format_data = json.loads((workdir / "format.json").read_text(encoding="utf-8"))
            plan = plan_sync(typed, projection, format_data)
            document = plan.document
        styles = StyleRegistry.from_json(
            json.loads((workdir / "styles.json").read_text(encoding="utf-8"))
        )
        (workdir / "regions.md").write_text(
            render_regions_md(document, styles), encoding="utf-8", newline="\n"
        )
    except Exception:
        pass  # derived view; edit.md remains the source of truth


def _region_labels(texts: list[str], styles: list[str]) -> list[str]:
    regions: list[tuple[str, str]] = []
    for unit_text, style in zip(texts, styles):
        if regions and regions[-1][1] == style:
            regions[-1] = (regions[-1][0] + unit_text, style)
        else:
            regions.append((unit_text, style))
    return [f"{text}[{style[:8]}]" for text, style in regions]


# --------------------------------------------------------------------------
# Tools
# --------------------------------------------------------------------------

@mcp.tool()
def engine_info() -> dict[str, Any]:
    """Return the Protocol-major-1 engine descriptor before any workdir opens.

    ``capabilities`` is the STATIC engine manifest (supported / conditional /
    unsupported, with the fallback for anything closed); for the current
    document's view use ``document_read(view="capabilities")``."""
    descriptor = dict(engine_descriptor())
    descriptor["capabilities"] = _STATIC_CAPABILITIES
    return descriptor


def workdir_open(workdir: str, author: str | None = None, track: bool | None = None) -> str:
    """Open a typed workdir as the session document; validates it and reports
    its freshness state and effective edit mode. Call once before any other
    tool. ``author`` sets the session revision author (fallback:
    $DOCX2TYPED_AUTHOR, then "Unknown"). ``track`` explicitly selects
    tracked (True) or direct (False) edits; None infers from the three-field
    state (source_track_enabled + pending revisions)."""
    with session.lock:
        path = Path(workdir).resolve()
        if not path.is_dir() or not (path / "typed.md").exists():
            raise ToolError("workdir-not-found", f"not a typed workdir: {path}")
        validate_workdir(path)
        state = classify_edit_state(path)
        format_data = json.loads((path / "format.json").read_text(encoding="utf-8"))
        typed = parse_typed((path / "typed.md").read_text(encoding="utf-8"))
        from .edit_sync import _document_has_revisions
        from .typed_core import effective_edit_mode

        mode = effective_edit_mode(
            source_track_enabled=bool(format_data.get("source_track_enabled")),
            has_pending_revisions=_document_has_revisions(typed),
            explicit=("track" if track else "direct") if track is not None else None,
        )
        collaboration = document_state(path)
        session.workdir = path
        session.author = author
        session.track_override = track
        session.mode = mode
        _remember_workdir(path)
        return _json(
            {
                "workdir": str(path),
                "state": state["state"],
                "edit_mode": mode,
                "author": author,
                "paragraphs": len(typed.paragraphs),
                "current_snapshot": collaboration["current_snapshot"],
                "staged_snapshot": collaboration["staged_snapshot"],
                "current_matches_filesystem": collaboration["current_matches_filesystem"],
            }
        )


@mcp.tool(name="workdir_open")
def _workdir_open_result(
    workdir: str,
    author: str | None = None,
    track: bool | None = None,
    contract_ranges: dict[str, dict[str, int]] | None = None,
    supported_features: list[str] | None = None,
    required_features: list[str] | None = None,
) -> dict[str, Any]:
    """Negotiate Protocol major 1, then open one validated workdir for this connection."""
    try:
        negotiate(contract_ranges, supported_features, required_features)
    except ProtocolMismatch as exc:
        envelope = result_envelope(
            "workdir_open",
            "failure",
            diagnostics=[
                diagnostic(
                    exc.code,
                    str(exc),
                    details=exc.details,
                    next_actions=["upgrade the incompatible client or engine"],
                )
            ],
        )
        return mcp_result(envelope, is_error=True)  # type: ignore[return-value]
    with session.lock:
        try:
            manifest = derived_workdir_manifest(workdir)
            opened = json.loads(workdir_open(workdir, author=author, track=track))
        except ToolError as exc:
            failure = diagnostic(exc.code, exc.detail)
        except FileNotFoundError as exc:
            failure = diagnostic("workdir-not-found", str(exc))
        except PermissionError as exc:
            failure = diagnostic("workdir-unreadable", str(exc))
        except (zipfile.BadZipFile, ValidationError, TypedError) as exc:
            failure = diagnostic(_domain_code(str(exc)), str(exc))
        except OSError as exc:
            failure = diagnostic("workdir-unreadable", str(exc))
        else:
            data = {
                "session": {
                    "schema": "docx2typed-session-descriptor-1",
                    "workdir": typed_path(opened["workdir"]),
                    "workdir_manifest_sha256": semantic_sha256(manifest),
                    "freshness": opened["state"],
                    "effective_mode": opened["edit_mode"],
                    "author": opened["author"],
                    "paragraphs": opened["paragraphs"],
                    "snapshot": {
                        "current": opened["current_snapshot"],
                        "staged": opened["staged_snapshot"],
                    },
                    "cas": {
                        "current_matches_filesystem": opened["current_matches_filesystem"],
                    },
                    "supported_tools": engine_descriptor()["tools"],
                }
            }
            envelope = result_envelope("workdir_open", "success", data=data)
            return mcp_result(envelope)  # type: ignore[return-value]
        envelope = result_envelope(
            "workdir_open",
            "failure",
            diagnostics=[failure],
        )
        return mcp_result(envelope, is_error=True)  # type: ignore[return-value]


@mcp.tool()
def workdir_status() -> str:
    """Freshness of the opened workdir plus the version HEAD names.

    ``draft_dirty`` is the projection drifting from canonical;
    ``version.dirty`` is canonical drifting from the saved version. They are
    different facts and both are reported (ADR 0044)."""
    with session.lock:
        workdir = session.require()
        state = classify_edit_state(workdir)
        head = store_head_version(workdir)
        history = store_history_list(workdir, limit=2)
        previous = next(
            (item.get("version") for item in history["versions"] if item.get("version") != head["version"]),
            None,
        )
        return _json(
            {
                "state": state["state"],
                "edit_body_sha256": state["edit_body_sha256"],
                "draft_dirty": state["state"] in {"dirty", "conflict"},
                "version": {
                    "current": head["version"],
                    "previous": previous,
                    "seq": head["seq"],
                    "tree": head["tree"],
                    "dirty": head["dirty"],
                },
                "versions": {
                    "total": history["total"],
                    "retained": history["retained"],
                    "trimmed": history["trimmed"],
                },
            }
        )


@mcp.tool()
def list_paragraphs() -> str:
    """    Advanced fallback lane: prefer document_read / document_search /
    document_patch (the default editing surface); use this tool only for
    exact per-region style ownership, diagnosis, or recovery.
    List draft paragraphs: id, visible-text summary, token count, deletions."""
    with session.lock:
        workdir = session.require()
        header, blocks = _read_edit(workdir)
        paragraphs: list[dict[str, Any]] = []
        for block in blocks:
            marker = block.splitlines()[0].strip()
            match = re.match(r'<!--@(p|new) (?:id|temp)="([^"]+)"', marker)
            if match:
                body = _block_body(block)
                visible = _visible_text(body)
                paragraphs.append(
                    {
                        "id": match.group(2),
                        "kind": match.group(1),
                        "summary": visible[:60],
                        "chars": len(visible),
                        "tokens": body.count("\u27e6"),
                        "deleted": False,
                    }
                )
                continue
            match = re.match(r'<!--@delete id="([^"]+)"', marker)
            if match:
                paragraphs.append(
                    {"id": match.group(1), "kind": "delete", "summary": "[deleted]", "chars": 0, "tokens": 0, "deleted": True}
                )
        return _json({"paragraphs": paragraphs})


@mcp.tool()
def get_paragraph(paragraph_id: str) -> str:
    """    Advanced fallback lane: prefer document_read / document_search /
    document_patch (the default editing surface); use this tool only for
    exact per-region style ownership, diagnosis, or recovery.
    Read one paragraph: the draft text and its style regions. Editing is
    region-scoped — replace_text rejects old text spanning regions, and
    batch_edit addresses regions by index — so use the styles array (or
    regions.md) to plan separate edits per region. style_id is authoritative:
    equal style_id = identical formatting; rpr holds the full canonical XML
    (translate it with docs/rpr-reference.md)."""
    with session.lock:
        workdir = session.require()
        header, blocks = _read_edit(workdir)
        index = _find_block(blocks, "p", paragraph_id)
        body = _block_body(blocks[index])
        texts, styles = _draft_paragraph_state(workdir, paragraph_id, mode=session.mode)
        regions: list[dict[str, Any]] = []
        for unit_text, style in zip(texts, styles):
            if regions and regions[-1]["style_id"] == style:
                regions[-1]["text"] += unit_text
            else:
                regions.append({"text": unit_text, **_style_info(workdir, style)})
        return _json(
            {
                "paragraph_id": paragraph_id,
                "text": body,
                "plain": _visible_text(body),
                "tokens": body.count("\u27e6"),
                "styles": regions,
            }
        )


@mcp.tool()
def replace_text(paragraph_id: str, old: str, new: str, operation_id: str | None = None, allow_comment_text: bool = False) -> CallToolResult:
    """    Advanced fallback lane: prefer document_read / document_search /
    document_patch (the default editing surface); use this tool only for
    exact per-region style ownership, diagnosis, or recovery.
    Replace exactly one occurrence of visible text in a paragraph draft.

    Contract: ``old`` must be unique in the paragraph AND cover a single
    style region (see get_paragraph styles). Text crossing style regions is
    rejected with the region boundaries — edit each region separately, e.g.
    replace_text(P0, '智能响应', '新词') then replace_text(P0, 'ABC', 'XYZ').
    Style ownership is decided by the engine with zero guessing: the region's
    style is preserved, insertions follow the caret context.

    Mutating: ``operation_id`` is optional; identical retries
    replay the original result, changed input fails operation-id-reused.
    Writes the draft only — run diff_preview then commit_sync."""
    with session.lock:
        if session.workdir is None:
            return _failure_result("replace_text", "workdir-not-open", "no workdir open; call workdir_open first", operation_id=operation_id)
        workdir = session.workdir
        manifest_before = _workdir_manifest_sha256(workdir)

        def run(target, tx=None):
            _require_comment_text_opt_in(paragraph_id, allow_comment_text)
            header, blocks = _read_edit(target)
            index = _find_block(blocks, "p", paragraph_id)
            marker = blocks[index].splitlines()[0]
            body = _block_body(blocks[index])
            texts, styles = _draft_paragraph_state(target, paragraph_id, mode=session.mode)
            _check_single_region(target, paragraph_id, old, texts, styles)
            new_body = _replace_in_body(body, old, new, paragraph_id)
            blocks[index] = marker + ("\n" + new_body if new_body else "")
            _write_edit(target, header, blocks)
            _refresh_regions(target)
            payload = {
                **base_evidence_payload(),
                "inputs": {"workdir": {"manifest_sha256": manifest_before}},
                "outputs": {"workdir": {"manifest_sha256": _workdir_manifest_sha256(target)}},
                "checks": [{"name": "draft-replaced", "status": "pass"}],
            }
            return (
                "success",
                {
                    "paragraph_id": paragraph_id,
                    "draft": "dirty",
                    "next": "diff_preview to inspect style ownership, then commit_sync",
                },
                "mutation",
                payload,
                [],
            )

        return _mutation_tool(
            operation_id,
            "replace_text",
            {
                "workdir": str(workdir),
                "paragraph_id": paragraph_id,
                "old": old,
                "new": new,
                "allow_comment_text": allow_comment_text,
            },
            workdir,
            directory=True,
            evidence_path=workdir / "run.evidence.json",
            run=run,
            store_workdir=workdir,
            preflight_scope=[paragraph_id],
        )


# --------------------------------------------------------------------------
# Capability manifest: what the engine can do (static) and what THIS document
# can safely do right now (document-level). Unsupported refusals name the
# capability id so an agent never has to guess why a lane is closed.
# --------------------------------------------------------------------------

_STATIC_CAPABILITIES: list[dict[str, Any]] = [
    {"capability": "word.text.replace", "support": "supported", "tools": ["document_patch", "document_replace", "replace_text"]},
    {"capability": "word.text.replace.tolerant-matching", "support": "supported", "notes": "width/punctuation/space variants folded; 1:1 mapping back to the document text"},
    {"capability": "word.text.replace.cross-revision-boundary", "support": "unsupported", "reason": "revision-ownership-not-decidable", "current_fallback": "split-into-region-scoped-hunks"},
    {"capability": "word.text.replace.comment", "support": "conditional", "requires": ["allow_comment_text=true"], "reason": "annotation-content-policy"},
    {"capability": "word.revision.edit-deleted-text", "support": "unsupported", "reason": "deleted-text-is-not-visible-text", "current_fallback": "settle-revision-then-edit"},
    {"capability": "word.revision.settle", "support": "supported", "tools": ["accept_revision", "reject_revision", "decide_all", "review_settle"]},
    {"capability": "word.revision.edit-within-revision", "support": "conditional", "requires": ["track=true (direct mode refuses)"], "reason": "revision-ownership"},
    {"capability": "word.revision.edit-inside-revision", "support": "supported", "notes": "nested revisions round-trip since the marker-binding fix (issue #83)"},
    {"capability": "word.save.rebuild-audit", "support": "supported", "notes": "commit_sync rehearses sync+build on a throwaway copy and refuses an unreproducible state before publishing"},
    {"capability": "word.format.run-properties", "support": "conditional", "requires": ["variant-present-in-source-document"], "tools": ["format_span"], "reason": "styles-must-mirror-the-source"},
    {"capability": "word.format.tracked-properties", "support": "unsupported", "reason": "rpr-change-not-native-yet", "current_fallback": "tracked-del-ins", "fidelity": "semantic-approximation", "issue": "#82"},
    {"capability": "word.structure.paragraph-insert-delete", "support": "supported", "tools": ["insert_paragraph", "delete_paragraph", "document_patch"]},
    {"capability": "word.structure.table-topology", "support": "supported", "tools": ["table_insert_row", "table_delete_row", "table_insert_col", "table_delete_col", "table_merge_cells", "table_split_cells"]},
    {"capability": "word.comment.delete", "support": "supported", "tools": ["delete_comment"]},
    {"capability": "word.container.header-footer-notes-boxes", "support": "supported", "notes": "paragraph text inside parts is editable like body text"},
    {"capability": "word.diagnostics.issues", "support": "supported", "tools": ["document_read(view=issues)"]},
    {"capability": "word.render.preview", "support": "unsupported", "reason": "no-render-pipeline", "current_fallback": "verify_output(structure/text/styles)"},
]

_CAPABILITY_BY_ID = {entry["capability"]: entry for entry in _STATIC_CAPABILITIES}


def _document_capabilities(workdir: Path) -> dict[str, Any]:
    """What THIS document can do right now: revision boundaries close the
    single-hunk lane, comment parts gate on opt-in, and format variants are
    only available when the source already carries them."""
    typed = parse_typed((workdir / "typed.md").read_text(encoding="utf-8"))
    styles_data = json.loads((workdir / "styles.json").read_text(encoding="utf-8"))
    state = classify_edit_state(workdir)
    revision_count = 0
    deletion_count = 0
    boundary_count = 0  # ANY revision container creates a boundary (ins too)
    comment_paragraphs = 0
    for paragraph in typed.paragraphs:
        if paragraph.paragraph_id.startswith("comments."):
            comment_paragraphs += 1
        for node in _iter_revision_nodes(paragraph.nodes):
            boundary_count += 1
            if node.kind == "insert":
                revision_count += 1
            elif node.kind in ("delete", "move_from"):
                deletion_count += 1
    variants = {
        "superscript": any("superscript" in str(value.get("features", {})) or "superscript" in str(value.get("label", "")) for value in styles_data["styles"].values()),
        "subscript": any("subscript" in str(value.get("features", {})) or "subscript" in str(value.get("label", "")) for value in styles_data["styles"].values()),
        "bold": any("bold" in str(value.get("features", {})) or "bold" in str(value.get("label", "")) for value in styles_data["styles"].values()),
    }
    entries: list[dict[str, Any]] = []
    for entry in _STATIC_CAPABILITIES:
        capability = entry["capability"]
        current = {"capability": capability, "support": entry["support"]}
        if capability == "word.text.replace.cross-revision-boundary" and boundary_count == 0:
            current["support"] = "supported"
            current["note"] = "this document has no revision containers, so no boundary can be crossed"
        elif capability == "word.format.run-properties":
            available = sorted(name for name, present in variants.items() if present)
            current["support"] = "conditional" if available else "unsupported"
            current["available_variants"] = available
            if not available:
                current["reason"] = "no-run-property-variant-in-source"
        elif capability == "word.text.replace.comment" and comment_paragraphs == 0:
            current["support"] = "not-applicable"
        elif capability == "word.format.tracked-properties" and (session.mode or "direct") == "direct":
            current["note"] = "direct-mode session: formatting applies without revisions"
        entries.append(current)
    return {
        "document": {
            "revision_before": state["edit_body_sha256"],
            "state": state["state"],
            "mode": session.mode or "unknown",
            "revision_containers": {"total": boundary_count, "insert": revision_count, "delete": deletion_count},
            "comment_paragraphs": comment_paragraphs,
            "format_variants": variants,
        },
        "capabilities": entries,
        "note": "static engine capability lives in engine_info().capabilities; this is the current-document view",
    }


def _iter_revision_nodes(nodes: list[Any]) -> Any:
    for node in nodes:
        if isinstance(node, RevisionNode):
            yield node
        if isinstance(node, (RevisionNode, RangeNode)):
            yield from _iter_revision_nodes(node.children)


def _document_structure(blocks: list[str]) -> list[dict[str, Any]]:
    """Compact section index: contiguous paragraph-id ranges per document part
    (body, headers, footnotes, comments, each table). Returned by document_read
    so "which section is this paragraph in" never needs a second read."""
    groups: list[dict[str, Any]] = []
    for block in blocks:
        ident = _block_ident(block)
        if not ident or ident[0] != "p":
            continue
        paragraph_id = ident[1]
        part = "body" if re.fullmatch(r"P\d+", paragraph_id) else paragraph_id.split(".")[0]
        if groups and groups[-1]["part"] == part:
            groups[-1]["to"] = paragraph_id
            groups[-1]["paragraphs"] += 1
        else:
            groups.append({"part": part, "from": paragraph_id, "to": paragraph_id, "paragraphs": 1})
    return groups


def _document_issues(workdir: Path) -> dict[str, Any]:
    """Cheap, deterministic document checks an editor would run before/after a
    pass: missing superscripts on element charges, mixed punctuation width,
    repeated full-name(first-use abbreviation) definitions, and comment
    anchors trapped inside tracked deletions. Each issue carries the paragraph
    id, the evidence, and (where a tool can fix it) a ready-to-send fix."""
    from .typed_core import Style as _Style

    typed = parse_typed((workdir / "typed.md").read_text(encoding="utf-8"))
    styles_data = json.loads((workdir / "styles.json").read_text(encoding="utf-8"))
    registry = StyleRegistry(
        {
            key: _Style(style_id=key, rpr=value["rPr"], canonical=value["canonical"], label=value.get("label", ""), features=value.get("features", {}))
            for key, value in styles_data["styles"].items()
        }
    )
    issues: list[dict[str, Any]] = []
    full_name_hits: dict[str, list[str]] = {}
    for paragraph in typed.paragraphs:
        units = flatten_paragraph(paragraph)
        texts = [unit.value[1] for unit in units if not unit.token]
        styles = [unit.style for unit in units if not unit.token]
        paragraph_text = "".join(texts)
        # 1. element charge that lost its superscript (Cu2+, Mn2+, Ca2+ …)
        for text, style in _merge_regions(texts, styles):
            features = registry.styles[style].features if style in registry.styles else {}
            if features.get("vertAlign") == "superscript":
                continue
            for match in re.finditer(r"[A-Z][a-z]?\d[+-]", text):
                charge = match.group(0)
                issues.append(
                    {
                        "kind": "element-charge-not-superscript",
                        "paragraph_id": paragraph.paragraph_id,
                        "evidence": f"{charge!r} in {text[max(0, match.start() - 12):match.end() + 8]!r}",
                        "fix": {
                            "tool": "format_span",
                            "paragraph_id": paragraph.paragraph_id,
                            "old": text,
                            "attributes": {"vertAlign": "superscript"},
                        },
                    }
                )
        # 2. punctuation width mixed inside one paragraph
        ascii_punct = [ch for ch in ",.;:()" if ch in paragraph_text]
        cjk_punct = [ch for ch in "，。、；：（）" if ch in paragraph_text]
        if ascii_punct and cjk_punct:
            issues.append(
                {
                    "kind": "punctuation-width-mixed",
                    "paragraph_id": paragraph.paragraph_id,
                    "evidence": f"ascii={''.join(ascii_punct)!r} cjk={''.join(cjk_punct)!r} in {paragraph_text[:60]!r}",
                    "note": "matching tolerates this, but the document text itself is inconsistent",
                }
            )
        # 3. full name (ABBR) defined more than once in the document
        for match in re.finditer(r"[\u4e00-\u9fff]{2,20}（([A-Za-z][A-Za-z0-9\-]{1,9})）", paragraph_text):
            full_name_hits.setdefault(match.group(1), []).append(paragraph.paragraph_id)
    for abbreviation, paragraph_ids in full_name_hits.items():
        if len(paragraph_ids) > 1:
            issues.append(
                {
                    "kind": "full-name-defined-repeatedly",
                    "paragraph_id": paragraph_ids[0],
                    "evidence": f"{abbreviation!r} defined {len(paragraph_ids)} times",
                    "paragraph_ids": paragraph_ids,
                    "note": "the full name should appear once; later uses keep only the abbreviation",
                }
            )
    # 4. comment anchors trapped inside a tracked deletion
    def scan_comments(nodes: list[Any], paragraph_id: str) -> None:
        for node in nodes:
            if isinstance(node, RevisionNode) and node.kind in ("delete", "move_from"):
                trapped = [child for child in node.children if isinstance(child, InlineNode) and "comment" in str(child.kind)]
                if trapped:
                    issues.append(
                        {
                            "kind": "comment-anchor-inside-deletion",
                            "paragraph_id": paragraph_id,
                            "evidence": f"{len(trapped)} comment anchor(s) inside w:del w:id={node.attrs.get('w:id')}",
                            "note": "settling this revision would drop the comment anchor; keep the comment or settle first",
                        }
                    )
            if isinstance(node, (RevisionNode, RangeNode)):
                scan_comments(node.children, paragraph_id)

    for paragraph in typed.paragraphs:
        scan_comments(paragraph.nodes, paragraph.paragraph_id)

    counts: dict[str, int] = {}
    for issue in issues:
        counts[issue["kind"]] = counts.get(issue["kind"], 0) + 1
    return {
        "issues": issues,
        "counts": counts,
        "note": (
            "document-level checks over the COMMITTED text; fix entries are ready-to-send "
            "tool calls (format_span for superscripts)"
        ),
    }


@mcp.tool()
def document_read(
    anchor: str | None = None,
    before: int = 5,
    after: int = 8,
    view: str = "auto",
) -> dict[str, Any]:
    """Read the editable projection as one virtual text file. Returns
    {"view", "revision", "state", "paragraphs", "content", "first_id",
    "last_id"} — ``content`` is the exact virtual-file text and doubles as
    the base for document_patch's unified-diff form (frozen invariant:
    document_read.content == diff base). ``revision`` is the opaque token
    to pass back as document_patch's base_revision.

    - view="auto" (default): full content for small documents, outline for
      large ones (>40k chars of projection).
    - view="content": the projection verbatim; with ``anchor`` (a paragraph
      id from a previous read/search) a ``before``/``after`` block window.
      Window content is still a valid diff base — hunks are located by
      marker id + exact body, never by line numbers.
    - view="outline": one line per paragraph orientation map.
    - view="issues": deterministic document checks (element charges missing a
      superscript, mixed punctuation width, a full name defined more than
      once, comment anchors trapped in tracked deletions). Each issue names
      the paragraph and, where possible, ships a ready-to-send fix.
    - view="spans" (requires ``anchor``): the paragraph's editable-span map —
      the maximal visible runs document_patch can match, cut at
      revision-control boundaries and style-region edges. Copy one span's
      ``text`` verbatim as ``old``; a run crossing two spans is refused.
      Refusals from document_patch/replace_text carry the same map in
      ``data.span_map``, so a failed patch is self-diagnosing.

    Locks, opaque placeholders, and revision gaps render read-only —
    patches that touch them fail closed. Read-only; never mutates the
    workdir."""
    if view not in ("content", "outline", "auto", "spans", "issues", "capabilities"):
        raise ToolError("document-read-invalid-view", f"view must be content, outline, auto, spans, issues, or capabilities, got {view!r}")
    with session.lock:
        workdir = session.require()
        state = classify_edit_state(workdir)
        header, blocks = _read_edit(workdir)
        full_text = header + "\n\n" + "\n\n".join(blocks) + "\n"
        selected = blocks
        if anchor is not None:
            index = next(
                (i for i, block in enumerate(blocks) if (_block_ident(block) or ("", ""))[1] == anchor),
                None,
            )
            if index is None:
                raise ToolError("paragraph-not-found", f"anchor {anchor} not found in the draft")
            selected = blocks[max(0, index - before) : index + after + 1]
        if view == "capabilities":
            return {
                "view": "capabilities",
                "revision": state["edit_body_sha256"],
                "state": state["state"],
                "paragraphs": len(blocks),
                **_document_capabilities(workdir),
            }
        if view == "issues":
            return {
                "view": "issues",
                "revision": state["edit_body_sha256"],
                "state": state["state"],
                "paragraphs": len(blocks),
                **_document_issues(workdir),
            }
        if view == "spans":
            if anchor is None:
                raise ToolError("document-read-invalid-view", "view=spans requires an anchor paragraph id")
            index = next(
                (i for i, block in enumerate(blocks) if (_block_ident(block) or ("", ""))[1] == anchor),
                None,
            )
            if index is None:
                raise ToolError("paragraph-not-found", f"anchor {anchor} not found in the draft")
            body = _block_body(blocks[index])
            try:
                texts, styles = _draft_paragraph_state(workdir, anchor, mode=session.mode)
            except ToolError:
                texts, styles = None, None
            return {
                "view": "spans",
                "revision": state["edit_body_sha256"],
                "state": state["state"],
                "paragraphs": len(blocks),
                "span_map": _span_map_from(anchor, body, texts, styles),
            }
        if view == "auto":
            view = "outline" if len(full_text) > 40_000 else "content"
        if view == "outline":
            lines: list[str] = [header]
            for block in selected:
                ident = _block_ident(block)
                body = _block_body(block)
                visible = _visible_text(body)
                ident_text = f'<!--@{ident[0]} id="{ident[1]}"-->' if ident else ""
                lines.append(f"{ident_text} {visible[:80]}{'…' if len(visible) > 80 else ''} [{len(visible)} chars]")
            content = "\n".join(lines) + "\n"
        elif anchor is not None:
            content = header + "\n\n" + "\n\n".join(selected) + "\n"
        else:
            content = full_text
        idents = [_block_ident(b) for b in (selected if anchor is not None or view == "outline" else blocks)]
        idents = [i for i in idents if i]
        return {
            "view": view,
            "revision": state["edit_body_sha256"],
            "state": state["state"],
            "structure": _document_structure(blocks),
            "paragraphs": len(blocks),
            "content": content,
            "first_id": idents[0][1] if idents else None,
            "last_id": idents[-1][1] if idents else None,
            "windowed": anchor is not None,
        }


@mcp.tool()
def document_search(
    query: str,
    context_chars: int = 3000,
    case_sensitive: bool = False,
    limit: int = 20,
    offset: int = 0,
    scope: str = "all",
) -> str:
    """Full-text search over the editable projection. Returns each match as
    the enclosing paragraph block with its <!--@p id="..."> anchor (full
    text when it fits in ``context_chars``, otherwise windows around each
    occurrence) plus neighbor ids for document_read windows. Not a hit list
    of ids — read the returned blocks as document context.

    Matching runs on the TOKEN-FREE visible text, so inline structure markers
    (revision edges, comment references, bookmarks, rPr changes) never break a
    query that reads as continuous; width/punctuation variants are tolerated
    the same way patches tolerate them.

    A hit is honest about its write contract: ``patchable_as_single_hunk`` is
    true only when the match lies inside ONE revision region — then
    ``matched_text`` is a ready-to-send patch ``old``. A match that crosses
    revision boundaries is still returned (it is valuable context) with
    ``patchable_as_single_hunk=false``, the crossed markers in
    ``boundary_crossings``, the touched ``span_indices``, and
    ``region="mixed"``.

    ``scope`` narrows the search the same way document_replace does (all by
    default; body / comments / a part prefix / one paragraph id), and
    ``offset`` pages through matching paragraphs (``total_blocks`` says how
    many there are) so a long document can be walked without guessing.

    Read-only; never mutates the workdir."""
    if not query:
        raise ToolError("document-search-empty-query", "query must not be empty")
    with session.lock:
        workdir = session.require()
        state = classify_edit_state(workdir)
        _, blocks = _read_edit(workdir)
        allowed_ids = set(_scope_paragraph_ids(workdir, scope))
        needle = query if case_sensitive else query.lower()
        entries: list[dict[str, Any]] = []
        total = 0
        for index, block in enumerate(blocks):
            ident = _block_ident(block)
            if ident is None or ident[0] != "p" or ident[1] not in allowed_ids:
                continue
            # Search the TOKEN-FREE visible text (the same coordinate system
            # patches match in) so inline markers (revision edges, comment
            # refs, bookmarks) never break a query that reads as continuous.
            flat, boundaries = _body_boundaries(_block_body(block))
            haystack = flat if case_sensitive else flat.lower()
            folded_haystack = _fold_text(haystack)
            needle = query if case_sensitive else query.lower()
            folded_needle = _fold_text(needle)
            offsets: list[int] = []
            # exact pass first, then the folded pass (folding is 1:1, so folded
            # offsets map straight back onto the visible text)
            for candidate, hay in ((needle, haystack), (folded_needle, folded_haystack)):
                if not candidate:
                    continue
                cursor = hay.find(candidate)
                while cursor != -1:
                    if cursor not in offsets:
                        offsets.append(cursor)
                    cursor = hay.find(candidate, cursor + 1)
                if offsets:
                    break
            offsets.sort()
            total += len(offsets)
            if not offsets:
                continue
            hit_length = len(needle)
            if len(flat) <= context_chars:
                excerpt = flat
            else:
                spans: list[str] = []
                for hit_offset in offsets:  # never reuse `offset`: that is the paging argument
                    half = context_chars // 2
                    window_start = max(0, hit_offset - half)
                    window_end = min(len(flat), hit_offset + hit_length + half)
                    spans.append(("…" if window_start else "") + flat[window_start:window_end] + ("…" if window_end < len(flat) else ""))
                excerpt = "\n⋯\n".join(spans)
            span_map_for_block = _span_map_from(ident[1], _block_body(block))
            occurrences: list[dict[str, Any]] = []
            for occurrence_offset in offsets:
                occ_text = flat[occurrence_offset : occurrence_offset + hit_length]
                occ_end = occurrence_offset + hit_length
                occ_crossed = sorted(
                    {kind for boundary_offset, kind in boundaries if occurrence_offset < boundary_offset < occ_end}
                )
                occ_regions = sorted(
                    {_region_at(boundaries, occurrence_offset), _region_at(boundaries, max(occurrence_offset, occ_end - 1))}
                )
                occurrence = {
                    "offset": occurrence_offset,
                    "matched_text": occ_text,
                    "region": "mixed" if occ_crossed else occ_regions[0],
                    "patchable_as_single_hunk": not occ_crossed,
                    "boundary_crossings": occ_crossed,
                    "normalized": occ_text != (query if case_sensitive else query),
                    # a ready address for THIS occurrence, so "the Nth one"
                    # needs no further hunting
                    "match_ref": _encode_match_ref(ident[1], occurrence_offset, occ_end, occ_text, state["edit_body_sha256"]),
                }
                if occ_crossed:
                    # the occurrence spans regions, so ITS ref will be refused:
                    # hand over the sub-spans that are each patchable, ready to
                    # send, instead of making the caller earn the refusal
                    occurrence["patchable_spans"] = [
                        {
                            "text": span["text"],
                            "match_ref": _encode_match_ref(
                                ident[1], span["start"], span["end"], span["text"], state["edit_body_sha256"]
                            ),
                        }
                        for span in span_map_for_block.get("spans", [])
                        if span["start"] >= occurrence_offset and span["end"] <= occ_end and span["text"]
                    ]
                occurrences.append(occurrence)
            first = occurrences[0]
            matched_text = first["matched_text"]
            hit_start, hit_end = first["offset"], first["offset"] + hit_length
            crossed = first["boundary_crossings"]
            region = first["region"]
            span_indices = [
                span["index"]
                for span in span_map_for_block.get("spans", [])
                if span["start"] < hit_end and span["end"] > hit_start
            ]
            prev_ident = _block_ident(blocks[index - 1]) if index else None
            next_ident = _block_ident(blocks[index + 1]) if index + 1 < len(blocks) else None
            entries.append(
                {
                    "id": ident[1],
                    "kind": ident[0],
                    "matches": len(offsets),
                    "occurrences": occurrences,
                    "text": excerpt,
                    "matched_text": matched_text,
                    "offset": hit_start,
                    "region": region,
                    "normalized": matched_text.lower() != needle if not case_sensitive else matched_text != needle,
                    # honest read/write contract: only a match inside ONE
                    # revision region is a ready-to-send patch old
                    "patchable_as_single_hunk": not crossed,
                    "boundary_crossings": crossed,
                    "span_indices": span_indices,
                    # version-bound address: pass it to document_patch
                    # ({"match_ref": ..., "new": ...}) or format_span(match_ref=...)
                    "match_ref": _encode_match_ref(ident[1], hit_start, hit_end, matched_text, state["edit_body_sha256"]),
                    "prev_id": prev_ident[1] if prev_ident else None,
                    "next_id": next_ident[1] if next_ident else None,
                }
            )
        total_blocks = len(entries)
        entries = entries[max(0, offset) : max(0, offset) + max(1, limit)]
        deleted_only = _projection_deleted_matches(workdir, query, case_sensitive)
        counts: dict[str, Any] = {
            "editable": total,
            "inside_deleted_tracked_changes": deleted_only,
            "total_in_projection": total + deleted_only,
        }
        if deleted_only:
            counts["note"] = (
                "the projection also contains this text inside deleted tracked changes; those "
                "occurrences are NOT editable and are excluded from `matches` and from "
                "document_replace — accept or reject that revision (review profile) to change them"
            )
        return _json(
            {
                "query": query,
                "scope": scope,
                "offset": max(0, offset),
                "counts": counts,
                "total_blocks": total_blocks,
                "how_to_edit": (
                    "each occurrence carries match_ref: edit exactly that spot with "
                    "document_patch({\"hunks\": [{\"match_ref\": <ref>, \"new\": <text>}]}) or "
                    "format_span(match_ref=<ref>, attributes={...}); use document_replace only "
                    "when you mean to change every match"
                ),
                "revision": state["edit_body_sha256"],
                "state": state["state"],
                "total_matches": total,
                "returned_blocks": len(entries),
                "matches": entries,
            }
        )


@mcp.tool()
def format_span(
    paragraph_id: str | None = None,
    old: str | None = None,
    match_ref: str | None = None,
    attributes: dict[str, Any] | None = None,
    style_id: str | None = None,
    span_index: int | None = None,
    operation_id: str | None = None,
    track: bool | None = None,
) -> CallToolResult:
    """    Change the FORMATTING of existing text (superscript, subscript, bold,
    italic) — the lane that text replacement cannot express.

    ``old`` selects the text exactly like document_patch (one editable span,
    copy it from document_read view=spans); ``attributes`` is a partial override
    map applied to the run properties already in force on that text, e.g.
    {"vertAlign": "superscript"} to fix Cu2+ / Mn2+ superscripts, or
    {"bold": false} to un-bold a phrase. The engine derives the resulting
    character style, registers it, and keeps every other run property.

    Direct mode: the run is restyled in place. Tracked mode: the change is
    emitted as a reviewable delete+insert pair under the session author (Word's
    own rPrChange is not synthesised yet).

    Mutating: ``operation_id`` may be omitted; identical retries replay."""
    with session.lock:
        if session.workdir is None:
            return _failure_result("format_span", "workdir-not-open", "no workdir open; call workdir_open first", operation_id=operation_id)
        workdir = session.workdir
        _adopt_requested_mode(track)
        manifest_before = _workdir_manifest_sha256(workdir)
        if bool(attributes) == bool(style_id):
            return _failure_result(
                "format_span", "format-invalid-attribute",
                'pass exactly one of attributes (e.g. {"vertAlign": "superscript"}) or style_id',
                operation_id=operation_id,
            )
        if match_ref is not None:
            try:
                paragraph_id, old, _anchored = _resolve_match_ref(workdir, match_ref)
            except ToolError as exc:
                return _failure_result("format_span", exc.code, exc.detail, operation_id=operation_id, details=getattr(exc, "details", None))
        if not paragraph_id:
            return _failure_result("format_span", "format-invalid-argument", "paragraph_id or match_ref is required", operation_id=operation_id)
        if bool(old) == (span_index is not None):
            return _failure_result(
                "format_span", "format-invalid-argument",
                "address the text with exactly one of old (text match) or region_index "
                "(the style-region index from document_read view=spans -> style_regions); "
                "region_index is an exact address and cannot misfire",
                operation_id=operation_id,
            )

        def run(target, tx=None):
            revision_before = classify_edit_state(target)["edit_body_sha256"]
            result = _format_span_impl(target, paragraph_id, old, attributes, style_id, span_index)
            # formatting writes typed.md directly, so the collaboration ledger
            # must be advanced here too: otherwise the very next commit_sync is
            # refused with current-snapshot-drift and NO tool in the editor
            # profile can clear it (the observed dead end).
            collaboration = document_state(target)
            if not collaboration["current_matches_filesystem"]:
                result["published_snapshot"] = publish_current(
                    target,
                    expected_parent_snapshot=collaboration["current_snapshot"]["id"],
                    origin="agent",
                    changed_paragraph_ids=[paragraph_id] if paragraph_id else [],
                )
            result["document_state"] = {
                "revision_before": revision_before,
                "revision_after": classify_edit_state(target)["edit_body_sha256"],
                "draft": "clean",
            }
            payload = {
                **base_evidence_payload(),
                "inputs": {"workdir": {"manifest_sha256": manifest_before}},
                "outputs": {"workdir": {"manifest_sha256": _workdir_manifest_sha256(target)}},
                "checks": [{"name": "format-span", "status": "pass", "style": result["style_id"]}],
            }
            return "success", result, "mutation", payload, []

        return _mutation_tool(
            operation_id,
            "format_span",
            {
                "workdir": str(workdir),
                "paragraph_id": paragraph_id,
                "old": old,
                "match_ref": match_ref,
                "attributes": attributes or {},
                "requested_style_id": style_id,
                "span_index": span_index,
                "track": track,
            },
            workdir,
            directory=True,
            evidence_path=workdir / "run.evidence.json",
            run=run,
            store_workdir=workdir,
        )


def _format_span_impl(
    workdir: Path,
    paragraph_id: str,
    old: str | None,
    attributes: dict[str, Any] | None,
    style_id: str | None = None,
    span_index: int | None = None,
) -> dict[str, Any]:
    """Restyled typed AST + refreshed projection, published like any mutation."""
    from .edit import (
        STATE_FILE,
        _stage_text,
        _replace_staged,
        create_edit_state,
        edit_body_sha256,
        render_edit_projection,
    )
    from .edit import _build_revision_context
    from .edit_sync import _revision_attrs, _revision_token_record, sync_segments_from_nodes
    from .typed_core import (
        RevisionNode,
        Style,
        TypedDocument,
        choose_base_style,
        serialize_typed,
        skeleton,
        style_id_for_rpr,
        style_label,
        visible_text,
    )

    # format_span rewrites the committed typed AST (styles live there), so it
    # needs a clean draft; otherwise anchors resolved from the draft would not
    # exist in typed.md and the failure would read as text-not-found.
    state_now = classify_edit_state(workdir)
    if state_now["state"] != "clean":
        raise ToolError(
            "format-requires-clean-draft",
            f"format_span works on the committed document, but the draft is {state_now['state']} — "
            "run commit_sync (or revert) first, then format",
        )
    typed_path, styles_path, format_path = workdir / "typed.md", workdir / "styles.json", workdir / "format.json"
    typed = parse_typed(typed_path.read_text(encoding="utf-8"))
    format_data = json.loads(format_path.read_text(encoding="utf-8"))
    styles_data = json.loads(styles_path.read_text(encoding="utf-8"))
    registry = StyleRegistry(
        {
            key: Style(style_id=key, rpr=value["rPr"], canonical=value["canonical"], label=value.get("label", ""), features=value.get("features", {}))
            for key, value in styles_data["styles"].items()
        }
    )
    paragraph = next((p for p in typed.paragraphs if p.paragraph_id == paragraph_id), None)
    if paragraph is None:
        raise ToolError("paragraph-not-found", f"paragraph {paragraph_id} not in typed.md")

    ranges, total = _visible_ranges(paragraph.nodes)
    flat = visible_text(paragraph.nodes)
    if span_index is not None:
        # Path-addressed formatting: take the span straight from the read
        # surface's map (discover, never guess -> no matching can misfire).
        regions = _typed_style_regions(paragraph.nodes)
        if span_index < 0 or span_index >= len(regions):
            raise ToolError(
                "span-index-out-of-range",
                f"{paragraph_id}: region_index {span_index} is out of range (the paragraph has "
                f"{len(regions)} style regions); re-read with document_read view=spans and use "
                "style_regions[].index",
                details={"region_count": len(regions), "region_index": span_index},
            )
        start, end = regions[span_index]
        old = flat[start:end]
    tolerated = _resolve_visible_match(flat, old)
    if tolerated is not None and tolerated["normalized"]:
        old = tolerated["matched_text"]
    if flat.count(old) == 0:
        span_map = _span_map_for(workdir, paragraph_id)
        divergence = _divergence_hint(span_map, old)
        hint = (
            f"your text matches up to {divergence['matched_text']!r} then diverges — the "
            f"document continues {divergence.get('document_continues', '')!r}"
            if divergence
            else "copy one span verbatim from data.span_map"
        )
        raise ToolError(
            "text-not-found",
            f"{paragraph_id}: text {old!r} not found in paragraph; {hint}",
            details={"span_map": span_map, "divergence": divergence},
        )
    if flat.count(old) > 1:
        raise ToolError(
            "text-ambiguous",
            f"{paragraph_id}: text {old!r} appears {flat.count(old)} times; use a longer unique span",
            details={"span_map": _span_map_for(workdir, paragraph_id)},
        )
    start = flat.index(old)
    end = start + len(old)
    covered = [item for item in ranges if item[0] < end and item[1] > start]
    if not covered:
        raise ToolError("text-not-found", f"{paragraph_id}: {old!r} is not editable text")
    if len({item[3] for item in covered}) > 1:
        raise ToolError(
            "format-span-crosses-revision-boundary",
            f"{paragraph_id}: {old!r} crosses a revision-control boundary; formatting cannot "
            "span two revision regions — format each region separately",
            details={"span_map": _span_map_for(workdir, paragraph_id)},
        )
    base_style = covered[0][2].style_id or choose_base_style(paragraph.nodes, paragraph.base_style or "")
    if style_id is not None:
        registry.require(style_id)  # unknown id -> TypedError -> workdir-invalid-ish
        base_rpr = registry.require(base_style).rpr
        new_style = style_id
        if new_style == base_style:
            raise ToolError("format-noop", f"{paragraph_id}: the text already carries {style_id}")
        new_rpr = None
    else:
        base_rpr = registry.require(base_style).rpr
        new_rpr = _rpr_with_overrides(base_rpr, attributes or {})
    if new_rpr is None and style_id is None:
        raise ToolError(
            "format-noop",
            f"{paragraph_id}: the requested attributes already match {base_style}'s run properties",
        )
    # The template is the fidelity source: styles.json must mirror the run
    # properties that already exist in the source document, so a format change
    # REUSES an existing variant instead of inventing one.
    if new_rpr is not None:
        new_style = style_id_for_rpr(new_rpr)
    if new_rpr is not None and new_style not in registry.styles:
        available = [
            {"style_id": style_id, "label": style.label, "features": style.features}
            for style_id, style in sorted(registry.styles.items())
            if any(feature in style.features for feature in (attributes or {}))
        ]
        raise ToolError(
            "format-style-unavailable",
            f"{paragraph_id}: this document has no run carrying "
            f"{json.dumps(attributes or {}, ensure_ascii=False)} on the target's base formatting, and "
            "style variables cannot be invented (styles.json must mirror the source document). "
            "Available variants with those properties: "
            + (", ".join(f"{item['style_id']} ({item['label']})" for item in available) or "none")
            + f". Reuse one by passing style_id=..., or apply the formatting once in Word "
            "(the source document is the fidelity source) and re-extract.",
            details={
                "available_styles": available,
                "base_style": base_style,
                "capability": "word.format.run-properties",
                "fallback": "reuse an existing variant (style_id=...) or add the formatting once in Word and re-extract",
            },
        )
    if new_rpr is not None and new_style == base_style:
        raise ToolError("format-noop", f"{paragraph_id}: the requested attributes produce the same style")

    mode = session.mode or "direct"
    author = session.author or "Unknown"
    tokens_new: dict[str, Any] = {}
    if mode == "track":
        ctx = _build_revision_context(typed, format_data, workdir, mode="track", author=author, author_source="session")
        # Only the target text enters the revision pair: unchanged text on
        # either side stays plain, so reviewers see a minimal change.
        keep_before, original_nodes, keep_after = _split_text_nodes(paragraph.nodes, start, end)
        restyled_nodes = [TextNode(new_style, node.text) for node in original_nodes if isinstance(node, TextNode)] or original_nodes

        def make_revision(kind: str, children: list[Any]) -> RevisionNode:
            token_id = ctx["next_token_id"]()
            attrs = _revision_attrs(kind, ctx["next_w_id"](), author, ctx["date"], ctx.get("date_utc", False))
            tokens_new[token_id] = _revision_token_record(kind, attrs)
            return RevisionNode(token_id, kind, attrs, children)

        paragraph.nodes = keep_before + [make_revision("delete", original_nodes), make_revision("insert", restyled_nodes)] + keep_after
    else:
        paragraph.nodes = _restyle_nodes(paragraph.nodes, start, end, new_style)

    typed_text = serialize_typed(typed)
    styles_text = json.dumps(registry.to_json(), ensure_ascii=False, indent=1, sort_keys=True) + "\n"
    typed_hash = _sha256_text(typed_text)
    format_data["styles_sha256"] = _sha256_text(styles_text)
    if tokens_new:
        format_data.setdefault("tokens", {}).update(tokens_new)
    # Record the governed baseline from the RE-PARSED document: parsing merges
    # adjacent same-style runs, so recording the pre-serialization nodes would
    # disagree with what the validator recomputes.
    reparsed = parse_typed(typed_text)
    reparsed_paragraph = next((p for p in reparsed.paragraphs if p.paragraph_id == paragraph_id), paragraph)
    for record in format_data.get("paragraphs", []):
        if record.get("id") == paragraph_id:
            record["sync_segments"] = sync_segments_from_nodes(reparsed_paragraph.nodes)
            record["sync_skeleton"] = skeleton(reparsed_paragraph.nodes)
    projection_text = render_edit_projection(typed, base_typed_sha256=typed_hash)
    state_text = json.dumps(
        create_edit_state(typed_hash, edit_body_sha256(projection_text)), ensure_ascii=False, indent=2, sort_keys=True
    ) + "\n"
    format_text = json.dumps(format_data, ensure_ascii=False, indent=2, sort_keys=True) + "\n"

    staged: dict[Path, Path] = {}
    try:
        for path, text in (
            (typed_path, typed_text),
            (styles_path, styles_text),
            (format_path, format_text),
            (workdir / PROJECTION_FILE, projection_text),
            (workdir / STATE_FILE, state_text),
        ):
            staged[path] = _stage_text(path, text)
        validate_workdir(workdir)  # staging files are not yet in place
        for path, staged_path in staged.items():
            _replace_staged(staged_path, path)
    finally:
        for staged_path in staged.values():
            if staged_path.exists():
                staged_path.unlink()
    _refresh_regions(workdir)
    return {
        "paragraph_id": paragraph_id,
        "old": old,
        "attributes": attributes,
        "base_style": base_style,
        "style_id": new_style,
        "edit_mode": mode,
        "state": "clean",
    }


def _sha256_text(text: str) -> str:
    import hashlib

    return hashlib.sha256(text.encode("utf-8")).hexdigest()


@mcp.tool()
def batch_edit(paragraph_id: str, edits: list[dict], operation_id: str | None = None, allow_comment_text: bool = False) -> CallToolResult:
    """    Advanced fallback lane: prefer document_read / document_search /
    document_patch (the default editing surface); use this tool only for
    exact per-region style ownership, diagnosis, or recovery.
    Edit several style regions of one paragraph atomically and immediately.

    Each edit targets exactly one region, addressed either by index
    (recommended, from regions.md / get_paragraph styles) or by text anchor:
      {"region": 1, "new": "..."}                 replace whole region
      {"region": 2, "old": "...", "new": "..."}   replace text inside region
      {"text": "...", "style_id": "...", "new": "..."}   text-anchor addressing
    Edits are applied sequentially, each as a single-region sync (the engine
    needs no style inference because the region is explicit). If any edit
    fails the whole batch is rolled back; on success all edits are committed
    and the workdir is clean. A region may be edited at most once per call.

    Mutating: ``operation_id`` may be omitted (the server generates a fresh
    id). Identical retries replay the original result; changed input or a
    reused id from ANY earlier call (success or failure) fails
    operation-id-reused."""
    with session.lock:
        if session.workdir is None:
            return _failure_result("batch_edit", "workdir-not-open", "no workdir open; call workdir_open first", operation_id=operation_id)
        workdir = session.workdir
        manifest_before = _workdir_manifest_sha256(workdir)

        def run(target, tx=None):
            workdir = target  # store mode: mutate the generation snapshot
            _require_comment_text_opt_in(paragraph_id, allow_comment_text)
            parent_snapshot = document_state(workdir)["current_snapshot"]["id"]
            texts, styles = _draft_paragraph_state(workdir, paragraph_id, mode=session.mode)
            regions = _merge_regions(texts, styles)
            resolved: list[tuple[int, str | None, str]] = []
            seen_regions: set[int] = set()
            for edit_no, edit in enumerate(edits, start=1):
                if not isinstance(edit, dict):
                    raise ToolError("invalid-edit", f"edit {edit_no}: must be an object")
                region_index = _resolve_region(edit, regions, edit_no)
                if region_index in seen_regions:
                    raise ToolError("invalid-edit", f"edit {edit_no}: region {region_index} is repeated")
                seen_regions.add(region_index)
                new = edit.get("new")
                if not isinstance(new, str):
                    raise ToolError("invalid-edit", f"edit {edit_no}: missing 'new' text")
                old = edit.get("old")
                if old is not None and not isinstance(old, str):
                    raise ToolError("invalid-edit", f"edit {edit_no}: 'old' must be text")
                if old is not None and old == new:
                    raise ToolError("patch-noop", f"edit {edit_no}: old and new are identical; nothing to change")
                anchor_text = edit.get("text")
                if old is None and isinstance(anchor_text, str) and anchor_text == new:
                    raise ToolError("patch-noop", f"edit {edit_no}: anchor text equals new; nothing to change")
                if old is None and "region" in edit and new == regions[region_index][0]:
                    raise ToolError("patch-noop", f"edit {edit_no}: new equals the whole region; nothing to change")
                if old is not None and ("\u27e6" in old or "\u27e7" in old):
                    raise ToolError(
                        "text-not-found",
                        f"edit {edit_no}: old must be visible text without placeholder markers",
                    )
                resolved.append((region_index, old, new))
            protected = [
                workdir / name
                for name in ("typed.md", PROJECTION_FILE, "edit.state.json", "format.json", "regions.md")
            ]
            backup = {path: path.read_bytes() for path in protected if path.exists()}
            try:
                header, blocks = _read_edit(workdir)
                index = _find_block(blocks, "p", paragraph_id)
                marker = blocks[index].splitlines()[0]
                body = _block_body(blocks[index])
                new_body = _apply_batch_to_body(body, regions, resolved)
                blocks[index] = marker + ("\n" + new_body if new_body else "")
                _write_edit(workdir, header, blocks)
                sync_edit_projection(
                    workdir,
                    track=session.mode == "track",
                    author=session.author,
                )
            except BaseException:
                for path, data in backup.items():
                    path.write_bytes(data)
                raise
            collaboration = document_state(workdir)
            published = None
            if not collaboration["current_matches_filesystem"]:
                published = publish_current(
                    workdir,
                    expected_parent_snapshot=parent_snapshot,
                    origin="agent",
                    changed_paragraph_ids=[paragraph_id],
                )
            _refresh_regions(workdir)
            payload = {
                **base_evidence_payload(),
                "inputs": {"workdir": {"manifest_sha256": manifest_before}},
                "outputs": {"workdir": {"manifest_sha256": _workdir_manifest_sha256(workdir)}},
                "checks": [{"name": "batch-edit", "status": "pass"}],
            }
            return (
                "success",
                {
                    "paragraph_id": paragraph_id,
                    "edits_applied": len(resolved),
                    "state": "clean",
                    "current_snapshot": published["current_snapshot"] if published else collaboration["current_snapshot"],
                    "next": "build_docx to export, or continue editing",
                },
                "mutation",
                payload,
                [],
            )

        return _mutation_tool(
            operation_id,
            "batch_edit",
            {
                "workdir": str(session.workdir),
                "paragraph_id": paragraph_id,
                "edits": edits,
                "allow_comment_text": allow_comment_text,
            },
            session.workdir,
            directory=True,
            evidence_path=session.workdir / "run.evidence.json",
            run=run,
            store_workdir=session.workdir,
            preflight_scope=[paragraph_id],
        )


@mcp.tool()
def insert_paragraph(after_id: str, text: str, inherit: str | None = None, operation_id: str | None = None) -> CallToolResult:
    """    Advanced fallback lane: prefer document_read / document_search /
    document_patch (the default editing surface); use this tool only for
    exact per-region style ownership, diagnosis, or recovery.
    Insert a new paragraph after ``after_id`` in the draft. ``inherit``
    copies the referenced paragraph's insertion style (defaults to
    ``after_id``). Text is visible plain text; structural tokens are not
    allowed in new paragraphs. In track mode the new paragraph carries a
    paragraph-mark insertion revision (R2.5).

    Mutating: ``operation_id`` may be omitted (the server generates a fresh
    id). Identical retries replay the original result; changed input or a
    reused id from ANY earlier call (success or failure) fails
    operation-id-reused."""
    with session.lock:
        if session.workdir is None:
            return _failure_result("insert_paragraph", "workdir-not-open", "no workdir open; call workdir_open first", operation_id=operation_id)
        workdir = session.workdir
        if after_id.startswith(("T", "B")) or ("." in after_id) or (inherit or "").startswith(("T", "B")) or "." in (inherit or ""):
            return _failure_result(
                "insert_paragraph",
                "table-structure-immutable",
                "paragraphs cannot be inserted into tables, text boxes, or "
                "header/footer/note parts; container structure operations are out of scope",
                operation_id=operation_id,
            )
        manifest_before = _workdir_manifest_sha256(workdir)

        def run(target, tx=None):
            header, blocks = _read_edit(target)
            index = _find_block(blocks, "p", after_id)
            resolved_inherit = inherit or after_id
            temps = [
                int(m.group(1))
                for block in blocks
                for m in [re.match(r'<!--@new temp="N(\d+)"', block)]
                if m
            ]
            temp = f"N{max(temps, default=0) + 1}"
            block = f'<!--@new temp="{temp}" inherit="{resolved_inherit}"-->\n{_escape_prose(text)}'
            blocks.insert(index + 1, block)
            _write_edit(target, header, blocks)
            _refresh_regions(target)
            payload = {
                **base_evidence_payload(),
                "inputs": {"workdir": {"manifest_sha256": manifest_before}},
                "outputs": {"workdir": {"manifest_sha256": _workdir_manifest_sha256(target)}},
                "checks": [{"name": "draft-inserted", "status": "pass"}],
            }
            return (
                "success",
                {
                    "temp_id": temp,
                    "inherit": resolved_inherit,
                    "draft": "dirty",
                    "next": "commit_sync allocates the formal paragraph ID",
                },
                "mutation",
                payload,
                [],
            )

        return _mutation_tool(
            operation_id,
            "insert_paragraph",
            {
                "workdir": str(workdir),
                "after_id": after_id,
                "text": text,
                "inherit": inherit,
            },
            workdir,
            directory=True,
            evidence_path=workdir / "run.evidence.json",
            run=run,
            store_workdir=workdir,
            preflight_scope=[after_id, inherit or after_id],
        )


@mcp.tool()
def delete_paragraph(paragraph_id: str, operation_id: str | None = None) -> CallToolResult:
    """    Advanced fallback lane: prefer document_read / document_search /
    document_patch (the default editing surface); use this tool only for
    exact per-region style ownership, diagnosis, or recovery.
    Delete a paragraph from the draft. Paragraphs with protected structure
    (tokens, section boundaries) are rejected by commit_sync. In track mode
    the paragraph stays in the document with a paragraph-mark deletion
    revision (R2.5 merge semantics).

    Mutating: ``operation_id`` may be omitted (the server generates a fresh
    id). Identical retries replay the original result; changed input or a
    reused id from ANY earlier call (success or failure) fails
    operation-id-reused."""
    with session.lock:
        if session.workdir is None:
            return _failure_result("delete_paragraph", "workdir-not-open", "no workdir open; call workdir_open first", operation_id=operation_id)
        workdir = session.workdir
        if paragraph_id.startswith(("T", "B")) or ("." in paragraph_id and not paragraph_id.startswith("P")):
            return _failure_result(
                "delete_paragraph",
                "table-structure-immutable",
                "container and part paragraphs cannot be deleted; container "
                "structure operations are out of scope",
                operation_id=operation_id,
            )
        manifest_before = _workdir_manifest_sha256(workdir)

        def run(target, tx=None):
            header, blocks = _read_edit(target)
            index = _find_block(blocks, "p", paragraph_id)
            blocks.pop(index)
            blocks.append(f'<!--@delete id="{paragraph_id}"-->')
            _write_edit(target, header, blocks)
            _refresh_regions(target)
            payload = {
                **base_evidence_payload(),
                "inputs": {"workdir": {"manifest_sha256": manifest_before}},
                "outputs": {"workdir": {"manifest_sha256": _workdir_manifest_sha256(target)}},
                "checks": [{"name": "draft-deleted", "status": "pass"}],
            }
            return (
                "success",
                {"paragraph_id": paragraph_id, "draft": "dirty", "next": "commit_sync"},
                "mutation",
                payload,
                [],
            )

        return _mutation_tool(
            operation_id,
            "delete_paragraph",
            {
                "workdir": str(workdir),
                "paragraph_id": paragraph_id,
            },
            workdir,
            directory=True,
            evidence_path=workdir / "run.evidence.json",
            run=run,
            store_workdir=workdir,
            preflight_scope=[paragraph_id],
        )


def _apply_document_hunks(
    workdir: Path, hunks: list[tuple[str, dict]], *, allow_comment_text: bool = False
) -> tuple[str, list[str], list[dict[str, Any]]]:
    """Validate every hunk against the current draft, then apply all of them
    to in-memory blocks. A paragraph may carry several non-overlapping
    replace hunks: each must be unique in the pre-batch paragraph and is
    applied anchored by its start offset in descending order, so earlier
    applications never shift later ones. Cross-region spans are NOT gated
    here — the sync engine's dry-run (plan_sync) decides deterministically
    whether a mixed-style replacement is accepted, with warnings.
    Deletes and inserts land first so recorded paragraph ids — never block
    indices — drive the replace application. Nothing is written until the
    caller performs the single _write_edit.
    Returns (header, blocks, applied-summary)."""
    header, blocks = _read_edit(workdir)
    pending: dict[str, list[tuple[int, str, str]]] = {}
    delete_ids: list[str] = []
    insert_specs: list[tuple[str, str, str | None]] = []
    applied: list[dict[str, Any]] = []
    problems: list[dict[str, Any]] = []
    normalized_notes: list[dict[str, Any]] = []

    def failed(index: int, kind: str, hunk: dict[str, Any], exc: ToolError) -> None:
        """Record one invalid hunk and keep validating the rest, so a batch
        reports EVERY broken hunk in one call instead of one per round."""
        extra = dict(exc.details or {})  # refusal payloads carry their own recovery data
        problems.append(
            {
                "hunk": index,
                "kind": kind,
                "paragraph_id": hunk.get("paragraph_id") or hunk.get("insert_after"),
                "code": exc.code,
                "message": exc.detail,
                **extra,
            }
        )

    for index, (kind, hunk) in enumerate(hunks, start=1):
        if kind == "match_ref":
            try:
                paragraph_id, resolved_old, anchored_offset = _resolve_match_ref(workdir, hunk["match_ref"])
                hunk = {
                    "paragraph_id": paragraph_id,
                    "old": resolved_old,
                    "new": hunk["new"],
                    "offset": anchored_offset,
                }
                kind = "replace"
            except ToolError as exc:
                failed(index, "match_ref", hunk, exc)
                continue
        if kind == "replace":
            # text replace on ANY paragraph already projected in edit.md is
            # facade-legal (cells, content controls, parts): body content
            # only, structure untouched.
            paragraph_id = hunk["paragraph_id"]
            try:
                _find_block(blocks, "p", paragraph_id)
                _require_comment_text_opt_in(paragraph_id, allow_comment_text)
                if paragraph_id in delete_ids:
                    raise ToolError(
                        "document-patch-paragraph-repeated",
                        f"{paragraph_id}: cannot replace and delete the same paragraph in one patch",
                    )
                texts, styles = _draft_paragraph_state(workdir, paragraph_id, mode=session.mode)
                visible = "".join(texts)
                if "\u27e6" in hunk["old"] or "\u27e7" in hunk["old"]:
                    raise ToolError(
                        "placeholder-in-edit-span",
                        f"{paragraph_id}: old includes a read-only placeholder token "
                        "(\u27e6...\u27e7); select contiguous editable text on one side "
                        "of the placeholder instead — copy one span verbatim from "
                        "data.span_map",
                        details={"span_map": _span_map_for(workdir, paragraph_id)},
                    )
                if hunk["old"] == hunk["new"]:
                    raise ToolError(
                        "patch-noop",
                        f"{paragraph_id}: old and new are identical; nothing to change",
                    )
                anchored = hunk.get("offset")
                if anchored is not None:
                    if not visible.startswith(hunk["old"], int(anchored)):
                        anchor_offset = int(anchored)
                        current_text = visible[anchor_offset : anchor_offset + len(hunk["old"])]
                        fresh_ref = (
                            _encode_match_ref(
                                paragraph_id, anchor_offset, anchor_offset + len(current_text),
                                current_text, classify_edit_state(workdir)["edit_body_sha256"],
                            )
                            if current_text
                            else None
                        )
                        raise ToolError(
                            "match-ref-stale",
                            f"{paragraph_id}: the text at offset {anchor_offset} is no longer "
                            f"{hunk['old']!r} — it now reads {current_text!r}; "
                            + ("data.fresh_match_ref addresses it" if fresh_ref else "re-run document_search"),
                            details={
                                "paragraph_id": paragraph_id,
                                "offset": anchor_offset,
                                "expected_text": hunk["old"],
                                "current_text": current_text,
                                **({"fresh_match_ref": fresh_ref} if fresh_ref else {}),
                            },
                        )
                    match = {
                        "start": int(anchored),
                        "end": int(anchored) + len(hunk["old"]),
                        "matched_text": hunk["old"],
                        "normalized": False,
                        "differences": [],
                    }
                else:
                    match = _resolve_visible_match(visible, hunk["old"])
                if match is None:
                    _revision_span_diagnostic(workdir, paragraph_id, hunk["old"])
                    count = visible.count(hunk["old"])
                    if count > 1:
                        raise ToolError(
                            "text-ambiguous",
                            f"{paragraph_id}: {hunk['old']!r} appears {count} times — resend with "
                            "a longer unique span",
                            details={"span_map": _span_map_for(workdir, paragraph_id), "matches": count},
                        )
                    span_map = _span_map_for(workdir, paragraph_id)
                    closest = _closest_spans(span_map, hunk["old"])
                    divergence = _divergence_hint(span_map, hunk["old"])
                    hidden = _deletion_containing(workdir, paragraph_id, hunk["old"])
                    if hidden:
                        raise ToolError(
                            "text-inside-tracked-deletion",
                            f"{paragraph_id}: {hunk['old']!r} is NOT visible text — it sits inside a "
                            f"tracked deletion (w:id={hidden.get('w_id')}, author={hidden.get('author')}). "
                            "Either settle that revision first (accept_revision/reject_revision or "
                            "decide_all) and then edit, or edit the inserted replacement text instead",
                            details={
                                "deletion": hidden,
                                "span_map": span_map,
                                "divergence": divergence,
                                "capability": "word.revision.edit-deleted-text",
                                "fallback": "settle the revision (accept_revision/reject_revision or decide_all), then edit the visible text",
                            },
                        )
                    suggested_old = (divergence or {}).get("span_text") or (
                        closest[0]["text"] if closest else None
                    )
                    recipe = (
                        {
                            "action": "resend-hunk-with-document-text",
                            "paragraph_id": paragraph_id,
                            "old": suggested_old,
                            "new": hunk["new"],
                        }
                        if suggested_old
                        else None
                    )
                    if divergence and divergence.get("anchor") == "prefix":
                        hint = (
                            f"your text matches the document up to {divergence['matched_text']!r} and "
                            f"then diverges — the document says {divergence['document_continues']!r}"
                        )
                    elif divergence:
                        hint = (
                            f"your text overlaps span #{divergence['span']} only in the middle "
                            f"({divergence['matched_text']!r}); the document has "
                            f"{divergence['document_before']!r} before it and "
                            f"{divergence['document_continues']!r} after — use spans from data.span_map"
                        )
                    elif closest:
                        hint = "closest span text: " + repr(closest[0]["text"])[:160]
                    else:
                        hint = "copy one span verbatim from data.span_map"
                    if recipe:
                        hint += "; data.fix carries the corrected hunk — resend it verbatim"
                    raise ToolError(
                        "text-not-found",
                        f"{paragraph_id}: {hunk['old']!r} not found in paragraph; {hint}",
                        details={
                            "span_map": span_map,
                            "closest_spans": closest,
                            "divergence": divergence,
                            "fix": recipe,
                        },
                    )
                if match["normalized"]:
                    normalized_notes.append(
                        {
                            "hunk": index,
                            "paragraph_id": paragraph_id,
                            "your_text": hunk["old"],
                            "document_text": match["matched_text"],
                            "differences": match["differences"],
                        }
                    )
                    hunk["old"] = match["matched_text"]
                block_index = _find_block(blocks, "p", paragraph_id)
                _flat_body, body_boundaries = _body_boundaries(_block_body(blocks[block_index]))
                span_region = _region_at(body_boundaries, match["start"])
                if (session.mode or "direct") == "direct" and span_region == "insert":
                    raise ToolError(
                        "revision-text-mutated-in-direct-mode",
                        f"{paragraph_id}: the target text sits INSIDE a tracked insertion, and direct "
                        "mode cannot change revision text (it would silently rewrite another author's "
                        "revision). Resend the same hunks in this call with track=true, or revert the "
                        "draft and re-open with track=true",
                        details={
                            "paragraph_id": paragraph_id,
                            "region": span_region,
                            "fix": {
                                "tool": "document_patch",
                                "track": True,
                                "hunks": [{"paragraph_id": paragraph_id, "old": match["matched_text"], "new": hunk["new"]}],
                            },
                            "capability": "word.revision.edit-within-revision",
                        },
                    )
                start = match["start"]
                count = 1
                _ = anchored  # anchored hunks already validated above
                spans = pending.setdefault(paragraph_id, [])
                for other_start, other_old, _ in spans:
                    if start < other_start + len(other_old) and other_start < start + len(hunk["old"]):
                        raise ToolError(
                            "document-patch-hunks-overlap",
                            f"{paragraph_id}: two replace hunks overlap at offset {start}; "
                            "merge them into one hunk",
                        )
                spans.append((start, hunk["old"], hunk["new"]))
                if len(spans) == 1:
                    applied.append({"kind": "replace", "paragraph_id": paragraph_id})
            except ToolError as exc:
                failed(index, kind, hunk, exc)
        elif kind == "insert":
            after_id = hunk["insert_after"]
            try:
                _require_comment_text_opt_in(after_id, allow_comment_text)
                _require_body_structure_mutable(after_id)
                resolved_inherit = hunk["inherit"] or after_id
                _require_body_structure_mutable(resolved_inherit)
                insert_specs.append((after_id, hunk["text"], hunk["inherit"]))
            except ToolError as exc:
                failed(index, kind, hunk, exc)
        else:
            paragraph_id = hunk["paragraph_id"]
            try:
                _require_comment_text_opt_in(paragraph_id, allow_comment_text)
                _require_body_structure_mutable(paragraph_id)
                if paragraph_id in pending:
                    raise ToolError(
                        "document-patch-paragraph-repeated",
                        f"{paragraph_id}: cannot replace and delete the same paragraph in one patch",
                    )
                delete_ids.append(paragraph_id)
                applied.append({"kind": "delete", "paragraph_id": paragraph_id})
            except ToolError as exc:
                failed(index, kind, hunk, exc)

    if problems:
        # One broken hunk keeps its own diagnostic code (wrapped with the hunk
        # number); several broken hunks surface together as one report.
        if len(problems) == 1:
            problem = problems[0]
            details = {"hunk": problem["hunk"], "problems": problems}
            for key, value in problem.items():
                if key not in ("hunk", "kind", "paragraph_id", "code", "message"):
                    details[key] = value
            raise ToolError(
                problem["code"],
                f"hunk #{problem['hunk']} ({problem['paragraph_id']}): {problem['message']}",
                details=details,
            )
        fixes = sum(1 for p in problems if p.get("fix"))
        raise ToolError(
            "patch-hunks-invalid",
            f"{len(problems)} of {len(hunks)} hunks are invalid — fix all of them and resend in ONE call: "
            + "; ".join(f"hunk #{p['hunk']} ({p['paragraph_id']}): {p['code']}" for p in problems)
            + (f"; {fixes} of them carry a ready-to-send fix in data.problems[].fix" if fixes else ""),
            details={"problems": problems},
        )

    if normalized_notes:
        applied.append({"kind": "matched-with-normalization", "matches": normalized_notes})
    # deletes first (ids, not indices), then inserts, then anchored replaces
    for paragraph_id in delete_ids:
        index = _find_block(blocks, "p", paragraph_id)
        blocks.pop(index)
        blocks.append(f'<!--@delete id="{paragraph_id}"-->')
    insert_offsets: dict[str, int] = {}
    for after_id, text, inherit in insert_specs:
        index = _find_block(blocks, "p", after_id)
        resolved_inherit = inherit or after_id
        _require_body_structure_mutable(resolved_inherit)
        temps = [
            int(m.group(1))
            for block in blocks
            for m in [re.match(r'<!--@new temp="N(\d+)"', block)]
            if m
        ]
        temp = f"N{max(temps, default=0) + 1}"
        offset = insert_offsets.get(after_id, 0)
        blocks.insert(index + 1 + offset, f'<!--@new temp="{temp}" inherit="{resolved_inherit}"-->\n{_escape_prose(text)}')
        insert_offsets[after_id] = offset + 1
        applied.append({"kind": "insert", "after_id": after_id, "temp_id": temp})
    for paragraph_id, spans in pending.items():
        index = _find_block(blocks, "p", paragraph_id)
        marker = blocks[index].splitlines()[0]
        body = _block_body(blocks[index])
        for start, old, new in sorted(spans, key=lambda span: -span[0]):
            body = _replace_in_body(body, old, new, paragraph_id, start_offset=start)
        blocks[index] = marker + ("\n" + body if body else "")
        for entry in applied:
            if entry["kind"] == "replace" and entry["paragraph_id"] == paragraph_id:
                entry["hunks"] = len(spans)
    return header, blocks, applied

# --------------------------------------------------------------------------
# Opaque, version-bound span references (search -> patch without copying text)
# --------------------------------------------------------------------------

def _encode_match_ref(paragraph_id: str, start: int, end: int, text: str, revision: str) -> str:
    """A version-bound address: revision + paragraph + offsets + text hash.
    Bind it to a mutation to make the edit independent of copied text."""
    import base64
    import hashlib

    payload = {
        "revision": revision,
        "paragraph_id": paragraph_id,
        "start": start,
        "end": end,
        "sha256": hashlib.sha256(text.encode("utf-8")).hexdigest()[:16],
    }
    return "ref_" + base64.urlsafe_b64encode(json.dumps(payload, separators=(",", ":")).encode("utf-8")).decode("ascii").rstrip("=")


def _decode_match_ref(ref: str) -> dict[str, Any]:
    import base64

    if not isinstance(ref, str) or not ref.startswith("ref_"):
        raise ToolError("match-ref-invalid", f"not a span reference: {ref!r}")
    body = ref[4:]
    body += "=" * (-len(body) % 4)
    try:
        payload = json.loads(base64.urlsafe_b64decode(body.encode("ascii")).decode("utf-8"))
    except Exception as exc:
        raise ToolError("match-ref-invalid", f"span reference is corrupt: {ref!r}") from exc
    for key in ("revision", "paragraph_id", "start", "end", "sha256"):
        if key not in payload:
            raise ToolError("match-ref-invalid", f"span reference is missing {key!r}")
    return payload


def _resolve_match_ref(workdir: Path, ref: str) -> tuple[str, str, int]:
    """(paragraph_id, exact old text) for a span reference, validating the
    bound revision + offsets + text hash against the CURRENT draft."""
    import hashlib

    payload = _decode_match_ref(ref)
    current = classify_edit_state(workdir)["edit_body_sha256"]
    paragraph_id = payload["paragraph_id"]
    start, end = int(payload["start"]), int(payload["end"])
    texts, _styles = _draft_paragraph_state(workdir, paragraph_id, mode=session.mode)
    flat = "".join(texts)

    def stale(reason: str, details: dict[str, Any]) -> ToolError:
        """A stale reference is recoverable: hand back what the document says
        at those offsets now, plus a fresh reference when the anchor still
        resolves, so the caller re-issues in one step instead of re-searching."""
        current_text = flat[start:end] if 0 <= start < end <= len(flat) else ""
        enriched = {
            "current_revision": current,
            "ref_revision": payload["revision"],
            "paragraph_id": paragraph_id,
            "offset": start,
            "current_text": current_text,
            **details,
        }
        if current_text:
            enriched["fresh_match_ref"] = _encode_match_ref(paragraph_id, start, start + len(current_text), current_text, current)
            enriched["fix"] = {"tool": "document_patch", "hunks": [{"match_ref": enriched["fresh_match_ref"], "new": "<replacement>"}]}
            return ToolError(
                "match-ref-stale",
                f"{reason}; the text at {paragraph_id}[{start}:{start + len(current_text)}] is now "
                f"{current_text!r} — data.fresh_match_ref addresses it (data.fix shows how)",
                details=enriched,
            )
        return ToolError("match-ref-stale", f"{reason}; re-run document_search for a current reference", details=enriched)

    if payload["revision"] != current and payload.get("sha256") is None:
        raise stale(f"span reference was taken at revision {payload['revision'][:12]}… but the draft is now {current[:12]}…", {})
    if start < 0 or end > len(flat) or start >= end:
        raise ToolError(
            "match-ref-stale",
            f"span reference points outside {paragraph_id}; re-run document_search",
            details={"paragraph_id": paragraph_id, "current_revision": current},
        )
    candidate = flat[start:end]
    if hashlib.sha256(candidate.encode("utf-8")).hexdigest()[:16] != payload["sha256"]:
        raise stale(f"the text at {paragraph_id}[{start}:{end}] changed since the reference was issued", {"expected_sha256": payload["sha256"]})
    if payload["revision"] != current:
        # offsets still hold: the caller's view is old but the anchor is intact
        pass
    return paragraph_id, candidate, start


def _scope_paragraph_ids(workdir: Path, scope: str) -> list[str]:
    """Resolve a replace scope by paragraph-id semantics (part markers are not
    reliable block kinds): ``P12`` body paragraph, ``header1.P2`` part
    paragraph, ``comments.P0`` comment text, ``T0.R1.C2.P0`` table cell.

    body (default) = plain ``P<n>`` ids (the main document story);
    all = every paragraph; comments = comment text (needs opt-in);
    a part key (header1, footer1, footnotes, endnotes, box ids) = that part;
    one paragraph id = exactly that paragraph.
    """
    _, blocks = _read_edit(workdir)
    ids = [ident[1] for ident in (_block_ident(block) for block in blocks) if ident and ident[0] == "p"]
    body_ids = [pid for pid in ids if re.fullmatch(r"P\d+", pid)]
    if scope in ("body", "", "/body"):
        return body_ids
    if scope in ("all", "/"):
        return list(ids)
    if scope in ("comments", "/comments"):
        return [pid for pid in ids if pid.startswith("comments.")]
    if scope.startswith("/"):
        scope = scope[1:]
        if scope in ("body", "all", "comments"):
            return _scope_paragraph_ids(workdir, scope)
    if any(pid == scope for pid in ids):
        return [scope]
    prefixed = [pid for pid in ids if pid.startswith(f"{scope}.")]
    if prefixed:
        return prefixed
    raise ToolError(
        "replace-scope-invalid",
        f"unknown scope {scope!r}; use body (default), all, comments, a part prefix "
        "(header1, footer1, footnotes, endnotes, T0), or one paragraph id",
        details={"available_scopes": sorted({pid.split('.')[0] for pid in ids if '.' in pid})[:20]},
    )


def _apply_replace_matches(workdir: Path, plan_by_paragraph: dict[str, list[dict[str, Any]]], replacement: str) -> tuple[str, list[str]]:
    """Apply the whole replacement plan to the in-memory draft (descending
    offsets per paragraph, so earlier anchors never shift). Nothing is written
    until the caller commits the batch."""
    header, blocks = _read_edit(workdir)
    for paragraph_id, matches in plan_by_paragraph.items():
        index = _find_block(blocks, "p", paragraph_id)
        marker = blocks[index].splitlines()[0]
        body = _block_body(blocks[index])
        for match in sorted(matches, key=lambda item: int(item["offset"]), reverse=True):
            body = _replace_in_body(
                body, match["matched_text"], replacement, paragraph_id, start_offset=int(match["offset"])
            )
        blocks[index] = marker + ("\n" + body if body else "")
    return header, blocks


@mcp.tool()
def document_replace(
    find: str,
    replace: str,
    scope: str = "body",
    regex: bool = False,
    case_sensitive: bool = True,
    expected_matches: int | None = None,
    allow_comment_text: bool = False,
    base_revision: str | None = None,
    operation_id: str | None = None,
    track: bool | None = None,
    dry_run: bool = False,
) -> CallToolResult:
    """    Document-wide replace in ONE atomic call — the "unify all of these"
    lane (terminology, formatting words, names). Distinct intent from
    document_patch: you do not name the places, you name the rule.

    Discovers every match in ``scope`` first (body by default; also ``all``,
    ``comments``, a part key such as header1/footnotes, or one paragraph id),
    classifies each one, and only writes when EVERY match is safe: a match that
    spans a revision boundary is reported, never silently rewritten, and fails
    the whole batch (all-or-nothing).

    - ``regex``: Rust-style/Python regular expressions with ``\\1`` capture
      expansion; literal (and width/punctuation tolerant) otherwise.
    - ``expected_matches``: fail closed unless the discovered count equals it —
      use it for high-stakes exact edits ("this must hit exactly 3 places").
    - Zero matches is NOT an error: the result reports ``changed=false`` with
      ``noop_reason="no-match"``.
    - ``dry_run``: discover and classify every match, then STOP without touching
      the document — use it to confirm a count or preview the target set instead
      of mutating just to find out how many matches there are.

    Mutating: ``operation_id`` may be omitted; identical retries replay."""
    if not find:
        return _failure_result("document_replace", "replace-find-empty", "find must not be empty", operation_id=operation_id)
    if expected_matches is not None and expected_matches < 0:
        return _failure_result("document_replace", "replace-expected-matches-invalid", "expected_matches must be >= 0", operation_id=operation_id)
    with session.lock:
        if session.workdir is None:
            return _failure_result("document_replace", "workdir-not-open", "no workdir open; call workdir_open first", operation_id=operation_id)
        workdir = session.workdir
        _adopt_requested_mode(track)
        manifest_before = _workdir_manifest_sha256(workdir)

        def run(target, tx=None):
            revision_before = classify_edit_state(target)["edit_body_sha256"]
            if base_revision is not None and base_revision != revision_before:
                raise ToolError(
                    "stale-document-view",
                    f"base_revision {base_revision!r} does not match the current draft "
                    f"({revision_before!r}); re-read and re-issue the replace",
                    details={"current_revision": revision_before},
                )
            paragraph_ids = _scope_paragraph_ids(target, scope)
            if scope in ("comments", "/comments") or any(pid.startswith("comments.") for pid in paragraph_ids):
                if not allow_comment_text:
                    raise ToolError(
                        "comment-text-requires-opt-in",
                        "scope touches comment paragraphs; pass allow_comment_text=true only when "
                        "the user explicitly asked to edit reviewer comment text",
                    )
            plan: list[dict[str, Any]] = []
            unsafe: list[dict[str, Any]] = []
            normalized = 0
            for paragraph_id in paragraph_ids:
                texts, _styles = _draft_paragraph_state(target, paragraph_id, mode=session.mode)
                flat = "".join(texts)
                if not flat:
                    continue
                offsets: list[tuple[int, str, bool]] = []
                if regex:
                    try:
                        pattern = re.compile(find, 0 if case_sensitive else re.IGNORECASE)
                    except re.error as exc:
                        raise ToolError("replace-regex-invalid", f"invalid regular expression: {exc}") from exc
                    for match in pattern.finditer(flat):
                        offsets.append((match.start(), match.group(0), False))
                else:
                    needle = find if case_sensitive else find.lower()
                    haystack = flat if case_sensitive else flat.lower()
                    folded_haystack = _fold_text(haystack)
                    folded_needle = _fold_text(needle)
                    found: list[int] = []
                    for candidate, hay in ((needle, haystack), (folded_needle, folded_haystack)):
                        cursor = hay.find(candidate)
                        while cursor != -1:
                            if cursor not in found:
                                found.append(cursor)
                            cursor = hay.find(candidate, cursor + 1)
                        if found:
                            break
                    for offset in sorted(found):
                        text = flat[offset : offset + len(needle)]
                        offsets.append((offset, text, text != find))
                _flat_unused, boundaries = _body_boundaries(_block_body([b for b in _read_edit(target)[1] if (_block_ident(b) or ("", ""))[1] == paragraph_id][0]))
                for offset, matched_text, was_folded in offsets:
                    end = offset + len(matched_text)
                    crossed = sorted({kind for boundary_offset, kind in boundaries if offset < boundary_offset < end})
                    region = "mixed" if crossed else _region_at(boundaries, offset)
                    entry = {
                        "paragraph_id": paragraph_id,
                        "offset": offset,
                        "matched_text": matched_text,
                        "region": region,
                        "patchable": not crossed,
                        "normalized": was_folded,
                        "match_ref": _encode_match_ref(paragraph_id, offset, end, matched_text, revision_before),
                    }
                    if crossed:
                        entry["reason"] = "edit-span-crosses-revision-boundary"
                        entry["boundary_crossings"] = crossed
                        unsafe.append(entry)
                    elif region == "insert" and (session.mode or "direct") == "direct":
                        # direct mode cannot rewrite text inside a tracked
                        # insertion: refuse HERE with the mode fix instead of
                        # letting the commit refuse after 11 replacements land
                        entry["reason"] = "revision-text-mutated-in-direct-mode"
                        entry["fix"] = {"track": True}
                        unsafe.append(entry)
                    else:
                        plan.append(entry)
                    if was_folded:
                        normalized += 1
            total = len(plan) + len(unsafe)
            if unsafe:
                mode_fix_needed = any(item.get("reason") == "revision-text-mutated-in-direct-mode" for item in unsafe)
                if mode_fix_needed:
                    raise ToolError(
                        "revision-text-mutated-in-direct-mode",
                        f"{len(unsafe)} of {total} matches sit INSIDE existing tracked insertions and "
                        "direct mode cannot change revision text; resend this call with track=true "
                        "(nothing was written — no revert needed)",
                        details={
                            "unsafe": unsafe,
                            "safe_count": len(plan),
                            "matches": total,
                            "capability": "word.revision.edit-within-revision",
                            "fix": {
                                "tool": "document_replace",
                                "track": True,
                                "args": {
                                    "find": find,
                                    "replace": replace,
                                    "scope": scope,
                                    "regex": regex,
                                    "case_sensitive": case_sensitive,
                                    **({"expected_matches": expected_matches} if expected_matches is not None else {}),
                                },
                            },
                        },
                    )
                raise ToolError(
                    "replace-unsafe-matches",
                    f"{len(unsafe)} of {total} matches sit across a revision boundary and cannot be "
                    "rewritten in one pass — settle those revisions (accept/reject or decide_all) or "
                    "edit each side individually; nothing was written",
                    details={
                        "unsafe": unsafe,
                        "safe_count": len(plan),
                        "matches": total,
                        "capability": "word.text.replace.cross-revision-boundary",
                    },
                )
            if expected_matches is not None and total != expected_matches:
                raise ToolError(
                    "replace-expected-matches-mismatch",
                    f"expected {expected_matches} match(es) but found {total}; nothing was written",
                    details={"expected_matches": expected_matches, "matches": total, "match_plan": plan},
                )
            if not plan:
                return (
                    "success",
                    {
                        "scope": scope,
                        "find": find,
                        "matches": 0,
                        "changed": 0,
                        "noop_reason": "no-match",
                        "match_plan": [],
                        "document_state": {"revision_before": revision_before, "revision_after": revision_before, "draft": "clean"},
                    },
                    "mutation",
                    {**base_evidence_payload(), "checks": [{"name": "document-replace", "status": "pass", "matches": 0}]},
                    [],
                )
            if dry_run:
                return (
                    "success",
                    {
                        "scope": scope,
                        "find": find,
                        "matches": total,
                        "changed": 0,
                        "dry_run": True,
                        "match_plan": plan,
                        "document_state": {
                            "revision_before": revision_before,
                            "revision_after": revision_before,
                            "draft": "clean",
                        },
                    },
                    "mutation",
                    {
                        **base_evidence_payload(),
                        "checks": [{"name": "document-replace", "status": "pass", "matches": total, "dry_run": True}],
                    },
                    [],
                )
            by_paragraph: dict[str, list[dict[str, Any]]] = {}
            for entry in plan:
                by_paragraph.setdefault(entry["paragraph_id"], []).append(entry)
            header, blocks = _apply_replace_matches(target, by_paragraph, replace)
            candidate = header + "\n\n" + "\n\n".join(blocks) + "\n"
            plan_result, mode = _plan_candidate(target, candidate)
            _write_edit(target, header, blocks)
            _refresh_regions(target)
            payload = {
                **base_evidence_payload(),
                "inputs": {"workdir": {"manifest_sha256": manifest_before}},
                "outputs": {"workdir": {"manifest_sha256": _workdir_manifest_sha256(target)}},
                "checks": [{"name": "document-replace", "status": "pass", "matches": len(plan)}],
            }
            return (
                "success",
                {
                    "scope": scope,
                    "find": find,
                    "replace": replace,
                    "matches": total,
                    "changed": len(plan),
                    "normalized_matches": normalized,
                    "edit_mode": mode,
                    "match_plan": plan,
                    "document_state": {
                        "revision_before": revision_before,
                        "revision_after": classify_edit_state(target)["edit_body_sha256"],
                        "draft": "dirty",
                    },
                    "warnings": ([_MODE_DEFAULT_NOTE["note"]] if _MODE_DEFAULT_NOTE["note"] else [])
                    + (
                        [
                            f"replace-hit-{len(plan)}-occurrences: this call changed {len(plan)} "
                            f"places in scope={scope}. If you meant to change ONE place, use "
                            "document_patch with a match_ref; if the count is the point, pass "
                            "expected_matches to pin it."
                        ]
                        if len(plan) > 1 and expected_matches is None
                        else []
                    )
                    + list(plan_result.warnings or []),
                    "next": "diff_preview to inspect, then commit_sync",
                },
                "mutation",
                payload,
                [],
            )

        return _mutation_tool(
            operation_id,
            "document_replace",
            {
                "workdir": str(workdir),
                "find": find,
                "replace": replace,
                "scope": scope,
                "regex": regex,
                "case_sensitive": case_sensitive,
                "expected_matches": expected_matches,
                "track": track,
            },
            workdir,
            directory=True,
            evidence_path=workdir / "run.evidence.json",
            run=run,
            store_workdir=workdir,
        )


@mcp.tool()
def document_patch(
    hunks: list[dict] | None = None,
    diff: str | None = None,
    base_revision: str | None = None,
    operation_id: str | None = None,
    allow_comment_text: bool = False,
    track: bool | None = None,
) -> CallToolResult:
    """Apply a batch of edits to the draft in one atomic call — the
    file-like editing surface over replace/insert/delete. Exactly one input:

    - ``hunks``: list of hunk dicts. A paragraph may carry several
      non-overlapping replace hunks (like git apply); overlapping spans are
      rejected. Replace: {"paragraph_id": "P3", "old": "...", "new":
      "..."} — old must be unique in the paragraph. Cross-region spans are
      allowed: the sync engine decides ownership (region-exact, or
      proportional-preserve with requires_style_review=true). Replace works
      on ANY projected paragraph — body prose,
      table cell text, content-control text, part paragraphs (structure
      stays locked; only text moves). Insert: {"insert_after": "P3",
      "text": "...", "inherit": "P2"?} and Delete: {"delete": "P4"} are
      body-surface only — container topology changes go through the
      structural tools.
    - ``diff``: unified diff against the editable projection as a virtual
      file (read it with document_read). Context must match exactly; each
      changed block becomes a minimal replace hunk. Whole-block
      insertions/deletions in a diff are rejected — use hunks for those.

    ``allow_comment_text``: comment paragraphs (``comments.P*``) are
    annotation content; replaces there are refused unless the caller passes
    true, which is only appropriate when the user explicitly asked to edit
    reviewer comment text.

    ``base_revision``: the opaque revision token from document_read /
    document_search (``revision=...``). Checked inside the mutation
    transaction — after the operation-id ledger, so an exact retry of a
    completed patch always replays its original result; a genuinely stale
    view fails with stale-document-view — re-read, then re-patch.
    Mixed-style spans are decided by the sync engine (deterministic
    mapping + warning when anchored; rejection otherwise), not by a
    paragraph primitive's single-region gate.

    All hunks are validated before anything is written: any failure leaves
    the draft untouched. Mutating: ``operation_id`` is optional; identical
    retries replay the original result, changed input fails
    operation-id-reused. Writes the draft only — run diff_preview then
    commit_sync."""
    if (hunks is None) == (diff is None):
        return _failure_result(
            "document_patch",
            "invalid-arguments",
            "provide exactly one of hunks or diff",
            operation_id=operation_id,
        )
    if session.workdir is None:
        return _failure_result("document_patch", "workdir-not-open", "no workdir open; call workdir_open first", operation_id=operation_id)
    with session.lock:
        workdir = session.workdir
        if diff is not None:
            try:
                ops = _parse_unified_diff(diff)
                old_side = [line for kind, line in ops if kind in ("=", "-")]
                new_side = [line for kind, line in ops if kind in ("=", "+")]
                real_header, real_blocks = _read_edit(workdir)
                _ensure_diff_base_matches(real_header, real_blocks, old_side)
                hunks = _hunks_from_projection_diff(old_side, new_side)
            except ToolError as exc:
                return _failure_result("document_patch", exc.code, exc.detail, operation_id=operation_id)
            if not hunks:
                return _failure_result("document_patch", "patch-empty", "the diff changes nothing", operation_id=operation_id)
        try:
            normalized = _normalize_patch_hunks(hunks or [])
        except ToolError as exc:
            return _failure_result("document_patch", exc.code, exc.detail, operation_id=operation_id)
        if not normalized:
            return _failure_result("document_patch", "invalid-arguments", "hunks must not be empty", operation_id=operation_id)
        _adopt_requested_mode(track)
        manifest_before = _workdir_manifest_sha256(workdir)

        preflight_scope = sorted(
            {
                *(h["paragraph_id"] for kind, h in normalized if kind in ("replace", "delete")),
                *(
                    anchor
                    for kind, h in normalized if kind == "insert"
                    for anchor in (h["insert_after"], h["inherit"] or h["insert_after"])
                ),
            }
        )

        healed: list[str] = []

        def run(target, tx=None):
            revision_before = classify_edit_state(target)["edit_body_sha256"]
            # base_revision rides INSIDE the transaction: an exact retry of a
            # completed operation replays from the ledger without reaching
            # this line, so a lost response can always be safely re-sent.
            if base_revision is not None:
                state_now = classify_edit_state(target)
                current = state_now["edit_body_sha256"]
                # The immediately-previous committed view is a legitimate
                # older read (concurrent edit healed below); any other hash
                # means the caller never read this document's state.
                previous = state_now.get("base_projection_sha256")
                if base_revision != current and base_revision != previous:
                    raise ToolError(
                        "stale-document-view",
                        f"base_revision {base_revision!r} matches neither the current draft "
                        f"({current!r}) nor the previous committed view ({previous!r}) — "
                        "re-read with document_read/document_search and re-patch "
                        "(current_revision is in data.current_revision)",
                        details={"current_revision": current, "previous_revision": previous},
                    )
                if base_revision != current:
                    if _hunks_still_applicable(target, normalized):
                        healed.append(
                            "stale-document-view-healed: the draft changed since your read "
                            "but every hunk anchor still resolves; the patch was applied "
                            "against the current draft"
                        )
                    else:
                        raise ToolError(
                            "stale-document-view",
                            f"base_revision {base_revision!r} does not match the current draft "
                            f"({current!r}) and at least one hunk no longer resolves — re-read "
                            "with document_read/document_search and re-patch (current revision "
                            "is in data.current_revision)",
                            details={"current_revision": current},
                        )
            header, blocks, applied = _apply_document_hunks(target, normalized, allow_comment_text=allow_comment_text)
            normalized_notes = next(
                (entry["matches"] for entry in applied if entry.get("kind") == "matched-with-normalization"),
                [],
            )
            candidate = header + "\n\n" + "\n\n".join(blocks) + "\n"
            plan, mode = _plan_candidate(target, candidate)
            _write_edit(target, header, blocks)
            _refresh_regions(target)
            def _patch_preview(paragraph_id: str) -> dict[str, Any] | None:
                block = next((b for b in blocks if _block_ident(b) == ("p", paragraph_id)), None)
                if block is None:
                    return None
                flat, _ = _body_boundaries(_block_body(block))
                anchor = next(
                    (
                        item.get("new")
                        for item in applied
                        if item.get("paragraph_id") == paragraph_id and item.get("new")
                    ),
                    None,
                )
                if anchor and anchor in flat:
                    start = flat.index(anchor)
                    head = max(0, start - 40)
                    tail = min(len(flat), start + len(anchor) + 40)
                    text = ("…" if head else "") + flat[head:tail] + ("…" if tail < len(flat) else "")
                else:
                    text = flat[:120] + ("…" if len(flat) > 120 else "")
                return {"paragraph_id": paragraph_id, "chars": len(flat), "result": text}

            # the edited paragraph as it now READS: duplication and broken
            # sentence joins are visible in this very response, instead of
            # forcing a read-back (or a revert) to notice them
            result_preview = [
                preview
                for preview in map(
                    _patch_preview,
                    sorted({item["paragraph_id"] for item in applied if "paragraph_id" in item}),
                )
                if preview
            ]
            repeated: list[str] = []
            for preview in result_preview:
                repeated_text = _repeated_join(preview["result"])
                if repeated_text:
                    repeated.append(
                        f"result-repeat {preview['paragraph_id']}: {repeated_text!r} now appears "
                        "twice in a row — that is a join error (usually text that was already "
                        "there), fix it in the next patch; nothing to revert"
                    )
            payload = {
                **base_evidence_payload(),
                "inputs": {"workdir": {"manifest_sha256": manifest_before}},
                "outputs": {"workdir": {"manifest_sha256": _workdir_manifest_sha256(target)}},
                "checks": [{"name": "document-patch", "status": "pass"}],
            }
            return (
                "success",
                {
                    "applied": applied,
                    "result_preview": result_preview,
                    "affected_paragraph_ids": sorted({a["paragraph_id"] for a in applied if "paragraph_id" in a}),
                    "edit_mode": mode,
                    "style_assignment": {
                        "policy": (
                            "proportional-preserve"
                            if (proportional_preserve := any(h.get("assignment_reason") == "proportional-preserve" for h in plan.hunks))
                            else "region-exact"
                        ),
                        "confidence": (
                            "policy"
                            if any(h.get("assignment_reason") == "proportional-preserve" for h in plan.hunks)
                            else "exact"
                        ),
                        "paragraph_ids": sorted({
                            h["paragraph_id"] for h in plan.hunks
                            if h.get("assignment_reason") == "proportional-preserve"
                        }),
                    },
                    "document_state": {
                        "revision_before": revision_before,
                        "revision_after": classify_edit_state(target)["edit_body_sha256"],
                        "draft": "dirty",
                    },
                    "warnings": ([_MODE_DEFAULT_NOTE["note"]] if _MODE_DEFAULT_NOTE["note"] else []) + list(plan.warnings or []) + healed + [
                        "matched-with-normalization: hunk #{hunk} ({pid}) matched the document "
                        "text {doc!r} after folding width/punctuation variants{detail}".format(
                            hunk=item["hunk"],
                            pid=item["paragraph_id"],
                            doc=item["document_text"],
                            detail=(
                                " (" + ", ".join(f"yours={d['yours']!r} doc={d['document']!r}" for d in item["differences"][:3]) + ")"
                                if item["differences"]
                                else ""
                            ),
                        )
                        for item in normalized_notes
                    ] + repeated,
                    "requires_style_review": proportional_preserve,
                    "style_note": (
                        "the engine distributed the new text across the original style "
                        "regions (proportional-preserve); no hunk splitting is needed — "
                        "inspect with diff_preview, then commit"
                        if proportional_preserve
                        else "region-exact style ownership; no action needed"
                    ),
                    "draft": "dirty",
                    "next": (
                        "run diff_preview and inspect the style redistribution "
                        "before commit_sync"
                        if proportional_preserve
                        else "diff_preview to inspect style ownership, then commit_sync"
                    ),
                },
                "mutation",
                payload,
                [],
            )

        return _mutation_tool(
            operation_id,
            "document_patch",
            {
                "workdir": str(workdir),
                "hunks": hunks,
                "diff": diff,
                "allow_comment_text": allow_comment_text,
                "base_revision": base_revision,
                "track": track,
            },
            workdir,
            directory=True,
            evidence_path=workdir / "run.evidence.json",
            run=run,
            store_workdir=workdir,
            preflight_scope=preflight_scope,
        )


@mcp.tool()
def diff_preview() -> str:
    """Dry-run of commit_sync: per changed paragraph, the hunks with style
    ownership (source_style_set -> assigned_styles), warnings, or the
    rejection reason. Read-only; never mutates the workdir. Tracked hunks
    report their generated revisions."""
    with session.lock:
        workdir = session.require()
        state = classify_edit_state(workdir)
        if state["state"] != "dirty":
            return _json({"state": state["state"], "changes": []})
        from .edit import _build_revision_context, parse_edit_projection
        from .edit_sync import _document_has_revisions
        from .typed_core import effective_edit_mode

        text = (workdir / PROJECTION_FILE).read_text(encoding="utf-8")
        projection = parse_edit_projection(text)
        typed = parse_typed((workdir / "typed.md").read_text(encoding="utf-8"))
        format_data = json.loads((workdir / "format.json").read_text(encoding="utf-8"))
        try:
            mode = session.mode or effective_edit_mode(
                source_track_enabled=bool(format_data.get("source_track_enabled")),
                has_pending_revisions=_document_has_revisions(typed),
            )
            revision_ctx = (
                _build_revision_context(
                    typed, format_data, workdir, mode=mode,
                    author=session.author or "", author_source="session",
                )
                if mode == "track"
                else None
            )
            plan = plan_sync(
                typed, projection, format_data,
                mode=mode, revision_ctx=revision_ctx,
            )
            return _json(
                {
                    "state": "dirty",
                    "edit_mode": mode,
                    "changed_paragraph_ids": plan.changed_ids,
                    "hunks": plan.hunks,
                    "warnings": plan.warnings,
                }
            )
        except ValidationError as exc:
            return _json({"state": "dirty", "edit_mode": mode, "rejected": str(exc), "changes": []})


def _probe_commit_buildability(workdir: Path) -> None:
    """Refuse a save whose result cannot be rebuilt faithfully.

    Some tracked-revision shapes (a revision nested inside another, as produced
    by editing text inside an existing insertion) round-trip with a different
    node skeleton than the model, which makes every later build fail with
    'output text or structure differs' — i.e. a bricked workdir. The sync is
    rehearsed on a throwaway copy and the build is run there, so the real
    workdir is never advanced into that state (issue #83)."""
    import shutil
    import tempfile

    scratch_root = Path(tempfile.mkdtemp(prefix="docx2typed-commit-probe-"))
    try:
        probe = scratch_root / "wd"
        shutil.copytree(workdir, probe, dirs_exist_ok=False)
        _commit_sync_impl(probe, origin="probe", agent_gate=False)
        built = build_workdir(probe, scratch_root / "probe.docx")
        verify_workdir(probe, built)
    except (ValidationError, TypedError, ToolError) as exc:
        message = str(exc)
        if "output text or structure differs" in message or "output final-view text differs" in message:
            raise ToolError(
                "commit-state-unbuildable",
                "this save would produce a tracked-revision shape the builder cannot reproduce "
                f"({message}). Nothing was committed. Settle the affected paragraph's revisions "
                "(accept_revision / reject_revision / decide_all) and redo the edit on plain text, "
                "or revert and switch the document to direct editing (track=false) if no revision "
                "history is required",
                details={"capability": "word.revision.edit-inside-revision", "issue": "#83", "build_error": message[:200]},
            ) from exc
        raise
    finally:
        shutil.rmtree(scratch_root, ignore_errors=True)


def _adopt_baseline(baseline: Path, target: Path) -> list[str]:
    """Adopt a freshly extracted baseline as the next state of THIS workspace.

    A structural operation (accept-all, table edit) changes the template and the
    source fingerprint, so the whole canonical state is replaced rather than
    patched. Doing that here, in the store lane, is what turns "a new workdir
    appears" into "the document gains its next version" (P3, PRD
    version-timeline): the user keeps one workspace and one timeline.
    """
    adopted: list[str] = []
    for name in (*CANONICAL_ASSETS, "revisions.json"):
        origin = baseline / name
        if origin.is_file():
            shutil.copyfile(origin, target / name)
            adopted.append(name)
    for name in ("edit.md", "edit.state.json"):
        (target / name).unlink(missing_ok=True)
    refresh_edit_projection(target, init=True)
    classify_edit_state(target)
    validate_workdir(target)
    return adopted


def _version_state_dir(workdir: Path, record: dict[str, Any], scratch: Path) -> Path:
    """The canonical state of a version: materialised from the object pool when
    the version has a tree there (ADR 0043), else copied from the generation that
    still carries it (a workdir saved before the pool existed)."""
    from . import objectstore

    tree_object = record.get("tree_object")
    if isinstance(tree_object, str) and tree_object and objectstore.has(workdir, "tree", tree_object):
        report = objectstore.verify(workdir, tree_object)
        if not report["ok"]:
            raise ToolError(
                "version-content-missing",
                f"{record.get('version')} content is incomplete: {', '.join(report['missing'][:3])}",
            )
        objectstore.materialize(workdir, tree_object, scratch)
        # the pool carries canonical state only: rebuild the bound
        # projection/sidecar pair so the directory is a usable workdir
        refresh_edit_projection(scratch, init=True)
        return scratch
    generation = store_dir_path(workdir) / "generations" / str(record.get("generation") or "")
    if generation.is_dir():
        return generation
    raise ToolError(
        "version-trimmed",
        f"{record.get('version')} has neither pooled content nor a generation left",
    )


def _paragraph_records(workdir: Path) -> dict[str, dict]:
    """format.json's paragraph records keyed by id."""
    try:
        data = json.loads((workdir / "format.json").read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return {}
    return {str(r.get("id")): r for r in (data.get("paragraphs") or []) if isinstance(r, dict)}


def _cherry_pick_guard(source: Path, target: Path, paragraph_ids: list[str]) -> str | None:
    """Why a selective restore is refused, or None when it is dependency-free.

    A paragraph is not self-contained: its format record carries ``token_ids``
    into a GLOBAL token table (revision open/close pairs, comment anchors,
    rpr-change, ranges — a real paragraph references a dozen or more), so
    swapping one across versions can dangle OOXML bytes or split an anchor pair.
    v1 therefore only moves dependency-free, zero-token paragraphs; table, SDT,
    and text-box topology is refused by container id as well (ADR 0045), never
    approximated."""
    source_records, current_records = _paragraph_records(source), _paragraph_records(target)
    problems: list[str] = []
    for paragraph_id in sorted(set(paragraph_ids)):
        if re.search(r"(?:^|\.)[TSB]\d+\.", paragraph_id):
            problems.append(f"{paragraph_id} is coupled to table/SDT/text-box topology")
            continue
        old, new = source_records.get(paragraph_id), current_records.get(paragraph_id)
        if old is None or new is None:
            problems.append(f"{paragraph_id} is missing from one of the two states")
            continue
        for label, record in (("the version", old), ("the current state", new)):
            tokens = record.get("token_ids") or []
            if tokens:
                names = ", ".join(str(item[0]) for item in tokens[:4])
                problems.append(f"{paragraph_id} references {len(tokens)} token(s) in {label} ({names}…)")
    if not problems:
        return None
    return (
        "selective restore moves dependency-free paragraphs only: "
        + "; ".join(problems)
        + ". Restore the whole version, or patch the paragraph explicitly with its target text."
    )


def _apply_cherry_pick(source: Path, target: Path, paragraph_ids: list[str]) -> list[str]:
    """Splice the selected paragraphs (text AND format record) from a version
    into the current canonical state. Returns the ids actually replaced."""
    from .typed_core import parse_typed, serialize_typed

    chosen = set(paragraph_ids)
    source_document = parse_typed((source / "typed.md").read_text(encoding="utf-8"))
    target_document = parse_typed((target / "typed.md").read_text(encoding="utf-8"))
    source_paragraphs = {p.paragraph_id: p for p in source_document.paragraphs}
    replaced: list[str] = []
    for index, paragraph in enumerate(target_document.paragraphs):
        if paragraph.paragraph_id in chosen and paragraph.paragraph_id in source_paragraphs:
            target_document.paragraphs[index] = source_paragraphs[paragraph.paragraph_id]
            replaced.append(paragraph.paragraph_id)
    missing = sorted(chosen - set(replaced))
    if missing:
        raise ToolError("version-content-missing", f"{', '.join(missing)} not found in the restored version")
    atomic_write_text(target / "typed.md", serialize_typed(target_document))

    source_records = _paragraph_records(source)
    format_path = target / "format.json"
    format_data = json.loads(format_path.read_text(encoding="utf-8"))
    for record in format_data.get("paragraphs") or []:
        record_id = record.get("id")
        source_record = source_records.get(record_id) if record_id in chosen else None
        if source_record is not None:
            # take the version's record wholesale: text and the record that
            # describes it must move together (they are one paragraph state)
            record.clear()
            record.update(source_record)
    atomic_write_text(
        format_path, json.dumps(format_data, ensure_ascii=False, indent=2, sort_keys=True) + "\n"
    )
    return sorted(replaced)


def _recorded_assets(generation_dir: Path) -> dict[str, str]:
    """path -> sha256 as recorded in a generation manifest ({} if unreadable)."""
    try:
        manifest = json.loads((generation_dir / "generation.json").read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return {}
    return {
        str(entry.get("path")): str(entry.get("sha256"))
        for entry in (manifest.get("assets") or [])
        if isinstance(entry, dict)
    }


def _changed_paragraph_texts(left: Path, right: Path) -> list[str]:
    """Paragraph ids whose visible text differs between two canonical states
    (used for a version's change summary). Read-only; unreadable sides compare
    as "no information" rather than raising."""
    from .typed_core import parse_typed, visible_text

    def texts(path: Path) -> dict[str, str]:
        try:
            document = parse_typed(path.read_text(encoding="utf-8"))
        except (OSError, Exception):  # noqa: BLE001 - a summary never blocks a save
            return {}
        return {p.paragraph_id: visible_text(p.nodes) for p in document.paragraphs}

    before, after = texts(left), texts(right)
    return sorted(
        pid for pid in set(before) | set(after) if before.get(pid) != after.get(pid)
    )


def _commit_decision(workdir: Path) -> dict[str, Any]:
    """What a save would do, decided before any generation is created.

    Three independent facts (ADR 0044): the draft may differ from canonical
    (`draft dirty`), canonical may differ from the version HEAD names
    (`version dirty`), and the collaboration ledger may be behind the files
    (`publish pending`). Only when all three are false is a save a true no-op.
    """
    state = classify_edit_state(workdir)
    head = store_head_version(workdir)
    collab = document_state_readonly(workdir)
    draft_dirty = state["state"] in {"dirty", "conflict"}
    version_dirty = bool(head["dirty"])
    publish_pending = not bool(collab.get("current_matches_filesystem"))
    return {
        "draft_dirty": draft_dirty,
        "version_dirty": version_dirty,
        "publish_pending": publish_pending,
        "noop": not (draft_dirty or version_dirty or publish_pending),
        "head": head,
        "draft_state": state["state"],
    }


def _commit_sync_impl(
    workdir: Path,
    *,
    origin: str,
    batch_id: str | None = None,
    agent_gate: bool = True,
) -> dict[str, Any]:
    state = _agent_preflight(workdir) if agent_gate else document_state(workdir)
    parent_snapshot = state["current_snapshot"]["id"]
    _, warnings, changed = sync_edit_projection(
        workdir, track=session.track_override, author=session.author
    )
    collaboration = document_state(workdir)
    published = None
    if changed and not collaboration["current_matches_filesystem"]:
        try:
            published = publish_current(
                workdir,
                expected_parent_snapshot=parent_snapshot,
                origin=origin,
                changed_paragraph_ids=changed,
                batch_id=batch_id,
            )
        except CollaborationError as exc:
            raise ToolError(exc.code, exc.detail) from exc
    return {
        "changed_paragraph_ids": changed,
        "warnings": warnings,
        "edit_mode": session.mode,
        "state": "clean",
        "current_snapshot": published["current_snapshot"] if published else collaboration["current_snapshot"],
    }


@mcp.tool()
def history_verify() -> str:
    """Check retained content and report deliberate trims separately.

    Read-only. Missing objects are reported per version by name; retention
    trims are reported as ``content: trimmed`` rather than corruption
    (ADR 0043)."""
    with session.lock:
        workdir = session.require()
        return _json(store_history_verify(workdir))


@mcp.tool()
def history_gc(keep_last: int = 50, dry_run: bool = True) -> str:
    """Reclaim history content past retention; commit metadata is never dropped.

    Keeps the most recent ``keep_last`` versions plus explicitly pinned
    versions. ``pin=True`` is an explicit retention root; ``label`` is
    descriptive metadata and naming alone does not pin. Applied trims are
    recorded in ``history-trim.jsonl`` behind a journaled decision,
    ``retention-marked``, ``retention-swept``, and ``completed`` phases, so
    recovery never infers retention from filesystem timestamps. The version
    commit remains in the graph. ``dry_run`` (default) reports without deleting
    or recording a trim."""
    with session.lock:
        workdir = session.require()
        return _json(store_history_gc(workdir, keep_last=keep_last, dry_run=dry_run))


@mcp.tool()
def history_list(limit: int = 20, offset: int = 0) -> str:
    """The version timeline: versions from HEAD backwards, newest first.

    The history is the version chain itself (each version names its parent), so
    this needs no index file. ``content`` says whether a version's content is
    still retained or has been trimmed by retention; a trimmed version still
    appears (its metadata is kept) but can no longer be restored."""
    with session.lock:
        workdir = session.require()
        history = store_history_list(workdir, limit=limit, offset=offset)
        head = store_head_version(workdir)
        history["draft_dirty"] = classify_edit_state(workdir)["state"] in {"dirty", "conflict"}
        history["version_dirty"] = bool(head["dirty"])
        return _json(history)


@mcp.tool()
def history_restore(
    version: str,
    paragraphs: list[str] | None = None,
    operation_id: str | None = None,
) -> CallToolResult:
    """Restore an earlier version by copying its state FORWARD into a new version.

    History never rewinds (ADR 0039): the restore creates a NEW version whose
    content comes from ``version``, and every version in between stays listed
    and restorable. The restored state is verified against the hashes recorded
    with that version, and the result is published like any other canonical
    write, so the caller's next ``commit_sync`` sees a consistent workspace.

    ``paragraphs=[...]`` is the guarded selective form (ADR 0045): v1 moves
    only dependency-free, zero-token paragraphs in both states. Revision,
    comment/bookmark, range, SDT/content-control, and table-topology coupling
    refuses with ``partial-restore-needs-dependent-state`` naming what it would
    need — a bare swap could dangle revision or anchor bytes. This is closest
    to ``git restore --source V12 -- path`` followed by a commit, not a strict
    commit-level cherry-pick.

    Refused when: the version is unknown (``version-not-found``), its content
    has been trimmed (``version-trimmed``), a recorded hash does not match
    (``version-content-missing``), the draft has unsaved edits
    (``restore-draft-dirty``), or the selection is coupled
    (``partial-restore-needs-dependent-state``)."""
    with session.lock:
        if session.workdir is None:
            return _failure_result("history_restore", "workdir-not-open", "no workdir open; call workdir_open first", operation_id=operation_id)
        workdir = session.workdir
        record = find_version(workdir, version)
        if record is None:
            return _failure_result(
                "history_restore",
                "version-not-found",
                f"{version} is not in this document's history; call history_list",
                operation_id=operation_id,
            )
        if version in trimmed_versions(workdir):
            return _failure_result(
                "history_restore",
                "version-trimmed",
                f"{version} was trimmed by retention (its content is gone on purpose); "
                "restore a version that is still retained",
                operation_id=operation_id,
            )
        pooled = isinstance(record.get("tree_object"), str) and record.get("tree_object")
        source_generation = store_dir_path(workdir) / "generations" / str(record.get("generation") or "")
        if not pooled and not source_generation.is_dir():
            return _failure_result(
                "history_restore",
                "version-trimmed",
                f"{version} content has been trimmed by retention and can no longer be restored",
                operation_id=operation_id,
            )
        draft = classify_edit_state(workdir)
        if draft["state"] in {"dirty", "conflict"}:
            return _failure_result(
                "history_restore",
                "restore-draft-dirty",
                "the draft has unsaved edits; commit_sync or revert them before restoring, "
                "otherwise the restore would silently discard them",
                operation_id=operation_id,
            )
        if paragraphs is not None and not paragraphs:
            return _failure_result(
                "history_restore",
                "restore-empty-selection",
                "paragraphs must name at least one paragraph; omit it to restore the whole version",
                operation_id=operation_id,
            )
        selection = sorted(set(paragraphs)) if paragraphs else None
        before_root = read_root(workdir)
        head_tree_before = store_head_version(workdir)["tree"]
        manifest_before = _workdir_manifest_sha256(workdir)

        def run(target, tx=None):
            scratch_state = (
                tx.staging("version-state") if tx is not None
                else Path(tempfile.mkdtemp(prefix="docx2typed-version-")) / "state"
            )
            source_state = _version_state_dir(workdir, record, scratch_state)
            recorded = _recorded_assets(source_state)
            for name in CANONICAL_ASSETS:
                origin = source_state / name
                if not origin.is_file():
                    raise ToolError("version-content-missing", f"{version} has no recorded {name}")
                expected = recorded.get(name)
                if expected is not None and file_sha256(origin) != expected:
                    raise ToolError(
                        "version-content-missing",
                        f"{version} content for {name} does not match the hash recorded with it",
                    )
                shutil.copyfile(origin, target / name)
            picked: list[str] = []
            if selection is not None:
                # selective restore: start from the CURRENT state and move only
                # the dependency-free paragraphs out of the version, so the rest
                # of the document keeps every later change
                current_root = read_root(workdir)
                shutil.copyfile(current_root / "typed.md", target / "typed.md")
                shutil.copyfile(current_root / "format.json", target / "format.json")
                refusal = _cherry_pick_guard(source_state, target, selection)
                if refusal is not None:
                    raise ToolError("partial-restore-needs-dependent-state", refusal)
                picked = _apply_cherry_pick(source_state, target, selection)
            # the projection and its sidecar are a hash-bound pair: regenerate
            # them from the restored canonical state instead of copying half a
            # binding (copying the pair raw fails as edit-header-tampered)
            for name in ("edit.md", "edit.state.json"):
                (target / name).unlink(missing_ok=True)
            refresh_edit_projection(target, init=True)
            classify_edit_state(target)
            validate_workdir(target)
            changed = _changed_paragraph_texts(before_root / "typed.md", target / "typed.md")
            unchanged = canonical_tree_digest(target) == head_tree_before
            if not unchanged:
                collaboration = document_state(target)
                publish_current(
                    target,
                    expected_parent_snapshot=collaboration["current_snapshot"]["id"],
                    origin="cherry-pick" if selection is not None else "restore",
                    restored_from=version,
                    changed_paragraph_ids=changed,
                )
            if tx is not None and not unchanged:
                picked_text = ", ".join(picked)
                label = f"selective restore {version}: {picked_text}" if picked else f"restore {version}"
                tx.mark_save_boundary(
                    origin="cherry-pick" if selection is not None else "restore",
                    restored_from=version,
                    label=label,
                    pin=False,  # descriptive, not a user naming
                )
            payload = {
                **base_evidence_payload(),
                "inputs": {"workdir": {"manifest_sha256": manifest_before}},
                "outputs": {"workdir": {"manifest_sha256": _workdir_manifest_sha256(target)}},
                "checks": [{"name": "history-restore", "status": "pass", "restored_from": version}],
            }
            return (
                "success",
                {
                    "restored_from": version,
                    "cherry_picked": picked or None,
                    "changed_paragraph_ids": changed,
                    "noop": unchanged,
                    "state": "clean",
                },
                "mutation",
                payload,
                [],
            )

        return _mutation_tool(
            operation_id,
            "history_restore",
            {"workdir": str(workdir), "version": version, "paragraphs": selection},
            workdir,
            directory=True,
            evidence_path=workdir / "run.evidence.json",
            run=run,
            store_workdir=workdir,
            require_agent_preflight=True,
        )


@mcp.tool()
def commit_sync(
    operation_id: str | None = None,
    label: str | None = None,
    pin: bool = False,
) -> CallToolResult:
    """Save the current document state — the save boundary that creates a Version.

    Two independent facts decide what happens (ADR 0044):
      - ``draft_dirty``  — edit.md differs from canonical -> the draft is synced;
      - ``version_dirty`` — canonical differs from the tree HEAD names -> a new
        Version is created (this is why a clean draft can still save a version,
        e.g. after format/decision operations);
    neither, and the collaboration ledger already matches -> a true no-op: no
    generation, no Version, nothing written.

    ``label`` is descriptive metadata; ``pin=True`` independently retains the
    version beyond the count limit. Naming a version does not pin it.
    Mutating: ``operation_id`` may be omitted (the server generates a fresh id);
    identical retries replay the original result."""
    with session.lock:
        if session.workdir is None:
            return _failure_result("commit_sync", "workdir-not-open", "no workdir open; call workdir_open first", operation_id=operation_id)
        workdir = session.workdir
        decision = _commit_decision(workdir)
        if decision["noop"] and operation_id is None:
            # nothing to write: no draft, no version pending, ledger current
            history = store_history_list(workdir, limit=1)
            envelope = result_envelope(
                "commit_sync",
                "success",
                data={
                    "noop": True,
                    "reason": "already saved: draft clean, no unsaved canonical change",
                    "changed_paragraph_ids": [],
                    "version": decision["head"]["version"],
                    "version_dirty": False,
                    "draft_dirty": False,
                    "current_snapshot": document_state_readonly(workdir).get("current_snapshot"),
                },
            )
            return mcp_result(envelope)
        manifest_before = _workdir_manifest_sha256(workdir)
        head_tree_before = decision["head"]["tree"]

        def run(target, tx=None):
            revision_before = classify_edit_state(target)["edit_body_sha256"]
            _probe_commit_buildability(target)
            result = _commit_sync_impl(target, origin="agent", agent_gate=False)
            creates_version = canonical_tree_digest(target) != head_tree_before
            if creates_version and tx is not None:
                # label is metadata; pin is an independent retention root
                tx.mark_save_boundary(origin="commit_sync", label=label, pin=pin)
            result["version"] = {
                "created": creates_version,
                "previous": decision["head"]["version"],
                "label": label if creates_version else None,
                "pin": pin if creates_version else False,
            }
            result["document_state"] = {
                "revision_before": revision_before,
                "revision_after": classify_edit_state(target)["edit_body_sha256"],
                "draft": "clean",
            }
            payload = {
                **base_evidence_payload(),
                "inputs": {"workdir": {"manifest_sha256": manifest_before}},
                "outputs": {"workdir": {"manifest_sha256": _workdir_manifest_sha256(target)}},
                "checks": [{"name": "commit-sync", "status": "pass"}],
            }
            return "success", result, "mutation", payload, []

        return _mutation_tool(
            operation_id,
            "commit_sync",
            {"workdir": str(workdir), "label": label, "pin": pin},
            workdir,
            directory=True,
            evidence_path=workdir / "run.evidence.json",
            run=run,
            store_workdir=workdir,
            require_agent_preflight=True,
        )


@mcp.tool()
def accept_revision(revision_key: str, expected_fingerprint: str, operation_id: str | None = None) -> CallToolResult:
    """Accept one tracked revision addressed by its revision_key
    (part|kind|w:id|fingerprint, from revisions.json) plus the expected
    fingerprint. Accept insert = unwrap its text; accept delete = remove it.
    Publish transactionally and regenerate all derived views. Requires a
    clean workdir.

    Mutating: ``operation_id`` may be omitted (the server generates a fresh
    id). Identical retries replay the original result; changed input or a
    reused id from ANY earlier call (success or failure) fails
    operation-id-reused."""
    with session.lock:
        if session.workdir is None:
            return _failure_result("accept_revision", "workdir-not-open", "no workdir open; call workdir_open first", operation_id=operation_id)
        workdir = session.workdir
        manifest_before = _workdir_manifest_sha256(workdir)

        def run(target, tx=None):
            from .decisions import _decide_single

            decision = _decide_single(
                target, revision_key, action="accept", author=session.author,
                expected_fingerprint=expected_fingerprint,
            )
            payload = {
                **base_evidence_payload(),
                "inputs": {"workdir": {"manifest_sha256": manifest_before}},
                "outputs": {"workdir": {"manifest_sha256": _workdir_manifest_sha256(target)}},
                "decision": {"action": "accept", "w_id": decision["w_id"], "paragraph_id": decision["paragraph_id"]},
                "checks": [{"name": "revision-accepted", "status": "pass"}],
            }
            return "success", {"decision": decision, "state": "clean"}, "mutation", payload, []

        return _mutation_tool(
            operation_id,
            "accept_revision",
            {
                "workdir": str(workdir),
                "revision_key": revision_key,
                "expected_fingerprint": expected_fingerprint,
            },
            workdir,
            directory=True,
            evidence_path=workdir / "run.evidence.json",
            run=run,
            store_workdir=workdir,
            require_agent_preflight=True,
        )


@mcp.tool()
def reject_revision(revision_key: str, expected_fingerprint: str, operation_id: str | None = None) -> CallToolResult:
    """Reject one tracked revision addressed by revision_key + fingerprint.
    Reject insert = remove its text; reject delete = restore its text.
    Publish transactionally; requires a clean workdir.

    Mutating: ``operation_id`` may be omitted (the server generates a fresh
    id). Identical retries replay the original result; changed input or a
    reused id from ANY earlier call (success or failure) fails
    operation-id-reused."""
    with session.lock:
        if session.workdir is None:
            return _failure_result("reject_revision", "workdir-not-open", "no workdir open; call workdir_open first", operation_id=operation_id)
        workdir = session.workdir
        manifest_before = _workdir_manifest_sha256(workdir)

        def run(target, tx=None):
            from .decisions import _decide_single

            decision = _decide_single(
                target, revision_key, action="reject", author=session.author,
                expected_fingerprint=expected_fingerprint,
            )
            payload = {
                **base_evidence_payload(),
                "inputs": {"workdir": {"manifest_sha256": manifest_before}},
                "outputs": {"workdir": {"manifest_sha256": _workdir_manifest_sha256(target)}},
                "decision": {"action": "reject", "w_id": decision["w_id"], "paragraph_id": decision["paragraph_id"]},
                "checks": [{"name": "revision-rejected", "status": "pass"}],
            }
            return "success", {"decision": decision, "state": "clean"}, "mutation", payload, []

        return _mutation_tool(
            operation_id,
            "reject_revision",
            {
                "workdir": str(workdir),
                "revision_key": revision_key,
                "expected_fingerprint": expected_fingerprint,
            },
            workdir,
            directory=True,
            evidence_path=workdir / "run.evidence.json",
            run=run,
            store_workdir=workdir,
            require_agent_preflight=True,
        )


@mcp.tool()
def reinsert_deleted_text(
    revision_key: str,
    expected_fingerprint: str,
    text: str | None = None,
    operation_id: str | None = None,
) -> CallToolResult:
    """Create a NEW insertion revision after an existing deletion (key +
    fingerprint), without touching the original deletion. ``text`` defaults
    to the deleted text.

    Mutating: ``operation_id`` may be omitted (the server generates a fresh
    id). Identical retries replay the original result; changed input or a
    reused id from ANY earlier call (success or failure) fails
    operation-id-reused."""
    with session.lock:
        if session.workdir is None:
            return _failure_result("reinsert_deleted_text", "workdir-not-open", "no workdir open; call workdir_open first", operation_id=operation_id)
        workdir = session.workdir
        manifest_before = _workdir_manifest_sha256(workdir)

        def run(target, tx=None):
            from .decisions import _decide_single

            decision = _decide_single(
                target, revision_key, action="reinsert",
                author=session.author, text=text,
                expected_fingerprint=expected_fingerprint,
            )
            payload = {
                **base_evidence_payload(),
                "inputs": {"workdir": {"manifest_sha256": manifest_before}},
                "outputs": {"workdir": {"manifest_sha256": _workdir_manifest_sha256(target)}},
                "decision": {"action": "reinsert", "w_id": decision["w_id"], "paragraph_id": decision["paragraph_id"]},
                "checks": [{"name": "revision-reinserted", "status": "pass"}],
            }
            return "success", {"decision": decision, "state": "clean"}, "mutation", payload, []

        return _mutation_tool(
            operation_id,
            "reinsert_deleted_text",
            {
                "workdir": str(workdir),
                "revision_key": revision_key,
                "expected_fingerprint": expected_fingerprint,
                "text": text,
            },
            workdir,
            directory=True,
            evidence_path=workdir / "run.evidence.json",
            run=run,
            store_workdir=workdir,
            require_agent_preflight=True,
        )


@mcp.tool()
def delete_comment(comment_id: str, operation_id: str | None = None) -> CallToolResult:
    """Delete one Word comment by its w:id: the comments.xml entry, every
    commentRangeStart/End anchor and commentReference in the document are
    removed. Publishes transactionally; requires a clean workdir.

    Mutating: ``operation_id`` may be omitted (the server generates a fresh
    id). Identical retries replay the original result; changed input or a
    reused id from ANY earlier call (success or failure) fails
    operation-id-reused."""
    with session.lock:
        if session.workdir is None:
            return _failure_result("delete_comment", "workdir-not-open", "no workdir open; call workdir_open first", operation_id=operation_id)
        workdir = session.workdir
        manifest_before = _workdir_manifest_sha256(workdir)

        def run(target, tx=None):
            from .decisions import _delete_comment

            decision = _delete_comment(target, comment_id)
            payload = {
                **base_evidence_payload(),
                "inputs": {"workdir": {"manifest_sha256": manifest_before}},
                "outputs": {"workdir": {"manifest_sha256": _workdir_manifest_sha256(target)}},
                "decision": {"action": "comment-delete", "comment_id": decision["comment_id"]},
                "checks": [{"name": "comment-deleted", "status": "pass"}],
            }
            return "success", {"decision": decision, "state": "clean"}, "mutation", payload, []

        return _mutation_tool(
            operation_id,
            "delete_comment",
            {
                "workdir": str(workdir),
                "comment_id": comment_id,
            },
            workdir,
            directory=True,
            evidence_path=workdir / "run.evidence.json",
            run=run,
            store_workdir=workdir,
            require_agent_preflight=True,
        )


def _table_op_tool(operation: str, table_ref: str, output: str, workdir_out: str | None, *numbers: int, operation_id: str, discard_content: bool = False) -> CallToolResult:
    """Structural table edit. Without ``workdir_out`` the new baseline is
    ADOPTED as this workspace's next version (P3): one workspace, one timeline,
    no sibling workdir. With it, the old behaviour (a separate baseline
    workdir) is preserved."""
    from .decisions import _apply_table_op

    with session.lock:
        if session.workdir is None:
            return _failure_result(f"table_{operation}", "workdir-not-open", "no workdir open; call workdir_open first", operation_id=operation_id)
        workdir = session.workdir
        adopt = workdir_out is None
        epoch_before = int((store_head_version(workdir) or {}).get("baseline_epoch") or 1)
        manifest_before = _workdir_manifest_sha256(workdir)

        def run(target, tx=None):
            baseline_dir = Path(str(workdir_out)).resolve() if workdir_out else (
                tx.staging("baseline") if tx is not None
                else Path(tempfile.mkdtemp(prefix="docx2typed-baseline-")) / "baseline"
            )
            if tx is not None:
                output_staged = tx.staging("decided.docx")
                created = _apply_table_op(
                    target, table_ref, operation, list(numbers),
                    output_staged, baseline_dir,
                    discard_content=discard_content,
                )
                tx.stage_external(Path(output).resolve(), output_staged, mode="create")
                # The final path does not exist yet: publish happens after the
                # prepared journal. Hash the staged artifact and record the
                # final path in the evidence payload.
                output_real = Path(output).resolve()
                docx_evidence = {"sha256": file_sha256(output_staged), "path": str(output_real)}
            else:
                created = _apply_table_op(
                    target, table_ref, operation, list(numbers),
                    Path(output), baseline_dir,
                    discard_content=discard_content,
                )
                output_real = Path(output).resolve()
                docx_evidence = {"sha256": file_sha256(output_real)}
            if adopt:
                _adopt_baseline(created, target)
                collaboration = document_state(target)
                publish_current(
                    target,
                    expected_parent_snapshot=collaboration["current_snapshot"]["id"],
                    origin="baseline-transition",
                    changed_paragraph_ids=[],
                )
                if tx is not None:
                    tx.mark_save_boundary(
                        origin="baseline-transition",
                        label=f"table {operation} (baseline E{epoch_before + 1})",
                        pin=False,
                        baseline_epoch=epoch_before + 1,
                    )
            payload = {
                **base_evidence_payload(),
                "inputs": {"workdir": {"manifest_sha256": manifest_before}},
                "outputs": {
                    "docx": docx_evidence,
                    "workdir": {"manifest_sha256": _workdir_manifest_sha256(created)},
                },
                "table": table_ref,
                "checks": [{"name": f"table-{operation}", "status": "pass"}],
            }
            return (
                "success",
                {
                    "operation": operation,
                    "table": table_ref,
                    "adopted": adopt,
                    "workdir": str(workdir) if adopt else str(created),
                },
                "mutation",
                payload,
                [],
            )

        return _mutation_tool(
            operation_id,
            f"table_{operation}",
            {
                "workdir": str(workdir),
                "table_ref": table_ref,
                "output": output,
                "workdir_out": workdir_out,
                "args": list(numbers),
                "discard_content": discard_content,
            },
            workdir if adopt else Path(str(workdir_out)).resolve(),
            directory=True,
            evidence_path=(workdir / "run.evidence.json") if adopt else (Path(str(workdir_out)).resolve() / "run.evidence.json"),
            run=run,
            store_workdir=workdir,
            store_generation=adopt,
            require_agent_preflight=True,
        )


@mcp.tool()
def table_insert_row(table_ref: str, after: int, output: str, workdir_out: str | None = None, operation_id: str | None = None) -> CallToolResult:
    """Insert an empty row after ``after`` (0-based) in ``table_ref`` (T0).
    Without ``workdir_out`` the new baseline is ADOPTED as this workspace's next version (a baseline transition: one workspace, one timeline); with it, a separate baseline workdir is produced instead. The source is never mutated in place.
    Mutating: ``operation_id`` is optional; omitted IDs are generated."""
    return _table_op_tool("insert-row", table_ref, output, workdir_out, after, operation_id=operation_id)


@mcp.tool()
def table_delete_row(table_ref: str, row: int, output: str, workdir_out: str | None = None, operation_id: str | None = None) -> CallToolResult:
    """Delete row ``row`` (0-based) from ``table_ref``.
    Mutating: ``operation_id`` is optional; omitted IDs are generated."""
    return _table_op_tool("delete-row", table_ref, output, workdir_out, row, operation_id=operation_id)


@mcp.tool()
def table_insert_col(table_ref: str, after: int, output: str, workdir_out: str | None = None, operation_id: str | None = None) -> CallToolResult:
    """Insert an empty column after ``after`` (0-based) in every row.
    Mutating: ``operation_id`` is optional; omitted IDs are generated."""
    return _table_op_tool("insert-col", table_ref, output, workdir_out, after, operation_id=operation_id)


@mcp.tool()
def table_delete_col(table_ref: str, col: int, output: str, workdir_out: str | None = None, operation_id: str | None = None) -> CallToolResult:
    """Delete column ``col`` (0-based) from ``table_ref``.
    Mutating: ``operation_id`` is optional; omitted IDs are generated."""
    return _table_op_tool("delete-col", table_ref, output, workdir_out, col, operation_id=operation_id)


@mcp.tool()
def table_merge_cells(table_ref: str, row: int, col: int, span: int, output: str, workdir_out: str | None = None, discard_content: bool = False, operation_id: str | None = None) -> CallToolResult:
    """Merge ``span`` cells horizontally starting at (row, col) via gridSpan.

    Fail-closed: when a spanned cell (beyond the first) carries text, the
    merge is refused with ``merge-would-discard-content`` unless
    ``discard_content=true`` explicitly drops it. The first cell's content
    is always kept. Mutating: ``operation_id`` is optional; omitted IDs are generated."""
    return _table_op_tool("merge-cells", table_ref, output, workdir_out, row, col, span, operation_id=operation_id, discard_content=discard_content)


@mcp.tool()
def table_split_cells(table_ref: str, row: int, col: int, span: int, output: str, workdir_out: str | None = None, operation_id: str | None = None) -> CallToolResult:
    """Split the cell at (row, col) into ``span`` cells.
    Mutating: ``operation_id`` is optional; omitted IDs are generated."""
    return _table_op_tool("split-cells", table_ref, output, workdir_out, row, col, span, operation_id=operation_id)


@mcp.tool()
def decide_all(
    action: str,
    output: str,
    workdir_out: str | None = None,
    operation_id: str | None = None,
) -> CallToolResult:
    """Accept or reject every revision and produce a clean baseline.

    With ``workdir_out`` the decided DOCX is re-extracted into a NEW workdir
    (normalization governance) and this workspace is left alone. WITHOUT it —
    the recommended form — the baseline is ADOPTED by this workspace as its
    next version: the template/source fingerprint switch is recorded as a
    baseline transition, so the user keeps one workspace and one timeline
    instead of accumulating sibling workdirs. ``action``: accept | reject.

    Mutating: ``operation_id`` may be omitted (the server generates a fresh
    id). Identical retries replay the original result; changed input or a
    reused id from ANY earlier call (success or failure) fails
    operation-id-reused."""
    with session.lock:
        if session.workdir is None:
            return _failure_result("decide_all", "workdir-not-open", "no workdir open; call workdir_open first", operation_id=operation_id)
        workdir = session.workdir
        if action not in ("accept", "reject"):
            return _failure_result("decide_all", "invalid-action", "action must be accept or reject", operation_id=operation_id)
        adopt = workdir_out is None
        epoch_before = int((store_head_version(workdir) or {}).get("baseline_epoch") or 1)
        manifest_before = _workdir_manifest_sha256(workdir)

        def run(target, tx=None):
            from .decisions import _decide_all

            baseline_dir = Path(workdir_out).resolve() if workdir_out else (
                tx.staging("baseline") if tx is not None
                else Path(tempfile.mkdtemp(prefix="docx2typed-baseline-")) / "baseline"
            )
            if tx is not None:
                output_staged = tx.staging("decided.docx")
                created = _decide_all(target, action, output_staged, baseline_dir)
                tx.stage_external(Path(output).resolve(), output_staged, mode="create")
                # Publish happens after the prepared journal: the final path
                # does not exist yet, so hash the staged artifact.
                output_real = Path(output).resolve()
                docx_evidence = {"sha256": file_sha256(output_staged), "path": str(output_real)}
            else:
                created = _decide_all(target, action, Path(output), baseline_dir)
                output_real = Path(output).resolve()
                docx_evidence = {"sha256": file_sha256(output_real)}
            if adopt:
                _adopt_baseline(created, target)
                collaboration = document_state(target)
                publish_current(
                    target,
                    expected_parent_snapshot=collaboration["current_snapshot"]["id"],
                    origin="baseline-transition",
                    changed_paragraph_ids=[],
                )
                if tx is not None:
                    tx.mark_save_boundary(
                        origin="baseline-transition",
                        label=f"{action} all revisions (baseline E{epoch_before + 1})",
                        pin=False,
                        baseline_epoch=epoch_before + 1,
                    )
            report = json.loads((created / "decisions.json").read_text(encoding="utf-8"))
            payload = {
                **base_evidence_payload(),
                "inputs": {"workdir": {"manifest_sha256": manifest_before}},
                "outputs": {
                    "docx": docx_evidence,
                    "workdir": {"manifest_sha256": _workdir_manifest_sha256(created)},
                },
                "action": action,
                "revision_count": report["revision_count"],
                "checks": [{"name": "decide-all", "status": "pass"}],
            }
            return (
                "success",
                {
                    "action": action,
                    "output": str(output_real),
                    "adopted": adopt,
                    "workdir": str(workdir) if adopt else str(created),
                    "note": (
                        "baseline adopted by this workspace as its next version "
                        f"(baseline epoch {epoch_before + 1}); no sibling workdir was created"
                        if adopt
                        else "original workdir untouched; decisions.json in the new workdir"
                    ),
                },
                "mutation",
                payload,
                [],
            )

        anchor = workdir if adopt else Path(str(workdir_out)).resolve()
        return _mutation_tool(
            operation_id,
            "decide_all",
            {
                "workdir": str(workdir),
                "action": action,
                "output": output,
                "workdir_out": workdir_out,
            },
            anchor,
            directory=True,
            evidence_path=(workdir / "run.evidence.json") if adopt else (anchor / "run.evidence.json"),
            run=run,
            store_workdir=workdir,
            store_generation=adopt,
            require_agent_preflight=True,
        )


def _comments_listing(workdir: Path) -> list[dict[str, str]]:
    """Comment inventory for one workdir: id, author, date, text, anchors.
    Lock-free; callers hold the session lock."""
    import re as _re
    import zipfile

    fmt = json.loads((workdir / "format.json").read_text(encoding="utf-8"))
    comments: list[dict[str, str]] = []
    for record in fmt.get("paragraphs", []):
        if record.get("part_key") == "comments" and not record.get("deleted"):
            entry_id = record.get("part_entry_id")
            if entry_id is not None:
                comments.append({
                    "id": str(entry_id),
                    "paragraph_id": record["id"],
                })
    # anchor mapping: body paragraph records carry token ids; the token
    # table records comment-start anchors with their w:id
    anchors: dict[str, list[str]] = {}
    tokens = fmt.get("tokens", {})
    for record in fmt.get("paragraphs", []):
        if record.get("part_key"):
            continue
        for token_id, _kind in record.get("token_ids", []) or []:
            token = tokens.get(token_id) or {}
            if token.get("kind") == "comment-start":
                attrs = token.get("attrs", {}) or {}
                anchors.setdefault(str(attrs.get("w:id")), []).append(record["id"])
    # author/date/text from the template's comments.xml (read-only)
    template = workdir / fmt.get("template", "_template.docx")
    meta: dict[str, dict[str, str]] = {}
    try:
        with zipfile.ZipFile(template) as archive:
            comments_xml = archive.read("word/comments.xml").decode("utf-8")
        for match in _re.finditer(
            r'<w:comment\s+[^>]*?w:id="(\d+)"[^>]*>.*?</w:comment>',
            comments_xml, _re.S,
        ):
            tag = match.group(0)
            author = _re.search(r'w:author="([^"]*)"', tag)
            date = _re.search(r'w:date="([^"]*)"', tag)
            text = "".join(_re.findall(r"<w:t[^>]*>([^<]*)</w:t>", tag))
            meta[match.group(1)] = {
                "author": author.group(1) if author else "",
                "date": date.group(1) if date else "",
                "text": text,
            }
    except Exception:  # noqa: BLE001 - metadata is best-effort
        pass
    result = []
    for comment in comments:
        comment.update(meta.get(comment["id"], {"author": "", "date": "", "text": ""}))
        comment["anchor_paragraphs"] = anchors.get(comment["id"], [])
        result.append(comment)
    return result


@mcp.tool()
def list_comments() -> str:
    """List every comment in the opened workdir: id, author, date, text,
    and the body paragraphs carrying its anchors. The comment workflow
    (delete_comment, decide_all) addresses comments by id."""
    with session.lock:
        workdir = session.require()
        return _json({"comments": _comments_listing(workdir)})


@mcp.tool()
def get_comment(comment_id: str) -> str:
    """Read one comment: id, author, date, text, and the body paragraphs
    carrying its anchors."""
    with session.lock:
        workdir = session.require()
        for comment in _comments_listing(workdir):
            if comment["id"] == str(comment_id):
                return _json(comment)
        raise ToolError("comment-not-found", f"comment {comment_id} not in the workdir")

@mcp.tool()
def review_preflight() -> str:
    """Return the agent gate, current snapshot, staged snapshot, and wake queue."""
    with session.lock:
        workdir = session.require()
        return _json(preflight(workdir, readonly=True))


@mcp.tool()
def review_state() -> str:
    """Read the collaboration session state without consuming review events."""
    with session.lock:
        return _json(document_state_readonly(session.require()))
@mcp.tool()
def review_external_preflight(
    expected_parent_snapshot: str,
    operation: str = "import",
    operation_id: str | None = None,
) -> CallToolResult:
    """Issue an idempotent CAS guard for an external import or rollback writer.

    The guard is recorded through the same operation ledger and evidence seam
    as other mutating MCP calls, so retries replay byte-exact and changed input
    fails closed."""
    with session.lock:
        if session.workdir is None:
            return _failure_result(
                "review_external_preflight",
                "workdir-not-open",
                "no workdir open; call workdir_open first",
                operation_id=operation_id,
            )
        workdir = session.workdir
        manifest_before = _workdir_manifest_sha256(workdir)

        def run(target, tx=None):
            data = external_write_guard(
                target,
                expected_parent_snapshot=expected_parent_snapshot,
                operation=operation,
            )
            payload = {
                **base_evidence_payload(),
                "inputs": {
                    "workdir": {"manifest_sha256": manifest_before},
                    "expected_parent_snapshot": expected_parent_snapshot,
                    "operation": operation,
                },
                "outputs": {
                    "workdir": {"manifest_sha256": _workdir_manifest_sha256(target)}
                },
                "checks": [{"name": "external-preflight", "status": "pass"}],
            }
            return "success", data, "mutation", payload, []

        return _mutation_tool(
            operation_id,
            "review_external_preflight",
            {
                "workdir": str(workdir),
                "expected_parent_snapshot": expected_parent_snapshot,
                "operation": operation,
            },
            workdir,
            directory=True,
            evidence_path=workdir / "external-preflight.evidence.json",
            run=run,
        )
@mcp.tool()
def review_settlement_plan(event_ids: list[str] | None = None) -> str:
    """Return mixed accept/reject/defer decisions, patches, and carry-forward guards."""
    with session.lock:
        return _json(settlement_plan(session.require(), [str(item) for item in event_ids] if event_ids else None))

@mcp.tool()
def review_settle(event_ids: list[str] | None = None, operation_id: str | None = None) -> CallToolResult:
    """Atomically settle accept/reject decisions and carry deferred items.

    Mutating: ``operation_id`` is optional; identical retries replay the
    original result, and changed input fails ``operation-id-reused``. If
    omitted, the server generates one. An empty event list still means
    "whatever is actionable now", so pass an explicit list for deterministic
    selection."""
    with session.lock:
        if session.workdir is None:
            return _failure_result("review_settle", "workdir-not-open", "no workdir open; call workdir_open first", operation_id=operation_id)
        workdir = session.workdir
        manifest_before = _workdir_manifest_sha256(workdir)
        wanted = [str(item) for item in event_ids] if event_ids else None

        def run(target, tx=None):
            data = settle_decisions(target, wanted)
            payload = {
                **base_evidence_payload(),
                "inputs": {"workdir": {"manifest_sha256": manifest_before}},
                "outputs": {"workdir": {"manifest_sha256": _workdir_manifest_sha256(target)}},
                "checks": [{"name": "review-settled", "status": "pass"}],
            }
            return "success", data, "mutation", payload, []

        return _mutation_tool(
            operation_id,
            "review_settle",
            {"workdir": str(workdir), "event_ids": wanted},
            workdir,
            directory=True,
            evidence_path=workdir / "run.evidence.json",
            run=run,
            store_workdir=workdir,
            require_agent_preflight=True,
        )


def _review_apply_batch(workdir: Path, batch_id: str, requested_event_id: str | None = None) -> dict[str, Any]:
    events = review_snapshot(workdir)["events"]
    requested = next(
        (item for item in events if str(item.get("event_id")) == str(requested_event_id)),
        None,
    ) if requested_event_id else None
    if requested_event_id and requested is None:
        raise ToolError("review-event-not-found", f"review event {requested_event_id} not found")
    if requested and requested.get("type") != "patch":
        raise ToolError("not-a-patch", f"review event {requested_event_id} is not a semantic patch")
    if requested and requested.get("delivery_state") == "applied":
        return {"event": requested, "state": "already-applied"}
    batch_events = [
        item for item in events
        if item.get("type") == "patch"
        and item.get("status") == "queued"
        and (not batch_id or str(item.get("batch_id")) == batch_id)
    ]
    batch_events.sort(
        key=lambda item: int(str(item.get("staged_snapshot", "H0.0")).rsplit(".", 1)[-1])
    )
    if requested and requested not in batch_events:
        raise ToolError("patch-not-queued", f"review event {requested_event_id} is not queued")
    if not batch_events:
        raise ToolError("patch-batch-empty", f"no queued patches in batch {batch_id or '<none>'}")
    state = document_state(workdir)
    if not state["current_matches_filesystem"]:
        raise ToolError("current-snapshot-drift", "typed.md differs from the canonical snapshot")
    expected_parent = state["current_snapshot"]["id"]
    for patch in batch_events:
        if patch.get("parent_snapshot") != expected_parent:
            raise ToolError(
                "patch-parent-mismatch",
                f"patch parent {patch.get('parent_snapshot')} does not match {expected_parent}",
            )
        expected_parent = str(patch.get("staged_snapshot") or expected_parent)
        _validate_collab_patch_target(workdir, patch)
    ranges: dict[str, list[tuple[int, int]]] = {}
    for patch in batch_events:
        target = patch["target"]
        start, end = int(target["start_offset"]), int(target["end_offset"])
        paragraph_id = str(patch["paragraph_id"])
        ranges.setdefault(paragraph_id, []).append((start, end))
    for paragraph_id, paragraph_ranges in ranges.items():
        previous_end = -1
        previous_start = -1
        for start, end in sorted(paragraph_ranges):
            if start < previous_end or (start == previous_start and start == end):
                raise ToolError("patch-overlap", f"{paragraph_id}: overlapping patches require a new selection")
            previous_start, previous_end = start, end
    claimed: list[dict[str, Any]] = []
    try:
        for patch in batch_events:
            claimed.append(update_review_event(workdir, str(patch["event_id"]), {"delivery_state": "in_progress"}))
        for patch in sorted(
            batch_events,
            key=lambda item: (str(item["paragraph_id"]), -int(item["target"]["start_offset"])),
        ):
            _apply_patch_to_draft(workdir, patch, validate=False)
        committed = _commit_sync_impl(workdir, origin="human_ui", batch_id=batch_id or None)
        if not committed["changed_paragraph_ids"]:
            raise ToolError("patch-noop", "human patch batch produced no canonical change")
        updated = [
            update_review_event(
                workdir,
                str(patch["event_id"]),
                {
                    "delivery_state": "applied",
                    "review_decision": "adjusted",
                    "applied_snapshot": committed["current_snapshot"]["id"],
                },
            )
            for patch in batch_events
        ]
        result = {"events": updated, "commit": committed, "state": "applied"}
        if requested_event_id:
            result["event"] = next(item for item in updated if str(item["event_id"]) == str(requested_event_id))
        return result
    except Exception as exc:  # noqa: BLE001 - restore draft and keep the batch queued
        try:
            refresh_edit_projection(workdir, discard=True)
        finally:
            for patch in claimed:
                update_review_event(
                    workdir,
                    str(patch["event_id"]),
                    {"delivery_state": "queued", "last_error": str(exc)},
                )
        raise ToolError("patch-apply-failed", str(exc)) from exc


@mcp.tool()
def review_apply_patch(event_id: str, operation_id: str | None = None) -> CallToolResult:
    """Apply the queued human patch batch containing ``event_id`` atomically.

    Mutating: ``operation_id`` is optional; identical retries
    replay the original result, changed input fails operation-id-reused. When
    ``operation_id`` is omitted the stable event-derived id
    ``review-apply-patch-<event_id>`` is used — the event uniquely names the
    one-shot apply, so a retry still replays byte-exact."""
    with session.lock:
        if session.workdir is None:
            return _failure_result("review_apply_patch", "workdir-not-open", "no workdir open; call workdir_open first", operation_id=operation_id)
        workdir = session.workdir
        op_id = operation_id or f"review-apply-patch-{event_id}"
        manifest_before = _workdir_manifest_sha256(workdir)

        def run(target, tx=None):
            event = next(
                (item for item in review_snapshot(target)["events"] if str(item.get("event_id")) == str(event_id)),
                None,
            )
            if event is None:
                raise ToolError("review-event-not-found", f"review event {event_id} not found")
            if event.get("delivery_state") == "applied":
                data = {"event": event, "state": "already-applied"}
            else:
                data = _review_apply_batch(target, str(event.get("batch_id") or ""), str(event_id))
            payload = {
                **base_evidence_payload(),
                "inputs": {"workdir": {"manifest_sha256": manifest_before}},
                "outputs": {"workdir": {"manifest_sha256": _workdir_manifest_sha256(target)}},
                "checks": [{"name": "review-patch-applied", "status": "pass"}],
            }
            return "success", data, "mutation", payload, []

        return _mutation_tool(
            op_id,
            "review_apply_patch",
            {"workdir": str(workdir), "event_id": event_id},
            workdir,
            directory=True,
            evidence_path=workdir / "run.evidence.json",
            run=run,
            store_workdir=workdir,
        )


@mcp.tool()
def review_apply_batch(batch_id: str, operation_id: str | None = None) -> CallToolResult:
    """Apply one queued human patch batch as one canonical transaction.

    Mutating: ``operation_id`` is optional; identical retries
    replay the original result, changed input fails operation-id-reused. When
    ``operation_id`` is omitted the stable batch-derived id
    ``review-apply-batch-<batch_id>`` is used — the batch uniquely names the
    one-shot apply, so a retry still replays byte-exact."""
    with session.lock:
        if session.workdir is None:
            return _failure_result("review_apply_batch", "workdir-not-open", "no workdir open; call workdir_open first", operation_id=operation_id)
        workdir = session.workdir
        op_id = operation_id or f"review-apply-batch-{batch_id}"
        manifest_before = _workdir_manifest_sha256(workdir)

        def run(target, tx=None):
            data = _review_apply_batch(target, str(batch_id))
            payload = {
                **base_evidence_payload(),
                "inputs": {"workdir": {"manifest_sha256": manifest_before}},
                "outputs": {"workdir": {"manifest_sha256": _workdir_manifest_sha256(target)}},
                "checks": [{"name": "review-batch-applied", "status": "pass"}],
            }
            return "success", data, "mutation", payload, []

        return _mutation_tool(
            op_id,
            "review_apply_batch",
            {"workdir": str(workdir), "batch_id": batch_id},
            workdir,
            directory=True,
            evidence_path=workdir / "run.evidence.json",
            run=run,
            store_workdir=workdir,
        )
@mcp.tool()
def review_inbox(include_acknowledged: bool = False) -> str:
    """Read queued review events together with the mandatory agent preflight."""
    with session.lock:
        workdir = session.require()
        queue = review_snapshot_readonly(workdir)
        gate = preflight(workdir, readonly=True)
        allowed = {"queued", "acknowledged"} if include_acknowledged else {"queued"}
        events = [event for event in queue["events"] if event.get("status") in allowed]
        batches = sorted({str(event["batch_id"]) for event in events if event.get("batch_id")})
        return _json(
            {
                "preflight": gate,
                "events": events,
                "counts": queue["counts"],
                "wake": {"required": bool(events), "batch_ids": batches, "event_count": len(events)},
            }
        )


@mcp.tool()
def review_ack(event_ids: list[str], operation_id: str | None = None) -> CallToolResult:
    """Acknowledge review events after the agent has consumed them.

    Mutating: ``operation_id`` is optional; identical retries replay the
    original result, and changed input fails ``operation-id-reused``. If
    omitted, the server generates one. The event list remains required."""
    with session.lock:
        if session.workdir is None:
            return _failure_result("review_ack", "workdir-not-open", "no workdir open; call workdir_open first", operation_id=operation_id)
        if not event_ids:
            return _failure_result("review_ack", "event-ids-required", "provide at least one review event id", operation_id=operation_id)
        workdir = session.workdir
        wanted = [str(item) for item in event_ids]
        manifest_before = _workdir_manifest_sha256(workdir)

        def run(target, tx=None):
            acknowledged = acknowledge_review(target, wanted)
            data = {"acknowledged": acknowledged, "counts": review_snapshot(target)["counts"]}
            payload = {
                **base_evidence_payload(),
                "inputs": {"workdir": {"manifest_sha256": manifest_before}},
                "outputs": {"workdir": {"manifest_sha256": _workdir_manifest_sha256(target)}},
                "checks": [{"name": "review-acknowledged", "status": "pass"}],
            }
            return "success", data, "mutation", payload, []

        return _mutation_tool(
            operation_id,
            "review_ack",
            {"workdir": str(workdir), "event_ids": wanted},
            workdir,
            directory=True,
            evidence_path=workdir / "run.evidence.json",
            run=run,
            store_workdir=workdir,
        )


@mcp.tool()
def revert(operation_id: str | None = None) -> CallToolResult:
    """Discard the uncommitted draft and regenerate the projection from the
    canonical typed source (equivalent to edit refresh --discard).

    Mutating: ``operation_id`` may be omitted (the server generates a fresh
    id). Identical retries replay the original result; changed input or a
    reused id from ANY earlier call (success or failure) fails
    operation-id-reused."""
    with session.lock:
        if session.workdir is None:
            return _failure_result("revert", "workdir-not-open", "no workdir open; call workdir_open first", operation_id=operation_id)
        workdir = session.workdir
        manifest_before = _workdir_manifest_sha256(workdir)

        def run(target, tx=None):
            refresh_edit_projection(target, discard=True)
            payload = {
                **base_evidence_payload(),
                "inputs": {"workdir": {"manifest_sha256": manifest_before}},
                "outputs": {"workdir": {"manifest_sha256": _workdir_manifest_sha256(target)}},
                "checks": [{"name": "draft-reverted", "status": "pass"}],
            }
            return "success", {"state": "clean", "message": "draft discarded"}, "mutation", payload, []

        return _mutation_tool(
            operation_id,
            "revert",
            {"workdir": str(workdir)},
            workdir,
            directory=True,
            evidence_path=workdir / "run.evidence.json",
            run=run,
            store_workdir=workdir,
        )


@mcp.tool()
def build_docx(
    output: str | None = None,
    operation_id: str | None = None,
    version: str | None = None,
 ) -> CallToolResult:
    """Export a DOCX from the saved state — never from an unnamed one.

    Without ``version`` this exports the current state and REQUIRES it to be
    saved: if canonical has drifted from the version HEAD names, the build is
    refused with ``version-save-required`` (an export that no version
    describes cannot be traced, ADR 0044). ``version="V12"`` exports that
    historical version without touching HEAD and without restoring it.

    Mutating: ``operation_id`` may be omitted (the server generates a fresh
    id). Identical retries replay the original result; changed input or a
    reused id from ANY earlier call (success or failure) fails
    operation-id-reused."""
    with session.lock:
        if session.workdir is None:
            return _failure_result("build_docx", "workdir-not-open", "no workdir open; call workdir_open first", operation_id=operation_id)
        workdir = session.workdir
        exported_version: str | None = None
        version_record: dict[str, Any] | None = None
        if version is not None:
            record = find_version(workdir, version)
            if record is None:
                return _failure_result(
                    "build_docx",
                    "version-not-found",
                    f"{version} is not in this document's history; call history_list",
                    operation_id=operation_id,
                )
            if version in trimmed_versions(workdir):
                return _failure_result(
                    "build_docx",
                    "version-trimmed",
                    f"{version} was trimmed by retention and can no longer be exported",
                    operation_id=operation_id,
                )
            pooled = isinstance(record.get("tree_object"), str) and record.get("tree_object")
            generation_dir = store_dir_path(workdir) / "generations" / str(record.get("generation") or "")
            if not pooled and not generation_dir.is_dir():
                return _failure_result(
                    "build_docx",
                    "version-trimmed",
                    f"{version} content has been trimmed by retention and can no longer be exported",
                    operation_id=operation_id,
                )
            version_record = record
            exported_version = version
        manifest_before = _workdir_manifest_sha256(workdir)
        resolved_output = (
            Path(output).resolve()
            if output
            else workdir.resolve().parent / f"{workdir.resolve().name}.docx"
        )
        format_data = json.loads((workdir / "format.json").read_text(encoding="utf-8"))
        try:
            validate_output_path(workdir, format_data, resolved_output)
        except ValidationError as exc:
            return _failure_result(
                "build_docx",
                "output-path-reserved",
                str(exc),
                operation_id=operation_id,
            )
        if exported_version is None:
            decision = _commit_decision(workdir)
            if decision["version_dirty"]:
                return _failure_result(
                    "build_docx",
                    "version-save-required",
                    "the document has changes that no version describes; call commit_sync "
                    "first (or build_docx(version=...) to export a saved version), so the "
                    "exported file can be traced to a version",
                    operation_id=operation_id,
                    details={"version_dirty": True, "head_version": decision["head"]["version"]},
                )
            exported_version = decision["head"]["version"]


        def run(target, tx=None):
            source = target
            if version_record is not None:
                # export a historical version: materialise its state into a
                # scratch workdir and build there, so HEAD and the live
                # workdir are untouched
                scratch = (tx.staging("version-state") if tx is not None else Path(
                    tempfile.mkdtemp(prefix="docx2typed-export-")) / "state")
                source = _version_state_dir(workdir, version_record, scratch)
            if tx is not None:
                staged = tx.staging("build.docx")
                built = _build_workdir_to_staging(source, staged)
                tx.stage_external(resolved_output, staged, mode="replace")
                published = resolved_output
            else:
                built = build_workdir(source, resolved_output)
                published = built
            # remember the PUBLISHED artifact (the staging path in the store
            # lane is transient), so verify_output can omit its argument
            session.last_build_output = Path(published).resolve() if published else resolved_output
            payload = {
                **base_evidence_payload(),
                "inputs": {"workdir": {"manifest_sha256": manifest_before}},
                "outputs": {
                    "docx": {"sha256": file_sha256(built), "bytes": built.stat().st_size}
                },
                "checks": [{"name": "build", "status": "pass"}],
            }
            return (
                "success",
                {
                    "output": str(published),
                    "version": exported_version,
                    "tree": (find_version(workdir, exported_version) or {}).get("head_tree")
                    if exported_version
                    else canonical_tree_digest(workdir),
                },
                "build",
                payload,
                [],
            )

        return _mutation_tool(
            operation_id,
            "build_docx",
            {
                "workdir": str(workdir),
                "output": output,
                "version": version,
            },
            workdir,
            directory=True,
            evidence_path=Path(str(resolved_output) + ".evidence.json"),
            run=run,
            store_workdir=workdir,
            store_generation=False,
        )


@mcp.tool()
def verify_output(output: str | None = None, operation_id: str | None = None) -> CallToolResult:
    """Independently verify a built DOCX against the workdir.

    Verification is idempotent like every other mutating MCP operation:
    identical retries replay the original envelope, while a reused ID after
    a changed workdir, draft, or output package fails closed."""
    import re as _re

    with session.lock:
        if output is None:
            if session.last_build_output is None:
                return _failure_result(
                    "verify_output",
                    "verify-output-required",
                    "no output path given and nothing has been built in this session yet; pass "
                    "output=<the .docx to verify> (a verify right after build_docx may omit it)",
                    operation_id=operation_id,
                )
            output = str(session.last_build_output)
        if session.workdir is None:
            return _failure_result(
                "verify_output",
                "workdir-not-open",
                "no workdir open; call workdir_open first",
                operation_id=operation_id,
            )
        workdir = session.workdir
        resolved_output = Path(output).resolve()
        manifest_before = _workdir_manifest_sha256(workdir)
        state = document_state(workdir)
        edit_state = classify_edit_state(workdir)
        try:
            output_sha256 = file_sha256(resolved_output)
        except OSError:
            output_sha256 = None
        current_snapshot = state.get("current_snapshot")
        snapshot_id = (
            current_snapshot.get("id")
            if isinstance(current_snapshot, dict)
            else None
        )
        canonical_args = {
            "workdir": str(workdir),
            "output": str(resolved_output),
            "output_sha256": output_sha256,
            "current_snapshot": current_snapshot,
            "current_matches_filesystem": state.get("current_matches_filesystem"),
            "edit": {
                "state": edit_state["state"],
                "typed_sha256": edit_state["typed_sha256"],
                "edit_body_sha256": edit_state["edit_body_sha256"],
            },
        }

        def run(target, tx=None):
            verify_workdir(target, resolved_output)
            evidence: dict[str, object] = {"verified": str(output)}
            try:
                with zipfile.ZipFile(resolved_output) as archive:
                    names = {
                        name
                        for name in archive.namelist()
                        if _re.match(rb"word/.*\.xml$", name.encode())
                    }
                    xml = b"".join(
                        archive.read(name) for name in sorted(names)
                    )
                    comments_xml = (
                        archive.read("word/comments.xml")
                        if "word/comments.xml" in names
                        else b""
                    )
                ins = len(_re.findall(rb"<w:ins[ >]", xml))
                dels = len(_re.findall(rb"<w:del[ >]", xml))
                authors = sorted(
                    {
                        value.decode("utf-8", errors="replace")
                        for value in _re.findall(rb'w:author="([^"]*)"', xml)
                    }
                )
                comment_ids = [
                    value.decode("utf-8", errors="replace")
                    for value in _re.findall(
                        rb'<w:comment w:id="(\d+)"', comments_xml
                    )
                ]
                evidence["checks"] = {
                    "text": "pass",
                    "styles": "pass",
                    "structure": "pass",
                    "package": "pass",
                    "revisions": "pass",
                    "comments": "pass",
                }
                evidence["revisions"] = {
                    "insert": ins,
                    "delete": dels,
                    "authors": authors,
                }
                evidence["comments"] = {"ids": comment_ids}
            except Exception as exc:  # noqa: BLE001 - evidence is best-effort
                evidence["evidence_error"] = str(exc)
            payload = {
                **base_evidence_payload(),
                "inputs": {
                    "workdir": {
                        "manifest_sha256": manifest_before,
                        "current_snapshot": snapshot_id,
                        "typed_sha256": edit_state["typed_sha256"],
                        "edit_state": edit_state["state"],
                        "edit_body_sha256": edit_state["edit_body_sha256"],
                    }
                },
                "outputs": {
                    "docx": {
                        "sha256": file_sha256(resolved_output),
                        "bytes": resolved_output.stat().st_size,
                    }
                },
                "verdict": "pass",
                "checks": evidence.get("checks", {}),
                "revisions": evidence.get("revisions", {}),
                "comments": {
                    "count": len(
                        evidence.get("comments", {}).get("ids", [])
                    )
                },
            }
            data = {
                **evidence,
                "current_snapshot": snapshot_id,
                "edit_state": edit_state["state"],
            }
            return "success", data, "verify", payload, []

        return _mutation_tool(
            operation_id,
            "verify_output",
            canonical_args,
            workdir,
            directory=True,
            evidence_path=Path(
                str(resolved_output) + ".verify.evidence.json"
            ),
            run=run,
            include_operation_id_on_evidence_failure=operation_id is not None,
        )


# Tool profiles: the default editing surface is deliberately small (fewer,
# higher-leverage tools select better); revision/comment/table work opts into
# the wider surfaces instead of every agent paying for them.
_PROFILES: dict[str, set[str] | None] = {
    "full": None,  # everything registered
    "editor": {
        "engine_info",
        "workdir_open",
        "workdir_status",
        "revert",
        # commit_sync can require the collaboration preflight; without these the
        # save boundary is unreachable inside the small profile
        "review_preflight",
        "review_ack",
        "review_state",
        "document_read",
        "document_search",
        "document_patch",
        "document_replace",
        "format_span",
        "diff_preview",
        "commit_sync",
        "history_list",
        "history_restore",
        "history_verify",
        "history_gc",
        "build_docx",
        "verify_output",
        # Structural and baseline operations edit THIS workspace as its next
        # version (P3), so they belong to the editing surface, not only to the
        # full one. Single revision decisions and comment tools stay in the
        # review profile: those are per-item review actions.
        "decide_all",
        "table_insert_row",
        "table_delete_row",
        "table_insert_col",
        "table_delete_col",
        "table_merge_cells",
        "table_split_cells",
    },
    "review": {
        "engine_info",
        "workdir_open",
        "workdir_status",
        "document_read",
        "document_search",
        "document_patch",
        "document_replace",
        "format_span",
        "diff_preview",
        "commit_sync",
        "build_docx",
        "verify_output",
        "accept_revision",
        "reject_revision",
        "reinsert_deleted_text",
        "decide_all",
        "list_comments",
        "get_comment",
        "delete_comment",
        "review_preflight",
        "review_state",
        "review_inbox",
        "review_ack",
        "review_settlement_plan",
        "review_settle",
        "review_apply_patch",
        "review_apply_batch",
    },
}


def apply_tool_profile(profile: str) -> list[str]:
    """Restrict the MCP surface to a profile; returns the removed tool names."""
    if profile not in _PROFILES:
        raise ValueError(f"unknown MCP profile {profile!r}; use one of {', '.join(sorted(_PROFILES))}")
    allowed = _PROFILES[profile]
    if allowed is None:
        return []
    global _ACTIVE_TOOL_NAMES
    _ACTIVE_TOOL_NAMES = set(allowed)
    removed: list[str] = []
    for name in sorted(set(mcp._tool_manager._tools) - allowed):
        try:
            mcp.remove_tool(name)
            removed.append(name)
        except Exception:
            continue
    return removed


def main() -> None:
    profile = os.environ.get("DOCX2TYPED_MCP_PROFILE", "full")
    argv = sys.argv[1:]
    if "--profile" in argv:
        profile = argv[argv.index("--profile") + 1] if len(argv) > argv.index("--profile") + 1 else "full"
    if profile != "full":
        apply_tool_profile(profile)
    mcp.run(transport="stdio")


if __name__ == "__main__":
    main()
