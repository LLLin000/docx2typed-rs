"""Capability task map + capability_matrix qualification check (issue #53).

Covers the governance surface added by issue #53:
- the committed task map validates against the frozen manifest (coverage,
  traceability, state counts, guard boundaries, stable-negative diagnostics);
- seeded drift mutations are rejected (missing capability, unknown case id,
  wrong trace, count mismatch, unregistered diagnostic, orphan task);
- the capability_matrix check executes the inline matrix cases and fails on
  a mismatched stable-negative diagnostic or a mutated workdir;
- the metamorphic declarations in release_acceptance match the task map;
- the frozen plan carries the capability_map identity and the
  capability-matrix check, and validate_plan accepts them.
"""
from __future__ import annotations

import json
from copy import deepcopy
from pathlib import Path

import pytest

from scripts.capability_map import (
    TaskMapError,
    agent_task_ids,
    load_task_map,
    metamorphic_cases,
    release_task_ids,
    validate_task_map,
)
from scripts.protocol import schema_bundle
from scripts.qualify import PlanError, validate_identities, validate_plan

ROOT = Path(__file__).resolve().parents[1]
PLAN_PATH = ROOT / "qualification" / "plan.json"
BUNDLE = schema_bundle()


def _task_map() -> dict:
    return json.loads((ROOT / "capabilities" / "task_map.json").read_text(encoding="utf-8"))


def _manifest() -> dict:
    return json.loads((ROOT / "capabilities" / "manifest.json").read_text(encoding="utf-8"))


def _validate(**overrides) -> dict:
    return validate_task_map(ROOT, bundle=BUNDLE, **overrides)


# ---------------------------------------------------------------------------
# Committed state validates
# ---------------------------------------------------------------------------


def test_committed_task_map_validates_against_manifest():
    verdict = _validate()
    assert verdict["valid"], verdict["detail"]
    counts = verdict["counts"]
    assert counts["capability_count"] == 41
    assert counts["task_count"] == 33
    assert counts["agent_count"] == 6
    assert counts["metamorphic_count"] == 11
    assert counts["supported"] == 26
    assert counts["supported-with-guard"] == 5
    assert counts["unsupported-by-design"] == 10
    assert counts["unknown"] == 0
    # task count is never assumed equal to capability count
    assert counts["task_count"] != counts["capability_count"]


def test_committed_task_map_traces_every_release_and_agent_task():
    task_map = _task_map()
    mapped = task_map["capabilities"]
    referenced = {cid for entry in mapped.values() for cid in entry["cases"]["tasks"]}
    referenced.update(cid for entry in mapped.values() for cid in entry["cases"]["agents"])
    assert set(release_task_ids(ROOT)) <= referenced
    assert set(agent_task_ids(ROOT)) <= referenced
    # every agent task declares its capability at the source
    agent_data = json.loads((ROOT / "capabilities" / "tasks" / "agent.json").read_text(encoding="utf-8"))
    assert all(isinstance(t.get("capability"), str) for t in agent_data["tasks"])


def test_committed_metamorphic_declarations_match_runner():
    task_map = _task_map()
    runner = dict(metamorphic_cases())
    mapped_meta = {
        cid: task_map["cases"][cid]["capability"]
        for entry in task_map["capabilities"].values()
        for cid in entry["cases"]["metamorphic"]
    }
    assert set(mapped_meta) == set(runner)
    assert mapped_meta == runner
    # m2's label is a real capability (the old structure.paragraph.edit label
    # was not in the manifest and broke traceability)
    assert "structure.paragraph.edit" not in {entry["id"] for entry in _manifest()["capabilities"]}
    assert mapped_meta["m2-revert-restores"] == "guard.freshness"


def test_failure_catalog_codes_are_registered_and_covered():
    task_map = _task_map()
    catalog = task_map["failure_catalog"]
    registered = set(BUNDLE["diagnostics"])
    assert catalog
    for code, entry in catalog.items():
        assert code in registered, code
        assert entry["cases"], code
    # every matrix case diagnostic is registered (zero unknown)
    for case_id, spec in task_map["matrix_cases"].items():
        diagnostic = spec.get("diagnostic")
        assert diagnostic is None or diagnostic in registered, (case_id, diagnostic)


# ---------------------------------------------------------------------------
# Seeded drift mutations fail validation
# ---------------------------------------------------------------------------


def test_manifest_capability_removed_fails_validation():
    manifest = _manifest()
    manifest["capabilities"] = [c for c in manifest["capabilities"] if c["id"] != "text.edit.body"]
    verdict = _validate(manifest=manifest)
    assert not verdict["valid"]
    assert any("not in the manifest" in p for p in verdict["detail"].split("; "))


def test_orphan_task_id_fails_validation():
    task_map = _task_map()
    task_map["capabilities"]["text.edit.body"]["cases"]["tasks"].append("t99-no-such-task")
    verdict = _validate(task_map=task_map)
    assert not verdict["valid"]
    assert any("t99-no-such-task" in p for p in verdict["detail"].split("; "))


def test_case_tracing_to_wrong_capability_fails_validation():
    task_map = _task_map()
    task_map["cases"]["t04-edit-cjk-word"]["capability"] = "fidelity.noop"
    verdict = _validate(task_map=task_map)
    assert not verdict["valid"]
    assert any("traces to" in p for p in verdict["detail"].split("; "))


def test_summary_count_drift_fails_validation():
    task_map = _task_map()
    task_map["summary"]["counts"]["supported"] = 99
    verdict = _validate(task_map=task_map)
    assert not verdict["valid"]
    assert any("summary counts" in p for p in verdict["detail"].split("; "))


def test_unregistered_stable_negative_diagnostic_fails_validation():
    task_map = _task_map()
    task_map["matrix_cases"]["cm-sn-style-edit"]["diagnostic"] = "not-a-real-code"
    verdict = _validate(task_map=task_map)
    assert not verdict["valid"]
    assert any("not registered" in p for p in verdict["detail"].split("; "))


def test_stable_negative_without_side_effect_proof_fails_validation():
    task_map = _task_map()
    spec = task_map["matrix_cases"]["cm-sn-style-edit"]
    spec.pop("no_mutation", None)
    spec.pop("no_output", None)
    verdict = _validate(task_map=task_map)
    assert not verdict["valid"]
    assert any("side-effect proof" in p for p in verdict["detail"].split("; "))


def test_guard_boundary_missing_fails_validation():
    task_map = _task_map()
    task_map["capabilities"]["text.edit.hyperlink"]["cases"]["matrix"] = []
    verdict = _validate(task_map=task_map)
    assert not verdict["valid"]
    assert any("refused-boundary" in p for p in verdict["detail"].split("; "))


def test_unsupported_capability_without_stable_negative_fails_validation():
    task_map = _task_map()
    task_map["capabilities"]["merge.three-way"]["cases"]["matrix"] = []
    verdict = _validate(task_map=task_map)
    assert not verdict["valid"]
    assert any("stable-negative" in p for p in verdict["detail"].split("; "))


def test_task_map_schema_tamper_raises(tmp_path):
    tampered = _task_map()
    tampered["schema"] = "docx2typed-capability-task-map-9"
    capabilities_dir = tmp_path / "capabilities"
    capabilities_dir.mkdir()
    (capabilities_dir / "task_map.json").write_text(json.dumps(tampered), encoding="utf-8")
    with pytest.raises(TaskMapError, match="task map schema"):
        from scripts.capability_map import load_task_map

        load_task_map(tmp_path)


# ---------------------------------------------------------------------------
# capability_matrix check: execution + failure modes
# ---------------------------------------------------------------------------


def _capability_matrix_plan() -> dict:
    return json.loads(PLAN_PATH.read_text(encoding="utf-8"))


def test_frozen_plan_validates_with_capability_matrix_check():
    plan = _capability_matrix_plan()
    validate_plan(plan)
    check = next(c for c in plan["checks"] if c["id"] == "capability-matrix")
    assert check["kind"] == "capability_matrix"
    assert "capability_map" in plan["identities"]
    assert plan["identities"]["capability_map"]["schema"] == "docx2typed-capability-task-map-1"


def test_capability_map_identity_pin_is_frozen():
    plan = _capability_matrix_plan()
    identity = plan["identities"]["capability_map"]
    assert len(identity["sha256"]) == 64
    content = (ROOT / identity["path"]).read_bytes()
    from scripts.protocol import semantic_sha256

    assert semantic_sha256(json.loads(content.decode("utf-8"))) == identity["sha256"]
    identities = validate_identities(plan, ROOT)
    assert identities["capability_map"]["valid"] is True, identities["capability_map"]["detail"]


def test_capability_map_pin_drift_fails_identity_validation():
    plan = _capability_matrix_plan()
    plan["identities"]["capability_map"]["sha256"] = "0" * 64
    identities = validate_identities(plan, ROOT)
    assert identities["capability_map"]["valid"] is False


def test_matrix_case_execution_passes_positive_and_refused(tmp_path, monkeypatch):
    """The capability_matrix check executes inline matrix cases through the
    capture-only adapters; a fast synthetic case (a CLI extract + an MCP
    workdir_open refusal) proves positive and refused paths both run."""
    from scripts.qualify import _execute_capability_matrix

    plan = _capability_matrix_plan()
    task_map = _task_map()
    synthetic = {
        "cm-positive-fast": {
            "source": "corpus/release/plain.docx",
            "probe": "positive",
            "setup": [
                {"id": "extract", "adapter": "cli", "command": ["python", "-m", "scripts", "--json", "extract", "{{source}}", "-o", "{{workdir}}", "--operation-id", "cm-test-extract"], "expect": {"rc": 0}}
            ],
            "ops": [
                {"id": "validate", "adapter": "cli", "command": ["python", "-m", "scripts", "--json", "validate", "{{workdir}}"], "expect": {"rc": 0}}
            ],
        },
        "cm-refused-fast": {
            "probe": "stable-negative",
            "diagnostic": "invalid-arguments",
            "no_mutation": True,
            "setup": [],
            "ops": [
                {"id": "probe", "adapter": "cli", "command": ["python", "-m", "scripts", "--json", "no-such-command"], "expect": {"rc": 2}}
            ],
        },
    }
    task_map["matrix_cases"].update(synthetic)
    task_map["capabilities"]["text.edit.body"]["cases"]["matrix"].extend(["cm-positive-fast", "cm-refused-fast"])
    task_map["cases"]["cm-positive-fast"] = {"capability": "text.edit.body", "kind": "matrix", "label": "fast positive"}
    task_map["cases"]["cm-refused-fast"] = {"capability": "text.edit.body", "kind": "matrix", "label": "fast refused"}
    task_map["summary"]["matrix_count"] = len(task_map["matrix_cases"])
    record = _execute_capability_matrix(
        plan, ROOT, tmp_path / "scratch", BUNDLE, task_map=task_map,
        only={"cm-positive-fast", "cm-refused-fast"},
    )
    assert record["result"] == "pass", record["detail"]
    by_id = {case["id"]: case for case in record["matrix_cases"]}
    assert by_id["cm-positive-fast"]["result"] == "pass"
    assert by_id["cm-refused-fast"]["result"] == "pass"


def test_matrix_case_diagnostic_mismatch_fails_the_check(tmp_path):
    from scripts.qualify import _execute_capability_matrix

    plan = _capability_matrix_plan()
    task_map = _task_map()
    task_map["matrix_cases"]["cm-invalid-arguments"]["diagnostic"] = "workdir-not-found"
    record = _execute_capability_matrix(
        plan, ROOT, tmp_path / "scratch", BUNDLE, task_map=task_map,
        only={"cm-invalid-arguments"},
    )
    assert record["result"] == "fail"
    assert "diagnostic" in record["detail"]


def test_matrix_case_workdir_mutation_fails_the_check(tmp_path):
    from scripts.qualify import _execute_capability_matrix

    plan = _capability_matrix_plan()
    task_map = _task_map()
    # A "refused" probe that actually mutates the workdir must fail.
    task_map["matrix_cases"]["cm-mutates"] = {
        "probe": "positive",
        "no_mutation": True,
        "setup": [],
        "ops": [
            {"id": "write", "op": "write", "path": "{{workdir}}/evidence.txt", "text": "x"}
        ],
    }
    task_map["capabilities"]["text.edit.body"]["cases"]["matrix"].append("cm-mutates")
    task_map["cases"]["cm-mutates"] = {"capability": "text.edit.body", "kind": "matrix", "label": "mutates"}
    task_map["summary"]["matrix_count"] = len(task_map["matrix_cases"])
    record = _execute_capability_matrix(
        plan, ROOT, tmp_path / "scratch", BUNDLE, task_map=task_map, only={"cm-mutates"}
    )
    assert record["result"] == "fail"
    assert "mutated" in record["detail"]


def test_matrix_case_structural_drift_fails_the_check(tmp_path):
    from scripts.qualify import _execute_capability_matrix

    plan = _capability_matrix_plan()
    task_map = _task_map()
    del task_map["capabilities"]["text.edit.body"]
    record = _execute_capability_matrix(
        plan, ROOT, tmp_path / "scratch", BUNDLE, task_map=task_map, only=set()
    )
    assert record["result"] == "fail"
    assert "task map drift" in record["detail"]


# ---------------------------------------------------------------------------
# Issue #53 gate findings: run:false never counts as pass; snapshot tracks
# the directory tree; the previously declared-not-run cases execute for real
# ---------------------------------------------------------------------------


def test_committed_matrix_has_zero_declared_not_run_cases():
    """Every committed matrix case must genuinely execute: run:false cases
    would fail the gate as not-run instead of counting as pass (issue #53)."""
    task_map = _task_map()
    declared = [cid for cid, spec in task_map["matrix_cases"].items() if spec.get("run") is False]
    assert declared == []
    assert task_map["summary"]["matrix_count"] == len(task_map["matrix_cases"])
    assert task_map["summary"]["matrix_count"] >= 40
    verdict = _validate()
    assert verdict["valid"], verdict["detail"]


def test_matrix_declared_not_run_is_never_counted_as_pass(tmp_path):
    """A run:false case is reported not-run and the gate fails until it
    executes; it is excluded from the executed/passed count."""
    from scripts.qualify import _execute_capability_matrix

    plan = _capability_matrix_plan()
    task_map = _task_map()
    task_map["matrix_cases"]["cm-declared-not-run"] = {
        "run": False,
        "evidence": "tests/test_store_recovery.py::test_writer_busy_and_timeout",
        "probe": "guard-refused",
        "diagnostic": "writer-busy",
    }
    task_map["capabilities"]["lock.writer-lane"]["cases"]["matrix"].append("cm-declared-not-run")
    task_map["cases"]["cm-declared-not-run"] = {
        "capability": "lock.writer-lane", "kind": "matrix", "label": "declared not-run",
    }
    task_map["summary"]["matrix_count"] = len(task_map["matrix_cases"])
    record = _execute_capability_matrix(
        plan, ROOT, tmp_path / "scratch", BUNDLE, task_map=task_map,
        only={"cm-declared-not-run"},
    )
    assert record["result"] == "fail"
    assert "declared not-run" in record["detail"]
    by_id = {case["id"]: case for case in record["matrix_cases"]}
    assert by_id["cm-declared-not-run"]["result"] == "not-run"
    # the not-run case never inflates the executed/passed count
    passed = [case for case in record["matrix_cases"] if case["result"] == "pass"]
    assert by_id["cm-declared-not-run"] not in passed


def test_matrix_snapshot_tracks_directory_tree(tmp_path):
    """The no-mutation snapshot covers the directory tree (empty dirs and
    dir existence), not just file hashes (issue #53)."""
    from scripts.qualify import _matrix_workdir_snapshot

    workdir = tmp_path / "wd"
    workdir.mkdir()
    (workdir / "a.txt").write_text("x", encoding="utf-8")
    (workdir / "empty-dir").mkdir()
    exists, snapshot = _matrix_workdir_snapshot(workdir)
    assert exists is True
    assert snapshot["a.txt"]  # file hash tracked
    assert snapshot["empty-dir/"] == "<dir>"  # empty dir tracked
    # creating a new empty directory is a detected mutation
    (workdir / "new-empty").mkdir()
    _, after = _matrix_workdir_snapshot(workdir)
    assert after != snapshot
    assert "new-empty/" in after
    # removing a directory is a detected mutation
    (workdir / "empty-dir").rmdir()
    _, after2 = _matrix_workdir_snapshot(workdir)
    assert after2 != snapshot
    assert "empty-dir/" not in after2
    # absent workdir marker
    assert _matrix_workdir_snapshot(tmp_path / "nope") == (False, {})


def test_matrix_empty_dir_creation_fails_no_mutation(tmp_path):
    """A refused probe that creates even an empty directory must fail the
    no_mutation side-effect proof."""
    from scripts.qualify import _execute_capability_matrix

    plan = _capability_matrix_plan()
    task_map = _task_map()
    task_map["matrix_cases"]["cm-mkdir"] = {
        "probe": "positive",
        "no_mutation": True,
        "setup": [],
        "ops": [
            {"id": "mkdir", "op": "mkdir", "path": "{{workdir}}/sneaky-empty"}
        ],
    }
    task_map["capabilities"]["text.edit.body"]["cases"]["matrix"].append("cm-mkdir")
    task_map["cases"]["cm-mkdir"] = {"capability": "text.edit.body", "kind": "matrix", "label": "mkdir"}
    task_map["summary"]["matrix_count"] = len(task_map["matrix_cases"])
    record = _execute_capability_matrix(
        plan, ROOT, tmp_path / "scratch", BUNDLE, task_map=task_map, only={"cm-mkdir"}
    )
    assert record["result"] == "fail"
    assert "mutated" in record["detail"]


def test_previously_declared_cases_execute_for_real(tmp_path):
    """cm-writer-busy / cm-fs-unqualified / cm-settle-opaque were run:false
    (proven only by external unit tests); they now execute through the public
    CLI seams and pass with real side-effect proof (issue #53)."""
    from scripts.qualify import _execute_matrix_case

    task_map = _task_map()
    scratch = tmp_path / "scratch"
    for case_id in ("cm-writer-busy", "cm-fs-unqualified", "cm-settle-opaque"):
        spec = task_map["matrix_cases"][case_id]
        assert spec.get("run") is not False
        record = _execute_matrix_case(case_id, spec, ROOT, scratch, BUNDLE)
        assert record["result"] == "pass", (case_id, record["detail"])
