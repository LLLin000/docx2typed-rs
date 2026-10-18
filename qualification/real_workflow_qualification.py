"""Collect executed real-Word lane reports without exposing private document text.

Run the two real_workflow_*_handoff.py lanes first; this entry validates their
outcomes and publishes anonymous assertion-level evidence, not a simulated run.
"""
from __future__ import annotations

import argparse
from collections import Counter
import hashlib
import json
from pathlib import Path

OUTCOMES = {"PASS", "EXPECTED_REJECT", "UNSUPPORTED_WORKFLOW", "CONTRACT_FAILURE"}


def collect(paths: list[Path]) -> dict:
    cases = []
    hosts = []
    seen = set()
    for path in paths:
        raw = path.read_bytes()
        report = json.loads(raw)
        if report.get("schema") != "real-workflow-qualification-1":
            raise ValueError(f"unexpected schema: {path}")
        host = report.get("host", {})
        hosts.append({k: host[k] for k in ("name", "application", "version", "build", "os",
                                         "execution_mode", "human_ui_review") if k in host})
        for case in report["cases"]:
            key = (report["workflow"], case["case_id"])
            if key in seen:
                raise ValueError(f"duplicate case: {key}")
            seen.add(key)
            outcome = case.get("outcome")
            status = case.get("completion_status", "incomplete").lower()
            if status == "complete" and outcome not in OUTCOMES:
                raise ValueError(f"completed case lacks an outcome: {key}")
            if status != "complete" and outcome is not None:
                raise ValueError(f"unexecuted case claims an outcome: {key}")
            assertions = case.get("assertions", [])
            passed = lambda a: a.get("pass") is True or a.get("result") == "pass"
            if outcome in ("PASS", "EXPECTED_REJECT") and (not assertions or not all(map(passed, assertions))):
                raise ValueError(f"successful/refused case has unproven assertions: {key}")
            # Only hashes and normalized assertion IDs leave private lane reports.
            artifacts = {k: v for k, v in case.get("artifacts", {}).items()
                         if ("sha256" in k and isinstance(v, str) and len(v) == 64)
                         or (k.startswith("baseline_epoch") and isinstance(v, int))}
            cases.append({"workflow": key[0], "case_id": key[1],
                          "input_sha256": case.get("input_sha256"),
                          "completion_status": status, "outcome": outcome,
                          "binary": report.get("binary"),
                          "manifest_sha256": report.get("manifest_sha256"),
                          "private_report_sha256": hashlib.sha256(raw).hexdigest(),
                          "steps": [{"name": s["name"], "result": s.get("outcome", s.get("ok"))}
                                    for s in case.get("steps", [])],
                          "assertions": [{"id": a.get("id", a.get("name")), "pass": passed(a)}
                                         for a in assertions],
                          "artifacts": artifacts, "limitations": case.get("limitations", [])})
    counts = Counter(c["outcome"] for c in cases if c["completion_status"] == "complete")
    return {"schema": "real-workflow-qualification-1", "qualification_id": "real-workflow-qualification-v1",
            "scope": ["word-handoff", "review-handoff"], "excluded": ["Zotero"],
            "hosts": hosts, "counts": {key: counts[key] for key in sorted(OUTCOMES)},
            "unexecuted": sum(c["completion_status"] != "complete" for c in cases), "cases": cases}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--report", type=Path, action="append", required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    if args.out.exists():
        raise SystemExit("refusing to overwrite existing workflow evidence")
    result = collect(args.report)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({"counts": result["counts"], "unexecuted": result["unexecuted"]}))


if __name__ == "__main__":
    main()
