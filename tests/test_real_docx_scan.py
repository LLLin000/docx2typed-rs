"""Focused unit checks for qualification/real_docx_scan.py (no binary, no I/O scan).

Covers: unique-target repeated-occurrence wrong-target oracle rejection,
opaque/non-target byte drift, correct unmodified packet acceptance,
no-match-not-EXPECTED_REJECT classification, artifact/metadata resume
mismatch, cleanup boundary.
"""
from __future__ import annotations

import json
import sys
import zipfile
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from qualification.real_docx_scan import (  # noqa: E402
    CONTRACT_FAILURE,
    EXPECTED_REJECT,
    NOT_APPLICABLE,
    PASS,
    _collect_single,
    _ensure_scratch,
    _hash_map_digest,
    _is_transport_error,
    _locate_paragraphs,
    _scan_tags,
    _sha256_file,
    check_resume_binding,
    refusal_justified,
    replacement_for,
    resolve_source,
    safe_remove_tree,
    select_target,
    select_unsupported,
    summarize,
    verify_edit,
)


NS = "http://schemas.openxmlformats.org/wordprocessingml/2006/main"


def _para(*texts: str, extra: str = "") -> str:
    runs = "".join(f'<w:r><w:rPr/><w:t>{t}</w:t></w:r>' for t in texts)
    return f"<w:p><w:pPr/>{runs}{extra}</w:p>"


def _doc(*paras: str) -> bytes:
    body = "".join(paras)
    return (f'<w:document xmlns:w="{NS}"><w:body>{body}'
            f"</w:body></w:document>").encode()


def _unique_doc() -> tuple[bytes, list[str]]:
    doc = _doc(_para("abcdefgh", "second run text here"))
    return doc, []


def test_case_variant_rejected_as_target():
    # document_find is case-INSENSITIVE (no case_sensitive param): exact-case
    # unique "Transmission" with "transmission" elsewhere is a 2-occurrence
    # engine query. Source selection must refuse it -- no false CONTRACT_FAILURE
    # from legitimate case variants. Expected query stays the exact token.
    doc = _doc(_para("Transmission"), _para("transmission"))
    target, reason = select_target(doc, [])
    assert target is None
    assert reason == "no safe eligible test target"


def test_casefold_cross_part_rejected():
    doc = _doc(_para("Transmission goes here"))
    target, reason = select_target(doc, ["prefix transmission suffix"])
    assert target is None
    assert reason == "no safe eligible test target"

def test_wrong_occurrence_does_not_verify():
    src, _ = _unique_doc()
    target, _ = select_target(src, [])
    assert target is not None
    expected = target["_expected"]
    query, replacement = target["_query"], target["_replacement"]
    qb, rb = query.encode(), replacement.encode()
    # Simulate an engine writing the replacement at the wrong byte: restore
    # the source bytes at the mapped range, plant the replacement at the tail.
    # Output then carries both query and replacement yet differs from the
    # independently expected member -- must NOT verify.
    wrong = bytearray(expected)
    start = target["query_start"]
    wrong[start:start + len(qb)] = qb
    wrong[-len(rb):] = rb
    src_map = {"word/document.xml": "s", "[Content_Types].xml": "c"}
    out_map = dict(src_map)
    check = verify_edit(src, bytes(wrong), expected, src_map, out_map, query, replacement)
    assert check["verdict"] == "mismatch"


def test_correct_unmodified_packet_accepted():
    src, _ = _unique_doc()
    target, _ = select_target(src, [])
    assert target is not None
    src_map = {"word/document.xml": "s", "[Content_Types].xml": "c"}
    check = verify_edit(src, target["_expected"], target["_expected"], src_map, dict(src_map),
                        target["_query"], target["_replacement"])
    assert check["verdict"] == "exact"


def test_opaque_and_non_target_drift_rejected():
    src, _ = _unique_doc()
    target, _ = select_target(src, [])
    assert target is not None
    expected = bytearray(target["_expected"])
    # Corrupt the tail (outside the intended hunk) by one byte.
    expected[-20] ^= 0x01
    src_map = {"word/document.xml": "s"}
    check = verify_edit(src, bytes(expected), target["_expected"], src_map, dict(src_map),
                        target["_query"], target["_replacement"])
    assert check["verdict"] == "mismatch"
    assert check["reason"]


def test_added_member_rejected():
    src, _ = _unique_doc()
    target, _ = select_target(src, [])
    assert target is not None
    src_map = {"word/document.xml": "s"}
    out_map = {"word/document.xml": "s", "word/extra.xml": "x"}
    check = verify_edit(src, target["_expected"], target["_expected"], src_map, out_map,
                        target["_query"], target["_replacement"])
    assert check["verdict"] == "mismatch"
    assert "member set" in check["reason"]


def test_paragraph_identity_matches_engine_direct_ids():
    # Direct body paragraphs keep P{n} across a table: the second direct
    # paragraph after a table still carries the engine's P1 identity and the
    # target pins that exact id.
    tbl = ('<w:tbl><w:tr><w:tc><w:p><w:r><w:t>celltext</w:t></w:r></w:p>'
           '</w:tc></w:tr></w:tbl>')
    doc = _doc(_para("plain intro text here"), tbl, _para("mappedquery unique text here"))
    target, _ = select_target(doc, [])
    assert target is not None
    assert target["paragraph_id"] == "P1"
    assert target["paragraph_ordinal"] == 1


def test_table_paragraph_never_claimed_safe():
    tbl = ('<w:tbl><w:tr><w:tc><w:p><w:r><w:t>tableonlyquery text</w:t></w:r></w:p>'
           '</w:tc></w:tr></w:tbl>')
    doc = _doc(tbl)
    target, reason = select_target(doc, [])
    assert target is None
    assert reason == "no safe eligible test target"


def test_tab_is_not_text_token():
    doc = (_doc('<w:p><w:pPr/><w:r><w:t>abcdefgh</w:t><w:tab/></w:r></w:p>'))
    located, why = _locate_paragraphs(doc)
    assert located is not None and len(located) == 1, why
    names = [t["name"] for t in _scan_tags(doc)]
    assert "tab" in names and names.count("t") == 2  # open+close only
    target, _ = select_target(doc, [])
    assert target is not None


def test_math_t_alongside_w_t_maps_valid_target():
    # Genuine consumer regression (doc626): 20 math-namespace m:t elements
    # alongside w:t must not break the oracle. Expat resolves the actual
    # namespace binding, so m:t never pairs as w:t and the w:t target maps.
    from qualification.real_docx_scan import _wt_ranges_expat, _all_wt, _parse_xml
    math_ns = "http://schemas.openxmlformats.org/officeDocument/2006/math"
    paras = "".join(
        f'<w:p><w:pPr/><w:r><w:t>uniqueword{i} here</w:t></w:r></w:p>' for i in range(3))
    math = (f'<w:p><w:pPr/><m:oMathPara xmlns:m="{math_ns}"><m:oMath><m:r>'
            f'<m:t>V</m:t></m:r></m:oMath></m:oMathPara></w:p>')
    doc = _doc(paras + math)
    root = _parse_xml(doc)
    assert root is not None
    wt_all = _all_wt(root)
    ranges = _wt_ranges_expat(doc, len(wt_all))
    assert ranges is not None and len(ranges) == len(wt_all)
    target, reason = select_target(doc, [])
    assert target is not None, reason
    assert doc[target["query_start"]:target["query_end"]] == target["_query"].encode()


def test_probe_snapshot_allows_harness_output_but_detects_mutation(tmp_path: Path):
    # Probe cleanliness: pre-probe snapshot allows harness-own pristine
    # outputs at stage root, but a mutated staged source or a new file
    # inside the probe window still fails closed.
    import hashlib
    from qualification.real_docx_scan import _only_expected_creation, _tree_snapshot
    stage = tmp_path / "stage"
    stage.mkdir()
    (stage / "pristine.docx").write_bytes(b"src")
    (stage / "pristine-out.docx").write_bytes(b"out")  # harness-own output
    probe_tree = _tree_snapshot(stage)
    assert _only_expected_creation(probe_tree, _tree_snapshot(stage)) is True
    (stage / "pristine.docx").write_bytes(b"mutated")
    assert _sha256_file(stage / "pristine.docx") != hashlib.sha256(b"src").hexdigest()
    (stage / "pristine.docx").write_bytes(b"src")
    (stage / "unexpected.docx").write_bytes(b"new")  # genuine probe-window write
    assert _only_expected_creation(probe_tree, _tree_snapshot(stage)) is False

def test_no_match_is_not_expected_reject():
    # Zero find occurrences for a source-unique query is an engine/state
    # mismatch (CONTRACT_FAILURE territory), never EXPECTED_REJECT; the
    # no-match path must be provable without a claimed refusal. We exercise
    # the collector's zero-count branch directly (counts-level shape).
    envelope = {"outcome": "success", "data": {"matches": [],
                                               "counts": {"occurrences": 0, "offset": 0,
                                                          "returned_blocks": 0, "total_matches": 0,
                                                          "complete": True},
                                               "next": None}}
    handle, found, _, transport = _collect_single(
        envelope, Path("x"), Path("y"), "abcdefgh", 5,
        lambda calls, tag: (_ for _ in ()).throw(AssertionError()), {"calls": []})
    assert handle is None and found["occurrences"] == 0 and transport is False


def test_paging_contract_complete_singleton_no_second_page():
    # Regression for the real harness defect: complete lives at counts level
    # with next null on a singleton page -- the collector must NOT issue
    # find-1. Any second call is a harness bug (query loss on phantom page).
    envelope = {"outcome": "success", "data": {"matches": [
        {"paragraph_id": "P0", "occurrences": [{"handle": "h1",
                                                "verdict": {"editable": True}}]}],
        "counts": {"occurrences": 1, "offset": 0, "returned_blocks": 1,
                   "total_matches": 1, "complete": True},
        "next": None}}
    def _boom(calls, tag):
        raise AssertionError(f"phantom page issued: {tag} {calls}")
    handle, found, pages, transport = _collect_single(
        envelope, Path("x"), Path("y"), "abcdefgh", 5, _boom, {"calls": []})
    assert (handle, found["occurrences"], pages, transport) == ("h1", 1, 1, False)


def test_paging_follows_next_call_cursor_with_dedupe():
    # Multi-page contract: page 1 incomplete with next.call offset; page 2
    # repeats one known handle plus the new span. Dedupe keeps EXACT1 at 2,
    # and the cursor comes from next.call, not top-level fields.
    first = {"outcome": "success", "data": {"matches": [
        {"paragraph_id": "P0", "occurrences": [{"handle": "h1",
                                                "verdict": {"editable": True}}]}],
        "counts": {"occurrences": 1, "offset": 0, "returned_blocks": 1,
                   "total_matches": 2, "complete": False},
        "next": {"call": {"query": "abcdefgh", "offset": 1}}}}
    second = {"outcome": "success", "data": {"matches": [
        {"paragraph_id": "P0", "occurrences": [{"handle": "h1",
                                                "verdict": {"editable": True}}]},
        {"paragraph_id": "P1", "occurrences": [{"handle": "h2",
                                                "verdict": {"editable": True}}]}],
        "counts": {"occurrences": 2, "offset": 1, "returned_blocks": 1,
                   "total_matches": 2, "complete": True},
        "next": None}}
    seen_args: list = []
    def _page(calls, tag):
        seen_args.append((tag, calls[1][1]))
        return [{"outcome": "success"}, second]
    handle, found, pages, transport = _collect_single(
        first, Path("x"), Path("y"), "abcdefgh", 5, _page, {"calls": []})
    assert transport is False and found["occurrences"] == 2
    assert seen_args and seen_args[0][1] == {"query": "abcdefgh", "limit": 200,
                                             "offset": 1}
    assert handle is None  # two unique spans: no EXACT1 claim, no query loss


def test_sound_refusal_needs_identity_and_matching_reason():
    doc, _ = _unique_doc()
    probe, _ = select_unsupported(doc)
    assert probe is None  # clean paragraph: no justified refusal target
    flagged = _doc(_para("locked text here", extra='<w:sdt><w:sdtPr/></w:sdt>'))
    probe2, _ = select_unsupported(flagged)
    assert probe2 is not None and probe2["flags"]["has_sdt"]
    good = refusal_justified(probe2, probe2["paragraph_id"],
                             "opaque-paragraph-mutated" if probe2["flags"].get("has_opaque")
                             else "prose-edit-unsupported")
    assert good is True
    # Wrong paragraph identity or a code the structure cannot produce: no claim.
    assert refusal_justified(probe2, "P999", "opaque-paragraph-mutated") is False
    assert refusal_justified(probe2, probe2["paragraph_id"], "text-ambiguous") is False
    assert refusal_justified({}, probe2["paragraph_id"], "opaque-paragraph-mutated") is False


def test_transport_error_gate():
    assert _is_transport_error({"outcome": "transport-error", "diagnostics": []}) is True
    assert _is_transport_error({"outcome": "success",
                                "diagnostics": [{"code": "missing-reply"}]}) is True
    assert _is_transport_error({"outcome": "success", "diagnostics": []}) is False
    assert _is_transport_error({"outcome": "success",
                                "diagnostics": [{"code": "opaque-paragraph-mutated"}]}) is False


def test_replacement_changes_one_equal_byte_char():
    query = "abcdefgh"
    out = replacement_for(query)
    assert len(out.encode()) == len(query.encode())
    diffs = sum(1 for a, b in zip(query, out) if a != b)
    assert diffs == 1
    assert "abcd" not in out or diffs == 1


def test_resume_binding_mismatch_refused():
    current = {"bin_sha256": "b", "manifest_sha256": "m", "rules_version": "r4",
               "scanner_sha256": "s", "engine_metadata_sha256": "e"}
    for key in current:
        with pytest.raises(ValueError):
            check_resume_binding({**current, key: "other"}, current)
    check_resume_binding(dict(current), current)  # no raise


def test_cleanup_boundary(tmp_path: Path):
    scratch = _ensure_scratch(tmp_path / "scratch")
    inside = scratch / "doc-1"
    inside.mkdir()
    safe_remove_tree(inside, scratch)
    assert not inside.exists()
    outside = tmp_path / "outside"
    outside.mkdir()
    with pytest.raises(ValueError):
        safe_remove_tree(outside, scratch)
    with pytest.raises(ValueError):
        safe_remove_tree(scratch, scratch)


def test_resolve_source_rejects_traversal(tmp_path: Path):
    manifest = tmp_path / "manifest.json"
    manifest.write_text(json.dumps({"documents": []}), encoding="utf-8")
    with pytest.raises(ValueError):
        resolve_source(manifest, {"document_id": "d", "source_path": "../evil.docx"})


def test_summarize_counts_and_incomplete(tmp_path: Path):
    rows = [
        {"document_id": "a", "completion_status": "COMPLETE", "category": PASS,
         "features": {"tables": True}},
        {"document_id": "b", "completion_status": "COMPLETE", "category": EXPECTED_REJECT,
         "features": {"tables": False}},
        {"document_id": "c", "completion_status": "COMPLETE", "category": NOT_APPLICABLE,
         "features": {"tables": False}},
        {"document_id": "d", "completion_status": "COMPLETE", "category": CONTRACT_FAILURE,
         "features": {"tables": True}},
        {"document_id": "e", "completion_status": "INCOMPLETE", "category": None, "stage": "disk"},
    ]
    results = tmp_path / "results.jsonl"
    results.write_text("\n".join(json.dumps(r) for r in rows) + "\n", encoding="utf-8")
    summary = summarize(tmp_path, tmp_path / "m.json",
                        {"bin_sha256": "b", "manifest_sha256": "m", "rules_version": "r4"},
                        {"binary": {"sha256": "b"}}, 4, 1)
    assert summary["counts"] == {PASS: 1, EXPECTED_REJECT: 1, NOT_APPLICABLE: 1, CONTRACT_FAILURE: 1}
    assert summary["incomplete"] == ["e"]
    assert summary["feature_stratification"]["tables"]["with"][PASS] == 1


def test_member_digest_stable_and_sensitive():
    first = {"b": "2", "a": "1"}
    assert _hash_map_digest(first) == _hash_map_digest({"a": "1", "b": "2"})
    assert _hash_map_digest(first) != _hash_map_digest({"a": "1", "b": "3"})


def test_multi_part_query_uniqueness_no_false_target():
    # Query unique in word/document.xml but also present in another word/*.xml
    # part must not produce a "safe" target.
    doc = _doc(_para("crosspartquery text here"))
    target, reason = select_target(doc, ["prefix crosspartquery suffix"])
    assert target is None
    assert reason == "no safe eligible test target"


def test_zip_member_comparison_shape(tmp_path: Path):
    # Public helper contract: untouched re-zips compare equal; tamper differs.
    src = tmp_path / "a.docx"
    with zipfile.ZipFile(src, "w", zipfile.ZIP_DEFLATED) as archive:
        archive.writestr("word/document.xml", b"<w:doc/>")
        archive.writestr("[Content_Types].xml", b"<Types/>")
    from qualification.real_docx_scan import compare_packages, member_shas
    first = member_shas(src)
    assert compare_packages(first, dict(first))["same"]
    altered = dict(first)
    altered["word/document.xml"] = "0" * 64
    assert compare_packages(first, altered)["changed"] == ["word/document.xml"]
