"""Implementation-independent qualification runner (issue #48).

A frozen qualification plan (qualification/plan.json, schema
``docx2typed-qualification-plan-1``) declares seven identities — capability,
agent_journey, failure_recovery, interop, fixture, contract,
canonicalization — plus data-driven checks.  This runner executes the checks
through capture-only adapters (scripts/qualify_adapters.py), canonicalizes
every result/diagnostic/evidence and side effect, compares no-op bytes and
touched semantic signatures/resources, and emits provenance-bound
verdict/report artifacts.  Adapters never compare; all pass/fail policy lives
here.

Canonicalization identity: ``docx2typed-qual-canon-1``:

- result envelopes: the declared schema is validated against the shipped
  schema bundle (unknown or malformed schemas are schema drift); the engine
  block is stripped and bound separately through the contract identity;
  remaining fields are deep-sorted.
- diagnostics: ordered by (code, canonical payload).
- evidence: an ordered list of canonical entries.
- side effects: a .docx becomes a per-part SHA-256 map plus a
  visible-text/structure signature; file existence is recorded per op.

A future Rust runner reinterprets the same plan and must reproduce the same
canonical verdicts; the self-comparison check pins that determinism for the
Python engine here.

Usage:
    python -m scripts.qualify --report reports/qualify     # execute the plan
    python -m scripts.qualify --freeze                     # re-pin the plan
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

try:
    from .capability_map import TASK_MAP_SCHEMA, load_task_map, validate_task_map
    from .protocol import engine_descriptor, schema_bundle, semantic_sha256
    from .qualify_adapters import (
        McpSession,
        capture_cli,
        capture_docx_parts,
        capture_zip_members,
        file_sha256,
        soffice_path,
    )
except ImportError:  # direct script execution has no package context.
    from capability_map import TASK_MAP_SCHEMA, load_task_map, validate_task_map  # type: ignore[no-redef]
    from protocol import engine_descriptor, schema_bundle, semantic_sha256
    from qualify_adapters import (
        McpSession,
        capture_cli,
        capture_docx_parts,
        capture_zip_members,
        file_sha256,
        soffice_path,
    )

REPO_ROOT = Path(__file__).resolve().parent.parent

PLAN_SCHEMA = "docx2typed-qualification-plan-1"
REPORT_SCHEMA = "docx2typed-qualification-report-1"
VERDICT_SCHEMA = "docx2typed-qualification-verdict-1"
CANON_SCHEMA = "docx2typed-qual-canon-1"
FREEZE_SCHEMA = "docx2typed-oracle-freeze-1"
DEFAULT_PLAN = REPO_ROOT / "qualification" / "plan.json"

IDENTITIES = (
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
)
KNOWN_ADAPTERS = ("cli", "mcp")
KNOWN_LOCAL_OPS = ("append", "write", "copy", "replace", "corrupt_generation")
KNOWN_CHECK_KINDS = (
    "identity",
    "noop_bytes",
    "touched_semantics",
    "journey",
    "failure_recovery",
    "interop",
    "capability_matrix",
    "self_comparison",
    "fixture_corpus",
    "resource_profiles",
    "office_evidence",
    "oracle_freeze",
)
KNOWN_COMPARE_KINDS = ("noop_bytes", "touched_semantics")
CLI_EXPECT_KEYS = ("rc", "rc_ne", "schema", "outcome", "stdout_contains", "side_effect_present", "side_effect_absent")
MCP_EXPECT_KEYS = ("transport", "is_error", "schema", "outcome", "diagnostic_contains")
RESULT_OUTCOMES = ("pass", "fail", "skip", "not-run", "error")

_TAG_OPEN = re.compile(rb"<w:ins[ >]|<w:del[ >]")
_P_OPEN = re.compile(rb"<w:p[ >]")
_TR_OPEN = re.compile(rb"<w:tr[ >]")
_TC_OPEN = re.compile(rb"<w:tc[ >]")
_TEXT = re.compile(rb"<w:t(?:\s[^>]*)?>(.*?)</w:t>", re.S)


class PlanError(ValueError):
    """The qualification plan is structurally invalid."""


class ReportError(ValueError):
    """A generated report does not satisfy the plan's coverage contract."""


class CanonError(ValueError):
    """A captured record drifted from its declared schema."""


# --------------------------------------------------------------------------
# Canonicalization (identity docx2typed-qual-canon-1)
# --------------------------------------------------------------------------


def canonical_json(value: Any) -> bytes:
    return json.dumps(
        value,
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8")


def _sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


_VOLATILE_EXACT = frozenset(
    {"generated", "timestamp", "time", "created", "updated", "issued", "published", "approved_time",
     "run_id", "operation_id"}
)


def strip_volatile(value: Any) -> Any:
    """Recursively drop versioned-volatile fields (timestamps, durations)
    from canonical verdict records.

    Canon identity docx2typed-qual-canon-1: fields named ``*_at`` or the
    exact names above carry wall-clock time and must not participate in
    determinism comparisons; the report keeps the full records.
    """
    if isinstance(value, dict):
        return {
            key: strip_volatile(item)
            for key, item in value.items()
            if key not in _VOLATILE_EXACT and not key.endswith("_at")
        }
    if isinstance(value, list):
        return [strip_volatile(item) for item in value]
    return value


def plan_sha256(plan: dict[str, Any]) -> str:
    """Frozen identity of the whole plan; any edit changes the hash."""
    return semantic_sha256(plan)


def section_sha256(plan: dict[str, Any], name: str) -> str:
    """Frozen identity of one plan identity section."""
    return semantic_sha256(plan["identities"][name])


def _deep_sorted(value: Any) -> Any:
    return json.loads(canonical_json(value))


def validate_envelope(record: dict[str, Any], bundle: dict[str, Any]) -> dict[str, Any]:
    """Validate a declared-schema record against the shipped schema bundle.

    An unknown schema string or a missing required field is schema drift and
    raises CanonError; the caller turns it into a failed op, never a pass.
    """
    schema = record.get("schema")
    if not isinstance(schema, str):
        raise CanonError(f"record declares no schema string: {schema!r}")
    spec = bundle.get("schemas", {}).get(schema)
    if spec is None:
        raise CanonError(f"unknown schema {schema!r} (schema drift)")
    missing = [key for key in spec.get("required", []) if key not in record]
    if missing:
        raise CanonError(f"{schema} missing required fields: {missing}")
    return _deep_sorted({key: value for key, value in record.items() if key != "engine"})


def canon_diagnostics(diagnostics: list[dict[str, Any]]) -> list[dict[str, Any]]:
    canon = [_deep_sorted(item) for item in diagnostics]
    return sorted(canon, key=lambda item: (item.get("code", ""), canonical_json(item)))


def canon_evidence(evidence: list[Any]) -> list[Any]:
    return [_deep_sorted(item) for item in evidence]


# Digest-valued fields whose bytes embed run-specific values (absolute
# paths, generation ids, edit timestamps) cannot participate in cross-run
# determinism (issue #54): two clean runs of a touched edit legitimately
# differ in revision timestamps and store generation ids.  The canonical
# verdict drops them; the full report keeps them (and the bundle archives
# the full reports).
_VOLATILE_DIGEST_KEYS = frozenset({"payload_sha256", "manifest_sha256", "typed_sha256", "sha256"})


def _drop_volatile_digests(value: Any) -> Any:
    if isinstance(value, dict):
        return {
            key: _drop_volatile_digests(item)
            for key, item in value.items()
            if key not in _VOLATILE_DIGEST_KEYS
        }
    if isinstance(value, list):
        return [_drop_volatile_digests(item) for item in value]
    return value


def canon_result(parsed: dict[str, Any], bundle: dict[str, Any]) -> dict[str, Any]:
    """Canonical result envelope: schema-validated, engine stripped, ordered
    diagnostics, canonical evidence, volatile digests dropped (issue #54)."""
    record = validate_envelope(parsed, bundle)
    if "diagnostics" in record:
        record["diagnostics"] = canon_diagnostics(record["diagnostics"])
    if "evidence" in record:
        record["evidence"] = canon_evidence(record["evidence"])
    return _drop_volatile_digests(record)


def _visible_text(xml: bytes) -> str:
    out: list[str] = []
    for match in _TEXT.finditer(xml):
        out.append(match.group(1).decode("utf-8", errors="replace"))
    return "".join(out)


def docx_signature(path: Path) -> dict[str, Any]:
    """Canonical side-effect record for a .docx (per-part hashes plus a
    visible-text/structure signature)."""
    parts = capture_docx_parts(path)
    record: dict[str, Any] = {"schema": "docx2typed-qual-docx-1"}
    if parts is None:
        record["parts"] = None
        record["visible_text"] = None
        record["structure"] = None
        return record
    members = capture_zip_members(path) or {}
    xml = b"".join(members.values())
    record["parts"] = parts
    record["visible_text"] = _visible_text(xml)
    record["structure"] = {
        "paragraphs": len(_P_OPEN.findall(xml)),
        "rows": len(_TR_OPEN.findall(xml)),
        "cells": len(_TC_OPEN.findall(xml)),
        "revision_marks": len(_TAG_OPEN.findall(xml)),
    }
    return record


# --------------------------------------------------------------------------
# Plan model
# --------------------------------------------------------------------------


def _require_sha256(field: str, value: Any) -> None:
    if (
        not isinstance(value, str)
        or len(value) != 64
        or any(char not in "0123456789abcdef" for char in value)
    ):
        raise PlanError(f"invalid {field}: expected a lowercase SHA-256 hex digest")


def validate_plan(plan: dict[str, Any]) -> dict[str, Any]:
    """Structural validation of the frozen plan.  Detects missing identity
    sections, duplicate ids, and unknown kinds/adapters/ops/compares."""
    if not isinstance(plan, dict):
        raise PlanError("plan is not an object")
    if plan.get("schema") != PLAN_SCHEMA:
        raise PlanError(f"plan schema {plan.get('schema')!r} != {PLAN_SCHEMA}")
    if plan.get("canon") != CANON_SCHEMA:
        raise PlanError(f"plan canon {plan.get('canon')!r} != {CANON_SCHEMA}")
    identities = plan.get("identities")
    if not isinstance(identities, dict):
        raise PlanError("plan.identities missing")
    missing = [name for name in IDENTITIES if name not in identities]
    if missing:
        raise PlanError(f"missing identity sections: {missing}")
    unknown_ids = sorted(name for name in identities if name not in IDENTITIES)
    if unknown_ids:
        raise PlanError(f"unknown identity sections: {unknown_ids}")
    checks = plan.get("checks")
    if not isinstance(checks, list) or not checks:
        raise PlanError("plan.checks must be a non-empty list")
    check_ids = [check.get("id") for check in checks]
    duplicates = sorted({check_id for check_id in check_ids if check_ids.count(check_id) > 1})
    if duplicates:
        raise PlanError(f"duplicate check ids: {duplicates}")
    for check in checks:
        _validate_check(check)
    _validate_identity_sections(plan)
    return plan


def _validate_check(check: dict[str, Any]) -> None:
    check_id = check.get("id")
    if not isinstance(check_id, str) or not check_id:
        raise PlanError("every check needs a non-empty id")
    kind = check.get("kind")
    if kind not in KNOWN_CHECK_KINDS:
        raise PlanError(f"check {check_id}: unknown kind {kind!r}")
    binds = check.get("binds", [])
    if not isinstance(binds, list) or any(name not in IDENTITIES for name in binds):
        raise PlanError(f"check {check_id}: unknown identity binding(s) {binds!r}")
    ops = check.get("ops", [])
    for op in ops:
        _validate_op(check_id, op)
    if kind in KNOWN_COMPARE_KINDS:
        compare = check.get("compare")
        if not isinstance(compare, dict) or "kind" not in compare:
            raise PlanError(f"check {check_id}: {kind} needs compare.kind={kind!r}")
        compare_kind = compare["kind"]
        if compare_kind not in KNOWN_COMPARE_KINDS:
            raise PlanError(f"check {check_id}: unknown compare kind {compare_kind!r}")
        if compare_kind != kind:
            raise PlanError(f"check {check_id}: {kind} needs compare.kind={kind!r}")
    elif "compare" in check:
        compare_kind = check["compare"].get("kind")
        if compare_kind not in KNOWN_COMPARE_KINDS:
            raise PlanError(f"check {check_id}: unknown compare kind {compare_kind!r}")
    if kind == "failure_recovery":
        scenarios = check.get("scenarios")
        if not isinstance(scenarios, list) or not scenarios:
            raise PlanError(f"check {check_id}: failure_recovery needs scenarios")
        scenario_ids = [scenario.get("id") for scenario in scenarios]
        scenario_dupes = sorted({sid for sid in scenario_ids if scenario_ids.count(sid) > 1})
        if scenario_dupes:
            raise PlanError(f"check {check_id}: duplicate scenario ids: {scenario_dupes}")
        for scenario in scenarios:
            for op in scenario.get("ops", []):
                _validate_op(f"{check_id}/{scenario.get('id')}", op)
        for op in check.get("setup", []):
            _validate_op(f"{check_id}/setup", op)
    if kind == "interop" and "probe" not in check:
        raise PlanError(f"check {check_id}: interop needs a probe")
    if kind == "interop":
        _validate_op(check_id, check["probe"])


def _validate_op(check_id: str, op: dict[str, Any]) -> None:
    op_id = op.get("id")
    if not isinstance(op_id, str) or not op_id:
        raise PlanError(f"check {check_id}: every op needs a non-empty id")
    if "adapter" in op:
        adapter = op["adapter"]
        if adapter not in KNOWN_ADAPTERS:
            raise PlanError(f"check {check_id}/{op_id}: unknown adapter {adapter!r}")
        expect = op.get("expect", {})
        allowed = CLI_EXPECT_KEYS if adapter == "cli" else MCP_EXPECT_KEYS
        unknown_expect = sorted(set(expect) - set(allowed))
        if unknown_expect:
            raise PlanError(f"check {check_id}/{op_id}: unknown expect key(s) {unknown_expect} for adapter {adapter}")
        if adapter == "cli" and not isinstance(op.get("command"), list):
            raise PlanError(f"check {check_id}/{op_id}: cli op needs a command list")
        if adapter == "mcp" and not isinstance(op.get("tool"), str):
            raise PlanError(f"check {check_id}/{op_id}: mcp op needs a tool name")
    elif "op" in op:
        if op["op"] not in KNOWN_LOCAL_OPS:
            raise PlanError(f"check {check_id}/{op_id}: unknown local op {op['op']!r}")
        if op["op"] == "copy":
            if not isinstance(op.get("from"), str) or not isinstance(op.get("to"), str):
                raise PlanError(f"check {check_id}/{op_id}: copy op needs from and to")
        elif op["op"] == "replace":
            if not isinstance(op.get("path"), str) or not isinstance(op.get("old"), str) or not isinstance(op.get("new"), str):
                raise PlanError(f"check {check_id}/{op_id}: replace op needs path, old and new")
        elif op["op"] == "corrupt_generation":
            if not isinstance(op.get("path"), str):
                raise PlanError(f"check {check_id}/{op_id}: corrupt_generation op needs a workdir path")
        elif not isinstance(op.get("path"), str) or not isinstance(op.get("text"), str):
            raise PlanError(f"check {check_id}/{op_id}: local op needs path and text")
    else:
        raise PlanError(f"check {check_id}/{op_id}: op needs an adapter or a local op")


def _validate_identity_sections(plan: dict[str, Any]) -> None:
    identities = plan["identities"]
    capability = identities["capability"]
    _require_sha256("capability.sha256", capability.get("sha256"))
    if not isinstance(capability.get("bound_capabilities"), list):
        raise PlanError("capability identity needs bound_capabilities")
    fixture = identities["fixture"]
    _require_sha256("fixture.manifest_sha256", fixture.get("manifest_sha256"))
    if not isinstance(fixture.get("fixtures"), dict):
        raise PlanError("fixture identity needs a fixtures map")
    for name, digest in fixture["fixtures"].items():
        _require_sha256(f"fixture.{name}", digest)
    contract = identities["contract"]
    _require_sha256("contract.schema_bundle_sha256", contract.get("schema_bundle_sha256"))
    ranges = contract.get("ranges")
    if not isinstance(ranges, dict) or not ranges:
        raise PlanError("contract identity needs ranges")
    for name, span in ranges.items():
        if not isinstance(span, dict) or set(span) != {"major", "min_minor", "max_minor"}:
            raise PlanError(f"contract ranges[{name}] malformed")
    if identities["canonicalization"].get("schema") != CANON_SCHEMA:
        raise PlanError("canonicalization identity drifted from the runner canon")
    for name in ("agent_journey", "failure_recovery", "interop"):
        referenced = identities[name].get("checks", [])
        check_ids = {check.get("id") for check in plan["checks"]}
        unknown_checks = [ref for ref in referenced if ref not in check_ids]
        if unknown_checks:
            raise PlanError(f"identity {name} references unknown checks: {unknown_checks}")
    for name in ("capability_map", "fixture_corpus", "resource_profiles", "office_evidence"):
        identity = identities[name]
        if not isinstance(identity.get("schema"), str) or not isinstance(identity.get("path"), str):
            raise PlanError(f"identity {name} needs schema and path")
        _require_sha256(f"{name}.sha256", identity.get("sha256"))
    if identities["capability_map"].get("schema") != TASK_MAP_SCHEMA:
        raise PlanError("capability_map identity drifted from the task map schema")


# --------------------------------------------------------------------------
# Identity validation (schema/capability/fixture/contract)
# --------------------------------------------------------------------------


def validate_identities(
    plan: dict[str, Any],
    root: Path,
    bundle: dict[str, Any] | None = None,
    descriptor: dict[str, Any] | None = None,
) -> dict[str, dict[str, Any]]:
    """Compare the plan's frozen pins against the live repository.  Drift in
    any pin is a failed identity, never a pass."""
    bundle = schema_bundle() if bundle is None else bundle
    descriptor = engine_descriptor() if descriptor is None else descriptor
    result: dict[str, dict[str, Any]] = {}
    identities = plan["identities"]

    capability = identities["capability"]
    cap_path = root / capability["path"]
    if not cap_path.is_file():
        result["capability"] = {"valid": False, "detail": f"missing {capability['path']}"}
    else:
        manifest = json.loads(cap_path.read_text(encoding="utf-8"))
        known = {entry.get("id") for entry in manifest.get("capabilities", [])}
        missing_caps = sorted(set(capability["bound_capabilities"]) - known)
        unknown_state = [
            cap for cap in capability["bound_capabilities"]
            if cap in known and manifest.get("unknown") and cap in manifest["unknown"]
        ]
        pin_ok = semantic_sha256(manifest) == capability["sha256"]
        valid = pin_ok and not missing_caps and not unknown_state
        result["capability"] = {
            "valid": valid,
            "detail": (
                "manifest pin matches; bound capabilities present"
                if valid
                else f"pin_ok={pin_ok} missing={missing_caps} unknown={unknown_state}"
            ),
        }

    fixture = identities["fixture"]
    manifest_path = root / fixture["manifest_path"]
    manifest_ok = manifest_path.is_file() and semantic_sha256(
        json.loads(manifest_path.read_text(encoding="utf-8"))
    ) == fixture["manifest_sha256"]
    missing_fixtures: list[str] = []
    drifted_fixtures: list[str] = []
    for name, digest in fixture["fixtures"].items():
        path = root / fixture["dir"] / name
        if not path.is_file():
            missing_fixtures.append(name)
        elif file_sha256(path) != digest:
            drifted_fixtures.append(name)
    valid = manifest_ok and not missing_fixtures and not drifted_fixtures
    result["fixture"] = {
        "valid": valid,
        "detail": (
            "fixture model manifest and pinned fixtures match"
            if valid
            else f"manifest_ok={manifest_ok} missing={missing_fixtures} drifted={drifted_fixtures}"
        ),
    }

    contract = identities["contract"]
    bundle_ok = semantic_sha256(bundle) == contract["schema_bundle_sha256"]
    ranges_ok = descriptor.get("contracts") == contract["ranges"]
    valid = bundle_ok and ranges_ok
    result["contract"] = {
        "valid": valid,
        "detail": (
            "contract ranges and schema bundle pin match the engine descriptor"
            if valid
            else f"bundle_pin_ok={bundle_ok} ranges_ok={ranges_ok}"
        ),
    }

    result["canonicalization"] = {
        "valid": identities["canonicalization"].get("schema") == CANON_SCHEMA,
        "detail": "canonicalization identity matches the runner",
    }

    for name in ("capability_map", "fixture_corpus", "resource_profiles", "office_evidence"):
        identity = identities[name]
        path = root / identity["path"]
        if not path.is_file():
            result[name] = {"valid": False, "detail": f"missing {identity['path']}"}
            continue
        try:
            content = json.loads(path.read_text(encoding="utf-8"))
            pin_ok = semantic_sha256(content) == identity["sha256"]
            schema_ok = content.get("schema") == identity.get("schema")
            result[name] = {
                "valid": pin_ok and schema_ok,
                "detail": (
                    "pin and schema match"
                    if pin_ok and schema_ok
                    else f"pin_ok={pin_ok} schema_ok={schema_ok}"
                ),
            }
        except (OSError, json.JSONDecodeError) as exc:
            result[name] = {"valid": False, "detail": f"unreadable: {exc}"}
    return result


# --------------------------------------------------------------------------
# Execution
# --------------------------------------------------------------------------


def _bind(template: str, ctx: dict[str, str]) -> str:
    for name, value in ctx.items():
        template = template.replace("{{" + name + "}}", value)
    return template


def _bind_value(value: Any, ctx: dict[str, str]) -> Any:
    if isinstance(value, str):
        return _bind(value, ctx)
    if isinstance(value, list):
        return [_bind_value(item, ctx) for item in value]
    if isinstance(value, dict):
        return {key: _bind_value(item, ctx) for key, item in value.items()}
    return value


_CTX_PATH_NAMES = ("source", "workdir", "output", "outdir", "pdf")
_ALT_SEP = "/" if os.sep == "\\" else "\\"
_GENERATION_ID = re.compile(r"(generations[\\/])[0-9a-f]{32}")


def _normalize_generation_ids(value: str) -> str:
    """Store generation directories are named uuid4().hex (run-specific by
    design, scripts/store.py); canonical values must not embed them."""
    return _GENERATION_ID.sub(r"\1<gen>", value)


def _scrub_ctx_paths(value: Any, ctx: dict[str, str]) -> Any:
    """Replace bound scratch/source paths with stable tokens so canonical
    records stay comparable across independent execution scratch dirs (canon
    identity docx2typed-qual-canon-1).  Every occurrence is replaced —
    exact values, children of a bound path (``<workdir>/out``, store
    generations sub-paths) and paths embedded in diagnostic messages — and
    store generation ids are normalized.  The full report keeps the real
    paths."""
    if isinstance(value, str):
        scrubbed = value
        # longest prefix first so ``<output>`` wins over its ``<workdir>``
        # parent when a value matches both.
        for name in sorted(_CTX_PATH_NAMES, key=lambda item: -len(ctx.get(item) or "")):
            prefix = ctx.get(name)
            if not prefix:
                continue
            scrubbed = scrubbed.replace(prefix + os.sep, "<" + name + ">" + os.sep)
            scrubbed = scrubbed.replace(prefix + _ALT_SEP, "<" + name + ">" + _ALT_SEP)
            scrubbed = scrubbed.replace(prefix, "<" + name + ">")
        return _normalize_generation_ids(scrubbed)
    if isinstance(value, list):
        return [_scrub_ctx_paths(item, ctx) for item in value]
    if isinstance(value, dict):
        return {key: _scrub_ctx_paths(item, ctx) for key, item in value.items()}
    return value


def _check_context(check: dict[str, Any], root: Path, scratch: Path, report_dir: Path) -> dict[str, str]:
    base = scratch / check["id"]
    source = (root / check["source"]).resolve() if check.get("source") else root / "corpus"
    outdir = base / "pdf"
    output = base / "out.docx"
    return {
        "source": str(source),
        "workdir": str(base / "wd"),
        "output": str(output),
        "outdir": str(outdir),
        "pdf": str(outdir / (output.stem + ".pdf")),
        "soffice": soffice_path() or "soffice",
        "report": str(report_dir),
    }


def _try_json(data: bytes) -> Any | None:
    try:
        return json.loads(data.decode("utf-8"))
    except Exception:  # noqa: BLE001 - raw text is a valid observation
        return None


def _expect_ok(expect: dict[str, Any], actual: dict[str, Any]) -> tuple[bool, str]:
    """Compare one op's captured facts against the plan's expectations.
    Every expectation is a hard gate: a passed expectation is checked, an
    unknown key fails."""
    failures: list[str] = []
    for key, wanted in expect.items():
        got = actual.get(key)
        if key == "rc":
            if got != wanted:
                failures.append(f"rc={got} want {wanted}")
        elif key == "rc_ne":
            if got == wanted:
                failures.append(f"rc={got} must not be {wanted}")
        elif key == "transport":
            if got != wanted:
                failures.append(f"transport={got} want {wanted}")
        elif key == "is_error":
            if got != wanted:
                failures.append(f"is_error={got} want {wanted}")
        elif key == "schema":
            if got != wanted:
                failures.append(f"schema={got!r} want {wanted!r}")
        elif key == "outcome":
            if got != wanted:
                failures.append(f"outcome={got!r} want {wanted!r}")
        elif key == "stdout_contains":
            if wanted not in actual.get("stdout_text", ""):
                failures.append(f"stdout lacks {wanted!r}")
        elif key == "diagnostic_contains":
            haystack = " ".join(actual.get("diagnostic_codes", []) + [actual.get("diagnostic_messages", "")])
            if wanted not in haystack:
                failures.append(f"diagnostics lack {wanted!r}")
        elif key == "side_effect_present":
            if not actual.get("side_effects", {}).get(wanted, False):
                failures.append(f"side effect missing: {wanted}")
        elif key == "side_effect_absent":
            if actual.get("side_effects", {}).get(wanted, False):
                failures.append(f"side effect present: {wanted}")
        else:
            failures.append(f"unchecked expectation {key}")
    return not failures, "; ".join(failures)


def _run_cli_op(op: dict[str, Any], ctx: dict[str, str], bundle: dict[str, Any]) -> dict[str, Any]:
    command_template = op["command"]
    command_bound = _bind_value(command_template, ctx)
    if command_bound and command_bound[0] == "python":
        command_bound[0] = sys.executable
    if op.get("env"):
        capture = capture_cli(
            command_bound,
            env={name: str(value) for name, value in _bind_value(op["env"], ctx).items()},
        )
    else:
        capture = capture_cli(command_bound)
    stdout_text = capture.stdout.decode("utf-8", errors="replace")
    stderr_text = capture.stderr.decode("utf-8", errors="replace")
    record: dict[str, Any] = {
        "op_id": op["id"],
        "adapter": "cli",
        "command": command_template,
        "command_bound": command_bound,
        "rc": capture.rc,
        "stdout_sha256": _sha256_bytes(capture.stdout),
        "stderr_sha256": _sha256_bytes(capture.stderr),
        "stdout_text": stdout_text,
        "stderr_text": stderr_text,
        "side_effects": {
            "output": Path(ctx["output"]).exists(),
            "pdf": Path(ctx["pdf"]).exists(),
        },
    }
    expect = _bind_value(op.get("expect", {}), ctx)
    parsed = _try_json(capture.stdout)
    if isinstance(parsed, dict) and isinstance(parsed.get("schema"), str):
        try:
            record["envelope"] = canon_result(_scrub_ctx_paths(parsed, ctx), bundle)
            record["schema"] = parsed["schema"]
            record["outcome"] = parsed.get("outcome")
        except CanonError as exc:
            record["schema"] = parsed.get("schema")
            record["canon_error"] = str(exc)
    elif "schema" in expect or "outcome" in expect:
        record["schema"] = parsed.get("schema") if isinstance(parsed, dict) else None
        record["outcome"] = parsed.get("outcome") if isinstance(parsed, dict) else None
    record["expect_passed"], record["detail"] = _expect_ok(expect, record)
    if record.get("canon_error"):
        record["detail"] = f"schema drift: {record['canon_error']}"
        record["expect_passed"] = False
    return record


def _run_mcp_op(session: McpSession, op: dict[str, Any], ctx: dict[str, str], bundle: dict[str, Any]) -> dict[str, Any]:
    args = _bind_value(op.get("args", {}), ctx)
    capture = session.call(op["tool"], **args)
    line = capture.stdout.decode("utf-8", errors="replace").strip()
    record: dict[str, Any] = {
        "op_id": op["id"],
        "adapter": "mcp",
        "tool": op["tool"],
    }
    if line.startswith("OK "):
        record["transport"] = "ok"
        try:
            payload = json.loads(line[3:])
        except json.JSONDecodeError:
            payload = line[3:]
        if isinstance(payload, dict) and "structuredContent" in payload:
            record["is_error"] = bool(payload.get("isError", False))
            envelope = payload.get("structuredContent")
            if isinstance(envelope, dict) and isinstance(envelope.get("schema"), str):
                record["schema"] = envelope["schema"]
                record["outcome"] = envelope.get("outcome")
                diagnostics = envelope.get("diagnostics") or []
                record["diagnostic_codes"] = [d.get("code", "") for d in diagnostics if isinstance(d, dict)]
                record["diagnostic_messages"] = _scrub_ctx_paths(
                    " ".join(str(d.get("message", "")) for d in diagnostics if isinstance(d, dict)),
                    ctx,
                )
                try:
                    record["envelope"] = canon_result(_scrub_ctx_paths(envelope, ctx), bundle)
                except CanonError as exc:
                    record["canon_error"] = str(exc)
            else:
                record["payload_sha256"] = semantic_sha256(
                    strip_volatile(_scrub_ctx_paths(envelope, ctx))
                )
        else:
            record["is_error"] = False
            if isinstance(payload, str):
                inner = _try_json(payload.encode("utf-8"))
                payload = inner if inner is not None else payload
            if isinstance(payload, dict):
                record["schema"] = payload.get("schema")
                record["payload_sha256"] = semantic_sha256(
                    strip_volatile(_scrub_ctx_paths(payload, ctx))
                )
            else:
                record["payload_sha256"] = _sha256_bytes(
                    canonical_json(strip_volatile(_scrub_ctx_paths(line, ctx)))
                )
    else:
        record["transport"] = "err"
        record["is_error"] = True
        record["detail"] = line[4:][:300] if line.startswith("ERR ") else line[:300]
    record["expect_passed"], detail = _expect_ok(_bind_value(op.get("expect", {}), ctx), record)
    record["detail"] = detail or record.get("detail", "")
    if record.get("canon_error"):
        record["detail"] = f"schema drift: {record['canon_error']}"
        record["expect_passed"] = False
    return record


def _run_local_op(op: dict[str, Any], ctx: dict[str, str]) -> dict[str, Any]:
    if op["op"] == "copy":
        source_dir = Path(_bind(op["from"], ctx))
        target_dir = Path(_bind(op["to"], ctx))
        shutil.copytree(source_dir, target_dir, dirs_exist_ok=True)
        return {
            "op_id": op["id"],
            "op": "copy",
            "from": op["from"],
            "to": op["to"],
            "expect_passed": True,
            "detail": "",
        }
    if op["op"] == "replace":
        path = Path(_bind(op["path"], ctx))
        text = path.read_text(encoding="utf-8")
        old_text, new_text = op["old"], op["new"]
        count = text.count(old_text)
        if count != 1:
            raise PlanError(f"{op['id']}: replace anchor {old_text!r} appears {count} times")
        path.write_text(text.replace(old_text, new_text, 1), encoding="utf-8", newline="\n")
        return {
            "op_id": op["id"],
            "op": "replace",
            "path": op["path"],
            "sha256_after": file_sha256(path),
            "expect_passed": True,
            "detail": "",
        }
    if op["op"] == "corrupt_generation":
        # Write an unparseable generation manifest into EVERY generation dir:
        # a pointer selecting any of them must quarantine as needs-recovery.
        store_dir = Path(_bind(op["path"], ctx)) / ".docx2typed-store" / "generations"
        targets = sorted(store_dir.glob("*/generation.json"))
        if not targets:
            raise PlanError(f"{op['id']}: no store generations found under {store_dir}")
        for target in targets:
            target.write_text("{}", encoding="utf-8", newline="\n")
        return {
            "op_id": op["id"],
            "op": "corrupt_generation",
            "path": op["path"],
            "generations": [str(t) for t in targets],
            "expect_passed": True,
            "detail": "",
        }
    if op["op"] == "hold_writer_lane":
        # Spawn a real process that acquires the store's fixed-inode Writer
        # lane and holds it until _execute_matrix_case terminates it.  The
        # probe CLI then hits the genuine OS-advisory lock and must fail with
        # writer-busy.  A bounded hold doubles as a crash safety net: even if
        # the harness dies mid-case the holder self-releases.
        workdir = Path(_bind(op["path"], ctx))
        marker = Path(_bind(op["marker"], ctx))
        seconds = int(op.get("seconds", 60))
        holder = subprocess.Popen(
            [
                sys.executable,
                "-c",
                (
                    "import sys, time, pathlib; sys.path.insert(0, %r);\n"
                    "from scripts.store import Store;\n"
                    "s = Store(%r);\n"
                    "with s.writer():\n"
                    "    pathlib.Path(%r).write_text('ready')\n"
                    "    time.sleep(%d)\n"
                )
                % (str(REPO_ROOT), str(workdir), str(marker), seconds),
            ],
            cwd=REPO_ROOT,
        )
        _WRITER_HOLDERS[holder.pid] = holder
        deadline = time.monotonic() + float(op.get("timeout", 30))
        while not marker.exists():
            if holder.poll() is not None:
                _WRITER_HOLDERS.pop(holder.pid, None)
                raise PlanError(f"{op['id']}: writer-lane holder exited early rc={holder.returncode}")
            if time.monotonic() > deadline:
                holder.kill()
                holder.wait(timeout=10)
                _WRITER_HOLDERS.pop(holder.pid, None)
                raise PlanError(f"{op['id']}: writer-lane holder never became ready")
            time.sleep(0.05)
        return {
            "op_id": op["id"],
            "op": "hold_writer_lane",
            "path": op["path"],
            "marker": op["marker"],
            "holder_pid": holder.pid,
            "expect_passed": True,
            "detail": "writer lane held",
        }
    if op["op"] == "mkdir":
        path = Path(_bind(op["path"], ctx))
        path.mkdir(parents=bool(op.get("parents", False)), exist_ok=True)
        return {
            "op_id": op["id"],
            "op": "mkdir",
            "path": op["path"],
            "expect_passed": True,
            "detail": "",
        }
    path = Path(_bind(op["path"], ctx))
    if op["op"] == "append":
        with path.open("a", encoding="utf-8") as handle:
            handle.write(op["text"])
    else:  # write
        with path.open("w", encoding="utf-8") as handle:
            handle.write(op["text"])
    return {
        "op_id": op["id"],
        "op": op["op"],
        "path": op["path"],
        "sha256_after": file_sha256(path),
        "expect_passed": True,
        "detail": "",
    }


def _run_ops(
    ops: list[dict[str, Any]],
    ctx: dict[str, str],
    bundle: dict[str, Any],
    session: McpSession | None = None,
) -> list[dict[str, Any]]:
    owns_session = session is None
    if session is None:
        session = McpSession()
    records: list[dict[str, Any]] = []
    try:
        for op in ops:
            if "adapter" in op:
                if op["adapter"] == "cli":
                    records.append(_run_cli_op(op, ctx, bundle))
                else:
                    records.append(_run_mcp_op(session, op, ctx, bundle))
            else:
                records.append(_run_local_op(op, ctx))
    finally:
        if owns_session:
            session.close()
    return records


def _all_ops_passed(records: list[dict[str, Any]]) -> tuple[bool, str]:
    failed = [record for record in records if not record["expect_passed"]]
    if not failed:
        return True, ""
    detail = "; ".join(f"{record['op_id']}: {record['detail']}" for record in failed[:3])
    return False, f"op failures: {detail}"


def _compare_noop_bytes(source: Path, output: Path) -> tuple[bool, str]:
    source_sig = docx_signature(source)
    output_sig = docx_signature(output)
    if output_sig["parts"] is None:
        return False, "output docx unreadable"
    changed = [
        name for name in source_sig["parts"]
        if source_sig["parts"][name] != output_sig["parts"].get(name)
    ]
    added = sorted(set(output_sig["parts"]) - set(source_sig["parts"]))
    removed = sorted(set(source_sig["parts"]) - set(output_sig["parts"]))
    if not changed and not added and not removed:
        return True, f"{len(output_sig['parts'])} parts byte-identical to source"
    return False, f"changed={changed[:5]} added={added[:5]} removed={removed[:5]}"


def _compare_touched_semantics(compare: dict[str, Any], source: Path, output: Path) -> tuple[bool, str]:
    source_sig = docx_signature(source)
    output_sig = docx_signature(output)
    if output_sig["visible_text"] is None:
        return False, "output docx unreadable"
    checks: list[tuple[bool, str]] = []
    if "positive_text" in compare:
        present = compare["positive_text"] in output_sig["visible_text"]
        checks.append((present, f"positive {compare['positive_text']!r} present={present}"))
    if "negative_text" in compare:
        absent = compare["negative_text"] not in output_sig["visible_text"]
        checks.append((absent, f"negative {compare['negative_text']!r} absent={absent}"))
    structure_same = source_sig["structure"] == output_sig["structure"]
    checks.append((structure_same, f"structure locked={structure_same}"))
    excepted = set(compare.get("unchanged_parts_except", []))
    source_parts = set(source_sig["parts"]) - excepted
    output_parts = set(output_sig["parts"]) - excepted
    changed = sorted(
        name for name in source_parts & output_parts
        if source_sig["parts"][name] != output_sig["parts"][name]
    )
    added = sorted(output_parts - source_parts)
    removed = sorted(source_parts - output_parts)
    unchanged = not changed and not added and not removed
    checks.append(
        (
            unchanged,
            f"untouched parts unchanged={unchanged} "
            f"(changed={changed[:3]} added={added[:3]} removed={removed[:3]})",
        )
    )
    ok = all(passed for passed, _ in checks)
    detail = "; ".join(text for _, text in checks)
    if not ok:
        detail = "; ".join(text for passed, text in checks if not passed)
    return ok, detail


def _bindings(plan: dict[str, Any], check: dict[str, Any]) -> dict[str, str]:
    return {name: section_sha256(plan, name) for name in check.get("binds", [])}


# --------------------------------------------------------------------------
# capability_matrix check (issue #53)
# --------------------------------------------------------------------------


# Fixed-inode writer-lane / probe lock files are advisory-lock infrastructure,
# not document state: their (re)creation by a refused probe is not a mutation.
_LOCK_EXCLUDES = frozenset(
    {".docx2typed-store/lock", ".docx2typed-store/.probe-lock"}
)

# Writer-lane holders spawned by the hold_writer_lane matrix op, keyed by
# pid.  _execute_matrix_case terminates every holder after the probe so a
# refused probe can never leave the lane wedged for later cases.
_WRITER_HOLDERS: dict[int, subprocess.Popen] = {}


def _matrix_workdir_snapshot(workdir: Path) -> tuple[bool, dict[str, str]]:
    """Recursive snapshot of a workdir (or absent marker): every file's
    SHA-256 plus the directory tree itself (empty dirs and dir existence), so
    a refused probe that creates or removes a directory is caught too. Used
    to prove a refused/stable-negative probe leaves zero side effects."""
    if not workdir.exists():
        return False, {}
    snapshot: dict[str, str] = {}
    for path in sorted(workdir.rglob("*")):
        rel = path.relative_to(workdir).as_posix()
        if rel in _LOCK_EXCLUDES:
            continue
        if path.is_dir():
            snapshot[rel + "/"] = "<dir>"
        elif path.is_file():
            snapshot[rel] = file_sha256(path)
    return True, snapshot


def _matrix_case_ctx(root: Path, scratch: Path, case_id: str, spec: dict[str, Any]) -> dict[str, str]:
    base = scratch / "capability-matrix" / case_id
    outdir = base / "pdf"
    output = base / "out.docx"
    source = (root / spec["source"]).resolve() if spec.get("source") else root / "corpus"
    return {
        "source": str(source),
        "workdir": str(base / "wd"),
        "output": str(output),
        "outdir": str(outdir),
        "pdf": str(outdir / (output.stem + ".pdf")),
        "soffice": soffice_path() or "soffice",
        "report": str(base),
    }


def _matrix_op_diagnostic(records: list[dict[str, Any]]) -> str | None:
    """The exact failure code observed across the probe records: MCP
    diagnostic_codes first, then CLI envelope diagnostics, then the CLI
    stdout/stderr text (human-mode commands print ``kebab-code: detail``)."""
    for record in records:
        codes = record.get("diagnostic_codes")
        if codes:
            return codes[0]
        envelope = record.get("envelope")
        if isinstance(envelope, dict):
            diagnostics = envelope.get("diagnostics") or []
            if diagnostics and isinstance(diagnostics[0], dict):
                return diagnostics[0].get("code")
    for record in records:
        haystack = " ".join(
            [record.get("stdout_text", ""), record.get("stderr_text", "")]
        )
        match = re.search(r"(?m)^\s*(?:ERROR:\s*)?([a-z][a-z0-9-]+):", haystack)
        if match:
            candidate = match.group(1)
            if candidate in schema_bundle()["diagnostics"]:
                return candidate
    return None


def _evaluate_matrix_checks(spec: dict[str, Any], ctx: dict[str, str], root: Path) -> tuple[bool, str]:
    """Evaluate the matrix case's postcondition checks (positive
    side-effect proof) against the produced artifact/workdir."""
    failures: list[str] = []
    for check in spec.get("checks", []):
        kind = check["kind"]
        try:
            if kind == "docx_text_present":
                needle = check["text"].encode("utf-8")
                part = check.get("part")
                members = capture_zip_members(Path(_bind(check["output"], ctx)))
                if check.get("visible"):
                    haystack = "".join(
                        _visible_text(members.get(name, b"")) for name in members
                    ).encode("utf-8")
                else:
                    haystack = members.get(part) if part else b"".join(members.values())
                if needle not in (haystack or b""):
                    failures.append(f"{kind}: {check['text']!r} absent")
            elif kind == "docx_text_absent":
                needle = check["text"].encode("utf-8")
                part = check.get("part")
                members = capture_zip_members(Path(_bind(check["output"], ctx)))
                if check.get("visible"):
                    haystack = "".join(
                        _visible_text(members.get(name, b"")) for name in members
                    ).encode("utf-8")
                else:
                    haystack = members.get(part) if part else b"".join(members.values())
                if needle in (haystack or b""):
                    failures.append(f"{kind}: {check['text']!r} present")
            elif kind == "docx_comment_entries_absent":
                members = capture_zip_members(Path(_bind(check["output"], ctx)))
                comments = members.get("word/comments.xml", b"")
                if b"<w:comment " in comments or b"<w:comment>" in comments:
                    failures.append(f"{kind}: comment entries remain")
            elif kind == "docx_comment_entries_present":
                members = capture_zip_members(Path(_bind(check["output"], ctx)))
                comments = members.get("word/comments.xml", b"")
                if b"<w:comment " not in comments and b"<w:comment>" not in comments:
                    failures.append(f"{kind}: comment entries absent")
            elif kind == "docx_opaque_counts_stable":
                # opaque interiors (fldSimple / oMath) must survive settlement
                # byte-count-identically: settlement only rewrites revision
                # wrapper bytes, never opaque interiors.
                def _opaque_counts(path: Path) -> tuple[int, int]:
                    members = capture_zip_members(path)
                    xml = members.get("word/document.xml", b"")
                    return (
                        len(re.findall(rb"<w:fldSimple[ >]", xml)),
                        len(re.findall(rb"<m:oMath[ >]", xml)),
                    )

                src_counts = _opaque_counts(Path(_bind(check["source"], ctx)))
                out_counts = _opaque_counts(Path(_bind(check["output"], ctx)))
                if src_counts != out_counts:
                    failures.append(f"{kind}: {src_counts} -> {out_counts}")
            elif kind == "file_exists":
                if not Path(_bind(check["path"], ctx)).exists():
                    failures.append(f"{kind}: {check['path']} missing")
            elif kind == "file_absent":
                if Path(_bind(check["path"], ctx)).exists():
                    failures.append(f"{kind}: {check['path']} present")
            elif kind == "dir_exists":
                if not Path(_bind(check["path"], ctx)).is_dir():
                    failures.append(f"{kind}: {check['path']} missing")
            elif kind == "file_size":
                path = Path(_bind(check["path"], ctx))
                actual = path.stat().st_size if path.is_file() else -1
                if actual != check["bytes"]:
                    failures.append(f"{kind}: {actual} != {check['bytes']}")
            elif kind == "store_backed":
                from scripts.store import has_store

                actual = has_store(Path(_bind(check["workdir"], ctx)))
                if actual != check.get("expected", True):
                    failures.append(f"{kind}: backed={actual}")
            elif kind == "store_generations":
                from scripts.store import store_dir_path

                gens = store_dir_path(Path(_bind(check["workdir"], ctx))) / "generations"
                count = len(list(gens.glob("*"))) if gens.is_dir() else 0
                if count < check.get("min", 1):
                    failures.append(f"{kind}: {count} < {check.get('min')}")
            elif kind == "pointer_generation_present":
                pointer_path = Path(_bind(check["workdir"], ctx)) / "workdir.json"
                try:
                    pointer = json.loads(pointer_path.read_text(encoding="utf-8"))
                except (OSError, json.JSONDecodeError):
                    failures.append(f"{kind}: unreadable pointer")
                else:
                    gen_dir = (
                        Path(_bind(check["workdir"], ctx))
                        / ".docx2typed-store" / "generations" / pointer.get("generation", "")
                    )
                    if not gen_dir.is_dir():
                        failures.append(f"{kind}: pointer generation directory missing")
            elif kind == "no_pending_recovery":
                from scripts.store import pending_recovery

                pending = pending_recovery(Path(_bind(check["workdir"], ctx)))
                if pending:
                    failures.append(f"{kind}: pending recovery: {pending[:2]}")
            else:
                failures.append(f"unknown matrix check kind {kind!r}")
        except Exception as exc:  # noqa: BLE001 - a check failure is a case failure
            failures.append(f"{kind}: {type(exc).__name__}: {exc}")
    return not failures, "; ".join(failures)


def _terminate_writer_holders(records: list[dict[str, Any]]) -> None:
    """Kill every writer-lane holder spawned by the given op records so a
    refused probe can never leave the lane wedged for later cases."""
    for record in records:
        pid = record.get("holder_pid") if isinstance(record, dict) else None
        holder = _WRITER_HOLDERS.pop(pid, None) if pid is not None else None
        if holder is not None and holder.poll() is None:
            holder.terminate()
            try:
                holder.wait(timeout=10)
            except subprocess.TimeoutExpired:
                holder.kill()
                holder.wait(timeout=10)


def _execute_matrix_case(
    case_id: str,
    spec: dict[str, Any],
    root: Path,
    scratch: Path,
    bundle: dict[str, Any],
) -> dict[str, Any]:
    """Execute one inline matrix case through the capture-only adapters:
    setup ops, then the probe ops, then the side-effect proof.  A case with
    ``run: false`` is reported as not-run and never counted as pass; the
    capability_matrix gate fails until every declared case actually runs."""
    if spec.get("run") is False:
        return {
            "id": case_id,
            "capability": spec.get("capability"),
            "probe": spec.get("probe"),
            "result": "not-run",
            "detail": "declared not-run; proven by "
            + str(spec.get("evidence", "declared unit test"))
            + "; not executed and not counted as pass",
            "ops": [],
        }
    ctx = _matrix_case_ctx(root, scratch, case_id, spec)
    Path(ctx["workdir"]).mkdir(parents=True, exist_ok=True)
    record: dict[str, Any] = {
        "id": case_id,
        "capability": spec.get("capability"),
        "probe": spec.get("probe"),
        "result": "pass",
        "detail": "",
        "ops": [],
    }
    session = McpSession() if any("adapter" in op for op in spec.get("setup", [])) or any(
        "adapter" in op for op in spec.get("ops", [])
    ) else None
    try:
        setup_records = _run_ops(spec.get("setup", []), ctx, bundle, session=session)
        record["setup_ops"] = setup_records
        setup_ok, setup_detail = _all_ops_passed(setup_records)
        if not setup_ok:
            record["result"] = "fail"
            record["detail"] = f"setup: {setup_detail}"
            return record
        before_exists, before = _matrix_workdir_snapshot(Path(ctx["workdir"]))
        output_path = Path(ctx["output"])
        before_output = (output_path.exists(), file_sha256(output_path) if output_path.is_file() else None)
        probe_records = _run_ops(spec.get("ops", []), ctx, bundle, session=session)
        record["ops"] = probe_records
        probe_ok, probe_detail = _all_ops_passed(probe_records)
        if not probe_ok:
            record["result"] = "fail"
            record["detail"] = probe_detail
            return record
        expected_diagnostic = spec.get("diagnostic")
        if expected_diagnostic is not None:
            actual = _matrix_op_diagnostic(probe_records)
            if actual != expected_diagnostic:
                record["result"] = "fail"
                record["detail"] = f"diagnostic {actual!r} != expected {expected_diagnostic!r}"
                return record
        if spec.get("no_mutation"):
            after_exists, after = _matrix_workdir_snapshot(Path(ctx["workdir"]))
            if before_exists != after_exists or before != after:
                record["result"] = "fail"
                record["detail"] = "workdir mutated by the refused probe"
                return record
        if spec.get("no_output"):
            after_output = (output_path.exists(), file_sha256(output_path) if output_path.is_file() else None)
            if after_output != before_output:
                record["result"] = "fail"
                record["detail"] = "probe created or replaced the output artifact"
                return record
        checks_ok, checks_detail = _evaluate_matrix_checks(spec, ctx, root)
        if not checks_ok:
            record["result"] = "fail"
            record["detail"] = checks_detail
            return record
        record["detail"] = "matrix case passed"
    except Exception as exc:  # noqa: BLE001 - a crashed case is a failure
        record["result"] = "fail"
        record["detail"] = f"{type(exc).__name__}: {exc}".replace(str(scratch), "<scratch>")
    finally:
        if session is not None:
            session.close()
        _terminate_writer_holders(record.get("setup_ops", []))
        _terminate_writer_holders(record.get("ops", []))
    return record


def _execute_capability_matrix(
    plan: dict[str, Any],
    root: Path,
    scratch: Path,
    bundle: dict[str, Any],
    task_map: dict[str, Any] | None = None,
    only: "Iterable[str] | None" = None,
) -> dict[str, Any]:
    """capability_matrix check: validate the committed task map against the
    frozen manifest (coverage, traceability, state counts), then execute the
    inline matrix cases through the public CLI/MCP seam.  A structural drift
    or any failed case fails the check.

    ``only`` narrows which cases EXECUTE (the map is still validated whole):
    a test that seeds one synthetic case does not need to re-run all forty
    real ones — that cost was most of the suite's wall clock."""
    task_map = task_map if task_map is not None else load_task_map(root)
    verdict = validate_task_map(root, bundle=bundle, task_map=task_map)
    matrix_cases = task_map.get("matrix_cases", {})
    if only is not None:
        wanted = set(only)
        matrix_cases = {cid: spec for cid, spec in matrix_cases.items() if cid in wanted}
    case_results = [
        _execute_matrix_case(case_id, spec, root, scratch / "capability-matrix", bundle)
        for case_id, spec in sorted(matrix_cases.items())
    ]
    executed = [case for case in case_results if case["result"] == "pass"]
    failed_cases = [case for case in case_results if case["result"] not in ("pass", "not-run")]
    not_run_cases = [case for case in case_results if case["result"] == "not-run"]
    record: dict[str, Any] = {
        "map_valid": verdict["valid"],
        "map_detail": verdict["detail"],
        "counts": verdict["counts"],
        "matrix_cases": case_results,
        "result": "pass",
        "detail": "",
    }
    if not verdict["valid"]:
        record["result"] = "fail"
        record["detail"] = "task map drift: " + verdict["detail"]
    elif not_run_cases:
        # Declared-not-run cases are never counted as pass: the gate fails
        # until every matrix case genuinely executes (issue #53).
        record["result"] = "fail"
        record["detail"] = (
            f"{len(not_run_cases)} matrix cases declared not-run and not executed: "
            + ", ".join(c["id"] for c in not_run_cases)
        )
    elif failed_cases:
        record["result"] = "fail"
        record["detail"] = "; ".join(f"{c['id']}: {c['detail']}" for c in failed_cases[:3])
    else:
        record["detail"] = (
            f"task map matches the frozen manifest; "
            f"{len(executed)}/{len(case_results)} matrix cases executed and passed"
        )
    return record


def _execute_check(
    check: dict[str, Any],
    plan: dict[str, Any],
    root: Path,
    scratch: Path,
    report_dir: Path,
    bundle: dict[str, Any],
) -> dict[str, Any]:
    kind = check["kind"]
    record: dict[str, Any] = {
        "id": check["id"],
        "kind": kind,
        "bindings": _bindings(plan, check),
        "result": "pass",
        "detail": "",
        "ops": [],
    }
    try:
        if kind == "identity":
            identities = validate_identities(plan, root, bundle=bundle)
            failed = [name for name, entry in identities.items() if not entry["valid"]]
            record["identities"] = {name: entry["valid"] for name, entry in identities.items()}
            if failed:
                record["result"] = "fail"
                record["detail"] = "; ".join(f"{name}: {identities[name]['detail']}" for name in failed)
        elif kind in ("noop_bytes", "touched_semantics"):
            ctx = _check_context(check, root, scratch, report_dir)
            Path(ctx["workdir"]).mkdir(parents=True, exist_ok=True)
            record["ops"] = _run_ops(check["ops"], ctx, bundle)
            passed, op_detail = _all_ops_passed(record["ops"])
            if not passed:
                record["result"] = "fail"
                record["detail"] = op_detail
            else:
                source = Path(ctx["source"])
                output = Path(ctx["output"])
                if kind == "noop_bytes":
                    ok, detail = _compare_noop_bytes(source, output)
                else:
                    ok, detail = _compare_touched_semantics(check["compare"], source, output)
                record["compare"] = {"kind": check["compare"]["kind"], "detail": detail}
                if not ok:
                    record["result"] = "fail"
                    record["detail"] = detail
        elif kind == "journey":
            ctx = _check_context(check, root, scratch, report_dir)
            Path(ctx["workdir"]).mkdir(parents=True, exist_ok=True)
            record["ops"] = _run_ops(check["ops"], ctx, bundle)
            passed, detail = _all_ops_passed(record["ops"])
            if not passed:
                record["result"] = "fail"
                record["detail"] = detail
        elif kind == "failure_recovery":
            scenario_results: list[dict[str, Any]] = []
            setup_workdir: str | None = None
            if check.get("setup"):
                setup_dir = scratch / check["id"] / "setup"
                setup_ctx = _check_context(check, root, scratch, report_dir)
                setup_ctx["workdir"] = str(setup_dir)
                setup_ctx["setup_workdir"] = str(setup_dir)
                Path(setup_dir).mkdir(parents=True, exist_ok=True)
                setup_records = _run_ops(check["setup"], setup_ctx, bundle)
                record["setup_ops"] = setup_records
                setup_ok, setup_detail = _all_ops_passed(setup_records)
                if not setup_ok:
                    record["result"] = "fail"
                    record["detail"] = f"setup: {setup_detail}"
                    return record
                setup_workdir = str(setup_dir)
            for scenario in check["scenarios"]:
                ctx = _check_context(check, root, scratch, report_dir)
                scenario_dir = scratch / check["id"] / scenario["id"]
                ctx["workdir"] = str(scenario_dir / "wd")
                ctx["output"] = str(scenario_dir / "out.docx")
                if setup_workdir is not None:
                    ctx["setup_workdir"] = setup_workdir
                Path(ctx["workdir"]).mkdir(parents=True, exist_ok=True)
                records = _run_ops(scenario["ops"], ctx, bundle)
                passed, detail = _all_ops_passed(records)
                scenario_results.append(
                    {"id": scenario["id"], "result": "pass" if passed else "fail", "detail": detail, "ops": records}
                )
            record["scenarios"] = scenario_results
            failed = [scenario for scenario in scenario_results if scenario["result"] != "pass"]
            if failed:
                record["result"] = "fail"
                record["detail"] = "; ".join(f"{s['id']}: {s['detail']}" for s in failed[:3])
        elif kind == "interop":
            ctx = _check_context(check, root, scratch, report_dir)
            Path(ctx["workdir"]).mkdir(parents=True, exist_ok=True)
            record["ops"] = _run_ops(check["ops"], ctx, bundle)
            passed, detail = _all_ops_passed(record["ops"])
            if not passed:
                record["result"] = "fail"
                record["detail"] = detail
            else:
                soffice = soffice_path()
                if soffice is None:
                    record["result"] = "skip"
                    record["detail"] = "LibreOffice not available; interop probe skipped"
                else:
                    probe_ctx = dict(ctx)
                    probe_ctx["soffice"] = soffice
                    Path(probe_ctx["outdir"]).mkdir(parents=True, exist_ok=True)
                    probe_records = _run_ops([check["probe"]], probe_ctx, bundle)
                    record["probe"] = probe_records[0]
                    probe_passed, probe_detail = _all_ops_passed(probe_records)
                    if not probe_passed:
                        record["result"] = "fail"
                        record["detail"] = probe_detail
                    else:
                        record["detail"] = "soffice conversion completed"
        elif kind == "capability_matrix":
            record.update(_execute_capability_matrix(plan, root, scratch, bundle))
        elif kind == "self_comparison":
            record["detail"] = "handled by the runner around all other checks"
        elif kind == "fixture_corpus":
            # Regenerate the corpus + model manifests via the same
            # release_fixtures entrypoint into a scratch dir and require
            # byte-identity with the committed files; a stale committed
            # corpus or a nondeterministic generator fails with the diff.
            from scripts.release_fixtures import verify_corpus

            regen = verify_corpus(root, scratch / "fixture-corpus" / "work", scratch / "fixture-corpus")
            record["regen_ok"] = regen["ok"]
            record["regen_diffs"] = regen["diffs"]
            if regen["ok"]:
                record["detail"] = "corpus manifest and model manifest byte-identical after regeneration"
            else:
                record["result"] = "fail"
                record["detail"] = regen["detail"]
        elif kind == "resource_profiles":
            from scripts.resource_limits import ProfileError, load_profiles, run_profile_checks, summarize

            try:
                records = run_profile_checks(load_profiles(root / "qualification" / "resource_profiles.json"))
            except ProfileError as exc:
                record["result"] = "fail"
                record["detail"] = f"profiles drift: {exc}"
            else:
                summary = summarize(records)
                record["resource_summary"] = summary
                if summary["failed"]:
                    record["result"] = "fail"
                    record["detail"] = f"{summary['failed']} resource checks failed"
                else:
                    record["detail"] = f"{summary['passed']}/{summary['checks']} resource checks passed"
        elif kind == "office_evidence":
            # Verify the plan's pinned revision, not whatever is latest, and
            # validate the file against the pinned identity: a drifted or
            # substituted evidence file fails before any verdict is scored.
            identity = plan["identities"]["office_evidence"]
            evidence_path = root / identity["path"]
            command = [
                sys.executable, "-m", "scripts.office_evidence", "--verify",
                "--evidence", str(evidence_path),
                "--expect-sha256", identity["sha256"],
            ]
            capture = capture_cli(command, timeout=120)
            stdout_text = capture.stdout.decode("utf-8", errors="replace")
            record["verify_rc"] = capture.rc
            record["verify_output_tail"] = stdout_text[-2000:]
            if capture.rc == 0:
                record["result"] = "pass"
                record["detail"] = stdout_text.strip().splitlines()[-1][:200] if stdout_text.strip() else "office evidence gate passed"
            else:
                record["result"] = "fail"
                record["detail"] = "office evidence gate failed closed: " + (
                    stdout_text.strip().splitlines()[-1][:160] if stdout_text.strip() else f"rc={capture.rc}"
                )
        elif kind == "oracle_freeze":
            record.update(_execute_oracle_freeze_check(plan, root))
        else:  # pragma: no cover - validate_plan prevents this
            record["result"] = "error"
            record["detail"] = f"unhandled kind {kind}"
    except Exception as exc:  # noqa: BLE001 - a crashed check is not-run, never pass
        record["result"] = "not-run"
        message = f"{type(exc).__name__}: {exc}"
        record["detail"] = message.replace(str(scratch), "<scratch>")
    return record


def _strip_check_volatiles(check: dict[str, Any]) -> dict[str, Any]:
    def clean(value: Any) -> Any:
        if isinstance(value, dict):
            return {
                key: clean(item)
                for key, item in value.items()
                if key not in (
                    "stdout_text",
                    "command_bound",
                    "stdout_sha256",
                    "stderr_sha256",
                    "verify_output_tail",
                    "payload_sha256",
                    # run-specific operational facts (issue #54): a writer-lane
                    # holder pid and bound scratch paths of a corrupted
                    # generation cannot participate in cross-run determinism.
                    "holder_pid",
                    "generations",
                )
            }
        if isinstance(value, list):
            return [clean(item) for item in value]
        return value

    return strip_volatile(clean(check))


def _canonical_check(check: dict[str, Any]) -> dict[str, Any]:
    canonical = _strip_check_volatiles(check)
    if canonical.get("kind") == "oracle_freeze":
        # The oracle-freeze detail narrates which freeze record was
        # consulted ("no prior freeze" before the first release vs "unchanged
        # since bundle-N" after).  The result is the gate; the narration
        # legitimately differs across the freeze boundary and must not move
        # the Semantic root (issue #54).  The full report keeps the detail.
        canonical.pop("detail", None)
    return canonical


def canonical_verdict(plan_sha: str, checks: list[dict[str, Any]]) -> dict[str, Any]:
    """Deterministic verdict record for self-comparison: no timestamps, no
    durations, stable per-op facts.  Volatile fields are stripped per canon
    identity docx2typed-qual-canon-1."""
    return {
        "plan_sha256": plan_sha,
        "checks": [
            _canonical_check(check)
            for check in sorted(checks, key=lambda check: check["id"])
        ],
    }


# --------------------------------------------------------------------------
# Oracle branch freeze (issue #54)
# --------------------------------------------------------------------------


def plan_semantic_sha256(plan: dict[str, Any]) -> str:
    """Semantic identity of the plan for Oracle-freeze enforcement: the
    frozen plan hash minus the volatile ``generated`` date, so re-freezing
    the same pins on another day is not a semantic change."""
    return semantic_sha256({key: value for key, value in plan.items() if key != "generated"})


def frozen_identities(plan: dict[str, Any]) -> dict[str, Any]:
    """Flat semantic identity map over the plan's pinned registries.

    This is the exact set the Oracle freeze record pins and the
    oracle-freeze check re-derives from the current plan: drift in any
    entry is a semantic change of the frozen Reference."""
    identities = plan["identities"]
    return {
        "canon": identities["canonicalization"]["schema"],
        "capability": identities["capability"]["sha256"],
        "capability_map": identities["capability_map"]["sha256"],
        "schema_bundle": identities["contract"]["schema_bundle_sha256"],
        "contract_ranges": identities["contract"]["ranges"],
        "fixture_manifest": identities["fixture"]["manifest_sha256"],
        "fixtures": identities["fixture"]["fixtures"],
        "fixture_corpus": identities["fixture_corpus"]["sha256"],
        "resource_profiles": identities["resource_profiles"]["sha256"],
        "office_evidence": identities["office_evidence"]["sha256"],
    }


def _bundle_number(name: str) -> int:
    match = re.fullmatch(r"bundle-(\d+)", name)
    return int(match.group(1)) if match else 0


def latest_freeze_record(root: Path = REPO_ROOT) -> tuple[dict[str, Any], Path] | None:
    """The newest committed Oracle freeze record (highest bundle number).

    Returns ``(record, freeze_path)`` or None when no Reference release has
    been recorded yet — the first release freezes the Oracle."""
    records: list[tuple[dict[str, Any], Path]] = []
    bundle_root = root / "reference"
    if bundle_root.is_dir():
        for freeze_path in bundle_root.glob("bundle-*/freeze.json"):
            try:
                data = json.loads(freeze_path.read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError):
                continue
            if isinstance(data, dict) and data.get("schema") == FREEZE_SCHEMA:
                records.append((data, freeze_path))
    if not records:
        return None
    return max(records, key=lambda item: _bundle_number(item[1].parent.name))


def _execute_oracle_freeze_check(plan: dict[str, Any], root: Path) -> dict[str, Any]:
    """Oracle branch freeze enforcement (issue #54).

    After a Reference release the plan/capability/schema/evidence
    identities are recorded in an immutable freeze record inside the
    versioned bundle.  Any drift from the newest recorded freeze fails
    closed: the Oracle branch is read-only for semantic changes; a semantic
    change requires a classified decision recorded as a new Oracle major."""
    latest = latest_freeze_record(root)
    if latest is None:
        return {
            "result": "pass",
            "detail": "no prior Oracle freeze recorded; enforcement vacuous until the first Reference release",
        }
    freeze, freeze_path = latest
    frozen = freeze.get("frozen", {})
    current = {"plan_sha256": plan_semantic_sha256(plan), **frozen_identities(plan)}
    recorded = {"plan_sha256": frozen.get("plan_semantic_sha256"), **frozen.get("identities", {})}
    drifted = sorted(key for key in current if current[key] != recorded.get(key))
    bundle_name = freeze_path.parent.name
    major = freeze.get("oracle_major", "?")
    if not drifted:
        return {
            "result": "pass",
            "detail": f"Oracle branch unchanged since {bundle_name} (Oracle major {major})",
        }
    return {
        "result": "fail",
        "detail": (
            f"Oracle branch semantic drift from {bundle_name} (Oracle major {major}): "
            + ", ".join(drifted)
            + "; requires a classified decision and a new Oracle major"
        ),
    }


# --------------------------------------------------------------------------
# Report / verdict
# --------------------------------------------------------------------------


def build_report(
    plan: dict[str, Any],
    plan_sha: str,
    identities: dict[str, dict[str, Any]],
    checks: list[dict[str, Any]],
    report_dir: Path,
) -> dict[str, Any]:
    counts = {outcome: sum(1 for check in checks if check["result"] == outcome) for outcome in RESULT_OUTCOMES}
    non_pass = [check for check in checks if check["result"] not in ("pass", "skip")]
    verdict_result = "fail" if non_pass else "pass"
    if non_pass:
        reason = "; ".join(f"{check['id']}: {check['result']} — {check['detail'][:160]}" for check in non_pass[:3])
    else:
        skipped = [check["id"] for check in checks if check["result"] == "skip"]
        reason = "all checks passed" + (f" ({len(skipped)} skipped: {', '.join(skipped)})" if skipped else "")
    summary = {**counts, "checks": len(checks)}
    verdict = {"result": verdict_result, "reason": reason}
    report: dict[str, Any] = {
        "schema": REPORT_SCHEMA,
        "canon": CANON_SCHEMA,
        "plan_sha256": plan_sha,
        "generated": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "identities": identities,
        "checks": checks,
        "summary": summary,
        "verdict": verdict,
    }
    (report_dir / "failures").mkdir(exist_ok=True)
    (report_dir / "report.json").write_text(
        json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    verdict_doc: dict[str, Any] = {
        "schema": VERDICT_SCHEMA,
        "canon": CANON_SCHEMA,
        "plan_sha256": plan_sha,
        "generated": report["generated"],
        "result": verdict_result,
        "reason": verdict["reason"],
        "bindings": {name: section_sha256(plan, name) for name in IDENTITIES},
        "checks": {check["id"]: check["result"] for check in checks},
        "not_run": [check["id"] for check in checks if check["result"] == "not-run"],
        "skipped": [check["id"] for check in checks if check["result"] == "skip"],
        "summary": summary,
    }
    (report_dir / "verdict.json").write_text(
        json.dumps(verdict_doc, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    for check in checks:
        if check["result"] not in ("pass", "skip"):
            (report_dir / "failures" / f"{check['id']}.txt").write_text(
                json.dumps(check, ensure_ascii=False, indent=2) + "\n",
                encoding="utf-8",
            )
    return report


def validate_report(plan: dict[str, Any], report: dict[str, Any]) -> dict[str, Any]:
    """Coverage audit of a generated report against its plan.  A plan check
    missing from the report (not-run) or a non-pass check reported as pass is
    a ReportError — failure cases can never be misreported as pass."""
    if report.get("schema") != REPORT_SCHEMA:
        raise ReportError(f"report schema {report.get('schema')!r} != {REPORT_SCHEMA}")
    expected_plan_sha = plan_sha256(plan)
    if report.get("plan_sha256") != expected_plan_sha:
        raise ReportError("report plan_sha256 does not match the plan")
    plan_ids = [check["id"] for check in plan["checks"]]
    report_ids = [check["id"] for check in report.get("checks", [])]
    duplicates = sorted({check_id for check_id in report_ids if report_ids.count(check_id) > 1})
    if duplicates:
        raise ReportError(f"report repeats checks: {duplicates}")
    not_run = [check_id for check_id in plan_ids if check_id not in report_ids]
    if not_run:
        raise ReportError(f"plan checks not run: {not_run}")
    extra = [check_id for check_id in report_ids if check_id not in plan_ids]
    if extra:
        raise ReportError(f"report contains unknown checks: {extra}")
    plan_by_id = {check["id"]: check for check in plan["checks"]}
    for check in report["checks"]:
        if check["result"] not in RESULT_OUTCOMES:
            raise ReportError(f"check {check['id']}: invalid result {check['result']!r}")
        expected_bindings = _bindings(plan, plan_by_id[check["id"]])
        if check.get("bindings") != expected_bindings:
            raise ReportError(f"check {check['id']}: binding drift from the plan")
    verdict = report.get("verdict", {})
    non_pass = [check for check in report["checks"] if check["result"] not in ("pass", "skip")]
    if non_pass and verdict.get("result") != "fail":
        raise ReportError(f"non-pass checks misreported as pass: {[c['id'] for c in non_pass]}")
    if not non_pass and verdict.get("result") != "pass":
        raise ReportError("all-pass report reported as fail")
    return report


# --------------------------------------------------------------------------
# Runner
# --------------------------------------------------------------------------


def run(
    plan: dict[str, Any],
    *,
    root: Path = REPO_ROOT,
    scratch: Path | None = None,
    report_dir: Path | None = None,
    self_compare: bool | None = None,
) -> dict[str, Any]:
    """Execute a frozen plan end to end and emit provenance-bound artifacts.

    ``self_compare`` defaults to following the plan: when the plan declares a
    self-comparison check, every other check runs twice in independent
    scratch dirs and the canonical verdicts must match.  Passing
    ``self_compare=False`` disables the second execution and reports the
    declared self-comparison check as an explicit skip, never a silent pass.
    """
    validate_plan(plan)
    plan_sha = plan_sha256(plan)
    scratch = Path(scratch) if scratch is not None else Path(tempfile.mkdtemp(prefix="docx2typed-qualify-"))
    report_dir = Path(report_dir) if report_dir is not None else scratch / "report"
    if scratch.exists():
        shutil.rmtree(scratch)
    scratch.mkdir(parents=True)
    report_dir.mkdir(parents=True, exist_ok=True)
    bundle = schema_bundle()
    declares_self_compare = any(check["kind"] == "self_comparison" for check in plan["checks"])
    compare = (self_compare if self_compare is not None else declares_self_compare) and declares_self_compare

    executable = [check for check in plan["checks"] if check["kind"] != "self_comparison"]
    # The two self-comparison executions run in independent scratch dirs so
    # no side effect of the first can influence the second.
    checks_v1 = [
        _execute_check(check, plan, root, scratch / "v1", report_dir, bundle)
        for check in executable
    ]
    self_check: dict[str, Any] | None = None
    if compare:
        checks_v2 = [
            _execute_check(check, plan, root, scratch / "v2", report_dir, bundle)
            for check in executable
        ]
        v1 = canonical_verdict(plan_sha, checks_v1)
        v2 = canonical_verdict(plan_sha, checks_v2)
        same = v1 == v2
        self_check = {
            "id": "self-comparison",
            "kind": "self_comparison",
            "bindings": _bindings(plan, next(c for c in plan["checks"] if c["kind"] == "self_comparison")),
            "result": "pass" if same else "fail",
            "detail": (
                "canonical verdicts identical across two executions"
                if same
                else "canonical verdicts differ across two executions"
            ),
            "ops": [],
        }
    elif declares_self_compare:
        # The plan declares a self-comparison check but the caller disabled
        # the second execution: report an explicit skip, never a silent pass.
        self_check = {
            "id": "self-comparison",
            "kind": "self_comparison",
            "bindings": _bindings(plan, next(c for c in plan["checks"] if c["kind"] == "self_comparison")),
            "result": "skip",
            "detail": "self-comparison disabled by caller (self_compare=False)",
            "ops": [],
        }
    checks = checks_v1 + ([self_check] if self_check is not None else [])
    identities = validate_identities(plan, root, bundle=bundle)
    return build_report(plan, plan_sha, identities, checks, report_dir)


# --------------------------------------------------------------------------
# Plan freeze / CLI
# --------------------------------------------------------------------------


def freeze_plan(plan_path: Path, root: Path = REPO_ROOT) -> dict[str, Any]:
    """Re-pin the plan's capability/fixture/contract pins from the live
    repository.  The independent freeze command is the only way a plan's pins
    legitimately change (run it on the branch baseline; the supervisor
    re-freezes once after protocol changes land)."""
    plan = json.loads(plan_path.read_text(encoding="utf-8"))
    if plan.get("schema") != PLAN_SCHEMA:
        raise PlanError(f"{plan_path} is not a {PLAN_SCHEMA} plan")
    bundle = schema_bundle()
    descriptor = engine_descriptor()
    capability = plan["identities"]["capability"]
    manifest = json.loads((root / capability["path"]).read_text(encoding="utf-8"))
    capability["sha256"] = semantic_sha256(manifest)
    fixture = plan["identities"]["fixture"]
    fixture_manifest = json.loads(
        (root / fixture["manifest_path"]).read_text(encoding="utf-8")
    )
    fixture["manifest_sha256"] = semantic_sha256(fixture_manifest)
    fixture["fixtures"] = {
        name: file_sha256(root / fixture["dir"] / name) for name in fixture["fixtures"]
    }
    contract = plan["identities"]["contract"]
    contract["schema_bundle_sha256"] = semantic_sha256(bundle)
    contract["ranges"] = descriptor["contracts"]
    capability_map = plan["identities"]["capability_map"]
    task_map_path = root / capability_map["path"]
    if not task_map_path.is_file():
        raise PlanError(f"freeze: {capability_map['path']} missing")
    task_map = json.loads(task_map_path.read_text(encoding="utf-8"))
    capability_map["sha256"] = semantic_sha256(task_map)
    capability_map["schema"] = task_map.get("schema", TASK_MAP_SCHEMA)
    for name in ("fixture_corpus", "resource_profiles", "office_evidence"):
        identity = plan["identities"][name]
        path = root / identity["path"]
        if not path.is_file():
            raise PlanError(f"freeze: {identity['path']} missing")
        identity["sha256"] = semantic_sha256(json.loads(path.read_text(encoding="utf-8")))
    plan["generated"] = datetime.now(timezone.utc).date().isoformat()
    validate_plan(plan)
    plan_path.write_text(json.dumps(plan, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    return plan


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--plan", default=str(DEFAULT_PLAN), help="frozen plan path")
    parser.add_argument("--report", default=None, help="report output dir (default: under scratch)")
    parser.add_argument("--work", default=None, help="scratch root (default: temp)")
    parser.add_argument("--freeze", action="store_true", help="re-pin the plan from the live repo")
    args = parser.parse_args(argv)
    plan_path = Path(args.plan)
    if args.freeze:
        try:
            freeze_plan(plan_path)
        except (OSError, json.JSONDecodeError, PlanError) as exc:
            print(f"freeze error: {exc}")
            return 2
        print(f"re-pinned: {plan_path}")
        return 0
    try:
        plan = json.loads(plan_path.read_text(encoding="utf-8"))
        validate_plan(plan)
    except (OSError, json.JSONDecodeError, PlanError) as exc:
        print(f"plan error: {exc}")
        return 2
    try:
        scratch = Path(args.work) if args.work else Path(tempfile.mkdtemp(prefix="docx2typed-qualify-"))
        report_dir = Path(args.report) if args.report else scratch / "report"
        report = run(plan, scratch=scratch, report_dir=report_dir)
        validate_report(plan, report)
    except ReportError as exc:
        print(f"report error: {exc}")
        return 1
    except Exception as exc:  # noqa: BLE001 - runner failures are fatal
        print(f"qualification error: {type(exc).__name__}: {exc}")
        return 1
    print(f"verdict: {report['verdict']['result']} — {report['verdict']['reason']}")
    print(f"report: {report_dir}")
    return 0 if report["verdict"]["result"] == "pass" else 1


if __name__ == "__main__":
    raise SystemExit(main())
