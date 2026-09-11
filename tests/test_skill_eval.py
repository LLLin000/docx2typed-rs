"""Behavioral checks for the compact-skill Agent trace evaluator."""
from __future__ import annotations

import json
from pathlib import Path
from typing import Any

from scripts.skill_eval import analyze_trace, compare_traces


def _trace(*events: dict[str, Any], task_id: str = "edit-exact", duration_ms: int = 100, variant: str = "B-router") -> dict[str, Any]:
    return {
        "schema": "docx2typed-agent-trace-1",
        "variant": variant,
        "task_id": task_id,
        "duration_ms": duration_ms,
        "events": list(events),
    }


def test_exact_patch_metrics_capture_one_locate_and_match_ref_route() -> None:
    report = analyze_trace(
        _trace(
            {"kind": "tool_call", "tool": "document_search", "ok": True, "duration_ms": 12},
            {
                "kind": "tool_call",
                "tool": "document_patch",
                "ok": True,
                "duration_ms": 8,
                "args": {"hunks": [{"match_ref": "m1", "replacement": "25 mg"}]},
            },
            {"kind": "tool_call", "tool": "commit_sync", "ok": True, "duration_ms": 4},
            {"kind": "tool_call", "tool": "verify_output", "ok": True, "duration_ms": 20},
        )
    )
    metrics = report["metrics"]
    assert report["route"] == {"pass": True, "violations": []}
    assert metrics["tool_calls"] == 4
    assert metrics["locate_calls"] == 1
    assert metrics["mutation_calls"] == 1
    assert metrics["commit_count"] == 1
    assert metrics["match_ref_usage"] == 1
    assert metrics["first_mutation_success"] is True
    assert metrics["verify_result"] == "pass"


def test_global_replace_does_not_require_a_locate_call() -> None:
    report = analyze_trace(
        _trace(
            {"kind": "tool_call", "tool": "document_replace", "ok": True, "duration_ms": 11},
            {"kind": "tool_call", "tool": "commit_sync", "ok": True, "duration_ms": 3},
            task_id="replace-all",
        )
    )
    assert report["route"]["pass"] is True
    assert report["metrics"]["locate_calls"] == 0
    assert report["metrics"]["document_replace_usage"] == 1
    assert report["metrics"]["mutation_calls"] == 1


def test_compare_reports_delta_and_route_regression(tmp_path: Path) -> None:
    baseline = tmp_path / "a.json"
    router = tmp_path / "b.json"
    baseline.write_text(
        json.dumps(
            _trace(
                {"kind": "tool_call", "tool": "document_read", "ok": True, "duration_ms": 2},
                {"kind": "tool_call", "tool": "get_paragraph", "ok": True, "duration_ms": 2},
                {"kind": "tool_call", "tool": "batch_edit", "ok": True, "duration_ms": 5},
                variant="A-baseline",
                duration_ms=40,
            )
        ),
        encoding="utf-8",
    )
    router.write_text(
        json.dumps(
            _trace(
                {"kind": "tool_call", "tool": "document_search", "ok": True, "duration_ms": 2},
                {
                    "kind": "tool_call",
                    "tool": "document_patch",
                    "ok": True,
                    "duration_ms": 3,
                    "args": {"hunks": [{"match_ref": "m1"}]},
                },
                variant="B-router",
                duration_ms=25,
            )
        ),
        encoding="utf-8",
    )
    comparison = compare_traces(baseline, router)
    assert comparison["a"]["variant"] == "A-baseline"
    assert comparison["b"]["variant"] == "B-router"
    assert comparison["delta_b_minus_a"]["median"]["tool_calls"] == -1
    assert comparison["delta_b_minus_a"]["median"]["agent_wall_time_ms"] == -15
    assert comparison["b"]["aggregate"]["route_pass_rate"] == 1.0
