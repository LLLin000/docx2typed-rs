"""Agent black-box benchmark (L5): natural-language tasks through MCP only.

The agent (human or model) receives a source DOCX, the MCP tool list, and
one natural-language prompt. It may call the MCP tools via the persistent
serve loop below (``--serve``) and the CLI ``extract``/``verify`` commands;
it must NOT touch typed.md, edit.md, XML, or private functions. The grader
(``--grade``) checks the produced DOCX with the same oracle vocabulary as
the release task suite.

Tasks: capabilities/tasks/agent.json. Run:

    python -m scripts.agent_bench --list
    python -m scripts.agent_bench --serve          # persistent MCP driver
    python -m scripts.agent_bench --grade <task-id> <output.docx> <workdir>
"""
from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import time
import zipfile
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parent.parent
TASKS = REPO_ROOT / "capabilities" / "tasks" / "agent.json"

SERVE_BANNER = "agent-bench serve ready"


def _load_tasks() -> list[dict[str, Any]]:
    return json.loads(TASKS.read_text(encoding="utf-8"))["tasks"]


def _word_parts(path: Path) -> dict[str, bytes]:
    with zipfile.ZipFile(path) as archive:
        return {
            name: archive.read(name)
            for name in archive.namelist()
            if re.match(rb"word/.*\.xml$", name.encode())
        }


def _visible(xml: bytes) -> str:
    return "".join(
        m.group(1).decode("utf-8", errors="replace")
        for m in re.finditer(rb"<w:t[^>]*>(.*?)</w:t>", xml, re.S)
    )


def _text_count(path: Path, text: str) -> int:
    needle = text.encode("utf-8")
    return sum(part.count(needle) for part in _word_parts(path).values())


def _residual(path: Path) -> int:
    pattern = re.compile(rb"<w:(ins|del)[ >]")
    return sum(len(pattern.findall(part)) for part in _word_parts(path).values())


def grade(task_id: str, output: Path, workdir: Path) -> dict[str, Any]:
    task = next(t for t in _load_tasks() if t["id"] == task_id)
    results: list[dict[str, Any]] = []

    def check(kind: str, ok: bool, detail: str = "") -> None:
        results.append({"kind": kind, "passed": ok, "detail": detail})

    verify_rc = subprocess.run(
        ["python", "-m", "scripts", "verify", str(workdir), str(output)],
        cwd=REPO_ROOT, capture_output=True, text=True,
    ).returncode
    check("verify_pass", verify_rc == 0, f"verify rc={verify_rc}")
    for spec in task.get("oracles", {}).get("positive", []):
        if spec["kind"] == "text_count":
            count = _text_count(output, spec["text"])
            check(f"text_count {spec['text']}", count == spec["count"], f"count={count}")
        elif spec["kind"] == "text_visible_contains":
            visible = "".join(_visible(p) for p in _word_parts(output).values())
            check(f"visible {spec['text']}", spec["text"] in visible, "present")
        elif spec["kind"] == "residual_revisions":
            count = _residual(output)
            check("residual_revisions", count == spec["count"], f"residual={count}")
        elif spec["kind"] == "comment_present":
            has = _text_count(output, f'<w:comment w:id="{spec["id"]}"') + _text_count(output, f'w:id="{spec["id"]}"')
            check(f"comment {spec['id']} present", has > 0, f"traces={has}")
        elif spec["kind"] == "comment_absent":
            has = _text_count(output, f'<w:comment w:id="{spec["id"]}"') + _text_count(output, f'w:id="{spec["id"]}"')
            check(f"comment {spec['id']} absent", has == 0, f"traces={has}")
    for spec in task.get("oracles", {}).get("negative", []):
        if spec["kind"] == "text_count":
            count = _text_count(output, spec["text"])
            check(f"absent {spec['text']}", count == spec["count"], f"count={count}")
    for spec in task.get("oracles", {}).get("fidelity", []):
        if spec["kind"] == "parts_unchanged":
            src = _word_parts(Path(REPO_ROOT / task["source"]))
            out = _word_parts(output)
            excepted = set(spec.get("except", []))
            changed = [n for n in src if n not in excepted and src[n] != out.get(n)]
            check("parts_unchanged", not changed, f"changed: {changed[:4]}")
        elif spec["kind"] == "row_count":
            xml = _word_parts(output)["word/document.xml"]
            count = len(re.findall(rb"<w:tr[ >]", xml))
            check("row_count", count == spec["count"], f"rows={count}")
        elif spec["kind"] == "no_text_duplication":
            xml = _word_parts(output)["word/document.xml"]
            src_xml = _word_parts(Path(REPO_ROOT / task["source"]))["word/document.xml"]
            src_cells = [m.group(0) for m in re.finditer(rb"<w:tc>.*?</w:tc>", src_xml, re.S)]
            dup = [c for c in src_cells if xml.count(c) > src_xml.count(c)]
            check("no_text_duplication", not dup, f"duplicated cells: {len(dup)}")
        elif spec["kind"] == "office_open":
            result = subprocess.run(
                [r"C:/Program Files/LibreOffice/program/soffice.exe", "--headless",
                 "--convert-to", "pdf", "--outdir", str(output.parent / "pdf"), str(output)],
                capture_output=True, text=True, timeout=600,
            )
            check("office_open", result.returncode == 0 and "convert" in (result.stdout or result.stderr), "converted")

    passed = all(r["passed"] for r in results)
    return {"task_id": task_id, "prompt": task["prompt"], "result": "pass" if passed else "fail", "oracles": results}


def _count_key(value: Any, key: str) -> int:
    if isinstance(value, dict):
        return (1 if key in value else 0) + sum(_count_key(v, key) for v in value.values())
    if isinstance(value, list):
        return sum(_count_key(v, key) for v in value)
    return 0


def _tool_result_ok(value: Any) -> bool:
    if hasattr(value, "isError"):
        if bool(getattr(value, "isError")):
            return False
        return _tool_result_ok(getattr(value, "structuredContent", None))
    if isinstance(value, dict):
        return value.get("outcome") != "failure" and value.get("is_error") is not True
    if isinstance(value, str):
        try:
            return _tool_result_ok(json.loads(value))
        except json.JSONDecodeError:
            return True
    return True


def _trace_event(request: dict[str, Any], value: Any, duration_ms: int, ok: bool) -> dict[str, Any]:
    args = request.get("args") if isinstance(request.get("args"), dict) else {}
    return {
        "kind": "tool_call",
        "tool": request.get("tool", ""),
        "ok": ok and _tool_result_ok(value),
        "duration_ms": duration_ms,
        "arg_keys": sorted(args),
        "match_ref_count": _count_key(args, "match_ref"),
        "wrong_tool": bool(args.get("_wrong_tool", False)),
        "recovery": bool(args.get("_recovery", False)),
        "raw_xml_escape": bool(args.get("_raw_xml_escape", False)),
    }

def serve(
    *,
    trace_out: Path | None = None,
    variant: str = "unknown",
    task_id: str | None = None,
) -> int:
    """Persistent MCP loop with optional timing trace output."""
    import importlib

    started = time.monotonic()
    events: list[dict[str, Any]] = []
    server = importlib.import_module("scripts.mcp_server")
    print(SERVE_BANNER, flush=True)
    for line in sys.stdin:
        if not line.strip():
            continue
        request: dict[str, Any] = {}
        call_started = time.monotonic()
        try:
            parsed = json.loads(line)
            if not isinstance(parsed, dict):
                raise TypeError("request must be an object")
            request = parsed
            out = getattr(server, request["tool"])(**request.get("args", {}))
            duration_ms = int((time.monotonic() - call_started) * 1000)
            events.append(_trace_event(request, out, duration_ms, True))
            print("RESULT " + json.dumps({"ok": True, "data": out}, ensure_ascii=True)[:2000], flush=True)
        except Exception as exc:  # noqa: BLE001 - structured tool failure
            duration_ms = int((time.monotonic() - call_started) * 1000)
            events.append(
                {
                    "kind": "tool_call",
                    "tool": request.get("tool", ""),
                    "ok": False,
                    "duration_ms": duration_ms,
                    "arg_keys": sorted(request.get("args", {})) if isinstance(request.get("args"), dict) else [],
                    "error": str(exc)[:500],
                    "match_ref_count": 0,
                    "wrong_tool": False,
                    "recovery": False,
                    "raw_xml_escape": False,
                }
            )
            print("RESULT " + json.dumps({"ok": False, "error": str(exc)[:500]}, ensure_ascii=True), flush=True)
    server_wall_time_ms = int((time.monotonic() - started) * 1000)
    if trace_out is not None:
        trace_out.parent.mkdir(parents=True, exist_ok=True)
        trace_out.write_text(
            json.dumps(
                {
                    "schema": "docx2typed-agent-trace-1",
                    "variant": variant,
                    "task_id": task_id,
                    "duration_ms": server_wall_time_ms,
                    "server_wall_time_ms": server_wall_time_ms,
                    "events": events,
                },
                ensure_ascii=False,
                indent=2,
            ) + "\n",
            encoding="utf-8",
        )
    return 0


def record(results_path: Path, *, model: str, agent_version: str, mcp_commit: str) -> dict[str, Any]:
    """Provenance record for an agent-qualification run: binds the results
    to the model, agent version, MCP server commit, and prompt hashes."""
    import hashlib

    def prompt_hash(task: dict[str, Any]) -> str:
        payload = json.dumps(
            {"prompt": task["prompt"], "source": task["source"], "oracles": task.get("oracles", {})},
            ensure_ascii=False, sort_keys=True,
        ).encode("utf-8")
        return hashlib.sha256(payload).hexdigest()[:16]

    results = json.loads(results_path.read_text(encoding="utf-8"))
    tasks = _load_tasks()
    return {
        "schema": "docx2typed-agent-qualification-1",
        "agent": {"model": model, "version": agent_version},
        "mcp_server_commit": mcp_commit,
        "task_schema_version": 1,
        "prompt_hashes": {t["id"]: prompt_hash(t) for t in tasks},
        "results_path": str(results_path),
        "result": results.get("summary"),
        "generated": __import__("datetime").datetime.now(__import__("datetime").timezone.utc).isoformat(timespec="seconds"),
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--list", action="store_true")
    parser.add_argument("--serve", action="store_true")
    parser.add_argument("--grade", nargs=3, metavar=("TASK_ID", "OUTPUT", "WORKDIR"))
    parser.add_argument("--record", metavar="RESULTS_JSON", help="write the agent-qualification provenance record")
    parser.add_argument("--trace-out", metavar="TRACE_JSON", help="write per-tool timing trace while serving")
    parser.add_argument("--variant", default="unknown", help="skill variant name stored in a serving trace")
    parser.add_argument("--task-id", default=None, help="evaluation task id stored in a serving trace")
    eval_group = parser.add_mutually_exclusive_group()
    eval_group.add_argument("--metrics", metavar="TRACE_JSON", help="analyze one trace or trace collection")
    eval_group.add_argument("--compare", nargs=2, metavar=("TRACE_A", "TRACE_B"), help="compare two trace variants")
    parser.add_argument("--eval-tasks", default=str(REPO_ROOT / "evals" / "skill" / "tasks.json"))
    parser.add_argument("--model", default="unknown")
    parser.add_argument("--agent-version", default="unknown")
    parser.add_argument("--mcp-commit", default="unknown")
    parser.add_argument("--out", default=None, help="output path for records or evaluation reports")
    args = parser.parse_args(argv)
    if args.metrics or args.compare:
        from scripts.skill_eval import analyze_file, compare_traces

        manifest = Path(args.eval_tasks) if args.eval_tasks else None
        report = (
            analyze_file(Path(args.metrics), manifest)
            if args.metrics
            else compare_traces(Path(args.compare[0]), Path(args.compare[1]), manifest)
        )
        text = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
        if args.out:
            Path(args.out).write_text(text, encoding="utf-8")
            print(f"report written: {args.out}")
        else:
            print(text, end="")
        return 0
    if args.list:
        for task in _load_tasks():
            print(f"{task['id']}: {task['prompt']}  [{task['source']}]")
        return 0
    if args.serve:
        return serve(
            trace_out=Path(args.trace_out) if args.trace_out else None,
            variant=args.variant,
            task_id=args.task_id,
        )
    if args.grade:
        task_id, output, workdir = args.grade
        report = grade(task_id, Path(output), Path(workdir))
        print(json.dumps(report, ensure_ascii=False, indent=2))
        return 0 if report["result"] == "pass" else 1
    if args.record:
        record_path = Path(args.out or "agent-qualification.json")
        record_path.write_text(
            json.dumps(record(Path(args.record), model=args.model, agent_version=args.agent_version, mcp_commit=args.mcp_commit), ensure_ascii=False, indent=2) + "\n",
            encoding="utf-8",
        )
        print(f"record written: {record_path}")
        return 0
    parser.print_help()
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
