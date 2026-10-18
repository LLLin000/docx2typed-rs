"""Focused tests for qualification/real_docx_corpus.py.

Uses tiny synthetic DOCX packets and the test-only ``expected_count``
override; production always enforces exactly 217. No real corpus bytes,
no private paths, no Rust changes.
"""
from __future__ import annotations

import hashlib
import importlib.util
import json
import random
import shutil
import sys
import zipfile
from pathlib import Path
from zipfile import ZipInfo

import pytest

ROOT = Path(__file__).resolve().parents[1]
MOD_PATH = ROOT / "qualification" / "real_docx_corpus.py"


def _load_module():
    spec = importlib.util.spec_from_file_location("real_docx_corpus", MOD_PATH)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules["real_docx_corpus"] = module
    spec.loader.exec_module(module)
    return module


rdc = _load_module()

DOC_SHELL = (
    '<?xml version="1.0" encoding="UTF-8"?>'
    '<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"'
    ' xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math">'
    "<w:body>{body}</w:body></w:document>"
)
TYPES = (
    '<?xml version="1.0" encoding="UTF-8"?>'
    '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
    '<Default Extension="rels" ContentType="application/vnd.openxmlformats-'
    'package.relationships+xml"/>'
    '<Default Extension="xml" ContentType="application/xml"/>'
    "</Types>"
)
RELS = (
    '<?xml version="1.0" encoding="UTF-8"?>'
    '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
    '<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/'
    '2006/relationships/officeDocument" Target="word/document.xml"/>'
    "</Relationships>"
)
PLAIN_BODY = "<w:p><w:r><w:t>same visible text</w:t></w:r></w:p>"
RICH_BODY = (
    "<w:p><w:r><w:t>hi</w:t></w:r></w:p>"
    "<w:tbl><w:tr><w:tc><w:p><w:r><w:t>c</w:t></w:r></w:p></w:tc></w:tr></w:tbl>"
    '<w:p><w:ins w:author="t" w:date="2026-01-01T00:00:00Z" w:id="1">'
    "<w:r><w:t>x</w:t></w:r></w:ins></w:p>"
    "<w:sdt><w:sdtPr><w:alias w:val=\"a\"/></w:sdtPr>"
    "<w:sdtContent><w:p><w:r><w:t>s</w:t></w:r></w:p></w:sdtContent></w:sdt>"
    '<w:p><w:r><w:fldChar w:fldCharType="begin"/></w:r></w:p>'
    '<w:fldSimple w:instr=" PAGE "><w:r><w:t>1</w:t></w:r></w:fldSimple>'
    "<w:p><m:oMathPara><m:oMath><m:r><m:t>y</m:t></m:r></m:oMath></m:oMathPara></w:p>"
)


def _write_docx(path: Path, body: str, extra: dict[str, bytes] | None = None) -> None:
    members = {
        "[Content_Types].xml": TYPES.encode(),
        "_rels/.rels": RELS.encode(),
        "word/document.xml": DOC_SHELL.format(body=body).encode(),
    }
    members.update(extra or {})
    with zipfile.ZipFile(path, "w", zipfile.ZIP_STORED) as archive:
        for name, data in members.items():
            archive.writestr(ZipInfo(name, date_time=(2026, 1, 1, 0, 0, 0)), data)


def _sha_size(path: Path) -> tuple[str, int]:
    return hashlib.sha256(path.read_bytes()).hexdigest(), path.stat().st_size


def _write_inventory(path: Path, files: list[Path], shuffle: bool = False) -> Path:
    entries = []
    for i, src in enumerate(files, 1):
        sha, size = _sha_size(src)
        entries.append(
            {"id": f"doc-{i:04d}", "sha256": sha, "bytes": size, "path": str(src)}
        )
    if shuffle:
        random.Random(42).shuffle(entries)
    path.write_text(json.dumps({"documents": entries}), encoding="utf-8")
    return path


def _freeze(src_dir: Path, n: int | None = None, **kw):
    files = sorted(src_dir.glob("*.docx"))
    inv = _write_inventory(
        src_dir / "inventory.json", files, shuffle=kw.pop("shuffle", False)
    )
    out = src_dir / kw.pop("out", "archive") / "manifest.json"
    count = len(files) if n is None else n
    return rdc.freeze_inventory(inv, out, count), out, inv


def test_freeze_repeat_equal(tmp_path):
    src = tmp_path / "src"
    src.mkdir()
    _write_docx(src / "a.docx", PLAIN_BODY)
    _write_docx(src / "b.docx", RICH_BODY, {"word/media/i.png": b"\x89PNG"})
    manifest, out, _ = _freeze(src)
    first = out.read_bytes()
    manifest2, _, _ = _freeze(src)
    assert out.read_bytes() == first
    assert manifest2 == manifest


def test_shuffled_snapshot_ids_unchanged(tmp_path):
    src = tmp_path / "src"
    src.mkdir()
    for name in ("a.docx", "b.docx", "c.docx"):
        _write_docx(src / name, PLAIN_BODY + f"<!--{name}-->")
    manifest_a, _, _ = _freeze(src)
    manifest_b, _, _ = _freeze(src, shuffle=True, out="archive-shuffled")
    assert [d["document_id"] for d in manifest_a["documents"]] == [
        d["document_id"] for d in manifest_b["documents"]
    ]
    assert all(d["document_id"] == f"doc-{d['sha256']}" for d in manifest_a["documents"])


def test_same_text_different_bytes_retained(tmp_path):
    src = tmp_path / "src"
    src.mkdir()
    _write_docx(src / "v1.docx", PLAIN_BODY + "<!--v1-->")
    _write_docx(src / "v2.docx", PLAIN_BODY + "<!--v2-->")
    manifest, _, _ = _freeze(src)
    assert len(manifest["documents"]) == 2
    assert manifest["documents"][0]["sha256"] != manifest["documents"][1]["sha256"]


def test_feature_counts_deterministic(tmp_path):
    src = tmp_path / "src"
    src.mkdir()
    _write_docx(src / "r.docx", RICH_BODY, {"word/media/i.png": b"\x89PNG"})
    manifest, _, _ = _freeze(src)
    features = manifest["documents"][0]["features"]
    assert features == {
        "tables": 1,
        "revisions": 1,
        "sdt": 1,
        "opaque_candidates": 3,
        "media": 1,
        "parse_error": None,
    }


def test_missing_original_rejected(tmp_path):
    src = tmp_path / "src"
    src.mkdir()
    _write_docx(src / "a.docx", PLAIN_BODY)
    sha, size = _sha_size(src / "a.docx")
    inv = src / "inv.json"
    inv.write_text(
        json.dumps(
            {
                "documents": [
                    {"id": "doc-0001", "sha256": sha, "bytes": size,
                     "path": str(src / "a.docx")},
                    {"id": "doc-0002", "sha256": "0" * 64, "bytes": 10,
                     "path": str(src / "gone.docx")},
                ]
            }
        ),
        encoding="utf-8",
    )
    with pytest.raises(ValueError, match="missing"):
        rdc.freeze_inventory(inv, tmp_path / "m.json", 2)
    rc = rdc.main(
        ["freeze", "--inventory", str(inv), "--out", str(tmp_path / "m.json"),
         "--expected-count", "2"]
    )
    assert rc == 1

def test_changed_bytes_rejected(tmp_path):
    src = tmp_path / "src"
    src.mkdir()
    target = src / "a.docx"
    _write_docx(target, PLAIN_BODY)
    inv = _write_inventory(src / "inv.json", [target])
    with open(target, "ab") as handle:
        handle.write(b"tamper")
    with pytest.raises(ValueError, match="drift"):
        rdc.freeze_inventory(inv, tmp_path / "m.json", 1)


def test_count_pin_enforced(tmp_path):
    src = tmp_path / "src"
    src.mkdir()
    _write_docx(src / "a.docx", PLAIN_BODY)
    inv = _write_inventory(src / "inv.json", [src / "a.docx"])
    with pytest.raises(ValueError, match="exactly 217"):
        rdc.freeze_inventory(inv, tmp_path / "m.json")  # production default


def test_manifest_overwrite_refused(tmp_path):
    src = tmp_path / "src"
    src.mkdir()
    _write_docx(src / "a.docx", PLAIN_BODY)
    _, out, _ = _freeze(src)
    out.write_bytes(b'{"tampered": true}')
    with pytest.raises(ValueError, match="[Oo]verwrite|different frozen"):
        _freeze(src)


def test_traversal_refused(tmp_path):
    src = tmp_path / "src"
    src.mkdir()
    _write_docx(src / "a.docx", PLAIN_BODY)
    manifest, out, _ = _freeze(src)
    evil = dict(manifest)
    docs = [dict(d) for d in manifest["documents"]]
    docs[0]["source_path"] = "../evil.docx"
    evil["documents"] = docs
    out.write_bytes(
        (json.dumps(evil, sort_keys=True, ensure_ascii=False, indent=2) + "\n").encode()
    )
    with pytest.raises(ValueError, match="[Tt]ravers"):
        rdc.resolve_source(out, docs[0])
    assert rdc.main(["verify", "--manifest", str(out), "--expected-count", "1"]) == 1


def test_verify_moved_root_and_no_sources_needed(tmp_path):
    src = tmp_path / "src"
    src.mkdir()
    _write_docx(src / "a.docx", PLAIN_BODY)
    _write_docx(src / "b.docx", RICH_BODY)
    _, out, _ = _freeze(src)
    moved = tmp_path / "moved"
    shutil.move(str(out.parent), str(moved))
    moved_manifest = moved / "manifest.json"
    assert rdc.main(
        ["verify", "--manifest", str(moved_manifest), "--root", str(moved),
         "--expected-count", "2"]
    ) == 0
    shutil.rmtree(src)  # sources gone; archive alone must verify
    assert rdc.main(
        ["verify", "--manifest", str(moved_manifest), "--expected-count", "2"]
    ) == 0


def test_archive_mutation_and_size_drift_fail(tmp_path):
    src = tmp_path / "src"
    src.mkdir()
    _write_docx(src / "a.docx", PLAIN_BODY)
    manifest, out, _ = _freeze(src)
    archived = out.parent / manifest["documents"][0]["source_path"]
    with open(archived, "r+b") as handle:
        handle.seek(60)
        handle.write(b"X")
    assert rdc.main(["verify", "--manifest", str(out), "--expected-count", "1"]) == 1
    result = rdc.validate_manifest(out)
    assert result["ok"] is False
    assert result["document_count"] == 1
    assert result["manifest_sha256"] == hashlib.sha256(out.read_bytes()).hexdigest()
    assert any("drift" in p for p in result["problems"])


def test_schema_and_count_drift_fail(tmp_path):
    src = tmp_path / "src"
    src.mkdir()
    _write_docx(src / "a.docx", PLAIN_BODY)
    _, out, _ = _freeze(src)
    data = json.loads(out.read_text(encoding="utf-8"))
    data["document_count"] = 999
    out.write_text(json.dumps(data, sort_keys=True, indent=2) + "\n", encoding="utf-8")
    assert rdc.main(["verify", "--manifest", str(out), "--expected-count", "1"]) == 1
    data["schema"] = "something-else-1"
    out.write_text(json.dumps(data, sort_keys=True, indent=2) + "\n", encoding="utf-8")
    assert rdc.main(["verify", "--manifest", str(out), "--expected-count", "1"]) == 1


def test_verify_missing_manifest_nonzero(tmp_path):
    assert (
        rdc.main(
            ["verify", "--manifest", str(tmp_path / "nope.json"), "--expected-count", "1"]
        )
        == 1
    )
