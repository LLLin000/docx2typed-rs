"""Contracts for the agent-facing skill router and evaluation manifests."""
from __future__ import annotations

import json
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def test_skill_is_compact_and_all_progressive_references_resolve() -> None:
    skill = (ROOT / "SKILL.md").read_text(encoding="utf-8")
    assert len(skill.splitlines()) <= 200
    for phrase in (
        "document_search",
        "document_patch",
        "document_replace",
        "format_span",
        "history_restore",
        "build_docx(version=...)",
        "structured recovery data once",
    ):
        assert phrase in skill
    links = re.findall(r"\]\((references/[^)]+\.md)\)", skill)
    assert links
    assert all((ROOT / link).is_file() for link in links)


def test_skill_benchmark_manifest_matches_task_manifest() -> None:
    tasks = json.loads((ROOT / "evals" / "skill" / "tasks.json").read_text(encoding="utf-8"))
    benchmark = json.loads((ROOT / "evals" / "skill" / "benchmark.json").read_text(encoding="utf-8"))
    assert tasks["schema"] == "docx2typed-skill-eval-1"
    assert benchmark["schema"] == "docx2typed-skill-benchmark-1"
    assert benchmark["task_manifest"] == "evals/skill/tasks.json"
    assert {variant["id"] for variant in benchmark["variants"]} == {"A-baseline", "B-router"}
    assert len(tasks["tasks"]) == 8
    assert "route_violation_count" in benchmark["primary_metrics"]
