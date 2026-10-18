"""Minimized real-docx-corpus-v1 violations; exercise the real five-tool binary."""
from pathlib import Path
import sys
import zipfile

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from qualification.real_docx_scan import _resolve_bin, mcp_session


@pytest.mark.xfail(strict=True, reason="corpus-v1: existing xml:space is duplicated by a whitespace-preserving direct edit")
def test_existing_xml_space_survives_direct_edit(tmp_path):
    binary = _resolve_bin(None)
    source = tmp_path / "source.docx"
    output = tmp_path / "output.docx"
    xml = (b'<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">'
           b'<w:body><w:p><w:r><w:t xml:space="preserve"> Original Article</w:t></w:r></w:p>'
           b'<w:sectPr/></w:body></w:document>')
    fixture = Path(__file__).resolve().parents[1] / "corpus/release/patent-shaped.docx"
    with zipfile.ZipFile(fixture) as original, zipfile.ZipFile(source, "w") as staged:
        for member in original.infolist():
            staged.writestr(member, xml if member.filename == "word/document.xml" else original.read(member))
    replies, _ = mcp_session(binary, [
        ("document_open", {"source": str(source), "track": False}),
        ("document_find", {"query": "Original"}),
    ], 60)
    assert replies[0]["outcome"] == "success", replies[0]
    match = replies[1]["data"]["matches"][0]
    occurrence = (match.get("occurrences") or [match])[0]
    assert occurrence["verdict"]["editable"] is True
    replies, _ = mcp_session(binary, [
        ("document_open", {"source": str(source), "track": False}),
        ("document_edit", {"edits": [{"handle": occurrence["handle"], "replace_with": "Priginal"}]}),
        ("document_save", {"output": str(output)}),
    ], 60)
    assert all(reply["outcome"] == "success" for reply in replies), replies
    with zipfile.ZipFile(output) as built:
        actual = built.read("word/document.xml")
    assert actual == xml.replace(b"Original", b"Priginal")
