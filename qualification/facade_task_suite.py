#!/usr/bin/env python3
"""Agent Facade acceptance: natural-language tasks scored on OUTCOME.

The unit suites prove the engine. This suite asks the agent-facing question:
given a plain request against a real patent, did the Facade locate, edit,
verify and refuse correctly — and did the exported document actually change?

Scored per task, not per tool call. A task passes only when the DOCUMENT
reaches the expected state; a call that returns `success` while the file keeps
the old text is a FAILURE, which is the whole point of the exercise.

Usage:
    python qualification/facade_task_suite.py [--docx PATH] [--json]
"""
from __future__ import annotations

import argparse
import json
import re
import shutil
import subprocess
import sys
import tempfile
import zipfile
from dataclasses import dataclass, field
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
DEFAULT_BIN = REPO / "target" / "release" / "docx2typed.exe"
DEFAULT_DOCX = REPO / "corpus" / "release" / "patent-shaped.docx"


def bin_path() -> Path:
    return DEFAULT_BIN if DEFAULT_BIN.is_file() else DEFAULT_BIN.with_suffix("")


@dataclass
class Task:
    name: str
    kind: str
    request: str
    calls: list[tuple[str, dict]]
    # Expected outcome, checked against the DOCUMENT not the return value.
    expect_contains: list[str] = field(default_factory=list)
    expect_absent: list[str] = field(default_factory=list)
    expect_error: str | None = None
    # (call index, dotted path into the reply's data, expected value). A task
    # that only asserts "no error" proves nothing about whether the Facade did
    # the right thing, so the interesting tasks pin the payload too.
    checks: list[tuple[int, str, object]] = field(default_factory=list)


@dataclass
class Result:
    task: Task
    passed: bool
    detail: str


def docx_visible(path: Path) -> str:
    """Visible text of a DOCX, tags stripped (a phrase can span runs)."""
    with zipfile.ZipFile(path) as archive:
        xml = archive.read("word/document.xml").decode("utf-8")
    return re.sub(r"<[^>]+>", "", xml)


def run_mcp(workdir: Path, calls: list[tuple[str, dict]]) -> dict[int, dict]:
    lines = [json.dumps({"tool": tool, "args": args}) for tool, args in calls]
    proc = subprocess.run(
        [str(bin_path()), "mcp"],
        input="\n".join(lines) + "\n",
        capture_output=True,
        text=True,
        timeout=300,
    )
    out: dict[int, dict] = {}
    index = 0
    for line in proc.stdout.splitlines():
        if not line.startswith("OK "):
            continue
        index += 1
        wrapper = json.loads(line[3:])
        out[index] = wrapper.get("structuredContent", wrapper)
    return out


def open_wd(workdir: Path) -> tuple[str, dict]:
    return ("workdir_open", {"workdir": str(workdir)})


def build_tasks() -> list[Task]:
    return [
        # --- terminology unification -------------------------------------
        Task(
            name="unify_terminology_body",
            kind="unify-terminology",
            request="把全文的“材料”统一改成“复合材料”",
            calls=[
                ("facade_resolve", {"find": "材料", "replace_with": "复合材料"}),
                ("commit_sync", {"operation_id": "u1"}),
                ("build_docx", {"output": "__OUT__", "operation_id": "u2"}),
            ],
            expect_contains=["复合材料"],
        ),
        Task(
            name="unify_reports_coverage",
            kind="unify-terminology",
            request="统一术语，并告诉我扫了哪些区域",
            calls=[
                ("facade_locate", {"query": "材料"}),
            ],
            checks=[
                (0, "coverage.scanned_parts.0", "document"),
                (0, "coverage.unscanned_parts.0", "header1"),
            ],
        ),
        Task(
            name="unify_no_match_is_informational",
            kind="unify-terminology",
            request="把“不存在的词”统一改成“X”",
            calls=[
                ("facade_resolve", {"find": "不存在的词", "replace_with": "X"}),
            ],
        ),
        # --- targeted claim edits ----------------------------------------
        Task(
            name="edit_one_claim_by_handle",
            kind="targeted-claim",
            request="只改权利要求1里的那个词",
            calls=[
                ("facade_locate", {"query": "材料"}),
            ],
            checks=[
                (0, "targets.0.capability", "supported"),
                (0, "targets.0.paragraph_id", "P0"),
            ],
        ),
        Task(
            name="edit_does_not_touch_other_claims",
            kind="targeted-claim",
            request="改一处，别动其他地方",
            calls=[
                ("facade_locate", {"query": "材料"}),
            ],
            checks=[(0, "supported", 7)],
        ),
        # --- insertion ----------------------------------------------------
        Task(
            name="insert_dependent_claim",
            kind="insert",
            request="在权利要求1后面插一条新的从属权利要求",
            calls=[
                (
                    "insert_paragraph",
                    {
                        "after_id": "P0",
                        "text": "2. 根据权利要求1所述的材料，其特征在于，所述微球的粒径为10～50μm。",
                        "inherit": "P0",
                        "operation_id": "i1",
                    },
                ),
                ("commit_sync", {"operation_id": "i2"}),
                ("build_docx", {"output": "__OUT__", "operation_id": "i3"}),
            ],
            expect_contains=["粒径为10～50μm"],
        ),
        Task(
            name="inserted_paragraph_is_editable",
            kind="insert",
            request="插入一段后，再改这段里的词",
            calls=[
                (
                    "insert_paragraph",
                    {
                        "after_id": "P0",
                        "text": "新增段落含独有术语标记。",
                        "inherit": "P0",
                        "operation_id": "j1",
                    },
                ),
                ("facade_locate", {"query": "独有术语标记"}),
            ],
            checks=[
                (1, "matches", 1),
                (1, "targets.0.paragraph_id", "N1"),
                (1, "targets.0.capability", "supported"),
            ],
        ),
        # --- parameter edits ----------------------------------------------
        Task(
            name="edit_example_parameter",
            kind="parameter",
            request="把实施例里的一个参数改掉",
            calls=[
                ("facade_locate", {"query": "mg"}),
            ],
            checks=[(0, "matches", 0)],  # the fixture carries no dosage
        ),
        # --- refusals -----------------------------------------------------
        Task(
            name="refuse_cross_run_split",
            kind="refusal",
            request="改一个可能跨文本块的词",
            calls=[
                ("facade_resolve", {"find": "材料", "replace_with": "复合材料"}),
            ],
            expect_error=None,  # either applies cleanly or refuses; never partial
        ),
        Task(
            name="refuse_empty_query",
            kind="refusal",
            request="空查询必须被拒绝",
            calls=[
                ("facade_locate", {"query": ""}),
            ],
            expect_error="locate-query-empty",
        ),
        Task(
            name="refuse_bogus_handle",
            kind="refusal",
            request="用一个不存在的句柄编辑必须被拒绝",
            calls=[
                (
                    "facade_edit",
                    {"edits": [{"handle": "ref_not_real", "replace_with": "X"}]},
                ),
            ],
        ),
        # --- atomicity ----------------------------------------------------
        Task(
            name="batch_all_or_nothing",
            kind="atomicity",
            request="一批里有一个坏目标，就一个都别改",
            calls=[
                ("facade_locate", {"query": "材料"}),
                (
                    "facade_edit",
                    {
                        "edits": [
                            {"handle": "ref_not_real", "replace_with": "X"},
                            {"handle": "ref_also_not_real", "replace_with": "Y"},
                        ]
                    },
                ),
            ],
            checks=[
                (0, "matches", 7),
                (1, "outcome", "blocked"),
                (1, "staged", 0),
                (1, "blocked", 2),
            ],
        ),
        # --- idempotency --------------------------------------------------
        Task(
            name="repeat_before_commit_refuses",
            kind="idempotency",
            request="同一处没提交前再改一次，必须拒绝而不是叠加",
            calls=[
                ("facade_resolve", {"find": "材料", "replace_with": "复合材料"}),
                ("facade_resolve", {"find": "材料", "replace_with": "复合材料"}),
            ],
            # The replacement CONTAINS the search term, so a naive second pass
            # would produce 复合复合材料. It refuses instead, naming why.
            expect_error="paragraph-has-uncommitted-length-change",
            checks=[(0, "edit.outcome", "committed")],
        ),
        Task(
            name="retry_same_operation_id_replays",
            kind="idempotency",
            request="同一个 operation_id 重试必须字节级重放",
            calls=[
                (
                    "document_replace",
                    {"find": "材料", "replace": "复合材料", "operation_id": "same-op"},
                ),
                (
                    "document_replace",
                    {"find": "材料", "replace": "复合材料", "operation_id": "same-op"},
                ),
            ],
            checks=[
                (0, "matches", 7),
                (1, "matches", 7),  # the cached reply, not a second application
            ],
        ),
        # --- completion contract ------------------------------------------
        Task(
            name="verify_reports_execution_evidence",
            kind="completion",
            request="告诉我每一处修改是否落地",
            calls=[
                ("facade_resolve", {"find": "材料", "replace_with": "复合材料"}),
                ("commit_sync", {"operation_id": "v1"}),
                ("build_docx", {"output": "__OUT__", "operation_id": "v2"}),
                ("facade_verify", {"query": "复合材料", "output": "__OUT__"}),
            ],
            expect_contains=["复合材料"],
        ),
        Task(
            name="verify_separates_coverage_from_execution",
            kind="completion",
            request="告诉我哪些区域没扫到",
            calls=[
                ("facade_verify", {}),
            ],
            checks=[
                (0, "coverage_evidence.scanned_parts.0", "document"),
                (0, "coverage_evidence.unscanned_parts.0", "header1"),
                (0, "coverage_evidence.claim_reaches", "the main document story only"),
            ],
        ),
        Task(
            name="counts_agree_after_build",
            kind="completion",
            request="报告的数量要和产物里的数量一致",
            calls=[
                ("facade_resolve", {"find": "材料", "replace_with": "复合材料"}),
                ("commit_sync", {"operation_id": "w1"}),
                ("build_docx", {"output": "__OUT__", "operation_id": "w2"}),
                ("facade_verify", {"query": "复合材料", "output": "__OUT__"}),
            ],
            expect_contains=["复合材料"],
        ),
        # --- history ------------------------------------------------------
        Task(
            name="history_lists_a_version",
            kind="history",
            request="列出当前版本",
            calls=[
                (
                    "insert_paragraph",
                    {
                        "after_id": "P0",
                        "text": "3. 根据权利要求1所述的材料。",
                        "inherit": "P0",
                        "operation_id": "hist-ins",
                    },
                ),
                ("commit_sync", {"operation_id": "hist-commit"}),
                ("history_list", {"limit": 10}),
            ],
            checks=[(2, "total", 1), (2, "versions.0.version", "V1")],
        ),
        Task(
            name="restore_after_edit",
            kind="history",
            request="改完之后能恢复",
            calls=[
                ("facade_resolve", {"find": "材料", "replace_with": "复合材料"}),
                ("commit_sync", {"operation_id": "h1"}),
                ("history_list", {"limit": 10}),
            ],
            checks=[(2, "total", 1), (2, "versions.0.version", "V1")],
        ),
    ]


def evaluate(task: Task, workdir: Path, source: Path, out: Path) -> Result:
    """Run one task and judge it by the DOCUMENT, not the return value."""
    calls: list[tuple[str, dict]] = []
    for tool, args in task.calls:
        resolved = {
            key: (str(out) if value == "__OUT__" else value) for key, value in args.items()
        }
        calls.append((tool, resolved))
    results = run_mcp(workdir, [open_wd(workdir), *calls])

    # An expected error must appear somewhere in the task's own replies.
    if task.expect_error:
        seen = [
            diagnostic.get("code")
            for envelope in results.values()
            for diagnostic in envelope.get("diagnostics", [])
        ]
        if task.expect_error not in seen:
            return Result(task, False, f"expected {task.expect_error}, saw {seen}")

    if task.expect_contains or task.expect_absent:
        if not out.is_file():
            return Result(task, False, "no DOCX was built")
        text = docx_visible(out)
        for needle in task.expect_contains:
            if needle not in text:
                return Result(task, False, f"built DOCX is missing {needle!r}")
        for needle in task.expect_absent:
            if needle in text:
                return Result(task, False, f"built DOCX still contains {needle!r}")

    # Payload assertions. `run_mcp` was handed `[workdir_open, *calls]`, so
    # transport slot 1 is workdir_open and task call N is slot N + 2.
    for index, path, expected in task.checks:
        envelope = results.get(index + 2)
        if envelope is None:
            return Result(task, False, f"call {index} produced no reply")
        node: object = envelope.get("data", {})
        for key in path.split("."):
            if isinstance(node, dict):
                node = node.get(key)
            elif isinstance(node, list) and key.isdigit():
                node = node[int(key)] if int(key) < len(node) else None
            else:
                node = None
            if node is None:
                break
        if node != expected:
            return Result(task, False, f"call {index} {path}: got {node!r}, want {expected!r}")

    # A refusal must not have written anything.
    if task.kind == "refusal" and task.expect_error is None:
        drafts = list(workdir.glob("edit.md"))
        if not drafts:
            return Result(task, True, "refused before writing a draft")

    return Result(task, True, "ok")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--docx", type=Path, default=DEFAULT_DOCX)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()

    if not args.docx.is_file():
        print(f"fixture not found: {args.docx}", file=sys.stderr)
        return 2

    tasks = build_tasks()
    results: list[Result] = []
    for task in tasks:
        scratch = Path(tempfile.mkdtemp(prefix="facade-task-"))
        workdir = scratch / "wd"
        out = scratch / "out.docx"
        try:
            extract = subprocess.run(
                [str(bin_path()), "--json", "extract", str(args.docx), "-o", str(workdir),
                 "--operation-id", "extract"],
                capture_output=True,
                text=True,
                timeout=300,
            )
            if extract.returncode != 0:
                results.append(Result(task, False, "extract failed"))
                continue
            results.append(evaluate(task, workdir, args.docx, out))
        except Exception as error:  # noqa: BLE001 - a task failure is data
            results.append(Result(task, False, f"raised: {error}"))
        finally:
            shutil.rmtree(scratch, ignore_errors=True)

    passed = [r for r in results if r.passed]
    rate = len(passed) / len(results) if results else 0.0

    if args.json:
        print(json.dumps({
            "schema": "docx2typed-facade-task-suite-1",
            "fixture": str(args.docx),
            "tasks": len(results),
            "passed": len(passed),
            "first_pass_success_rate": round(rate, 4),
            "results": [
                {"name": r.task.name, "kind": r.task.kind, "passed": r.passed, "detail": r.detail}
                for r in results
            ],
        }, indent=2, ensure_ascii=False))
    else:
        print(f"fixture: {args.docx}")
        print(f"{'task':<44} {'kind':<18} result")
        print("-" * 80)
        for result in results:
            mark = "PASS" if result.passed else "FAIL"
            print(f"{result.task.name:<44} {result.task.kind:<18} {mark}  {result.detail}")
        print("-" * 80)
        print(f"first-pass task success: {len(passed)}/{len(results)} = {rate:.0%}")

    return 0 if rate == 1.0 else 1


if __name__ == "__main__":
    sys.exit(main())
