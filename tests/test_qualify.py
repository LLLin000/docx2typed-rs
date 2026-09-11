"""Public tests for the implementation-independent qualification runner.

Covers issue #48's acceptance criteria through seeded mutations at the public
seams: the frozen plan, identity validation, canonicalization, comparisons,
and the generated verdict/report artifacts.  Failure cases (missing,
duplicate, unknown, not-run, schema drift) must never be misreported as
pass, and the Python engine's canonical verdicts must be self-comparison
deterministic.
"""
from __future__ import annotations

import json
import sys
from copy import deepcopy
from pathlib import Path

import pytest

from scripts.qualify import (
    CANON_SCHEMA,
    PLAN_SCHEMA,
    CanonError,
    PlanError,
    ReportError,
    canonical_verdict,
    canon_result,
    freeze_plan,
    plan_sha256,
    run,
    section_sha256,
    validate_identities,
    validate_plan,
    validate_report,
)
from scripts.qualify_adapters import capture_cli, capture_docx_parts, file_sha256
from scripts.protocol import schema_bundle

ROOT = Path(__file__).resolve().parents[1]
PLAN_PATH = ROOT / "qualification" / "plan.json"


def _load_plan() -> dict:
    return json.loads(PLAN_PATH.read_text(encoding="utf-8"))


def _mutated_plan(checks: list[dict], **identity_patch) -> dict:
    plan = _load_plan()
    plan["checks"] = checks
    for name in ("agent_journey", "failure_recovery", "interop"):
        plan["identities"][name]["checks"] = []
    for name, patch in identity_patch.items():
        plan["identities"][name].update(patch)
    return plan


EXTRACT_OP = {
    "id": "extract",
    "adapter": "cli",
    "command": ["python", "-m", "scripts", "extract", "{{source}}", "-o", "{{workdir}}"],
    "expect": {"rc": 0},
}


def _noop_check() -> dict:
    return {
        "id": "noop-bytes",
        "kind": "noop_bytes",
        "binds": ["fixture", "contract", "canonicalization"],
        "source": "corpus/release/plain.docx",
        "ops": [
            EXTRACT_OP,
            {
                "id": "build",
                "adapter": "cli",
                "command": ["python", "-m", "scripts", "build", "{{workdir}}", "-o", "{{output}}"],
                "expect": {"rc": 0},
            },
        ],
        "compare": {"kind": "noop_bytes"},
    }


# ---------------------------------------------------------------------------
# Frozen plan
# ---------------------------------------------------------------------------


def test_committed_plan_is_frozen_and_deterministic():
    plan = _load_plan()
    assert plan["schema"] == PLAN_SCHEMA
    assert plan["canon"] == CANON_SCHEMA
    validate_plan(plan)
    assert plan_sha256(plan) == plan_sha256(deepcopy(plan))
    assert set(plan["identities"]) == {
        "capability",
        "capability_map",
        "agent_journey",
        "failure_recovery",
        "interop",
        "fixture",
        "contract",
        "canonicalization",
        "fixture_corpus",
        "resource_profiles",
        "office_evidence",
    }


# ---------------------------------------------------------------------------
# Seeded mutations: plan validation failures
# ---------------------------------------------------------------------------


def test_missing_identity_section_fails_plan_validation():
    plan = _load_plan()
    del plan["identities"]["fixture"]
    with pytest.raises(PlanError, match="missing identity"):
        validate_plan(plan)


def test_duplicate_check_ids_fail_plan_validation():
    plan = _load_plan()
    plan["checks"].append(deepcopy(plan["checks"][0]))
    with pytest.raises(PlanError, match="duplicate check ids"):
        validate_plan(plan)


def test_unknown_adapter_fails_plan_validation():
    plan = _load_plan()
    plan["checks"][1]["ops"][0]["adapter"] = "telepathy"
    with pytest.raises(PlanError, match="unknown adapter"):
        validate_plan(plan)


def test_unknown_check_kind_fails_plan_validation():
    plan = _load_plan()
    plan["checks"][0]["kind"] = "teleport"
    with pytest.raises(PlanError, match="unknown kind"):
        validate_plan(plan)


def test_unknown_compare_kind_fails_plan_validation():
    plan = _load_plan()
    plan["checks"][1]["compare"] = {"kind": "time_machine"}
    with pytest.raises(PlanError, match="unknown compare kind"):
        validate_plan(plan)


def test_unknown_identity_binding_fails_plan_validation():
    plan = _load_plan()
    plan["checks"][0]["binds"] = ["crystal-ball"]
    with pytest.raises(PlanError, match="unknown identity binding"):
        validate_plan(plan)


# ---------------------------------------------------------------------------
# Seeded mutations: identity drift
# ---------------------------------------------------------------------------


def test_capability_pin_drift_fails_identity_validation():
    plan = _mutated_plan(deepcopy(_load_plan()["checks"]), capability={"sha256": "0" * 64})
    identities = validate_identities(plan, ROOT)
    assert identities["capability"]["valid"] is False
    assert identities["fixture"]["valid"] is True


def test_fixture_pin_drift_fails_identity_validation():
    plan = _mutated_plan(
        deepcopy(_load_plan()["checks"]),
        fixture={"fixtures": {"plain.docx": "0" * 64}},
    )
    identities = validate_identities(plan, ROOT)
    assert identities["fixture"]["valid"] is False


def test_missing_fixture_file_fails_identity_validation():
    plan = _mutated_plan(
        deepcopy(_load_plan()["checks"]),
        fixture={"fixtures": {"no-such.docx": "0" * 64}},
    )
    identities = validate_identities(plan, ROOT)
    assert identities["fixture"]["valid"] is False


def test_contract_range_drift_fails_identity_validation():
    plan = _mutated_plan(
        deepcopy(_load_plan()["checks"]),
        contract={"ranges": {"cli": {"major": 2, "min_minor": 0, "max_minor": 0}}},
    )
    identities = validate_identities(plan, ROOT)
    assert identities["contract"]["valid"] is False


def test_schema_bundle_pin_drift_fails_identity_validation():
    plan = _mutated_plan(
        deepcopy(_load_plan()["checks"]),
        contract={"schema_bundle_sha256": "0" * 64},
    )
    identities = validate_identities(plan, ROOT)
    assert identities["contract"]["valid"] is False


# ---------------------------------------------------------------------------
# Schema drift
# ---------------------------------------------------------------------------


def test_unknown_schema_envelope_is_schema_drift():
    bundle = schema_bundle()
    drifted = {
        "schema": "docx2typed-result-99",
        "operation": "validate",
        "outcome": "success",
        "data": {},
        "diagnostics": [],
        "evidence": [],
        "engine": {},
    }
    with pytest.raises(CanonError, match="unknown schema"):
        canon_result(drifted, bundle)


def test_missing_required_fields_are_schema_drift():
    bundle = schema_bundle()
    drifted = {"schema": "docx2typed-result-1", "operation": "validate", "outcome": "success"}
    with pytest.raises(CanonError, match="missing required"):
        canon_result(drifted, bundle)


def test_drifted_cli_envelope_fails_the_check_and_the_verdict(tmp_path):
    drifted_op = {
        "id": "drifted-envelope",
        "adapter": "cli",
        "command": [
            "python",
            "-c",
            "import json; print(json.dumps({'schema': 'docx2typed-result-99', 'operation': 'validate', 'outcome': 'success', 'data': {}, 'diagnostics': [], 'evidence': [], 'engine': {}}))",
        ],
        "expect": {"rc": 0, "schema": "docx2typed-result-1"},
    }
    journey = {
        "id": "schema-drift",
        "kind": "journey",
        "binds": ["contract", "canonicalization"],
        "source": "corpus/release/plain.docx",
        "ops": [drifted_op],
    }
    plan = _mutated_plan([journey, deepcopy(_load_plan()["checks"][-1])])
    report = run(plan, root=ROOT, scratch=tmp_path / "scratch", report_dir=tmp_path / "report")
    drift_check = next(check for check in report["checks"] if check["id"] == "schema-drift")
    assert drift_check["result"] == "fail"
    assert "schema drift" in drift_check["detail"]
    assert report["verdict"]["result"] == "fail"
    assert validate_report(plan, report)["verdict"]["result"] == "fail"


# ---------------------------------------------------------------------------
# Not-run cannot be misreported as pass
# ---------------------------------------------------------------------------


def _not_run_plan() -> dict:
    check = _noop_check()
    check["ops"].insert(
        0, {"id": "boom", "op": "append", "path": "{{workdir}}", "text": "x"}
    )
    return _mutated_plan([check, deepcopy(_load_plan()["checks"][-1])])


def test_crashed_check_is_recorded_not_run_and_fails_the_verdict(tmp_path):
    plan = _not_run_plan()
    report = run(plan, root=ROOT, scratch=tmp_path / "scratch", report_dir=tmp_path / "report")
    check = next(check for check in report["checks"] if check["id"] == "noop-bytes")
    assert check["result"] == "not-run"
    assert report["verdict"]["result"] == "fail"
    assert report["verdict"]["reason"] != ""
    assert (tmp_path / "report" / "verdict.json").is_file()
    assert (tmp_path / "report" / "failures" / "noop-bytes.txt").is_file()
    validate_report(plan, report)


def test_report_missing_a_plan_check_is_rejected():
    plan = _load_plan()
    report = {
        "schema": "docx2typed-qualification-report-1",
        "plan_sha256": plan_sha256(plan),
        "checks": [{"id": "noop-bytes", "kind": "noop_bytes", "result": "pass", "detail": "", "bindings": {}}],
        "verdict": {"result": "pass", "reason": ""},
    }
    with pytest.raises(ReportError, match="not run"):
        validate_report(plan, report)


def _synthetic_report(plan: dict, results: dict[str, str]) -> dict:
    """Internally consistent report with the plan's exact check ids and
    bindings; used to probe validate_report's coverage gates cheaply."""
    return {
        "schema": "docx2typed-qualification-report-1",
        "plan_sha256": plan_sha256(plan),
        "checks": [
            {
                "id": check["id"],
                "kind": check["kind"],
                "result": results.get(check["id"], "pass"),
                "detail": "",
                "bindings": {name: section_sha256(plan, name) for name in check.get("binds", [])},
            }
            for check in plan["checks"]
        ],
        "verdict": {
            "result": "pass" if all(results.get(c["id"], "pass") in ("pass", "skip") for c in plan["checks"]) else "fail",
            "reason": "",
        },
    }


def test_not_run_check_flipped_to_pass_verdict_is_rejected():
    plan = _not_run_plan()
    report = _synthetic_report(plan, {"noop-bytes": "not-run", "self-comparison": "pass"})
    report["verdict"]["result"] = "pass"  # tamper: non-pass check, pass verdict
    with pytest.raises(ReportError, match="misreported as pass"):
        validate_report(plan, report)


def test_not_run_result_with_fail_verdict_is_accepted():
    plan = _not_run_plan()
    report = _synthetic_report(plan, {"noop-bytes": "not-run", "self-comparison": "pass"})
    assert validate_report(plan, report)["verdict"]["result"] == "fail"


def test_binding_drift_in_report_is_rejected():
    plan = _load_plan()
    report = _synthetic_report(plan, {})
    report["checks"][0]["bindings"] = {"canonicalization": "0" * 64}
    with pytest.raises(ReportError, match="binding drift"):
        validate_report(plan, report)


# ---------------------------------------------------------------------------
# Self-comparison determinism
# ---------------------------------------------------------------------------


def test_python_self_comparison_success(tmp_path, monkeypatch):
    """The runner's self-comparison: every non-self check executes at most
    twice (in independent scratch dirs) and the canonical verdicts must
    match.  A deterministic injected capture adapter keeps this test free of
    external processes; the committed plan itself is exercised by the
    end-to-end run (and by the CLI default)."""
    from scripts.qualify_adapters import Capture

    def fake_capture_cli(command, cwd=None, timeout=60):
        # Deterministic: extract is a no-op, build emits a copy of the corpus
        # fixture so the byte-identity compare sees identical parts.
        if "build" in command:
            out = Path(command[command.index("-o") + 1])
            out.write_bytes((ROOT / "corpus/release/plain.docx").read_bytes())
        return Capture(rc=0, stdout=b"", stderr=b"", duration_ms=1)

    monkeypatch.setattr("scripts.qualify.capture_cli", fake_capture_cli)
    plan = _mutated_plan([_noop_check(), deepcopy(_load_plan()["checks"][-1])])
    report = run(plan, root=ROOT, scratch=tmp_path / "scratch", report_dir=tmp_path / "report")
    self_check = next(check for check in report["checks"] if check["id"] == "self-comparison")
    assert self_check["result"] == "pass"
    assert report["verdict"]["result"] == "pass"
    noop = next(check for check in report["checks"] if check["id"] == "noop-bytes")
    assert noop["result"] == "pass"
    # the canonical verdict projection is a pure function of the checks
    verdict = canonical_verdict(report["plan_sha256"], report["checks"])
    assert verdict == canonical_verdict(report["plan_sha256"], deepcopy(report["checks"]))
    validate_report(plan, report)


# ---------------------------------------------------------------------------
# End-to-end: the committed plan qualifies the Python engine
# ---------------------------------------------------------------------------


@pytest.mark.slow
def test_committed_plan_end_to_end_qualifies_and_publishes_artifacts(tmp_path):
    """The committed plan runs end to end and publishes artifacts.  The
    office-evidence check fails closed on this runner because blocking cells
    (Word macOS, Linux Fresh/Still, Word LTSC 2024, human repair
    observation) are recorded not-run — missing evidence never silently
    shrinks the matrix (issue #42/#52)."""
    plan = _load_plan()
    scratch = tmp_path / "scratch"
    report_dir = tmp_path / "report"
    # One execution of every non-self check; the declared self-comparison
    # check is reported as an explicit skip (never a silent pass) so the full
    # plan stays cheap to run in the suite.
    report = run(plan, root=ROOT, scratch=scratch, report_dir=report_dir, self_compare=False)
    validate_report(plan, report)
    for check in report["checks"]:
        assert check["result"] in ("pass", "fail", "skip", "not-run")
    # Deterministic structural checks pass on every host.
    for check_id in ("identity", "noop-bytes", "journey", "touched-semantics", "failure-recovery",
                     "fixture-corpus", "resource-profiles", "self-comparison"):
        check = next(c for c in report["checks"] if c["id"] == check_id)
        assert check["result"] in ("pass", "skip"), (check_id, check["result"], check["detail"])
    # The office-evidence gate fails closed while blocking cells are not-run
    # (the committed evidence records the not-run cells honestly).
    office = next(c for c in report["checks"] if c["id"] == "office-evidence")
    assert office["result"] == "fail"
    assert "gate failed closed" in office["detail"] or "BLOCKING" in office["detail"]
    assert report["verdict"]["result"] == "fail"
    self_check = next(check for check in report["checks"] if check["id"] == "self-comparison")
    assert self_check["result"] == "skip"
    # canonical repeat: the canonical verdict projection is deterministic
    verdict = canonical_verdict(report["plan_sha256"], report["checks"])
    assert verdict == canonical_verdict(report["plan_sha256"], deepcopy(report["checks"]))
    assert (report_dir / "report.json").is_file()
    assert (report_dir / "verdict.json").is_file()
    verdict_doc = json.loads((report_dir / "verdict.json").read_text(encoding="utf-8"))
    assert verdict_doc["result"] == "fail"
    assert verdict_doc["plan_sha256"] == plan_sha256(plan)
    assert set(verdict_doc["bindings"]) == {
        "capability",
        "capability_map",
        "agent_journey",
        "failure_recovery",
        "interop",
        "fixture",
        "contract",
        "canonicalization",
        "fixture_corpus",
        "resource_profiles",
        "office_evidence",
    }


# ---------------------------------------------------------------------------
# Adapters are raw captures (and runner policy fails on seeded mutations)
# ---------------------------------------------------------------------------


def test_cli_adapter_captures_raw_bytes_and_rc():
    capture = capture_cli([sys.executable, "-c", "import sys; print('raw')"])
    assert capture.rc == 0
    assert capture.stdout.strip() == b"raw"
    assert capture.stderr == b""
    failed = capture_cli([sys.executable, "-c", "import sys; sys.stderr.write('boom'); sys.exit(3)"])
    assert failed.rc == 3
    assert failed.stdout == b""
    assert b"boom" in failed.stderr


def test_docx_adapter_captures_per_part_hashes_only():
    plain = ROOT / "corpus/release/plain.docx"
    parts = capture_docx_parts(plain)
    assert parts is not None and len(parts) > 3
    assert all(len(digest) == 64 for digest in parts.values())
    assert capture_docx_parts(ROOT / "no-such.docx") is None
    assert len(file_sha256(plain)) == 64


# ---------------------------------------------------------------------------
# Freeze regenerates matching pins on the branch baseline
# ---------------------------------------------------------------------------


def test_freeze_regenerates_matching_pins(tmp_path):
    original = _load_plan()
    frozen_path = tmp_path / "plan.json"
    frozen_path.write_text(json.dumps(original, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    frozen = freeze_plan(frozen_path, root=ROOT)
    validate_plan(frozen)
    assert frozen["identities"]["capability"]["sha256"] == original["identities"]["capability"]["sha256"]
    assert frozen["identities"]["fixture"] == original["identities"]["fixture"]
    assert frozen["identities"]["contract"] == original["identities"]["contract"]


# ---------------------------------------------------------------------------
# Cross-run canonicalization (issue #54): paths, generation ids, volatile
# digests must not leak into canonical verdicts
# ---------------------------------------------------------------------------


def test_scrub_ctx_paths_normalizes_children_and_embedded_paths():
    from scripts.qualify import _scrub_ctx_paths

    prefix = r"C:\T\rel\v1\capability-matrix\capability-matrix\cm-x\wd"
    ctx = {
        "source": r"C:\repo\corpus\release\plain.docx",
        "workdir": prefix,
        "output": prefix + r"\out.docx",
        "outdir": prefix + r"\pdf",
        "pdf": prefix + r"\pdf\out.pdf",
    }
    assert _scrub_ctx_paths(ctx["output"], ctx) == "<output>"
    assert _scrub_ctx_paths(prefix + r"\out", ctx) == r"<workdir>\out"
    assert _scrub_ctx_paths(prefix + r"\no-such", ctx) == r"<workdir>\no-such"
    gen = prefix + r"\.docx2typed-store\generations\e69b82bbbf0845efbeb357e86201e911\edit.state.json"
    assert _scrub_ctx_paths(gen, ctx) == r"<workdir>\.docx2typed-store\generations\<gen>\edit.state.json"
    message = "decided output already exists: " + ctx["output"]
    assert _scrub_ctx_paths(message, ctx) == "decided output already exists: <output>"
    message = "another writer holds the workdir lane: " + prefix + r"\.docx2typed-store\lock"
    assert _scrub_ctx_paths(message, ctx) == r"another writer holds the workdir lane: <workdir>\.docx2typed-store\lock"
    # a value outside the bound paths is untouched (generation ids still normalized)
    assert _scrub_ctx_paths(r"C:\elsewhere\generations\deadbeefdeadbeefdeadbeefdeadbeef\x", ctx) == (
        r"C:\elsewhere\generations\<gen>\x"
    )
    assert _scrub_ctx_paths(r"C:\plain\value", ctx) == r"C:\plain\value"


def test_canon_result_drops_volatile_digests():
    from scripts.qualify import canon_result

    envelope = {
        "schema": "docx2typed-result-1",
        "operation": "verify",
        "outcome": "success",
        "data": {"current_snapshot": {"typed_sha256": "a" * 64}},
        "diagnostics": [],
        "evidence": [
            {
                "schema": "docx2typed-evidence-1",
                "payload": {
                    "inputs": {"workdir": {"manifest_sha256": "b" * 64}},
                    "outputs": {"docx": {"bytes": 100, "sha256": "c" * 64}},
                },
                "payload_sha256": "d" * 64,
            }
        ],
        "engine": {},
    }
    canon = canon_result(envelope, schema_bundle())
    assert "typed_sha256" not in json.dumps(canon)
    assert "manifest_sha256" not in json.dumps(canon)
    assert "payload_sha256" not in json.dumps(canon)
    assert '"sha256"' not in json.dumps(canon)
    # stable facts survive
    assert canon["outcome"] == "success"
    assert canon["evidence"][0]["payload"]["outputs"]["docx"]["bytes"] == 100
