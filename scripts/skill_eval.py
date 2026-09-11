"""Analyze docx2typed Agent traces without running a model.

The Agent supplies a trace while talking to ``agent_bench --serve``. This
module turns that trace into route and efficiency metrics, then compares two
prompt/skill variants. It deliberately stores no document text.
"""
from __future__ import annotations

import argparse
import json
import statistics
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_TASKS = REPO_ROOT / "evals" / "skill" / "tasks.json"

LOCATE_TOOLS = frozenset(
    {
        "document_search",
        "document_read",
        "history_list",
        "get_paragraph",
        "list_paragraphs",
        "view",
    }
)
MUTATION_TOOLS = frozenset(
    {
        "document_patch",
        "document_replace",
        "format_span",
        "history_restore",
        "decide_all",
        "accept_revision",
        "reject_revision",
        "reinsert_deleted_text",
        "delete_comment",
        "delete_paragraph",
        "insert_paragraph",
        "replace_text",
        "batch_edit",
        "table_delete_col",
        "table_delete_row",
        "table_insert_col",
        "table_insert_row",
        "table_merge_cells",
        "table_split_cells",
        "review_apply_batch",
        "review_apply_patch",
        "review_settle",
    }
)
PRIMITIVE_TOOLS = frozenset(
    {
        "get_paragraph",
        "list_paragraphs",
        "batch_edit",
        "replace_text",
        "delete_paragraph",
        "insert_paragraph",
    }
)
COMMIT_TOOLS = frozenset({"commit_sync"})
BUILD_TOOLS = frozenset({"build_docx"})
VERIFY_TOOLS = frozenset({"verify_output"})


def _tool_name(event: dict[str, Any]) -> str:
    raw = str(event.get("tool", ""))
    for prefix in ("mcp__docx_typed_", "docx_typed_"):
        if raw.startswith(prefix):
            raw = raw[len(prefix) :]
    return raw.rsplit(".", 1)[-1]


def _events(trace: dict[str, Any]) -> list[dict[str, Any]]:
    value = trace.get("events", [])
    if not isinstance(value, list):
        return []
    return [event for event in value if isinstance(event, dict)]


def _count_key(value: Any, key: str) -> int:
    if isinstance(value, dict):
        return (1 if key in value else 0) + sum(_count_key(v, key) for v in value.values())
    if isinstance(value, list):
        return sum(_count_key(v, key) for v in value)
    return 0


def _event_match_refs(event: dict[str, Any]) -> int:
    explicit = event.get("match_ref_count")
    if isinstance(explicit, int):
        return explicit
    args = event.get("args", event.get("arguments", {}))
    return _count_key(args, "match_ref")


def _event_ok(event: dict[str, Any]) -> bool:
    value = event.get("ok", True)
    return value is not False


def _load_tasks(path: Path) -> dict[str, dict[str, Any]]:
    payload = json.loads(path.read_text(encoding="utf-8"))
    return {task["id"]: task for task in payload.get("tasks", [])}


def _override(trace: dict[str, Any], name: str, computed: Any) -> Any:
    explicit = trace.get("metrics")
    if isinstance(explicit, dict) and name in explicit:
        return explicit[name]
    if name in trace:
        return trace[name]
    return computed


def _route_violations(task: dict[str, Any] | None, events: list[dict[str, Any]], metrics: dict[str, Any]) -> list[str]:
    if task is None:
        return []
    expect = task.get("expect", {})
    tools = [_tool_name(event) for event in events]
    violations: list[str] = []
    max_locate = expect.get("max_locate_calls")
    if max_locate is not None and metrics["locate_calls"] > max_locate:
        violations.append(f"locate_calls>{max_locate}")
    max_mutation = expect.get("max_mutation_calls")
    if max_mutation is not None and metrics["mutation_calls"] > max_mutation:
        violations.append(f"mutation_calls>{max_mutation}")
    max_primitive = expect.get("max_primitive_calls")
    if max_primitive is not None and metrics["fallback_primitive_calls"] > max_primitive:
        violations.append(f"fallback_primitive_calls>{max_primitive}")
    required = expect.get("required_mutation")
    if required and required not in tools:
        violations.append(f"missing_required_tool:{required}")
    for forbidden in expect.get("forbidden_tools", []):
        if forbidden in tools:
            violations.append(f"forbidden_tool:{forbidden}")
    return violations


def analyze_trace(trace: dict[str, Any], task_manifest: Path | None = DEFAULT_TASKS) -> dict[str, Any]:
    """Return route and efficiency metrics for one trace object."""
    events = _events(trace)
    tools = [_tool_name(event) for event in events]
    locate = sum(tool in LOCATE_TOOLS for tool in tools)
    mutations = sum(tool in MUTATION_TOOLS for tool in tools)
    primitive = sum(tool in PRIMITIVE_TOOLS for tool in tools)
    commit = sum(tool in COMMIT_TOOLS for tool in tools)
    build = sum(tool in BUILD_TOOLS for tool in tools)
    verify_events = [event for event, tool in zip(events, tools) if tool in VERIFY_TOOLS]
    first_mutation = next((event for event, tool in zip(events, tools) if tool in MUTATION_TOOLS), None)
    duration_sum = sum(max(0, int(event.get("duration_ms", event.get("duration", 0)) or 0)) for event in events)
    read_events = [event for event, tool in zip(events, tools) if tool == "document_read"]
    reread = sum(bool(event.get("re_read", False)) for event in events)
    if reread == 0:
        reread = max(0, len(read_events) - 1)
    recovery = sum(
        bool(event.get("recovery", False)) or event.get("kind") in {"refusal", "recovery"}
        for event in events
    )
    clarifications = sum(
        bool(event.get("user_clarification", False)) or event.get("kind") == "user_clarification"
        for event in events
    )
    raw_xml = sum(
        bool(event.get("raw_xml_escape", False)) or bool(event.get("raw_xml", False))
        for event in events
    )
    computed: dict[str, Any] = {
        "skill_triggered": bool(trace.get("skill_triggered", trace.get("variant", "unknown") != "unknown")),
        "agent_wall_time_ms": int(trace.get("agent_wall_time_ms", trace.get("duration_ms", duration_sum)) or 0),
        "tool_calls": len(events),
        "locate_calls": locate,
        "mutation_calls": mutations,
        "commit_count": commit,
        "build_count": build,
        "verify_result": "not-run" if not verify_events else ("pass" if all(_event_ok(e) for e in verify_events) else "fail"),
        "wrong_tool_calls": sum(bool(event.get("wrong_tool", False)) for event in events),
        "recovery_calls": recovery,
        "re_reads": reread,
        "first_mutation_success": None if first_mutation is None else _event_ok(first_mutation),
        "match_ref_usage": sum(_event_match_refs(event) for event in events),
        "document_replace_usage": tools.count("document_replace"),
        "diff_preview_usage": tools.count("diff_preview"),
        "user_clarifications": clarifications,
        "fallback_primitive_calls": primitive,
        "raw_xml_escape": raw_xml,
    }
    metrics = {name: _override(trace, name, value) for name, value in computed.items()}
    task_value = trace.get("task_id")
    task_id = task_value if isinstance(task_value, str) else None
    tasks = _load_tasks(task_manifest) if task_manifest is not None and task_manifest.exists() else {}
    task = tasks.get(task_id) if task_id is not None else None
    violations = _route_violations(task, events, metrics)
    metrics["route_violation_count"] = len(violations)
    return {
        "schema": "docx2typed-skill-metrics-1",
        "variant": trace.get("variant", "unknown"),
        "task_id": task_id,
        "metrics": metrics,
        "route": {"pass": not violations, "violations": violations},
    }


def _load_runs(path: Path) -> list[dict[str, Any]]:
    payload = json.loads(path.read_text(encoding="utf-8"))
    if isinstance(payload, list):
        return [item for item in payload if isinstance(item, dict)]
    if isinstance(payload, dict) and isinstance(payload.get("runs"), list):
        return [item for item in payload["runs"] if isinstance(item, dict)]
    if not isinstance(payload, dict):
        raise ValueError("trace JSON must be an object, list, or runs collection")
    return [payload]


def _aggregate(reports: list[dict[str, Any]]) -> dict[str, Any]:
    numeric: dict[str, list[float]] = {}
    success_values: list[float] = []
    for report in reports:
        for name, value in report["metrics"].items():
            if isinstance(value, bool) or not isinstance(value, (int, float)):
                continue
            numeric.setdefault(name, []).append(float(value))
        success = report["metrics"].get("first_mutation_success")
        if isinstance(success, bool):
            success_values.append(float(success))
    means = {name: statistics.mean(values) for name, values in numeric.items()}
    medians = {name: statistics.median(values) for name, values in numeric.items()}
    return {
        "runs": len(reports),
        "mean": means,
        "median": medians,
        "first_mutation_success_rate": statistics.mean(success_values) if success_values else None,
        "route_pass_rate": statistics.mean(
            float(report["route"]["pass"]) for report in reports
        ) if reports else None,
    }


def analyze_file(path: Path, task_manifest: Path | None = DEFAULT_TASKS) -> dict[str, Any]:
    """Analyze one trace or a JSON collection of traces."""
    reports = [analyze_trace(trace, task_manifest) for trace in _load_runs(path)]
    if len(reports) == 1:
        return reports[0]
    return {
        "schema": "docx2typed-skill-metrics-collection-1",
        "variant": reports[0]["variant"] if reports else "unknown",
        "runs": reports,
        "aggregate": _aggregate(reports),
    }


def _aggregate_view(report: dict[str, Any]) -> dict[str, Any]:
    if isinstance(report.get("aggregate"), dict):
        return report["aggregate"]
    return _aggregate([report])


def compare_traces(a: Path, b: Path, task_manifest: Path | None = DEFAULT_TASKS) -> dict[str, Any]:
    """Compare B against A; positive deltas mean B used more."""
    report_a = analyze_file(a, task_manifest)
    report_b = analyze_file(b, task_manifest)
    agg_a = _aggregate_view(report_a)
    agg_b = _aggregate_view(report_b)
    delta: dict[str, dict[str, float]] = {"mean": {}, "median": {}}
    percent: dict[str, dict[str, float | None]] = {"mean": {}, "median": {}}
    for bucket in ("mean", "median"):
        keys = set(agg_a.get(bucket, {})) | set(agg_b.get(bucket, {}))
        for key in sorted(keys):
            left = agg_a.get(bucket, {}).get(key, 0)
            right = agg_b.get(bucket, {}).get(key, 0)
            if not isinstance(left, (int, float)) or isinstance(left, bool):
                continue
            if not isinstance(right, (int, float)) or isinstance(right, bool):
                continue
            delta[bucket][key] = right - left
            percent[bucket][key] = None if left == 0 else (right - left) / left * 100
    return {
        "schema": "docx2typed-skill-compare-1",
        "a": {"variant": report_a.get("variant", "unknown"), "aggregate": agg_a},
        "b": {"variant": report_b.get("variant", "unknown"), "aggregate": agg_b},
        "delta_b_minus_a": delta,
        "percent_change_b_vs_a": percent,
    }


def _emit(payload: dict[str, Any], out: Path | None) -> None:
    text = json.dumps(payload, ensure_ascii=False, indent=2) + "\n"
    if out is None:
        print(text, end="")
    else:
        out.write_text(text, encoding="utf-8")
        print(f"report written: {out}")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--metrics", metavar="TRACE_JSON")
    group.add_argument("--compare", nargs=2, metavar=("TRACE_A", "TRACE_B"))
    parser.add_argument("--eval-tasks", default=str(DEFAULT_TASKS))
    parser.add_argument("--out", type=Path)
    args = parser.parse_args(argv)
    manifest = Path(args.eval_tasks) if args.eval_tasks else None
    if args.metrics:
        _emit(analyze_file(Path(args.metrics), manifest), args.out)
    else:
        _emit(compare_traces(Path(args.compare[0]), Path(args.compare[1]), manifest), args.out)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
