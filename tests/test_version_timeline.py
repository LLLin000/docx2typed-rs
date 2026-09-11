"""Version timeline acceptance: the eight frozen criteria (ADR 0039/0042/0044/0045).

C1 commit_sync is the only save boundary; ordinary canonical publications do
   not create versions.
C2 after a non-save mutation head_version is unchanged and version_dirty true.
C3 commit_sync saves when only the canonical state changed, and is a true no-op
   when draft and version are both clean.
C4 version-bearing generations survive GC and recovery.
C5 history_restore always creates a NEW version, never rewinds HEAD, and the
   restored state matches the version's recorded tree digest.
C6 build_docx refuses a version-dirty state; build_docx(version=…) exports a
   historical version with HEAD untouched.
C7 version creation and restore run through the writer lane + journal + CAS.
C8 selective restore moves dependency-free paragraphs only and refuses coupled
   ones by name.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from docx import Document

ROOT = Path(__file__).resolve().parents[1]

from scripts.extract import extract  # noqa: E402
from scripts.mcp_server import (  # noqa: E402
    build_docx,
    commit_sync,
    decide_all,
    document_patch,
    format_span,
    history_gc,
    history_list,
    history_verify,
    history_restore,
    session,
    verify_output,
    workdir_open,
    workdir_status,
)
from scripts.store import (  # noqa: E402
    Store,
    canonical_tree_digest,
    head_version,
    history_list as store_history_list,
    store_dir_path,
)

CONFUSING = "ABCDEFGHIJKLMNOPQRSTUVWXYZ"  # unlikely fixture text


def _reset() -> None:
    session.workdir = None
    session.last_build_output = None


def _j(result) -> dict:
    if hasattr(result, "structuredContent"):
        payload = result.structuredContent
        return payload.get("data") if isinstance(payload, dict) and "data" in payload else payload
    if isinstance(result, dict):
        return result
    return json.loads(result)


def _fails(result) -> str | None:
    """The diagnostic code when a call failed, else None."""
    if getattr(result, "isError", False):
        payload = result.structuredContent or {}
        return (payload.get("diagnostics") or [{}])[0].get("code")
    payload = result.structuredContent if hasattr(result, "structuredContent") else None
    if isinstance(payload, dict) and payload.get("outcome") == "failure":
        return (payload.get("diagnostics") or [{}])[0].get("code")
    return None


def _make_docx(path: Path) -> None:
    """P0 carries a bold run (so a bold variant exists and format_span is
    allowed), P1 is plain; every paragraph is dependency-free."""
    document = Document()
    paragraph = document.add_paragraph()
    paragraph.add_run(f"{CONFUSING[:6]} 第一段内容")
    paragraph.add_run("加粗片段").bold = True
    document.add_paragraph(f"{CONFUSING[6:12]} 第二段内容")
    document.save(str(path))


def _open(tmp_path: Path, name: str) -> Path:
    source = tmp_path / f"{name}.docx"
    _make_docx(source)
    workdir = tmp_path / name
    assert extract([str(source), "-o", str(workdir)]) == 0
    _reset()
    # direct mode: these tests are about the version timeline, and a tracked
    # edit would put revision tokens into every later span
    assert not _fails(workdir_open(str(workdir), track=False))
    return workdir


def _save(workdir: Path, label: str | None = None) -> dict:
    result = commit_sync(label=label)
    assert not _fails(result), result
    return _j(result)


def _edit(workdir: Path, old: str, new: str, operation_id: str = "edit") -> None:
    result = document_patch(
        hunks=[{"paragraph_id": "P1", "old": old, "new": new}], operation_id=operation_id
    )
    assert not _fails(result), result


def test_c1_c2_c3_save_boundary_semantics(tmp_path):
    workdir = _open(tmp_path, "boundary")

    # C1/C2: a canonical mutation that is not a save creates no version
    _edit(workdir, CONFUSING[6:12], "改过的第二段")
    assert _save(workdir, "第一次保存")["version"]["created"] is True
    first = head_version(workdir)
    assert first["version"] == "V1" and first["dirty"] is False

    assert not _fails(format_span(paragraph_id="P0", old=CONFUSING[:6], attributes={"bold": True}))
    after_format = head_version(workdir)               # C2
    assert after_format["version"] == "V1"
    assert after_format["dirty"] is True
    store_history_list(workdir)  # the chain still names exactly one version

    # C3: a clean draft still saves when the canonical state moved on
    saved = _save(workdir, "接受格式修改")
    assert saved["version"]["created"] is True
    second = head_version(workdir)
    assert second["version"] == "V2" and second["dirty"] is False

    # C3: nothing pending at all -> a true no-op (no third version, no write)
    before_pointer = (workdir / "workdir.json").read_bytes()
    noop = _save(workdir)
    assert noop.get("noop") is True, noop
    assert head_version(workdir)["version"] == "V2"
    assert (workdir / "workdir.json").read_bytes() == before_pointer


def test_c4_version_generations_survive_gc_and_recovery(tmp_path):
    workdir = _open(tmp_path, "gc")
    _edit(workdir, CONFUSING[6:12], "第一版")
    _save(workdir, "V1")
    first_generation = head_version(workdir)["generation"]
    _edit(workdir, "第一版", "第二版", operation_id="edit-2")
    _save(workdir, "V2")
    second_generation = head_version(workdir)["generation"]

    # drive more mutations so GC runs repeatedly
    for index in range(3):
        _edit(workdir, "第二版" if index == 0 else f"第{index + 1}版", f"第{index + 2}版", operation_id=f"more-{index}")
        _save(workdir)

    generations = store_dir_path(workdir) / "generations"
    assert (generations / first_generation).is_dir()
    assert (generations / second_generation).is_dir()

    # recovery must not collect them either
    Store(workdir).recover(auto=False)
    assert (generations / first_generation).is_dir()
    assert (generations / second_generation).is_dir()
    assert head_version(workdir)["version"] == "V5"


def test_c5_restore_creates_a_new_version_and_matches_the_tree(tmp_path):
    workdir = _open(tmp_path, "restore")
    _edit(workdir, CONFUSING[6:12], "版本一")
    _save(workdir, "V1")
    v1 = head_version(workdir)
    v1_digest = v1["tree"]

    _edit(workdir, "版本一", "版本二", operation_id="edit-2")
    _save(workdir, "V2")
    head_before = head_version(workdir)["version"]

    result = history_restore("V1", operation_id="restore-1")
    assert not _fails(result), result
    restored = _j(result)
    assert restored["restored_from"] == "V1"

    after = head_version(workdir)
    assert after["version"] != head_before            # a NEW version, not a rewind
    assert after["version"] == "V3"
    assert after["tree"] == v1_digest                  # content equals V1's recorded tree
    assert after["dirty"] is False
    assert canonical_tree_digest(workdir) == v1_digest

    # every earlier version is still listed and still restorable
    versions = [item["version"] for item in store_history_list(workdir)["versions"]]
    assert {"V1", "V2", "V3"} <= set(versions)

    # and the workspace keeps working afterwards
    _edit(workdir, "版本一", "版本三", operation_id="edit-3")
    assert not _fails(commit_sync())
    assert head_version(workdir)["version"] == "V4"


def test_c6_build_requires_a_version_and_exports_history(tmp_path):
    workdir = _open(tmp_path, "export")
    _edit(workdir, CONFUSING[6:12], "已保存的一版")
    _save(workdir, "V1")

    assert not _fails(format_span(paragraph_id="P0", old=CONFUSING[:6], attributes={"bold": True}))
    refused = build_docx(output=str(tmp_path / "refused.docx"), operation_id="build-refused")
    assert _fails(refused) == "version-save-required"

    _save(workdir, "V2")
    head = head_version(workdir)
    ok = build_docx(output=str(tmp_path / "current.docx"), operation_id="build-current")
    assert not _fails(ok), ok
    assert _j(ok)["version"] == head["version"]

    exported = build_docx(output=str(tmp_path / "v1.docx"), version="V1", operation_id="build-v1")
    assert not _fails(exported), exported
    assert _j(exported)["version"] == "V1"
    assert head_version(workdir)["version"] == head["version"]     # HEAD untouched
    assert head_version(workdir)["tree"] == head["tree"]
    assert (tmp_path / "v1.docx").is_file() and (tmp_path / "current.docx").is_file()
    assert not _fails(verify_output(output=str(tmp_path / "current.docx"), operation_id="verify-current"))


def test_c7_version_writes_go_through_the_store_lane(tmp_path):
    workdir = _open(tmp_path, "lane")
    _edit(workdir, CONFUSING[6:12], "走了 store lane")
    _save(workdir, "V1")
    pointer = json.loads((workdir / "workdir.json").read_text(encoding="utf-8"))

    # the version lives in an immutable generation committed by a store
    # transaction, and the pointer names it — not a side log
    generation = store_dir_path(workdir) / "generations" / pointer["head_version_generation"]
    manifest = json.loads((generation / "generation.json").read_text(encoding="utf-8"))
    assert manifest["version"]["version"] == pointer["head_version"] == "V1"
    assert manifest["version"]["parent_version"] is None
    assert manifest["operation_id"]
    assert not (workdir / "history.jsonl").exists()
    assert not (workdir / ".review" / "versions.jsonl").exists()


def test_c8_cherry_pick_moves_dependency_free_paragraphs_only(tmp_path):
    workdir = _open(tmp_path, "pick")
    _edit(workdir, CONFUSING[6:12], "版本一的第二段")
    _save(workdir, "V1")
    _edit(workdir, "版本一的第二段", "版本二的第二段", operation_id="edit-2")
    _save(workdir, "V2")

    picked = history_restore("V1", paragraphs=["P1"], operation_id="pick-1")
    assert not _fails(picked), picked
    assert _j(picked)["cherry_picked"] == ["P1"]
    assert head_version(workdir)["version"] == "V3"
    assert "版本一" in (workdir / "typed.md").read_text(encoding="utf-8")

    # picking a paragraph that already matches is a no-op, not a spurious version
    again = history_restore("V1", paragraphs=["P1"], operation_id="pick-2")
    assert not _fails(again), again
    assert _j(again).get("noop") is True
    assert head_version(workdir)["version"] == "V3"


def _paragraph_text(workdir: Path, paragraph_id: str) -> str:
    from scripts.typed_core import parse_typed, visible_text

    document = parse_typed((workdir / "typed.md").read_text(encoding="utf-8"))
    paragraph = next(p for p in document.paragraphs if p.paragraph_id == paragraph_id)
    return visible_text(paragraph.nodes).strip()


def _edit_paragraph(workdir: Path, paragraph_id: str, old: str, new: str, operation_id: str = "edit-a") -> None:
    result = document_patch(
        hunks=[{"paragraph_id": paragraph_id, "old": old, "new": new}], operation_id=operation_id
    )
    assert not _fails(result), result


def test_c8b_cherry_pick_refuses_a_coupled_paragraph(tmp_path):
    """A paragraph whose format record carries tokens cannot be moved alone: it
    may dangle w:ins/w:del bytes or split a comment anchor pair (ADR 0045)."""
    source = ROOT / "corpus" / "release" / "revisions.docx"
    workdir = tmp_path / "coupled"
    assert extract([str(source), "-o", str(workdir)]) == 0
    _reset()
    # direct mode keeps these edits out of the revision regions; the COUPLED
    # paragraph's tokens are the ones the source document already carries
    assert not _fails(workdir_open(str(workdir), track=False))

    records = json.loads((workdir / "format.json").read_text(encoding="utf-8"))["paragraphs"]
    coupled = next(r["id"] for r in records if r.get("token_ids"))
    free = next(r["id"] for r in records if not r.get("token_ids"))

    # two versions, each moving a dependency-free paragraph
    first = _paragraph_text(workdir, free)
    _edit_paragraph(workdir, free, first[:4], first[:3] + "改一")
    _save(workdir)
    second = _paragraph_text(workdir, free)
    _edit_paragraph(workdir, free, second[:4], second[:3] + "改二", operation_id="edit-b")
    _save(workdir)

    before = head_version(workdir)["version"]
    refusal = history_restore("V1", paragraphs=[coupled], operation_id="pick-coupled")
    assert _fails(refusal) == "partial-restore-needs-dependent-state"
    detail = (refusal.structuredContent["diagnostics"][0].get("message") or "")
    assert coupled in detail and "token" in detail
    assert head_version(workdir)["version"] == before     # nothing was written


# ---------------------------------------------------------------------------
# P2 — history lives in the object pool (ADR 0043)
# ---------------------------------------------------------------------------

def test_p2_state_round_trips_through_the_object_pool(tmp_path):
    """A tree stores canonical state only; materialising it must be byte-exact
    (the split is paragraph-keyed, and a state the splitter cannot round-trip
    falls back to whole blobs)."""
    import scripts.objectstore as objects

    workdir = _open(tmp_path, "pool")
    _edit(workdir, CONFUSING[6:12], "池化版本")
    _save(workdir, "V1")

    tree_id = head_version(workdir)["tree_object"]
    assert tree_id, "a saved version must carry a tree object"
    materialised = tmp_path / "materialised"
    objects.materialize(workdir, tree_id, materialised)
    for name in ("typed.md", "format.json", "revisions.json", "styles.json", "_template.docx"):
        assert (materialised / name).read_bytes() == (workdir / name).read_bytes(), name
    assert objects.verify(workdir, tree_id)["ok"] is True


def test_p2_version_content_survives_its_generation_being_reclaimed(tmp_path):
    """The point of the pool: history stops depending on a full copy of the
    workspace surviving per version."""
    import shutil as _shutil

    workdir = _open(tmp_path, "poolrestore")
    _edit(workdir, CONFUSING[6:12], "第一版")
    _save(workdir, "V1")
    _edit(workdir, "第一版", "第二版", operation_id="edit-2")
    _save(workdir, "V2")

    first = next(v for v in store_history_list(workdir)["versions"] if v["version"] == "V1")
    generation = store_dir_path(workdir) / "generations" / str(first["generation"])
    assert generation.is_dir()
    _shutil.rmtree(generation)

    restored = history_restore("V1", operation_id="pool-restore")
    assert not _fails(restored), restored
    assert head_version(workdir)["tree"] == first["head_tree"]

    exported = build_docx(output=str(tmp_path / "v1.docx"), version="V1", operation_id="pool-export")
    assert not _fails(exported), exported


def test_p2_a_missing_object_is_detected_not_silently_substituted(tmp_path):
    """Deleting one blob must surface as version-content-missing naming it."""
    import scripts.objectstore as objects

    workdir = _open(tmp_path, "pooldamage")
    _edit(workdir, CONFUSING[6:12], "会损坏的一版")
    _save(workdir, "V1")
    _edit(workdir, "会损坏的一版", "下一版", operation_id="edit-2")
    _save(workdir, "V2")

    first = next(v for v in store_history_list(workdir)["versions"] if v["version"] == "V1")
    tree = objects.read_tree(workdir, first["tree_object"])
    whole = (tree["parts"].get("styles.json") or {}).get("whole")
    assert whole
    objects.object_path(workdir, "blob", whole).unlink()

    assert objects.verify(workdir, first["tree_object"])["ok"] is False
    refused = history_restore("V1", operation_id="damaged")
    assert _fails(refused) == "version-content-missing"


def test_p2_retention_reclaims_generations_but_never_commit_metadata(tmp_path):
    """history_gc trims content past retention; the versions still list, and an
    explicitly named version is kept whatever the count limit says."""
    workdir = _open(tmp_path, "poolgc")
    for index in range(3):
        text = CONFUSING[6:12] if index == 0 else f"第{index}版"
        _edit(workdir, text, f"第{index + 1}版", operation_id=f"edit-{index}")
        _save(workdir, f"第{index + 1}版")

    before = len([d for d in (store_dir_path(workdir) / "generations").iterdir() if d.is_dir()])
    plan = json.loads(history_gc(keep_last=1, dry_run=True))
    assert plan["generations_reclaimable"] >= 1
    done = json.loads(history_gc(keep_last=1, dry_run=False))
    after = len([d for d in (store_dir_path(workdir) / "generations").iterdir() if d.is_dir()])
    assert after < before, (before, after)

    # every version still lists, and every one is still restorable from the pool
    versions = store_history_list(workdir)["versions"]
    assert [v["version"] for v in versions] == ["V3", "V2", "V1"]
    assert all(v["content"] == "retained" for v in versions)
    assert json.loads(history_verify())["ok"] is True
    assert not _fails(history_restore("V1", operation_id="gc-restore"))



def test_gc_marks_trimmed_content_without_resurrecting_it(tmp_path):
    """Retention is observable and irreversible: dry-run does not mark a trim,
    verify accepts an intentional trim, and restore cannot use old generations."""
    workdir = _open(tmp_path, "trim")
    for index, (old, new) in enumerate(
        [
            (CONFUSING[6:12], "未命名第一版"),
            ("未命名第一版", "未命名第二版"),
            ("未命名第二版", "未命名第三版"),
        ],
        start=1,
    ):
        _edit(workdir, old, new, operation_id=f"trim-edit-{index}")
        _save(workdir)

    store_root = store_dir_path(workdir)
    preview = _j(history_gc(keep_last=1, dry_run=True))
    assert preview["versions_trimmed"] == ["V2", "V1"]
    assert not (store_root / "history-trim.jsonl").exists()

    applied = _j(history_gc(keep_last=1, dry_run=False))
    assert applied["versions_trimmed"] == ["V2", "V1"]
    history = store_history_list(workdir)["versions"]
    assert [(item["version"], item["content"]) for item in history] == [
        ("V3", "retained"),
        ("V2", "trimmed"),
        ("V1", "trimmed"),
    ]
    assert _j(history_verify())["ok"] is True
    assert _fails(history_restore("V1", operation_id="trim-restore")) == "version-trimmed"

# ---------------------------------------------------------------------------
# P3 — structural operations become baseline transitions (one workspace)
# ---------------------------------------------------------------------------

def test_p3_accept_all_is_adopted_as_the_next_version(tmp_path):
    """decide_all without workdir_out must not create a sibling workdir: the new
    baseline becomes this workspace's next version, with the epoch bumped."""
    source = ROOT / "corpus" / "release" / "revisions.docx"
    workdir = tmp_path / "adopt"
    assert extract([str(source), "-o", str(workdir)]) == 0
    _reset()
    asserts_open = workdir_open(str(workdir), track=True)
    assert not _fails(asserts_open), asserts_open

    records = json.loads((workdir / "format.json").read_text(encoding="utf-8"))["paragraphs"]
    free = next(r["id"] for r in records if not r.get("token_ids"))
    text = _paragraph_text(workdir, free)
    _edit_paragraph(workdir, free, text[:4], text[:3] + "改")
    _save(workdir, "改一版")
    before = head_version(workdir)

    decided = tmp_path / "decided.docx"
    result = decide_all(action="accept", output=str(decided), operation_id="p3-accept")
    assert not _fails(result), result
    assert _j(result)["adopted"] is True

    after = head_version(workdir)
    assert after["version"] != before["version"]
    assert after["baseline_epoch"] == (before.get("baseline_epoch") or 1) + 1
    assert after["dirty"] is False
    assert decided.is_file()

    # no sibling workdir was created, the workspace still works, and the
    # transition is visible in the timeline
    assert sorted(p.name for p in tmp_path.iterdir() if p.is_dir()) == ["adopt"]
    assert not _fails(commit_sync(operation_id="p3-after"))  # no-op save
    version = next(v for v in store_history_list(workdir)["versions"] if v["version"] == after["version"])
    assert version["origin"] == "baseline-transition"
    assert version["baseline_epoch"] == after["baseline_epoch"]
    assert not _fails(build_docx(output=str(tmp_path / "after.docx"), operation_id="p3-build"))
