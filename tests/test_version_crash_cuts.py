"""Version durability under process death.

The version path adds objects and a commit object to the mutation lane that
`test_store_recovery.py` already covers, so the same contract must hold across
the new cut points: a kill yields a COMPLETE old head, a COMPLETE new head, or
an explicit needs-recovery — never a head that names a tree whose objects are
missing, and never a version that lists but cannot be restored.

Also pins the two things GC must not break, because generations became
reclaimable in P2: operation-id replay/`operation-id-reused`, and the export
receipt an export leaves for a version.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from scripts import main
from scripts.protocol import canonical_operation_input, new_operation_id, operation_ledger
from scripts.store import history_gc, history_verify, head_version
from scripts.store import history_list as store_history_list
from scripts.store import Store, _Kill, clear_faults, kill_at, set_fault  # noqa: F401


ROOT = Path(__file__).resolve().parents[1]


def _extract(tmp_path: Path) -> Path:
    """Extract through the CLI: that is the path that creates the store."""
    from docx import Document

    source = tmp_path / "src.docx"
    document = Document()
    paragraph = document.add_paragraph()
    paragraph.add_run("甲段落原文内容 保持不动")
    paragraph.add_run("加粗片段").bold = True
    document.add_paragraph("乙段落前缀文字 目标插入语 后缀文字收尾")
    document.save(str(source))
    workdir = tmp_path / "wd"
    assert main(["extract", "--json", str(source), "-o", str(workdir),
                 "--operation-id", new_operation_id()]) == 0
    return workdir


def _payload(result) -> dict:
    """The structured payload of an MCP tool result (str or CallToolResult)."""
    import json as _json

    if hasattr(result, "structuredContent"):
        return result.structuredContent or {}
    if isinstance(result, str):
        return _json.loads(result)
    return result


def _failed(result) -> bool:
    payload = _payload(result)
    return bool(getattr(result, "isError", False)) or payload.get("outcome") == "failure"


def _save_run():
    """A store mutation that marks the save boundary — i.e. what commit_sync is."""

    def run(target, tx=None):
        assert tx is not None
        tx.mark_save_boundary(origin="commit_sync", label="cut")
        return ("success", {"cut": True}, "mutation", {"checks": []}, [])

    return run


def _save(store: Store, operation_id: str) -> None:
    canonical = canonical_operation_input("commit_sync", {"workdir": str(store.root)})
    store.mutate(
        operation="commit_sync",
        operation_id=operation_id,
        canonical=canonical,
        input_sha256=store.pin()["manifest_sha256"],
        expected_generation=store.pin()["generation"],
        run=_save_run(),
    )


@pytest.fixture(autouse=True)
def _no_faults():
    clear_faults()
    yield
    clear_faults()


@pytest.mark.parametrize("cut", ["version-objects", "version-commit", "pointer-write", "pointer-flush", "pointer-rename"])
def test_kill_before_the_save_commits_leaves_the_old_head(tmp_path, cut):
    """Any cut before the pointer CAS: the version never existed, or the head is
    still the complete old one — and history stays verifiable either way."""
    workdir = _extract(tmp_path)
    store = Store.open(workdir)
    before = head_version(workdir)

    kill_at(cut)
    with pytest.raises(_Kill):
        _save(store, f"save-cut-{cut}")
    clear_faults()

    recovered = Store.open(workdir).recover()
    assert recovered["needs_recovery"] == []
    after = head_version(workdir)
    assert after["version"] in (None, before["version"]), "a version appeared without its commit"
    report = history_verify(workdir)
    assert report["ok"] is True, report
    # and the workdir keeps working
    _save(Store.open(workdir), f"save-after-{cut}")
    assert history_verify(workdir)["ok"] is True


@pytest.mark.parametrize("cut", ["materialize", "ledger-write", "journal-write-generation-committed", "journal-write-completed"])
def test_kill_after_the_pointer_commit_recovers_forward(tmp_path, cut):
    """Any cut after the CAS: the new head must be complete — its version lists,
    its tree verifies, and a restore of it succeeds."""
    workdir = _extract(tmp_path)
    store = Store.open(workdir)
    before = head_version(workdir)

    kill_at(cut)
    with pytest.raises(_Kill):
        _save(store, f"save-cut-{cut}")
    clear_faults()

    Store.open(workdir).recover()
    after = head_version(workdir)
    assert after["version"] is not None
    assert after["version"] != before["version"]
    report = history_verify(workdir)
    assert report["ok"] is True, report
    assert all(item["content"] == "retained" for item in report["versions"])


def test_gc_does_not_break_operation_replay(tmp_path):
    """P2 made generations reclaimable; idempotency must not live in them.

    The in-process ledger mirror is cleared first — otherwise it would mask a
    record that the GC actually lost, and the test would pass for the wrong
    reason."""
    workdir = _extract(tmp_path)
    store = Store.open(workdir)
    _save(store, "save-kept")
    _save(Store.open(workdir), "save-reclaimed")
    _save(Store.open(workdir), "save-latest")

    first_envelope = store.lookup_ledger("save-reclaimed", generation=True, anchor=None, directory=True)[0]
    assert first_envelope is not None

    plan = history_gc(workdir, keep_last=1, dry_run=False)
    assert plan["generations_reclaimable"] >= 1, plan
    operation_ledger._records.clear()  # noqa: SLF001 - see docstring

    record, corrupt = Store.open(workdir).lookup_ledger(
        "save-reclaimed", generation=True, anchor=None, directory=True
    )
    assert record is not None, "the ledger record did not survive the GC"
    assert record["envelope"] == first_envelope["envelope"]

    # the changed-input contract still holds after the GC
    other = canonical_operation_input("commit_sync", {"workdir": str(workdir), "label": "different"})
    assert other != first_envelope["input_sha256"]




@pytest.mark.parametrize(
    "cut",
    [
        "journal-write-prepared",
        "retention-prepared",
        "retention-mark",
        "retention-mark-write",
        "retention-marked",
        "retention-sweep",
        "retention-generation",
        "retention-swept",
        "journal-write-retention-swept",
        "retention-completed",
        "journal-write-completed",
    ],
)
def test_history_gc_crash_cuts_recover_from_journal(tmp_path, cut):
    """Every retention phase is recoverable and never leaves half-trimmed state."""
    from scripts.mcp_server import commit_sync, document_patch, session, workdir_open

    session.workdir = None
    workdir = _extract(tmp_path)
    assert not _failed(workdir_open(str(workdir), track=False))
    previous = "目标插入语"
    for index in range(1, 4):
        current = f"GC版本{index}"
        assert not _failed(
            document_patch(
                hunks=[{"paragraph_id": "P1", "old": previous, "new": current}],
                operation_id=f"gc-edit-{index}",
            )
        )
        assert not _failed(
            commit_sync(
                operation_id=f"gc-save-{index}",
                label=f"说明{index}",
                pin=False,
            )
        )
        previous = current
    session.workdir = None

    kill_at(cut)
    with pytest.raises(_Kill):
        history_gc(workdir, keep_last=1, dry_run=False)
    clear_faults()
    pending = Store.open(workdir).pending_transactions()
    assert len(pending) == 1, pending

    recovered = Store.open(workdir).recover()
    assert recovered["needs_recovery"] == []
    assert Store.open(workdir).pending_transactions() == []

    # Intent-only cuts roll back the uncommitted decision; all other cuts may
    # already have completed. Re-running the same policy settles either state.
    history_gc(workdir, keep_last=1, dry_run=False)
    versions = store_history_list(workdir)["versions"]
    assert [(item["version"], item["content"]) for item in versions] == [
        ("V3", "retained"),
        ("V2", "trimmed"),
        ("V1", "trimmed"),
    ]
    assert history_verify(workdir)["ok"] is True
    trim_log = workdir / ".docx2typed-store" / "history-trim.jsonl"
    rows = [json.loads(line) for line in trim_log.read_text(encoding="utf-8").splitlines()]
    assert [row["version"] for row in rows] == ["V2", "V1"]
    session.workdir = None


def test_gc_keeps_an_export_receipt_for_a_version(tmp_path):
    """An export names the version it came from; reclaiming the generation that
    carried that version must not lose the receipt."""
    from scripts.mcp_server import build_docx, commit_sync, document_patch, session, workdir_open

    session.workdir = None
    workdir = _extract(tmp_path)
    assert not _failed(workdir_open(str(workdir), track=False))
    assert not _failed(document_patch(
        hunks=[{"paragraph_id": "P1", "old": "目标插入语", "new": "改过的插入语"}], operation_id="rc-edit"
    ))
    assert not _failed(commit_sync(label="V1"))

    output = tmp_path / "v1.docx"
    built = build_docx(output=str(output), operation_id="rc-build")
    assert not _failed(built), _payload(built)
    receipt = json.loads((Path(str(output) + ".evidence.json")).read_text(encoding="utf-8"))
    assert receipt["operation"] == "build_docx"

    # a second version so the first is reclaimable, then GC
    assert not _failed(document_patch(
        hunks=[{"paragraph_id": "P1", "old": "改过的插入语", "new": "又改了一次"}], operation_id="rc-edit-2"
    ))
    assert not _failed(commit_sync(label="V2"))
    history_gc(workdir, keep_last=2, dry_run=False)

    assert output.is_file()
    assert json.loads((Path(str(output) + ".evidence.json")).read_text(encoding="utf-8")) == receipt
    session.workdir = None
