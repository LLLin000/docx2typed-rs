"""Shared collaboration snapshots and zero-guess text patches."""
from __future__ import annotations

import hashlib
import json
import os
import tempfile
import uuid
from contextlib import contextmanager
from datetime import datetime, timezone
from functools import wraps
from pathlib import Path
from typing import Any

COLLAB_DIR = ".review"
SESSION_FILE = "session.json"
HISTORY_FILE = "history.jsonl"
COLLAB_SCHEMA = "docx2typed-review-session-1"

SNAPSHOT_DIR = "snapshots"
SNAPSHOT_SCHEMA = "docx2typed-review-snapshot-1"
PATCH_SCHEMA = "docx2typed-document-patch-1"


def _now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


class CollaborationError(ValueError):
    """A fail-closed collaboration contract violation."""

    def __init__(self, code: str, message: str) -> None:
        super().__init__(f"{code}: {message}")
        self.code = code
        self.detail = message


def _root(workdir: Path) -> Path:
    root = Path(workdir).resolve() / COLLAB_DIR
    root.mkdir(parents=True, exist_ok=True)
    return root


def _session_path(workdir: Path) -> Path:
    return Path(workdir).resolve() / COLLAB_DIR / SESSION_FILE

def _history_path(workdir: Path) -> Path:
    return Path(workdir).resolve() / COLLAB_DIR / HISTORY_FILE


def _snapshot_root(workdir: Path) -> Path:
    root = _root(workdir) / SNAPSHOT_DIR
    root.mkdir(parents=True, exist_ok=True)
    return root


def _snapshot_path(workdir: Path, snapshot_id: str) -> Path:
    if not snapshot_id or not snapshot_id.startswith("C"):
        raise ValueError("invalid snapshot id")
    return _snapshot_root(workdir) / f"{snapshot_id}.json"


def _persist_snapshot(workdir: Path, snapshot: dict[str, Any]) -> None:
    """Persist a renderable, read-only view of one canonical round."""
    snapshot_id = str(snapshot.get("id", ""))
    if not snapshot_id:
        return
    path = _snapshot_path(workdir, snapshot_id)
    if path.exists():
        return
    try:
        from .review_console import render_document_fragment

        fragment = render_document_fragment(workdir)
        payload = {
            "schema": SNAPSHOT_SCHEMA,
            "snapshot": snapshot,
            **fragment,
        }
    except Exception as exc:  # noqa: BLE001 - history must not block a write
        payload = {
            "schema": SNAPSHOT_SCHEMA,
            "snapshot": snapshot,
            "error": str(exc),
        }
    _atomic_json(path, payload)

@contextmanager
def writer_lane(workdir: Path):
    """Claim the single canonical writer lane with the store's OS-advisory
    lock (flock/msvcrt, fixed inode). Process death releases the lock, so a
    crash mid-write can never leave the review writer permanently busy; the
    lock file is never deleted or reclaimed by PID/age."""
    lock_path = _root(workdir) / "writer.lock"
    try:
        from .store import WriterBusy, advisory_lane
    except ImportError:  # pragma: no cover - direct script invocation fallback
        from store import WriterBusy, advisory_lane  # type: ignore[no-redef]
    try:
        with advisory_lane(lock_path):
            yield
    except WriterBusy as exc:
        raise CollaborationError("writer-busy", "another canonical transaction is active") from exc

def _writer_transaction(function):
    @wraps(function)
    def wrapped(workdir: Path, *args: Any, **kwargs: Any):
        with writer_lane(workdir):
            return function(workdir, *args, **kwargs)
    return wrapped


def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _sha256_json(value: object) -> str:
    payload = json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
    return hashlib.sha256(payload.encode("utf-8")).hexdigest()


def _atomic_json(path: Path, value: dict[str, Any]) -> None:
    fd, temp_name = tempfile.mkstemp(prefix=f".{path.stem}-", suffix=".tmp", dir=path.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8", newline="\n") as handle:
            json.dump(value, handle, ensure_ascii=False, indent=2)
            handle.write("\n")
        os.replace(temp_name, path)
    finally:
        if os.path.exists(temp_name):
            os.unlink(temp_name)


def _append_history(workdir: Path, event: dict[str, Any]) -> None:
    record = {"schema": "docx2typed-review-history-1", "recorded_at": _now(), **event}
    with _history_path(workdir).open("a", encoding="utf-8", newline="\n") as handle:
        json.dump(record, handle, ensure_ascii=False, sort_keys=True)
        handle.write("\n")


def _snapshot_id(prefix: str, number: int) -> str:
    return f"{prefix}{number}"


def _empty_staged(current: dict[str, Any]) -> dict[str, Any]:
    return {
        "id": f"H{current['id'][1:]}",
        "parent_snapshot": current["id"],
        "base_snapshot": current["id"],
        "patch_ids": [],
        "patch_chain_sha256": _sha256_json([]),
    }


def _new_session(workdir: Path, typed_sha256: str) -> dict[str, Any]:
    current = {
        "id": _snapshot_id("C", 0),
        "typed_sha256": typed_sha256,
        "parent_snapshot": None,
        "origin": "source",
        "changed_paragraph_ids": [],
        "published_at": _now(),
    }
    return {
        "schema": COLLAB_SCHEMA,
        "review_base": {
            "id": _snapshot_id("S", 0),
            "typed_sha256": typed_sha256,
            "parent_snapshot": None,
            "origin": "source",
            "created_at": _now(),
        },
        "current_snapshot": current,
        "staged_snapshot": _empty_staged(current),
        "writer": {"state": "idle", "batch_id": None},
    }


def _read_session(workdir: Path) -> dict[str, Any] | None:
    try:
        value = json.loads(_session_path(workdir).read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None
    return value if isinstance(value, dict) else None


def _validate_session(value: dict[str, Any]) -> dict[str, Any]:
    if value.get("schema") != COLLAB_SCHEMA:
        raise CollaborationError("session-schema", "unexpected collaboration session schema")
    for key in ("review_base", "current_snapshot", "staged_snapshot"):
        if not isinstance(value.get(key), dict):
            raise CollaborationError("session-state", f"missing {key}")
    return value


def ensure_session(workdir: Path) -> dict[str, Any]:
    workdir = Path(workdir).resolve()
    typed_path = workdir / "typed.md"
    if not typed_path.exists():
        raise CollaborationError("workdir-not-found", f"typed.md not found in {workdir}")
    _root(workdir)
    typed_sha256 = _sha256_file(typed_path)
    state = _read_session(workdir)
    if state is None:
        state = _new_session(workdir, typed_sha256)
        _atomic_json(_session_path(workdir), state)
        _persist_snapshot(workdir, state["current_snapshot"])
        _append_history(workdir, {"event": "session-created", "snapshot": state["current_snapshot"]})
        return state
    state = _validate_session(state)
    if not _snapshot_path(workdir, str(state["current_snapshot"].get("id", ""))).exists():
        _persist_snapshot(workdir, state["current_snapshot"])
    return state


def document_state(workdir: Path) -> dict[str, Any]:
    state = ensure_session(workdir)
    actual = _sha256_file(Path(workdir).resolve() / "typed.md")
    result = json.loads(json.dumps(state, ensure_ascii=False))
    result["filesystem_typed_sha256"] = actual
    result["current_matches_filesystem"] = actual == state["current_snapshot"]["typed_sha256"]
    return result


def document_state_readonly(workdir: Path) -> dict[str, Any]:
    """Document/session state without any side effect: never creates the
    session, a snapshot, or a history record. A session that has never seen a
    write reads as an empty state with no current snapshot."""
    workdir = Path(workdir).resolve()
    state = _read_session(workdir)
    if state is None:
        try:
            typed_sha256 = _sha256_file(workdir / "typed.md")
        except OSError:
            typed_sha256 = None
        return {
            "schema": COLLAB_SCHEMA,
            "review_base": None,
            "current_snapshot": None,
            "staged_snapshot": None,
            "writer": {"state": "idle", "batch_id": None},
            "filesystem_typed_sha256": typed_sha256,
            "current_matches_filesystem": False,
        }
    state = _validate_session(state)
    try:
        actual = _sha256_file(workdir / "typed.md")
    except OSError:
        actual = None
    result = json.loads(json.dumps(state, ensure_ascii=False))
    result["filesystem_typed_sha256"] = actual
    result["current_matches_filesystem"] = actual == state["current_snapshot"]["typed_sha256"]
    return result

def preflight(
    workdir: Path,
    paragraph_ids: list[str] | None = None,
    *,
    readonly: bool = False,
) -> dict[str, Any]:
    """Return the gate an agent must pass before changing the canonical AST.

    ``paragraph_ids`` scopes the human-patch gate for paragraph-local edits.
    ``None`` keeps the conservative document-wide gate.  ``readonly`` is used
    by inspection tools and never creates the collaboration session or queue.
    """
    from .review_queue import snapshot as review_snapshot
    from .review_queue import snapshot_readonly

    state = document_state_readonly(workdir) if readonly else document_state(workdir)
    queue = snapshot_readonly(workdir) if readonly else review_snapshot(workdir)
    scope = {str(item) for item in paragraph_ids} if paragraph_ids is not None else None
    queued = [
        event
        for event in queue["events"]
        if event.get("status") == "queued"
        and event.get("delivery_state") not in {"in_progress", "applied", "acknowledged"}
    ]

    def patch_paragraph_ids(event: dict[str, Any]) -> set[str]:
        ids = {str(event.get("paragraph_id") or "")}
        target = event.get("target")
        if isinstance(target, dict):
            ids.add(str(target.get("paragraph_id") or ""))
        return {item for item in ids if item}

    queued_patches = [event for event in queued if event.get("type") == "patch"]
    blocked_patches = [
        event
        for event in queued_patches
        if scope is None or patch_paragraph_ids(event).intersection(scope)
    ]
    reasons: list[str] = []
    if not state["current_matches_filesystem"]:
        reasons.append("current-snapshot-drift")
    if blocked_patches:
        reasons.append("queued-human-patch")
    return {
        "ready": not reasons,
        "reasons": reasons,
        "scope": sorted(scope) if scope is not None else None,
        "current_snapshot": state["current_snapshot"],
        "review_base": state["review_base"],
        "staged_snapshot": state["staged_snapshot"],
        "queued_events": queued,
        "queued_patches": queued_patches,
        "blocked_patches": blocked_patches,
    }


def settlement_plan(workdir: Path, event_ids: list[str] | None = None) -> dict[str, Any]:
    """Partition one queued batch into canonical decisions and human patches."""
    from .review_queue import snapshot_readonly

    state = document_state_readonly(workdir)
    wanted = set(event_ids or [])
    events = [
        event
        for event in snapshot_readonly(workdir)["events"]
        if (not wanted or str(event.get("event_id")) in wanted)
        and event.get("status") in {"queued", "acknowledged"}
    ]
    decisions = [event for event in events if event.get("type") == "decision"]
    patches = [event for event in events if event.get("type") == "patch"]
    current_id = (
        str(state["current_snapshot"].get("id"))
        if isinstance(state.get("current_snapshot"), dict)
        else None
    )
    carry_forward = [
        event
        for event in events
        if event.get("review_decision") == "defer"
        or (
            event.get("type") == "patch"
            and event.get("delivery_state") != "applied"
            and current_id is not None
            and event.get("parent_snapshot") != current_id
        )
    ]
    return {
        "schema": "docx2typed-review-settlement-1",
        "review_base": state["review_base"],
        "current_snapshot": state["current_snapshot"],
        "staged_snapshot": state["staged_snapshot"],
        "decisions": decisions,
        "patches": patches,
        "carry_forward": carry_forward,
        "ready_for_agent_write": not carry_forward and state["current_matches_filesystem"],
    }

def external_write_guard(workdir: Path, *, expected_parent_snapshot: str, operation: str) -> dict[str, Any]:
    """Validate an import/rollback caller before it touches the workdir."""
    if operation not in {"import", "rollback"}:
        raise CollaborationError("external-operation", "operation must be import or rollback")
    state = document_state(workdir)
    current = state["current_snapshot"]
    if current["id"] != expected_parent_snapshot:
        raise CollaborationError(
            "current-parent-mismatch",
            f"expected {expected_parent_snapshot}, current is {current['id']}",
        )
    if not state["current_matches_filesystem"]:
        raise CollaborationError("current-snapshot-drift", "typed.md differs from the canonical snapshot")
    return {
        "schema": "docx2typed-review-external-guard-1",
        "operation": operation,
        "expected_parent_snapshot": current["id"],
        "typed_sha256": current["typed_sha256"],
        "issued_at": _now(),
    }


def _bounded_text(value: Any, name: str, limit: int) -> str:
    if value is None:
        return ""
    text = str(value)
    if len(text) > limit:
        raise CollaborationError("patch-too-large", f"{name} exceeds {limit} characters")
    return text


def _validate_patch(patch: dict[str, Any]) -> dict[str, Any]:
    if patch.get("type") != "patch":
        raise CollaborationError("patch-type", "collaboration event must have type=patch")
    normalized = dict(patch)
    normalized["schema"] = PATCH_SCHEMA
    normalized["event_id"] = str(patch.get("event_id") or uuid.uuid4().hex)
    normalized["client_id"] = _bounded_text(patch.get("client_id") or normalized["event_id"], "client_id", 200)
    normalized["origin"] = _bounded_text(patch.get("origin"), "origin", 32)
    if normalized["origin"] not in {"human_ui", "human_external", "agent"}:
        raise CollaborationError("patch-origin", "origin must be human_ui, human_external, or agent")
    normalized["author"] = _bounded_text(patch.get("author"), "author", 200)
    normalized["parent_snapshot"] = _bounded_text(patch.get("parent_snapshot"), "parent_snapshot", 120)
    normalized["paragraph_id"] = _bounded_text(patch.get("paragraph_id"), "paragraph_id", 120)
    if not normalized["parent_snapshot"] or not normalized["paragraph_id"]:
        raise CollaborationError("patch-target", "parent_snapshot and paragraph_id are required")
    if patch.get("kind", "replace") != "replace":
        raise CollaborationError("patch-kind", "only semantic text replacement patches are supported")
    target = patch.get("target")
    if not isinstance(target, dict):
        raise CollaborationError("patch-target", "target must be an object")
    start = target.get("start_offset")
    end = target.get("end_offset")
    if not isinstance(start, int) or not isinstance(end, int) or start < 0 or end < start:
        raise CollaborationError("patch-range", "target offsets must be an ordered non-negative range")
    before = _bounded_text(patch.get("before"), "before", 8_000)
    after = _bounded_text(patch.get("after"), "after", 8_000)
    expected = _bounded_text(target.get("expected_text"), "target.expected_text", 8_000)
    if before != expected:
        raise CollaborationError("patch-precondition", "before must equal target.expected_text")
    if start == end and before:
        raise CollaborationError("patch-range", "insertions must use an empty before text")
    if start != end and not before:
        raise CollaborationError("patch-range", "deletions or replacements require non-empty before text")
    normalized["kind"] = "replace"
    normalized["target"] = {
        "start_offset": start,
        "end_offset": end,
        "expected_text": expected,
        "left_context": _bounded_text(target.get("left_context"), "target.left_context", 2_000),
        "right_context": _bounded_text(target.get("right_context"), "target.right_context", 2_000),
        "paragraph_fingerprint": _bounded_text(target.get("paragraph_fingerprint"), "target.paragraph_fingerprint", 200),
        "region_fingerprint": _bounded_text(target.get("region_fingerprint"), "target.region_fingerprint", 200),
        "style_region_ids": [str(item) for item in (target.get("style_region_ids") or [])],
    }
    normalized["before"] = before
    normalized["after"] = after
    normalized["review_item_id"] = _bounded_text(
        patch.get("review_item_id") or f"review-{normalized['event_id']}",
        "review_item_id",
        200,
    )
    normalized["delivery_state"] = "staged"
    normalized["review_decision"] = "pending"
    return normalized


@_writer_transaction
def stage_patch(workdir: Path, patch: dict[str, Any]) -> dict[str, Any]:
    from .review_queue import upsert_event

    state = ensure_session(workdir)
    normalized = _validate_patch(patch)
    staged = state["staged_snapshot"]
    expected_parent = staged["id"] if staged["patch_ids"] else state["current_snapshot"]["id"]
    if normalized["parent_snapshot"] != expected_parent:
        raise CollaborationError(
            "staged-parent-mismatch",
            f"patch parent {normalized['parent_snapshot']} does not match {expected_parent}",
        )
    record = upsert_event(workdir, normalized)
    patch_ids = [*staged["patch_ids"], record["event_id"]]
    current_id = state["current_snapshot"]["id"]
    staged_id = f"H{current_id[1:]}.{len(patch_ids)}"
    record["staged_parent_snapshot"] = expected_parent
    record["staged_snapshot"] = staged_id
    state["staged_snapshot"] = {
        "id": staged_id,
        "parent_snapshot": expected_parent,
        "base_snapshot": current_id,
        "patch_ids": patch_ids,
        "patch_chain_sha256": _sha256_json(patch_ids),
    }
    # The queue owns the event file; this update records the staged coordinates
    # without changing the queue state machine.
    from .review_queue import update_event

    record = update_event(workdir, record["event_id"], {
        "staged_parent_snapshot": expected_parent,
        "staged_snapshot": staged_id,
        "review_item_id": record["review_item_id"],
    })
    _atomic_json(_session_path(workdir), state)
    _append_history(workdir, {
        "event": "patch-staged",
        "event_id": record["event_id"],
        "review_item_id": record["review_item_id"],
        "parent_snapshot": expected_parent,
        "staged_snapshot": staged_id,
        "origin": record.get("origin"),
    })
    return record


def _publish_current_locked(
    workdir: Path,
    *,
    expected_parent_snapshot: str,
    origin: str,
    changed_paragraph_ids: list[str],
    batch_id: str | None = None,
    restored_from: str | None = None,
) -> dict[str, Any]:
    state = ensure_session(workdir)
    current = state["current_snapshot"]
    if current["id"] != expected_parent_snapshot:
        raise CollaborationError(
            "current-parent-mismatch",
            f"expected {expected_parent_snapshot}, current is {current['id']}",
        )
    actual_hash = _sha256_file(Path(workdir).resolve() / "typed.md")
    if actual_hash == current["typed_sha256"]:
        raise CollaborationError("current-not-changed", "canonical typed.md has not changed")
    new_number = int(current["id"][1:]) + 1
    new_current = {
        "id": _snapshot_id("C", new_number),
        "typed_sha256": actual_hash,
        "parent_snapshot": current["id"],
        "origin": origin,
        "changed_paragraph_ids": sorted(set(changed_paragraph_ids)),
        "batch_id": batch_id,
        "published_at": _now(),
    }
    if restored_from is not None:
        # a restore is a normal publish whose content came from a version
        new_current["restored_from"] = restored_from
    state["current_snapshot"] = new_current
    state["staged_snapshot"] = _empty_staged(new_current)
    state["writer"] = {"state": "idle", "batch_id": None}
    _atomic_json(_session_path(workdir), state)
    _append_history(workdir, {
        "event": "current-published",
        "previous_snapshot": current["id"],
        "current_snapshot": new_current,
        "batch_id": batch_id,
    })
    _persist_snapshot(workdir, new_current)
    return {"previous_snapshot": current["id"], "current_snapshot": new_current}

@_writer_transaction
def publish_current(
    workdir: Path,
    *,
    expected_parent_snapshot: str,
    origin: str,
    changed_paragraph_ids: list[str],
    batch_id: str | None = None,
    restored_from: str | None = None,
) -> dict[str, Any]:
    return _publish_current_locked(
        workdir,
        expected_parent_snapshot=expected_parent_snapshot,
        origin=origin,
        changed_paragraph_ids=changed_paragraph_ids,
        batch_id=batch_id,
    )
@_writer_transaction
def settle_decisions(workdir: Path, event_ids: list[str] | None = None) -> dict[str, Any]:
    """Atomically settle native revision decisions and carry deferred items."""
    from .decisions import _find_paragraph_with_revision, _parse_revision_key
    from .edit import (
        _build_evidence,
        _publish_sync,
        _sha256,
        _sync_format_records,
        _write_regions,
        _write_revisions,
        classify_edit_state,
        create_edit_state,
        edit_body_sha256,
        render_edit_projection,
    )
    from .edit_sync import SyncPlan
    from .review_queue import list_events, update_event
    from .typed_core import (
        RevisionNode,
        TypedDocument,
        apply_revision_decision,
        contains_opaque,
        merge_adjacent_text,
        parse_typed,
        serialize_typed,
    )

    state = document_state(workdir)
    if not state["current_matches_filesystem"]:
        raise CollaborationError("current-snapshot-drift", "typed.md differs from the canonical snapshot")
    edit_state = classify_edit_state(workdir)
    if edit_state["state"] != "clean":
        raise CollaborationError("draft-not-clean", "settlement requires a clean canonical workdir")
    wanted = {str(item) for item in event_ids or []}
    events = [
        event
        for event in list_events(workdir)
        if (not wanted or str(event.get("event_id")) in wanted)
        and event.get("status") in {"queued", "acknowledged"}
        and event.get("type") == "decision"
        and event.get("delivery_state") not in {"applied", "acknowledged"}
    ]
    actionable = [
        event
        for event in events
        if event.get("review_decision") in {"accept", "reject", "defer"}
    ]
    if not actionable:
        settled = [
            event for event in list_events(workdir)
            if wanted and str(event.get("event_id")) in wanted and event.get("settled_snapshot")
        ]
        if settled:
            current = document_state(workdir)["current_snapshot"]
            return {
                "schema": "docx2typed-review-settlement-1",
                "state": "already-settled",
                "review_base": document_state(workdir)["review_base"],
                "current_snapshot": current,
                "settled_event_ids": [event["event_id"] for event in settled],
            }
        raise CollaborationError("settlement-empty", "no accept, reject, or defer decisions are ready")

    typed_before_text = (workdir / "typed.md").read_text(encoding="utf-8")
    typed = parse_typed(typed_before_text)
    format_data = json.loads((workdir / "format.json").read_text(encoding="utf-8"))
    targets: list[tuple[dict[str, Any], Any, str, str, str]] = []
    deferred: list[dict[str, Any]] = []
    seen_wids: set[str] = set()
    descendants: dict[str, set[str]] = {}

    def walk(nodes: list[Any], ancestors: tuple[str, ...] = ()) -> None:
        for node in nodes:
            if isinstance(node, RevisionNode):
                w_id = str(node.attrs.get("w:id", ""))
                for ancestor in ancestors:
                    descendants.setdefault(ancestor, set()).add(w_id)
                walk(list(node.children), ancestors + ((w_id,) if w_id else ()))
            else:
                walk(list(getattr(node, "children", []) or []), ancestors)

    for paragraph in typed.paragraphs:
        walk(list(paragraph.nodes))
    for event in actionable:
        action = str(event["review_decision"])
        if action == "defer":
            deferred.append(event)
            continue
        revision_key = str(event.get("revision_key", ""))
        part, kind, w_id, fingerprint = _parse_revision_key(revision_key)
        if part != "word/document.xml":
            raise CollaborationError(
                "revision-outside-editable-surface",
                f"{revision_key} can only be viewed",
            )
        if w_id in seen_wids:
            raise CollaborationError("duplicate-decision", f"revision {w_id} has more than one settlement decision")
        seen_wids.add(w_id)
        paragraph = _find_paragraph_with_revision(typed, w_id)
        if paragraph is None:
            raise CollaborationError("revision-not-found", revision_key)
        if contains_opaque(paragraph.nodes):
            raise CollaborationError(
                "revision-outside-editable-surface",
                f"paragraph {paragraph.paragraph_id} contains unsupported structure",
            )
        targets.append((event, paragraph, kind, w_id, fingerprint))
    target_ids = {w_id for _, _, _, w_id, _ in targets}
    for outer, children in descendants.items():
        if outer in target_ids and children & target_ids:
            raise CollaborationError("nested-decision-conflict", f"revision {outer} has a decided descendant")

    started_at = _now()
    decisions: list[dict[str, Any]] = []
    changed_ids: list[str] = []
    for event, paragraph, kind, w_id, fingerprint in targets:
        decision = apply_revision_decision(
            paragraph,
            w_id=w_id,
            kind=kind,
            fingerprint=fingerprint,
            action=str(event["review_decision"]),
        )
        paragraph.nodes = merge_adjacent_text(paragraph.nodes)
        decisions.append({
            **decision,
            "review_item_id": event.get("review_item_id"),
            "event_id": event.get("event_id"),
        })
        changed_ids.append(paragraph.paragraph_id)

    typed_text = serialize_typed(typed)
    published = {
        "previous_snapshot": state["current_snapshot"],
        "current_snapshot": state["current_snapshot"],
    }
    if typed_text != typed_before_text:
        typed_hash = _sha256(typed_text.encode("utf-8"))
        projection_text = render_edit_projection(typed, base_typed_sha256=typed_hash)
        body_hash = edit_body_sha256(projection_text)
        new_edit_state = create_edit_state(typed_hash, body_hash)
        plan = SyncPlan(TypedDocument(dict(typed.meta)))
        plan.document.paragraphs = typed.paragraphs
        plan.changed_ids = sorted(set(changed_ids))
        format_text = _sync_format_records(workdir, format_data, plan)
        evidence = _build_evidence(
            command="docx2typed review settle",
            status="ok",
            started_at=started_at,
            state_before=edit_state["state"],
            typed_before=edit_state["typed_sha256"],
            typed_after=typed_hash,
            base_projection=edit_state["base_projection_sha256"],
            projection_before=edit_state["edit_body_sha256"],
            projection_after=body_hash,
            discarded=None,
            diagnostics=None,
            changed_ids=sorted(set(changed_ids)),
            decisions=decisions,
        )
        _publish_sync(workdir, typed_text, projection_text, new_edit_state, format_text, evidence)
        _write_regions(workdir, typed)
        _write_revisions(workdir, typed)
        published = _publish_current_locked(
            workdir,
            expected_parent_snapshot=state["current_snapshot"]["id"],
            origin="settlement",
            changed_paragraph_ids=sorted(set(changed_ids)),
            batch_id=f"settlement-{uuid.uuid4().hex}",
        )
    state = ensure_session(workdir)

    previous_base = state["review_base"]
    next_base = {
        "id": f"S{int(previous_base['id'][1:]) + 1}",
        "typed_sha256": state["current_snapshot"]["typed_sha256"],
        "parent_snapshot": previous_base["id"],
        "origin": "settlement",
        "created_at": _now(),
    }
    state["review_base"] = next_base
    _atomic_json(_session_path(workdir), state)
    snapshot_id = state["current_snapshot"]["id"]
    for event in actionable:
        update_event(
            workdir,
            str(event["event_id"]),
            {
                "delivery_state": "applied",
                "settled_snapshot": snapshot_id,
                "carry_forward": event in deferred,
            },
        )
    _append_history(workdir, {
        "event": "settlement-completed",
        "previous_review_base": previous_base,
        "review_base": next_base,
        "current_snapshot": state["current_snapshot"],
        "decisions": decisions,
        "deferred_event_ids": [event["event_id"] for event in deferred],
    })
    return {
        "schema": "docx2typed-review-settlement-1",
        "review_base": next_base,
        "current_snapshot": state["current_snapshot"],
        "decisions": decisions,
        "deferred": deferred,
        "carry_forward": deferred,
        "settled_event_ids": [event["event_id"] for event in actionable],
    }
