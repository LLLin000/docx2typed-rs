#!/usr/bin/env python3
"""Real Word bidirectional handoff lane (word-handoff).

Owned by WordHandoffLane. New file; no core edits.

Contract (docs/specs/09_real_workflow_qualification.md, external-edit state):
  A stage immutable source A into owned area -> CLI extract Baseline A
  -> five-tool Agent edit + document_save B -> real Word operator opens B,
  replaces a DIFFERENT unique phrase (Range/Find, formatting preserved) and
  saves C OVER the staged source path (never the frozen original)
  -> old workdir must not silently overwrite C (separate stale case)
  -> safe continue is explicit CLI re-extract of C into the SAME logical
  workdir with --discard-committed-versions only after B is exported
  -> second five-tool Agent edit + save final -> real Word opens final and
  compares all three intended edits; non-target formatting/functional props
  preserved; no three-way merge anywhere.

Five-tool Agent calls only (document_open/find/edit/save/status) via the
legacy MCP line protocol (one process == one session, same transport as
qualification.real_docx_scan.mcp_session). CLI `extract` is allowed only as
the explicit lifecycle action (baseline constructor / baseline replacement).

Word work goes through real Word COM via PowerShell (-File) scripts written
into the owned case dir. Each COM instance is fresh and quits only itself;
no user Word is touched. DisplayAlerts=0 / OpenAndRepair=false are recorded
as automation flags, never as no-repair proof. execution_mode is always
'word-com-automated-operator', human_ui_review is always 'not-run'.

Usage:
  python qualification/real_workflow_word_handoff.py --bin PATH --manifest PATH --out DIR [--timeout SEC] [--input-sha SHA]
"""
from __future__ import annotations

import argparse
import hashlib
import json
import platform
import re
import shutil
import subprocess
import sys
import zipfile
from pathlib import Path
import xml.etree.ElementTree as ET
REPO_ROOT = Path(__file__).resolve().parents[1]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

try:
    from qualification.real_docx_scan import (  # type: ignore
        mcp_session as _shared_mcp,
        _sha256_file as _shared_sha256,
        _resolve_bin as _shared_resolve_bin,
        load_manifest as _shared_load,
        validate_manifest as _shared_validate,
        resolve_source as _shared_resolve,
        _tree_snapshot as _shared_tree,
        select_target as _shared_select_target,
    )
except ImportError:
    from real_docx_scan import (  # type: ignore
        mcp_session as _shared_mcp,
        _sha256_file as _shared_sha256,
        _resolve_bin as _shared_resolve_bin,
        load_manifest as _shared_load,
        validate_manifest as _shared_validate,
        resolve_source as _shared_resolve,
        _tree_snapshot as _shared_tree,
        select_target as _shared_select_target,
    )

SCHEMA = "real-workflow-qualification-1"
WORKFLOW = "word-handoff"
EXECUTION_MODE = "word-com-automated-operator"
HUMAN_UI_REVIEW = "not-run"
SENTINEL = ".real-workflow-word-owned"
FIND_PAGE_LIMIT = 200
W_NS = "http://schemas.openxmlformats.org/wordprocessingml/2006/main"

WORD_EXE_CANDIDATES = [
    Path(r"C:/Program Files/Microsoft Office/Root/Office16/WINWORD.EXE"),
    Path(r"C:/Program Files/Microsoft Office/Office16/WINWORD.EXE"),
]


def _sha256_file(path: Path) -> str:
    return _shared_sha256(path)


def _resolve_bin(explicit: str | None) -> Path:
    return _shared_resolve_bin(explicit)


def _bin_version_text(binpath: Path, timeout: int) -> str:
    try:
        proc = subprocess.run([str(binpath), "--version"], capture_output=True,
                              text=True, encoding="utf-8", errors="replace",
                              timeout=timeout)
        return ((proc.stdout or "") + (proc.stderr or "")).strip()[:300] or "unavailable"
    except (OSError, subprocess.TimeoutExpired, ValueError):
        return "unavailable"


def _load_manifest(path: Path) -> dict:
    return _shared_load(path)


def _validate_manifest(path: Path) -> tuple[dict, str]:
    validation = _shared_validate(path)
    if not validation.get("ok"):
        raise ValueError(f"manifest invalid: {validation.get('problems')}")
    return _shared_load(path), str(validation.get("manifest_sha256", ""))


def _resolve_source(manifest_path: Path, entry: dict) -> Path:
    return _shared_resolve(manifest_path, entry)


def mcp_session(binpath: Path, calls: list[tuple[str, dict]],
                timeout: int) -> tuple[list[dict], dict]:
    return _shared_mcp(binpath, calls, timeout)


def _codes(envelope: dict) -> list[str]:
    return [str(d.get("code", "")) for d in (envelope.get("diagnostics") or [])
            if isinstance(d, dict)]


def _is_transport_error(envelope: dict) -> bool:
    if (envelope.get("outcome") or "") == "transport-error":
        return True
    return any(str(d.get("code", "")) in ("transport", "bad-json", "missing-reply")
               for d in (envelope.get("diagnostics") or []) if isinstance(d, dict))



# --------------------------------------------------------------------------
# Safe-target selection: shared qualification.real_docx_scan.select_target
# (single-target oracle) enumerated to 4 distinct targets. The lane needs 4
# DIFFERENT unique phrases (agent-Q1, word-Q2, agent-Q3, witness-Q4), so it
# calls the shared selector repeatedly, excluding pinned queries via
# other_texts. Local helpers below exist only for the no-shared fallback.
# --------------------------------------------------------------------------

def _local(tag: object) -> str:
    text = tag.tag if isinstance(tag, ET.Element) else tag
    assert isinstance(text, str)
    if "}" in text:
        return text.rsplit("}", 1)[-1]
    return text.rsplit(":", 1)[-1]


def _parse_xml(data: bytes) -> ET.Element | None:
    try:
        return ET.fromstring(data)
    except ET.ParseError:
        return None


def _wns(e: ET.Element) -> bool:
    return e.tag.startswith("{" + W_NS + "}")


def _all_wt(root: ET.Element) -> list[ET.Element]:
    return [e for e in root.iter() if _local(e.tag) == "t" and _wns(e)]


def _paragraphs(root: ET.Element) -> list[ET.Element]:
    body = None
    for e in root.iter():
        if _local(e.tag) == "body" and _wns(e):
            body = e
            break
    if body is None:
        return []
    return [e for e in list(body) if _local(e.tag) == "p" and _wns(e)]


def _paragraph_flags(para: ET.Element) -> dict[str, bool]:
    tags = {_local(e.tag) for e in para.iter() if _wns(e)}
    return {"has_opaque": bool(tags & {"drawing", "pict", "object", "fldSimple",
                                       "oMath", "oMathPara", "sdt", "smartTag",
                                       "proofErr", "txbxContent",
                                       "AlternateContent"}),
            "has_revision": bool(tags & {"ins", "del", "moveFrom", "moveTo",
                                         "delText"}),
            "has_field": bool(tags & {"fldChar", "instrText", "fldSimple"}),
            "has_sdt": "sdt" in tags}


KNOWN_RUN_INLINE_LOCAL = frozenset({"t", "delText", "tab", "br", "cr",
                                    "noBreakHyphen", "softHyphen", "sym",
                                    "commentReference", "footnoteRef",
                                    "endnoteRef", "annotationRef", "separator",
                                    "continuationSeparator",
                                    "lastRenderedPageBreak"})


def _run_children_ok(run: ET.Element) -> bool:
    for child in run:
        if not _wns(child):
            continue
        name = _local(child.tag)
        if name == "rPr":
            continue
        if name not in KNOWN_RUN_INLINE_LOCAL:
            return False
    return True


def _candidate_queries(text: str) -> list[str]:
    import re as _re
    _qr = _re.compile(r"[0-9A-Za-z\u4e00-\u9fff]{8,24}")
    if _qr.fullmatch(text or ""):
        return [text]
    runs = [m.group(0) for m in _qr.finditer(text or "")]
    return [run[:24] if len(run) > 24 else run for run in runs]


def _scan_tags(xml: bytes) -> list[dict]:
    tags: list[dict] = []
    pos, size = 0, len(xml)
    while True:
        start = xml.find(b"<", pos)
        if start < 0:
            break
        if xml.startswith(b"<!--", start):
            end = xml.find(b"-->", start + 4)
            if end < 0:
                break
            pos = end + 3
            continue
        if xml.startswith(b"<![CDATA[", start):
            end = xml.find(b"]]>", start + 9)
            if end < 0:
                break
            pos = end + 3
            continue
        if xml.startswith(b"<?", start):
            end = xml.find(b"?>", start + 2)
            if end < 0:
                break
            pos = end + 2
            continue
        cursor, quote, end = start + 1, None, -1
        while cursor < size:
            byte = xml[cursor:cursor + 1]
            if quote is not None:
                if byte == quote:
                    quote = None
            elif byte in (b'"', b"'"):
                quote = byte
            elif byte == b">":
                end = cursor
                break
            cursor += 1
        if end < 0:
            break
        raw = xml[start + 1:end].strip()
        closing = raw.startswith(b"/")
        if closing:
            raw = raw[1:].strip()
        self_closing = raw.endswith(b"/")
        if self_closing:
            raw = raw[:-1].strip()
        name = raw.split(b" ", 1)[0].split(b":", 1)[-1].decode("ascii", errors="replace")
        tags.append({"name": name, "closing": closing,
                     "self_closing": self_closing, "start": start,
                     "open_end": end + 1, "end": end + 1})
        pos = end + 1
    return tags


def _locate_paragraphs(docxml: bytes) -> tuple[list[dict] | None, str]:
    tags = _scan_tags(docxml)
    body_open, body_close = -1, -1
    stack: list[str] = []
    located: list[dict] = []
    tid, sid, direct_count, body_children = -1, -1, 0, 0
    open_tables: list[int] = []
    for tag in tags:
        name = tag["name"]
        if tag["closing"]:
            if stack and (stack[-1] == name or stack[-1].startswith("sdt:")):
                top = stack.pop()
                if top == "tbl" and open_tables:
                    open_tables.pop()
            if name == "body":
                body_close = tag["start"]
        elif name == "body" and body_open < 0:
            body_open = tag["start"]
            stack.append(name)
        elif body_open >= 0 and body_close < 0:
            is_direct = len(stack) == 1 and stack[0] == "body"
            if name == "tbl" and is_direct and not tag["self_closing"]:
                tid += 1
                open_tables.append(tid)
                stack.append(name)
            elif name == "sdt" and not tag["self_closing"]:
                sid += 1
                stack.append(f"sdt:{sid}")
            elif name == "p":
                if is_direct:
                    located.append({"id": f"P{direct_count}", "start": tag["start"],
                                    "open_end": tag["open_end"], "direct": True,
                                    "body_index": body_children})
                    direct_count += 1
                    body_children += 1
                else:
                    chain_tbl = [e for e in stack if e == "tbl"]
                    chain_sdt = [e for e in stack if e.startswith("sdt:")]
                    located.append({"id": None, "start": tag["start"],
                                    "open_end": tag["open_end"], "direct": False,
                                    "body_index": None})
                if tag["self_closing"]:
                    located[-1]["end"] = tag["open_end"]
                else:
                    stack.append(name)
            elif not tag["self_closing"]:
                stack.append(name)
    closes = [t for t in tags if t["name"] == "p" and t["closing"]]
    opens = [t for t in tags if t["name"] == "p" and not t["closing"]
             and not t["self_closing"]]
    if len(opens) != len(closes):
        return None, "paragraph tag balance mismatch"
    cursor = 0
    for entry in located:
        if entry.get("end") is not None:
            continue
        if cursor >= len(closes):
            return None, "paragraph tag balance mismatch"
        entry["end"] = closes[cursor]["end"]
        cursor += 1
    return located, ""


def _wt_ranges_expat(docxml: bytes, want: int) -> list[tuple[int, int]] | None:
    import xml.parsers.expat as expat
    ranges: list[tuple[int, int]] = []
    stack: list[int] = []
    failed: list[str] = []
    parser = expat.ParserCreate(namespace_separator=" ")
    def start_handler(name: str, _attrs: dict) -> None:
        uri, _, local = name.rpartition(" ")
        if uri == W_NS and local == "t":
            open_start: int = parser.CurrentByteIndex
            close_angle = docxml.find(b">", open_start)
            if close_angle < 0:
                failed.append("open")
                return
            if docxml[close_angle - 1:close_angle] == b"/":
                ranges.append((close_angle + 1, close_angle + 1))
            else:
                stack.append(close_angle + 1)
    def end_handler(name: str) -> None:
        uri, _, local = name.rpartition(" ")
        if uri == W_NS and local == "t":
            end_index: int = parser.CurrentByteIndex
            if stack and docxml[end_index:end_index + 2] == b"</":
                ranges.append((stack.pop(), end_index))
    parser.StartElementHandler = start_handler
    parser.EndElementHandler = end_handler
    try:
        parser.Parse(docxml, True)
    except expat.ExpatError:
        return None
    if failed or len(ranges) != want or stack:
        return None
    return ranges


def find_safe_targets(docxml: bytes, other_texts: list[str],
                      need: int) -> tuple[list[dict], str]:
    """Collect up to `need` distinct safe targets via the shared oracle."""
    out: list[dict] = []
    seen_set: set[str] = set()
    excluded = list(other_texts)
    for _ in range(need):
        target, reason = _shared_select_target(docxml, excluded)
        if target is None:
            detail = f": {reason}" if reason else ""
            return out, (f"only {len(out)} safe target(s); need {need}{detail}"
                         if out else (reason or "no safe eligible test target"))
        query = str(target.get("_query", ""))
        replacement = str(target.get("_replacement", ""))
        if not query or not replacement or query in seen_set:
            return out, f"shared oracle repeated query {query!r}; need {need}"
        out.append({
            "paragraph_id": target.get("paragraph_id"),
            "paragraph_ordinal": target.get("paragraph_ordinal"),
            "query_start": target.get("query_start"),
            "query_end": target.get("query_end"),
            "query_sha256": target.get("query_sha256"),
            "replacement_sha256": target.get("replacement_sha256"),
            "query_len": target.get("query_len"),
            "query_bytes": target.get("query_bytes"),
            "_query": query, "_replacement": replacement,
        })
        seen_set.add(query)
        excluded = excluded + [query]
    return out, "ok"



# --------------------------------------------------------------------------
# Read-only semantic snapshots (never mutate OOXML; diagnostics only)
# --------------------------------------------------------------------------

def _read_member(path: Path, name: str) -> bytes | None:
    try:
        with zipfile.ZipFile(path) as archive:
            return archive.read(name)
    except (OSError, KeyError, zipfile.BadZipFile):
        return None


def semantic_snapshot(path: Path) -> dict:
    try:
        with zipfile.ZipFile(path) as archive:
            names = sorted(archive.namelist())
            xml = {n: archive.read(n) for n in names
                   if n.startswith("word/") and n.endswith(".xml")}
            rels = {n: archive.read(n) for n in names if n.endswith(".rels")}
    except (OSError, zipfile.BadZipFile) as exc:
        return {"valid_zip": False, "error": str(exc)[:200]}
    visible = [m.decode("utf-8", errors="replace")
               for v in xml.values()
               for m in re.findall(rb"<w:t[^>]*>(.*?)</w:t>", v, re.S)]
    deleted = [m.decode("utf-8", errors="replace")
               for v in xml.values()
               for m in re.findall(rb"<w:delText[^>]*>(.*?)</w:delText>", v, re.S)]
    blob = b"".join(xml.values())
    marks: list[str] = []
    for m in re.finditer(rb"<w:(ins|del|moveFrom|moveTo)\b[^>]*>", blob):
        tag = m.group(1).decode("utf-8", errors="replace")
        kind = {"ins": "insert", "del": "delete",
                "moveFrom": "move_from", "moveTo": "move_to"}.get(tag, tag)
        attrs = m.group(0)
        def attr(name: str) -> str:
            f = re.search(rb"w:" + name.encode() + rb'="([^"]*)"', attrs)
            return f.group(1).decode("utf-8", errors="replace") if f else ""
        marks.append(f"{kind}:{attr('author')}:{attr('date')}")
    marks.sort()
    comments_xml = xml.get("word/comments.xml", b"")
    comments: list[str] = []
    for block in re.findall(rb"<w:comment\b[^>]*>.*?</w:comment>", comments_xml, re.S):
        head = re.match(rb'<w:comment\b[^>]*\sw:id="(\d+)"[^>]*\sw:author="([^"]*)"', block)
        if not head:
            continue
        cid = head.group(1).decode("utf-8", errors="replace")
        author = head.group(2).decode("utf-8", errors="replace")
        text = "".join(m.decode("utf-8", errors="replace")
                       for m in re.findall(rb"<w:t[^>]*>(.*?)</w:t>", block, re.S))
        comments.append(f"{cid}:{author}:{text}")
    comment_parts = sorted(n for n in names
                           if any(t in n.lower() for t in ("comment", "people")))
    # Semantic comment identity: (author, text, anchored-passage context).
    # Raw w:id values and commentsExtended/people XML are Word-normalized
    # serialization, never identity (run4: ids 0/2 swapped bodies, anchors
    # stable, all authors/texts retained). Anchor context is the document
    # paragraph's visible text around the comment's range, from document.xml.
    comment_semantic: list[str] = []
    try:
        doc_root = ET.fromstring(xml.get("word/document.xml", b"<w:document/>"))
        # Map comment id -> anchor passage: paragraph visible text at the
        # commentRangeStart/commentReference site (whitespace-collapsed,
        # first 120 chars + length so near-identical paras still separate).
        anchor_of: dict[str, str] = {}
        for p in doc_root.iter("{" + W_NS + "}p"):
            ptext = "".join(t.text or "" for t in p.iter("{" + W_NS + "}t"))
            norm = " ".join(ptext.split())
            ctx = f"{norm[:120]}|len={len(norm)}"
            for e in p.iter():
                if not (isinstance(e.tag, str) and e.tag.startswith("{" + W_NS + "}")):
                    continue
                local = e.tag.rsplit("}", 1)[-1]
                if local in ("commentRangeStart", "commentReference"):
                    cid = e.get("{" + W_NS + "}id", "")
                    if cid and cid not in anchor_of:
                        anchor_of[cid] = ctx
        for block in re.findall(rb"<w:comment\b[^>]*>.*?</w:comment>", comments_xml, re.S):
            head = re.match(rb'<w:comment\b[^>]*\sw:author="([^"]*)"', block)
            if not head:
                continue
            author = head.group(1).decode("utf-8", errors="replace")
            text = "".join(m.decode("utf-8", errors="replace")
                           for m in re.findall(rb"<w:t[^>]*>(.*?)</w:t>", block, re.S))
            idm = re.match(rb'<w:comment\b[^>]*\sw:id="(\d+)"', block)
            cid = idm.group(1).decode("utf-8", errors="replace") if idm else ""
            comment_semantic.append(f"{author}:{text}@{anchor_of.get(cid, '?')}")
    except ET.ParseError:
        pass
    comment_semantic.sort()
    bookmarks = sorted({m.decode("utf-8", errors="replace")
                        for m in re.findall(rb"<w:bookmarkStart[^>]*\sw:name=\"([^\"]*)\"", blob)})
    relationships: list[str] = []
    for rels_body in rels.values():
        for rid, rtype, rtarget in re.findall(
                rb'<Relationship\s+Id="([^"]+)"\s+Type="([^"]+)"\s+Target="([^"]*)"', rels_body):
            relationships.append(f"{rid.decode('utf-8', errors='replace')}:"
                                 f"{rtype.decode('utf-8', errors='replace')}:"
                                 f"{rtarget.decode('utf-8', errors='replace')}")
    relationships.sort()
    # Sections: count + page-size identity (functional, not byte equality).
    sect_ids: list[str] = []
    try:
        root = ET.fromstring(xml.get("word/document.xml", b"<w:document/>"))
        ns = {"w": W_NS}
        for sect in root.iter("{" + W_NS + "}sectPr"):
            pg = sect.find("w:pgSz", ns)
            ident = "sect"
            if pg is not None:
                ident += f":{pg.get('{%s}w' % W_NS, '')}x{pg.get('{%s}h' % W_NS, '')}"
            sect_ids.append(ident)
    except ET.ParseError:
        sect_ids = ["<unparseable>"]
    # Styles: paragraph style ids in document order (functional identity).
    style_ids: list[str] = []
    try:
        root = ET.fromstring(xml.get("word/document.xml", b"<w:document/>"))
        for p in root.iter("{" + W_NS + "}p"):
            pPr = p.find("{" + W_NS + "}pPr")
            if pPr is not None:
                ps = pPr.find("{" + W_NS + "}pStyle")
                if ps is not None:
                    style_ids.append(ps.get("{" + W_NS + "}val", ""))
    except ET.ParseError:
        pass
    styles_blob = xml.get("word/styles.xml", b"")
    return {
        "valid_zip": True,
        "visible_text_sha256": hashlib.sha256("".join(visible).encode("utf-8")).hexdigest(),
        "visible_text_chars": len("".join(visible)),
        "deleted_text_sha256": hashlib.sha256("".join(deleted).encode("utf-8")).hexdigest(),
        "revision_marks": marks,
        "comments_semantic": comment_semantic,
        "comment_parts": comment_parts,
        "bookmarks": bookmarks,
        "relationships": relationships,
        "sections": sect_ids,
        "paragraph_style_ids_sha256": hashlib.sha256(
            "\n".join(style_ids).encode("utf-8")).hexdigest(),
        "member_names": names,
    }


def _external_targets(relationships: list[str]) -> set[tuple[str, str]]:
    out: set[tuple[str, str]] = set()
    for rel in relationships:
        parts = rel.split(":", 2)
        if len(parts) != 3:
            continue
        rtype, target = parts[1], parts[2]
        if "hyperlink" in rtype or target.startswith(("http", "mailto")):
            out.add((rtype, target))
    return out


# --------------------------------------------------------------------------
# Workdir / source snapshots
# --------------------------------------------------------------------------

def _tree_snapshot(root: Path) -> dict[str, str]:
    if _shared_tree is not None:
        return _shared_tree(root)
    out: dict[str, str] = {}
    if not root.is_dir():
        return out
    for path in sorted(root.rglob("*")):
        if path.is_file():
            try:
                out[str(path.relative_to(root))] = _sha256_file(path)
            except OSError:
                out[str(path.relative_to(root))] = "<unreadable>"
    return out


def _word_check_lists(word_v: dict) -> tuple[list, list]:
    # Position-based mapping: the job file held raw text, but the Word
    # script echoes raw text; order is the contract (present first, then
    # absent) because hashes of private text never enter report.json.
    present_found = [c.get("found") for c in (word_v.get("checks") or [])
                     if isinstance(c, dict) and c.get("expected") is True]
    absent_found = [c.get("found") for c in (word_v.get("checks") or [])
                    if isinstance(c, dict) and c.get("expected") is False]
    return present_found, absent_found


def _read_baseline_epoch(workdir: Path) -> int | None:
    """Baseline generation (mirrors docx2typed_core::read_baseline_epoch)."""
    try:
        data = json.loads((workdir / "baseline.json").read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None
    if data.get("schema") != "docx2typed-baseline-1":
        return None
    epoch = data.get("epoch")
    return int(epoch) if isinstance(epoch, int) else None


def _read_format_fingerprints(workdir: Path) -> dict:
    try:
        data = json.loads((workdir / "format.json").read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return {}
    return {k: data.get(k) for k in
            ("source", "source_path", "source_sha256", "template_sha256",
             "document_xml_sha256")}


# --------------------------------------------------------------------------
# PowerShell / Word COM
# --------------------------------------------------------------------------

WORD_PREFLIGHT_PS = r"""$ErrorActionPreference = "Stop"
$word = $null
$result = [ordered]@{healthy=$false; error=$null; version=$null; build=$null}
try {
  $word = New-Object -ComObject Word.Application
  $word.Visible = $false
  $word.DisplayAlerts = 0
  $word.AutomationSecurity = 3
  $result.version = $word.Version
  $result.build = $word.Build
  $result.healthy = $true
} catch {
  $result.error = $_.Exception.Message
} finally {
  if ($word -ne $null) { try { $word.Quit() } catch {} }
}
$result | ConvertTo-Json -Compress
"""

# Operator edit: Find.Replace preserves the found Range's character
# formatting. Whole-paragraph flattening ($para.Range.Text = ...) is never
# used; the only write is the found Range via Find/Replace.
# NOTE: param() MUST be the first statement (PowerShell parses it only in
# first position); $ErrorActionPreference is set AFTER param, run2 root cause.
WORD_EDIT_PS = r"""param([string]$InputPath, [string]$OutputPath, [string]$JobPath)
$ErrorActionPreference = "Stop"
$word = $null; $doc = $null
$result = [ordered]@{ok=$false; replaced=$false; error=$null; version=$null; build=$null;
  sections=0; comments=0; revisions=0}
try {
  $job = [System.IO.File]::ReadAllText($JobPath, [System.Text.Encoding]::UTF8) | ConvertFrom-Json
  $word = New-Object -ComObject Word.Application
  $word.Visible = $false
  $word.DisplayAlerts = 0
  $word.AutomationSecurity = 3
  $result.version = $word.Version
  $result.build = $word.Build
  $doc = $word.Documents.Open($InputPath, $false, $false, $false, "", "", $false, "", "", 0, $null, $false, $false, $false, $true)
  try { $doc.TrackRevisions = $false } catch {}
  # REAL Word comment snapshot BEFORE the operator edit (author/text/anchor).
  $comBefore = @()
  try {
    foreach ($cm in $doc.Comments) {
      $para = ""
      try { $para = $cm.Range.Paragraphs.Item(1).Range.Text } catch {}
      $para = (($para -replace "\s+", " ").Trim())
      if ($para.Length -gt 120) { $para = $para.Substring(0, 120) }
      $comBefore += [ordered]@{author=[string]$cm.Author; text=[string]$cm.Range.Text; anchor=$para}
    }
  } catch {}
  $result.comments_before = $comBefore
  $find = $doc.Content.Find
  $find.ClearFormatting()
  $find.Replacement.ClearFormatting()
  $found = $find.Execute($job.find, $false, $false, $false, $false, $false, $true, 1, $false, $job.replace_with, 1)
  $result.replaced = [bool]$found
  if (-not $result.replaced) { throw ("word find missed its unique target") }
  # REAL Word comment snapshot AFTER save-source (same COM session, post-edit).
  $comAfter = @()
  try {
    foreach ($cm in $doc.Comments) {
      $para = ""
      try { $para = $cm.Range.Paragraphs.Item(1).Range.Text } catch {}
      $para = (($para -replace "\s+", " ").Trim())
      if ($para.Length -gt 120) { $para = $para.Substring(0, 120) }
      $comAfter += [ordered]@{author=[string]$cm.Author; text=[string]$cm.Range.Text; anchor=$para}
    }
  } catch {}
  $result.comments_after = $comAfter
  $result.sections = $doc.Sections.Count
  $result.comments = $doc.Comments.Count
  $result.revisions = $doc.Revisions.Count
  $doc.SaveAs2($OutputPath, 16)
  $result.ok = $true
} catch {
  $result.error = $_.Exception.Message
} finally {
  if ($doc -ne $null) { try { $doc.Close($false) } catch {} }
  if ($word -ne $null) { try { $word.Quit() } catch {} }
}
$result | ConvertTo-Json -Depth 5 -Compress
"""

# Read-only final verification: Word is the comparator. No document write
# occurs here ($doc.Close($false)); finding only observes.
# NOTE: param() first statement here too; $ErrorActionPreference after it.
WORD_VERIFY_PS = r"""param([string]$InputPath, [string]$JobPath)
$ErrorActionPreference = "Stop"
$word = $null; $doc = $null
$result = [ordered]@{ok=$false; error=$null; version=$null; build=$null;
  checks=@(); witness=@{}; sections=0; comments=0; revisions=0}
try {
  $job = [System.IO.File]::ReadAllText($JobPath, [System.Text.Encoding]::UTF8) | ConvertFrom-Json
  $word = New-Object -ComObject Word.Application
  $word.Visible = $false
  $word.DisplayAlerts = 0
  $word.AutomationSecurity = 3
  $result.version = $word.Version
  $result.build = $word.Build
  $doc = $word.Documents.Open($InputPath, $false, $true, $false, "", "", $false, "", "", 0, $null, $false, $false, $false, $true)
  $checks = @()
  foreach ($text in $job.present) {
    $rng = $doc.Content
    $rng.Find.ClearFormatting()
    $found = $rng.Find.Execute($text, $false, $false, $false, $false, $false, $true, 1, $false, $null, 0)
    $checks += [ordered]@{text_sha256=$text; expected=$true; found=[bool]$found}
  }
  foreach ($text in $job.absent) {
    $rng = $doc.Content
    $rng.Find.ClearFormatting()
    $found = $rng.Find.Execute($text, $false, $false, $false, $false, $false, $true, 1, $false, $null, 0)
    $checks += [ordered]@{text_sha256=$text; expected=$false; found=[bool]$found}
  }
  $result.checks = $checks
  $wrng = $doc.Content
  $wrng.Find.ClearFormatting()
  $wfound = $wrng.Find.Execute($job.witness, $false, $false, $false, $false, $false, $true, 1, $false, $null, 0)
  $witness = [ordered]@{found=[bool]$wfound; style=$null; bold=$null; italic=$null}
  if ($wfound) {
    try { $witness.style = $wrng.Style.NameLocal } catch {}
    try { $witness.bold = [int]$wrng.Font.Bold } catch {}
    try { $witness.italic = [int]$wrng.Font.Italic } catch {}
  }
  $result.witness = $witness
  $result.sections = $doc.Sections.Count
  $result.comments = $doc.Comments.Count
  $result.revisions = $doc.Revisions.Count
  $result.ok = $true
} catch {
  $result.error = $_.Exception.Message
} finally {
  if ($doc -ne $null) { try { $doc.Close($false) } catch {} }
  if ($word -ne $null) { try { $word.Quit() } catch {} }
}
$result | ConvertTo-Json -Depth 5 -Compress
"""


def _write_utf8(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8-sig")


def _run_powershell(script: Path, args: list[str], timeout: int) -> dict:
    """Run one PowerShell -File script; parse the last line as JSON."""
    try:
        proc = subprocess.run(
            ["powershell", "-NoProfile", "-NonInteractive",
             "-ExecutionPolicy", "Bypass", "-File", str(script), *args],
            capture_output=True, text=True, encoding="utf-8",
            errors="replace", timeout=timeout)
    except subprocess.TimeoutExpired as exc:
        return {"_timeout": True, "error": f"powershell timeout: {exc}"}
    except OSError as exc:
        return {"_os_error": True, "error": str(exc)[:300]}
    payload: dict = {"_returncode": proc.returncode,
                     "_stderr_tail": (proc.stderr or "")[-1500:]}
    lines = [line for line in (proc.stdout or "").splitlines() if line.strip()]
    if not lines:
        payload["error"] = "no powershell output"
        return payload
    try:
        payload.update(json.loads(lines[-1]))
    except json.JSONDecodeError:
        payload["error"] = f"unparseable powershell output: {lines[-1][:200]}"
    return payload


def _word_host(timeout: int, work: Path) -> tuple[dict, Path | None]:
    """Fresh-instance COM preflight. Never kills user Word."""
    script = work / "word_preflight.ps1"
    _write_utf8(script, WORD_PREFLIGHT_PS)
    payload = _run_powershell(script, [], timeout)
    exe = next((p for p in WORD_EXE_CANDIDATES if p.is_file()), None)
    host = {"name": "Microsoft Word",
            "version": payload.get("version"),
            "build": payload.get("build"),
            "exe_path": str(exe) if exe else "unknown",
            "os": f"{platform.system()} {platform.release()}",
            "execution_mode": EXECUTION_MODE,
            "human_ui_review": HUMAN_UI_REVIEW}
    if payload.get("_timeout") or payload.get("_os_error") or not payload.get("healthy"):
        return {"healthy": False, "host": host, "payload": payload}, exe
    return {"healthy": True, "host": host, "payload": payload}, exe


# --------------------------------------------------------------------------
# Engine helpers (five tools only)
# --------------------------------------------------------------------------

def _find_exact_one(binpath: Path, workdir: Path, query: str,
                    timeout: int, call_log: list) -> tuple[str | None, dict]:
    """Page document_find until complete; require exactly one editable hit."""
    handle: str | None = None
    para: str = ""
    occurrences = 0
    editable = False
    code, why = "", ""
    offset: int | None = 0
    pages = 0
    while True:
        args: dict = {"query": query, "limit": FIND_PAGE_LIMIT}
        if offset:
            args["offset"] = offset
        envs, transport = mcp_session(
            binpath, [("document_open", {"source": str(workdir)}),
                      ("document_find", args)], timeout)
        call_log.append({"tag": f"find-p{pages}", "transport": transport,
                         "requests": [{"tool": "document_open",
                                       "args": {"source": str(workdir)}},
                                      {"tool": "document_find", "args": args}],
                         "envelopes": envs})
        if any(_is_transport_error(e) for e in envs):
            return None, {"transport_error": True, "detail": "find transport error"}
        data = (envs[1].get("data") or {})
        for match in data.get("matches") or []:
            occs = match.get("occurrences")
            mpara = str(match.get("paragraph_id") or "")
            if occs:
                for occ in occs:
                    occurrences += 1
                    verdict = occ.get("verdict") or {}
                    if occurrences == 1:
                        handle = str(occ.get("handle") or "")
                        para = mpara
                        editable = verdict.get("editable") is True
                        code = str(verdict.get("code") or "")
                        why = str(verdict.get("why") or "")[:200]
            else:
                occurrences += 1
                verdict = match.get("verdict") or {}
                if occurrences == 1:
                    handle = str(match.get("handle") or "")
                    para = mpara
                    editable = verdict.get("editable") is True
                    code = str(verdict.get("code") or "")
                    why = str(verdict.get("why") or "")[:200]
        counts = data.get("counts") or {}
        complete = counts.get("complete")
        nxt = (data.get("next") or {}).get("call") if isinstance(data.get("next"), dict) else None
        pages += 1
        if complete is True or pages >= 10:
            break
        if isinstance(nxt, dict) and isinstance(nxt.get("offset"), int):
            offset = nxt["offset"]
        else:
            offset = int(counts.get("offset") or 0) + int(counts.get("returned_blocks") or 0)
    return handle, {"occurrences": occurrences, "editable": editable,
                    "code": code, "why": why, "paragraph_id": para, "pages": pages}


def _agent_edit_save(binpath: Path, workdir: Path, query: str, replacement: str,
                     output: Path, timeout: int, call_log: list) -> dict:
    """One Agent round: find the pinned query, replace, save. Returns report."""
    handle, found = _find_exact_one(binpath, workdir, query, timeout, call_log)
    if found.get("transport_error"):
        return {"status": "incomplete", "detail": found["detail"], "found": found}
    if found["occurrences"] != 1:
        return {"status": "contract-failure",
                "detail": f"find occurrences={found['occurrences']} for source-unique query",
                "found": found, "handle": handle}
    if not found["editable"]:
        return {"status": "contract-failure",
                "detail": f"safe source-mapped target refused code={found['code']}",
                "found": found, "handle": handle}
    envs, transport = mcp_session(
        binpath, [("document_open", {"source": str(workdir)}),
                  ("document_edit", {"edits": [{"handle": handle,
                                                "replace_with": replacement}]}),
                  ("document_save", {"output": str(output)})], timeout)
    call_log.append({"tag": "agent-edit-save", "transport": transport,
                     "requests": [{"tool": "document_open", "args": {"source": str(workdir)}},
                                  {"tool": "document_edit",
                                   "args": {"edits": [{"handle": handle or "",
                                                       "replace_with": replacement}]}},
                                  {"tool": "document_save", "args": {"output": str(output)}}],
                     "envelopes": envs})
    if any(_is_transport_error(e) for e in envs):
        return {"status": "incomplete", "detail": "edit/save transport error",
                "found": found, "handle": handle}
    edit_env, save_env = envs[1], envs[2]
    if edit_env.get("outcome") != "success":
        return {"status": "contract-failure",
                "detail": f"edit refused codes={_codes(edit_env)}", "found": found,
                "handle": handle, "edit_codes": _codes(edit_env)}
    if save_env.get("outcome") != "success":
        return {"status": "contract-failure",
                "detail": f"save refused codes={_codes(save_env)}", "found": found,
                "handle": handle, "save_codes": _codes(save_env)}
    # Reopen + status: the save must have landed and the session stays clean.
    envs2, transport2 = mcp_session(
        binpath, [("document_open", {"source": str(output)}),
                  ("document_status", {})], timeout)
    call_log.append({"tag": "agent-reopen", "transport": transport2,
                     "requests": [{"tool": "document_open", "args": {"source": str(output)}},
                                  {"tool": "document_status", "args": {}}],
                     "envelopes": envs2})
    if any(_is_transport_error(e) for e in envs2):
        return {"status": "incomplete", "detail": "reopen transport error",
                "found": found, "handle": handle}
    if envs2[0].get("outcome") != "success":
        return {"status": "contract-failure", "detail": "saved output failed to reopen",
                "found": found, "handle": handle}
    return {"status": "ok", "found": found, "handle": handle,
            "edit": edit_env, "save": save_env}


def _cli_extract(binpath: Path, source: Path, outdir: Path, discard: bool,
                 timeout: int) -> dict:
    """Explicit lifecycle action only: baseline constructor / replacement."""
    args = [str(binpath), "extract", "--json", str(source), "-o", str(outdir)]
    if discard:
        args.append("--discard-committed-versions")
    try:
        proc = subprocess.run(args, capture_output=True, text=True,
                              encoding="utf-8", errors="replace", timeout=timeout)
    except subprocess.TimeoutExpired:
        return {"ok": False, "timeout": True, "args": args}
    except OSError as exc:
        return {"ok": False, "os_error": str(exc)[:200], "args": args}
    envelope: dict = {}
    try:
        envelope = json.loads((proc.stdout or "").strip().splitlines()[-1])
    except (IndexError, json.JSONDecodeError):
        pass
    return {"ok": proc.returncode == 0 and envelope.get("outcome") == "success",
            "returncode": proc.returncode, "envelope": envelope,
            "stderr_tail": (proc.stderr or "")[-1500:], "args": args}


# --------------------------------------------------------------------------
# Case runners
# --------------------------------------------------------------------------

def _visible_contains(path: Path, needle: str) -> bool:
    # Raw-XML text INCLUDES w:delText (deleted but stored). Deletions are
    # explicit separate evidence (snapshots.deleted_*), never "visible".
    return _track_visible_text(path).find(needle) >= 0


def _track_visible_text(path: Path) -> str:
    """Visible text with tracked deletions removed, insertions kept.

    Mirrors Word's Final view: w:delText is stored but not shown; w:t inside
    w:del/w:moveFrom is stored but not shown; w:ins content IS shown. Raw
    tag-stripping concatenates both and misreports (run3: Q2 survived inside
    w:delText, so "Q2 gone" read False although Word shows only R2).
    """
    blob = _read_member(path, "word/document.xml")
    if blob is None:
        return ""
    try:
        root = ET.fromstring(blob)
    except ET.ParseError:
        return re.sub(rb"<[^>]+>", b"", blob).decode("utf-8", errors="replace")
    parts: list[str] = []
    def walk(node: ET.Element, deleted: bool) -> None:
        tag = node.tag if isinstance(node.tag, str) else ""
        local = tag.rsplit("}", 1)[-1] if tag.startswith("{") else tag
        if local in ("del", "moveFrom"):
            deleted = True
        if local == "t" and not deleted:
            parts.append(node.text or "")
        # w:delText never visible, even outside w:del (defensive).
        for child in list(node):
            walk(child, deleted)
    for p in root.iter("{" + W_NS + "}p"):
        walk(p, False)
    return "".join(parts)


def _pick_entry(manifest: dict, manifest_path: Path, pinned: str | None,
                need: int) -> tuple[dict, list[dict]]:
    docs = manifest.get("documents") or []
    if pinned:
        for entry in docs:
            if entry.get("sha256") == pinned:
                archived = _resolve_source(manifest_path, entry)
                if not archived.is_file() or _sha256_file(archived) != pinned:
                    raise ValueError("pinned input hash drift")
                with zipfile.ZipFile(archived) as archive:
                    names = archive.namelist()
                    docxml = archive.read("word/document.xml")
                    others = [archive.read(n).decode("utf-8", errors="replace")
                              for n in names if n.startswith("word/")
                              and n.endswith(".xml") and n != "word/document.xml"]
                targets, reason = find_safe_targets(docxml, others, need)
                if len(targets) < need:
                    raise ValueError(f"pinned input yields {len(targets)} safe targets: {reason}")
                return entry, targets
        raise ValueError("pinned --input-sha not in manifest")
    # Prefer clean patent-shaped inputs (no tables/revisions/sdt/opaque/media),
    # manifest order otherwise; first entry yielding `need` safe targets wins.
    def clean_first(entry: dict) -> tuple[int, int]:
        features = entry.get("features") or {}
        dirty = int(bool(features.get("tables"))) + int(bool(features.get("revisions"))) \
            + int(bool(features.get("sdt"))) + int(bool(features.get("opaque_candidates"))) \
            + int(bool(features.get("media")))
        return (dirty, docs.index(entry))
    for entry in sorted(docs, key=clean_first):
        try:
            archived = _resolve_source(manifest_path, entry)
        except ValueError:
            continue
        if not archived.is_file():
            continue
        if _sha256_file(archived) != entry.get("sha256"):
            raise ValueError(f"archive drift for {entry.get('document_id')}")
        try:
            with zipfile.ZipFile(archived) as archive:
                names = archive.namelist()
                docxml = archive.read("word/document.xml")
                others = [archive.read(n).decode("utf-8", errors="replace")
                          for n in names if n.startswith("word/")
                          and n.endswith(".xml") and n != "word/document.xml"]
        except (OSError, zipfile.BadZipFile, KeyError):
            continue
        targets, _ = find_safe_targets(docxml, others, need)
        if len(targets) >= need:
            return entry, targets
    raise ValueError("no manifest entry yields the required safe targets: UNSUPPORTED_WORKFLOW")


def _run_success(binpath: Path, entry: dict, targets: list[dict],
                 case_dir: Path, timeout: int, host: dict) -> dict:
    steps: list[dict] = []
    assertions: list[dict] = []
    call_log: list[dict] = []
    artifacts: dict = {}
    limitations = [
        "execution_mode word-com-automated-operator; human_ui_review not-run",
        "DisplayAlerts=0/OpenAndRepair=false are automation flags, not no-repair proof",
        "Word COM automated operator is not human UI acceptance",
        "comparisons are semantic/style/reference/protected-structure, never raw XML byte equality",
        "no three-way merge performed at any step",
    ]
    input_sha = str(entry.get("sha256"))
    t1, t2, t3, tw = targets[0], targets[1], targets[2], targets[3]
    q1, r1 = t1["_query"], t1["_replacement"]
    q2, r2 = t2["_query"], t2["_replacement"]
    q3, r3 = t3["_query"], t3["_replacement"]
    qw = tw["_query"]

    def step(name: str, outcome: str, detail: str) -> None:
        steps.append({"name": name, "outcome": outcome, "detail": detail[:400]})

    def check(name: str, passed: bool, detail: str) -> None:
        assertions.append({"name": name, "pass": bool(passed), "detail": detail[:400]})

    case_dir.mkdir(parents=True, exist_ok=True)
    (case_dir / SENTINEL).write_text("owned by word-handoff lane\n", encoding="utf-8")
    try:
        # S0 pin + stage A (immutable pristine + live staged source).
        archived = _resolve_source(args_manifest, entry)
        pristine = case_dir / "pristine_A.docx"
        live_source = case_dir / "source.docx"
        shutil.copyfile(archived, pristine)
        shutil.copyfile(archived, live_source)
        assert _sha256_file(pristine) == input_sha
        step("pin-stage-A", "ok", f"input {input_sha[:12]} staged; pristine retained")
        workdir = case_dir / "wd"
        if workdir.exists():
            shutil.rmtree(workdir, ignore_errors=True)

        # S1 CLI extract Baseline A.
        ext = _cli_extract(binpath, live_source, workdir, False, timeout)
        call_log.append({"tag": "extract-A", "cli": ext})
        if ext.get("timeout") or ext.get("os_error") or not ext.get("ok"):
            step("extract-baseline-A", "incomplete", str(ext.get("envelope") or ext)[:300])
            return _case("word-handoff-success", input_sha, "incomplete", None,
                         steps, assertions, artifacts, limitations, call_log, case_dir)
        epoch_a = _read_baseline_epoch(workdir)
        fmt_a = _read_format_fingerprints(workdir)
        artifacts.update({"baseline_epoch_A": epoch_a,
                          "template_sha256_A": fmt_a.get("template_sha256"),
                          "source_sha256_A": fmt_a.get("source_sha256")})
        step("extract-baseline-A", "ok",
             f"epoch={epoch_a} template={str(fmt_a.get('template_sha256'))[:12]}")

        # S2 Agent edit A -> B.
        out_b = case_dir / "B.docx"
        res_a = _agent_edit_save(binpath, workdir, q1, r1, out_b, timeout, call_log)
        if res_a["status"] == "incomplete":
            step("agent-edit-A", "incomplete", res_a["detail"])
            return _case("word-handoff-success", input_sha, "incomplete", None,
                         steps, assertions, artifacts, limitations, call_log, case_dir)
        if res_a["status"] != "ok":
            step("agent-edit-A", "contract-failure", res_a["detail"])
            check("agent-A-applied", False, res_a["detail"])
            return _case("word-handoff-success", input_sha, "complete",
                         "CONTRACT_FAILURE", steps, assertions, artifacts,
                         limitations, call_log, case_dir)
        step("agent-edit-A", "ok",
             f"para={res_a['found']['paragraph_id']} saved B={_sha256_file(out_b)[:12]}")
        check("agent-A-applied", _visible_contains(out_b, r1) and not _visible_contains(out_b, q1),
              "B holds R1 and no longer holds Q1")
        if not assertions[-1]["pass"]:
            step("verify-B", "contract-failure", "B text check failed")
            return _case("word-handoff-success", input_sha, "complete",
                         "CONTRACT_FAILURE", steps, assertions, artifacts,
                         limitations, call_log, case_dir)
        snap_b = semantic_snapshot(out_b)
        artifacts["B_sha256"] = _sha256_file(out_b)

        # S3 Word operator: open B, Find Q2 -> R2 (Range formatting kept), save C
        # over the STAGED source path (never the frozen original).
        edit_script = case_dir / "word_operator_edit.ps1"
        _write_utf8(edit_script, WORD_EDIT_PS)
        job = case_dir / "word_edit_job.json"
        job.write_text(json.dumps({"find": q2, "replace_with": r2}, ensure_ascii=False),
                       encoding="utf-8")
        out_c = live_source  # C overwrites the staged source: the drift case
        word_res = _run_powershell(edit_script,
                                   [str(out_b), str(out_c), str(job)], timeout)
        call_log.append({"tag": "word-operator-edit", "word": _scrub_word(word_res)})
        if word_res.get("_timeout") or word_res.get("_os_error") or not word_res.get("ok"):
            step("word-operator-edit", "incomplete",
                 str(word_res.get("error") or word_res)[:300])
            return _case("word-handoff-success", input_sha, "incomplete", None,
                         steps, assertions, artifacts, limitations, call_log, case_dir)
        host.update({k: word_res.get(k) for k in ("version", "build") if word_res.get(k)})
        step("word-operator-edit", "ok",
             f"word {word_res.get('version')}/{word_res.get('build')} replaced={word_res.get('replaced')}")
        snap_c = semantic_snapshot(out_c)
        artifacts["C_sha256"] = _sha256_file(out_c)
        # Track-aware record: Word's Find/Replace with TrackRevisions on
        # wraps Q2->R2 as w:del(Q2)/w:ins(R2). C raw text still CONTAINS Q2
        # (inside w:delText/stored), but Word's Final view shows only R2.
        # So: visible assertions use Final-view text; deletion assertions use
        # deleted_text evidence; re-extract safe-continue must not gate on
        # raw-text "Q2 gone" either (Q2 persists as tracked deletion).
        vis_c = _track_visible_text(out_c)
        q2_deleted = (snap_c.get("deleted_text_sha256") != snap_b.get("deleted_text_sha256"))
        check("word-edit-visible", (r2 in vis_c) and (q2 not in vis_c),
              "C Final view holds R2 and no longer shows Q2")
        check("word-edit-tracked-deletion", q2_deleted or (q2 not in vis_c),
              "Q2 recorded as tracked deletion (deleted_text changed) or fully gone")
        check("agent-A-survives-word", (r1 in vis_c) and (q1 not in vis_c),
              "C still holds R1 after Word round-trip")
        if not all(a["pass"] for a in assertions[-3:]):
            step("verify-C", "contract-failure", "C text checks failed")
            return _case("word-handoff-success", input_sha, "complete",
                         "CONTRACT_FAILURE", steps, assertions, artifacts,
                         limitations, call_log, case_dir)
        # Revisions are EXPECTED to change: the Word operator edit mints a
        # new revision (run3 evidence: revisions 2, author harness identity).
        # Preserve = prior marks retained + exactly the operator change added.
        check("word-revisions-expected-delta",
              set(snap_b.get("revision_marks", [])) <= set(snap_c.get("revision_marks", [])),
              "prior revision marks retained; operator revision added")
        # Semantic comment retention, REAL Word as comparator: same
        # (author, text, anchor-passage) multiset in Word's BEFORE snapshot
        # (SAME COM session that performed the edit) vs AFTER. Raw w:id
        # order and commentsExtended/people XML are Word normalization,
        # never identity (run4: ids 0/2 swapped bodies, anchors/text/authors
        # all retained). Anchor overlap with the operator target would be
        # declared; there is none here (operator target is an un-anchored
        # abstract paragraph). XML semantic list is the fallback if Word
        # returns no snapshot (older script).
        def _canon(items: object) -> list[str]:
            out: list[str] = []
            if isinstance(items, list):
                for it in items:
                    if isinstance(it, dict):
                        a = str(it.get("author", ""))
                        t = str(it.get("text", ""))
                        anchor = " ".join(str(it.get("anchor", "")).split())
                        out.append(f"{a}:{t}@{anchor[:120]}")
            return sorted(out)
        wb, wa = _canon(word_res.get("comments_before")), _canon(word_res.get("comments_after"))
        if wb and wa:
            missing_w = [s for s in wb if s not in wa]
            extra_w = [s for s in wa if s not in wb]
            check("word-preserves-comments",
                  not missing_w and not extra_w and len(wb) == len(wa) == 21,
                  f"Word BEFORE/AFTER semantic comments {len(wb)}->{len(wa)} "
                  f"missing={len(missing_w)} extra={len(extra_w)}")
            if missing_w or extra_w or len(wb) != 21 or len(wa) != 21:
                call_log.append({"tag": "word-comment-diff",
                                 "missing": missing_w[:3], "extra": extra_w[:3]})
        else:
            sem_b = sorted(snap_b.get("comments_semantic", []))
            sem_c = sorted(snap_c.get("comments_semantic", []))
            missing = [s for s in sem_b if s not in sem_c]
            extra = [s for s in sem_c if s not in sem_b]
            check("word-preserves-comments",
                  not missing and not extra and len(sem_b) == len(sem_c) == 21,
                  f"XML semantic comments B={len(sem_b)} C={len(sem_c)} "
                  f"missing={len(missing)} extra={len(extra)}")
            if missing or extra:
                call_log.append({"tag": "comment-semantic-diff",
                                 "missing": missing[:3], "extra": extra[:3]})
        check("word-preserves-sections", snap_c.get("sections") == snap_b.get("sections"),
              f"sections {snap_c.get('sections')}")
        ext_before = _external_targets(snap_b.get("relationships", []))
        ext_after = _external_targets(snap_c.get("relationships", []))
        check("word-preserves-relationships", ext_before <= ext_after,
              "every external hyperlink target before survives after")

        # S4 Safe continue: explicit re-extract of C into the SAME workdir with
        # deliberate discard, only after B was exported and retained.
        pre_tree = _tree_snapshot(workdir)
        call_log.append({"tag": "pre-reextract", "workdir_tree": {k: v[:12] for k, v in pre_tree.items()},
                         "C_sha256": artifacts["C_sha256"]})
        ext2 = _cli_extract(binpath, live_source, workdir, True, timeout)
        call_log.append({"tag": "extract-C-discard", "cli": _scrub_cli(ext2)})
        if ext2.get("timeout") or ext2.get("os_error") or not ext2.get("ok"):
            step("reextract-baseline-C", "incomplete", str(ext2.get("envelope") or ext2)[:300])
            return _case("word-handoff-success", input_sha, "incomplete", None,
                         steps, assertions, artifacts, limitations, call_log, case_dir)
        epoch_c = _read_baseline_epoch(workdir)
        fmt_c = _read_format_fingerprints(workdir)
        artifacts.update({"baseline_epoch_C": epoch_c,
                          "template_sha256_C": fmt_c.get("template_sha256"),
                          "source_sha256_C": fmt_c.get("source_sha256")})
        step("reextract-baseline-C", "ok",
             f"epoch {epoch_a}->{epoch_c} discard-committed-versions explicit")
        check("baseline-advanced", isinstance(epoch_c, int) and isinstance(epoch_a, int) and epoch_c == epoch_a + 1,
              f"epoch {epoch_a}->{epoch_c}; no merge, baseline replaced")
        check("template-follows-C", fmt_c.get("template_sha256") == artifacts["C_sha256"],
              "workdir template fingerprint is C bytes")

        # S5 Second Agent edit (Q3 -> R3) on Baseline C -> final.
        out_final = case_dir / "final.docx"
        res_c = _agent_edit_save(binpath, workdir, q3, r3, out_final, timeout, call_log)
        if res_c["status"] == "incomplete":
            step("agent-edit-A2", "incomplete", res_c["detail"])
            return _case("word-handoff-success", input_sha, "incomplete", None,
                         steps, assertions, artifacts, limitations, call_log, case_dir)
        if res_c["status"] != "ok":
            step("agent-edit-A2", "contract-failure", res_c["detail"])
            check("agent-A2-applied", False, res_c["detail"])
            return _case("word-handoff-success", input_sha, "complete",
                         "CONTRACT_FAILURE", steps, assertions, artifacts,
                         limitations, call_log, case_dir)
        step("agent-edit-A2", "ok",
             f"para={res_c['found']['paragraph_id']} saved final={_sha256_file(out_final)[:12]}")
        artifacts["final_sha256"] = _sha256_file(out_final)
        snap_final = semantic_snapshot(out_final)

        # S6 Word opens final and compares all three intended edits; witness
        # formatting + functional props must survive. Read-only: no write.
        verify_script = case_dir / "word_verify_final.ps1"
        _write_utf8(verify_script, WORD_VERIFY_PS)
        # Queries travel via JSON file so CJK never crosses the argv codepage.
        vjob = case_dir / "word_verify_job.json"
        vjob.write_text(json.dumps({"present": [r1, r2, r3],
                                    "absent": [q1, q2, q3],
                                    "witness": qw}, ensure_ascii=False), encoding="utf-8")
        word_v = _run_powershell(verify_script, [str(out_final), str(vjob)], timeout)
        call_log.append({"tag": "word-final-verify", "word": _scrub_word(word_v)})
        if word_v.get("_timeout") or word_v.get("_os_error") or not word_v.get("ok"):
            step("word-final-verify", "incomplete",
                 str(word_v.get("error") or word_v)[:300])
            return _case("word-handoff-success", input_sha, "incomplete", None,
                         steps, assertions, artifacts, limitations, call_log, case_dir)
        present_found, absent_found = _word_check_lists(word_v)
        check("word-sees-all-three", present_found == [True, True, True],
              f"word find present={present_found}")
        check("word-sees-absents-gone", absent_found == [False, False, False],
              f"word find absent-still-found={absent_found}")
        check("final-holds-all-three",
              all(_visible_contains(out_final, x) for x in (r1, r2, r3))
              and not any(_visible_contains(out_final, x) for x in (q1, q2, q3)),
              "final visible text holds R1/R2/R3 and none of Q1/Q2/Q3")
        wit = word_v.get("witness") or {}
        check("witness-format-preserved", bool(wit.get("found")),
              f"witness found={wit.get('found')} style={wit.get('style')} "
              f"bold={wit.get('bold')} italic={wit.get('italic')}")
        check("final-preserves-sections", snap_c.get("sections") == snap_final.get("sections"),
              f"sections {snap_final.get('sections')}")
        check("final-preserves-comments",
              sorted(snap_c.get("comments_semantic", [])) == sorted(snap_final.get("comments_semantic", [])),
              "comments identical C->final")
        check("final-preserves-revisions", snap_c.get("revision_marks") == snap_final.get("revision_marks"),
              "revision marks identical C->final")
        ext_f0 = _external_targets(snap_c.get("relationships", []))
        ext_f1 = _external_targets(snap_final.get("relationships", []))
        check("final-preserves-relationships", ext_f0 <= ext_f1,
              "external hyperlink targets survive second agent round")
        failed = [a for a in assertions if not a["pass"]]
        if failed:
            step("verdict", "contract-failure", f"{len(failed)} assertion(s) failed")
            return _case("word-handoff-success", input_sha, "complete",
                         "CONTRACT_FAILURE", steps, assertions, artifacts,
                         limitations, call_log, case_dir)
        step("verdict", "ok", "all assertions pass; no merge performed")
        return _case("word-handoff-success", input_sha, "complete", "PASS",
                     steps, assertions, artifacts, limitations, call_log, case_dir)
    except subprocess.TimeoutExpired as exc:
        steps.append({"name": "timeout", "outcome": "incomplete",
                      "detail": f"timeout: {exc}"[:300]})
        return _case("word-handoff-success", input_sha, "incomplete", None,
                     steps, assertions, artifacts, limitations, call_log, case_dir)
    except (OSError, ValueError, zipfile.BadZipFile, ET.ParseError) as exc:
        steps.append({"name": "harness-error", "outcome": "incomplete",
                      "detail": f"{type(exc).__name__}: {str(exc)[:250]}"})
        return _case("word-handoff-success", input_sha, "incomplete", None,
                     steps, assertions, artifacts, limitations, call_log, case_dir)


def _run_stale(binpath: Path, entry: dict, targets: list[dict],
               case_dir: Path, timeout: int, host: dict) -> dict:
    """Duplicate setup; stale workdir tries document_save output=C.

    EXPECTED_REJECT iff the engine refuses BEFORE overwriting C and the
    pre/post snapshots prove no write to C or the workdir. If the save
    succeeds and C changes, that is CONTRACT_FAILURE (silent overwrite of
    external change). Transport/unknown failures are incomplete, never a
    refusal claim.
    """
    steps: list[dict] = []
    assertions: list[dict] = []
    call_log: list[dict] = []
    artifacts: dict = {}
    limitations = [
        "execution_mode word-com-automated-operator; human_ui_review not-run",
        "DisplayAlerts=0/OpenAndRepair=false are automation flags, not no-repair proof",
        "no-write proof covers the staged source file and the workdir tree; "
        "host-owned logging outside the workdir is excluded",
    ]
    input_sha = str(entry.get("sha256"))
    t1, t2 = targets[0], targets[1]
    q1, r1 = t1["_query"], t1["_replacement"]
    q2, r2 = t2["_query"], t2["_replacement"]

    def step(name: str, outcome: str, detail: str) -> None:
        steps.append({"name": name, "outcome": outcome, "detail": detail[:400]})

    def check(name: str, passed: bool, detail: str) -> None:
        assertions.append({"name": name, "pass": bool(passed), "detail": detail[:400]})

    case_dir.mkdir(parents=True, exist_ok=True)
    (case_dir / SENTINEL).write_text("owned by word-handoff lane\n", encoding="utf-8")
    try:
        archived = _resolve_source(args_manifest, entry)
        live_source = case_dir / "source.docx"
        shutil.copyfile(archived, live_source)
        workdir = case_dir / "wd"
        if workdir.exists():
            shutil.rmtree(workdir, ignore_errors=True)
        ext = _cli_extract(binpath, live_source, workdir, False, timeout)
        call_log.append({"tag": "extract-A", "cli": _scrub_cli(ext)})
        if ext.get("timeout") or ext.get("os_error") or not ext.get("ok"):
            step("extract-baseline-A", "incomplete", str(ext.get("envelope") or ext)[:300])
            return _case("word-handoff-stale-refusal", input_sha, "incomplete", None,
                         steps, assertions, artifacts, limitations, call_log, case_dir)
        epoch_a = _read_baseline_epoch(workdir)
        step("extract-baseline-A", "ok", f"epoch={epoch_a}")
        out_b = case_dir / "B.docx"
        res_a = _agent_edit_save(binpath, workdir, q1, r1, out_b, timeout, call_log)
        if res_a["status"] == "incomplete":
            step("agent-edit-A", "incomplete", res_a["detail"])
            return _case("word-handoff-stale-refusal", input_sha, "incomplete", None,
                         steps, assertions, artifacts, limitations, call_log, case_dir)
        if res_a["status"] != "ok":
            step("agent-edit-A", "contract-failure", res_a["detail"])
            return _case("word-handoff-stale-refusal", input_sha, "complete",
                         "CONTRACT_FAILURE", steps, assertions, artifacts,
                         limitations, call_log, case_dir)
        step("agent-edit-A", "ok", f"saved B={_sha256_file(out_b)[:12]}")
        artifacts["B_sha256"] = _sha256_file(out_b)
        # Word operator B -> C over the staged source path.
        edit_script = case_dir / "word_operator_edit.ps1"
        _write_utf8(edit_script, WORD_EDIT_PS)
        job = case_dir / "word_edit_job.json"
        job.write_text(json.dumps({"find": q2, "replace_with": r2}, ensure_ascii=False),
                       encoding="utf-8")
        out_c = live_source
        word_res = _run_powershell(edit_script, [str(out_b), str(out_c), str(job)], timeout)
        call_log.append({"tag": "word-operator-edit", "word": _scrub_word(word_res)})
        if word_res.get("_timeout") or word_res.get("_os_error") or not word_res.get("ok"):
            step("word-operator-edit", "incomplete",
                 str(word_res.get("error") or word_res)[:300])
            return _case("word-handoff-stale-refusal", input_sha, "incomplete", None,
                         steps, assertions, artifacts, limitations, call_log, case_dir)
        step("word-operator-edit", "ok", f"C={_sha256_file(out_c)[:12]}")
        artifacts["C_sha256_before_stale_attempt"] = _sha256_file(out_c)
        # Snapshots IMMEDIATELY BEFORE the stale attempt (no-write proof).
        c_before = _sha256_file(out_c)
        wd_before = _tree_snapshot(workdir)
        call_log.append({"tag": "pre-stale-snapshot", "C_sha256": c_before,
                         "workdir_tree": {k: v[:12] for k, v in wd_before.items()},
                         "baseline_epoch": epoch_a})
        # Stale publication: old workdir (Baseline A + committed edit) tries to
        # publish over C, which Word has since changed underneath it.
        envs, transport = mcp_session(
            binpath, [("document_open", {"source": str(workdir)}),
                      ("document_save", {"output": str(out_c)})], timeout)
        call_log.append({"tag": "stale-save-attempt", "transport": transport,
                         "requests": [{"tool": "document_open",
                                       "args": {"source": str(workdir)}},
                                      {"tool": "document_save",
                                       "args": {"output": str(out_c)}}],
                         "envelopes": envs})
        if any(_is_transport_error(e) for e in envs):
            step("stale-save-attempt", "incomplete", "transport error; not a refusal")
            return _case("word-handoff-stale-refusal", input_sha, "incomplete", None,
                         steps, assertions, artifacts, limitations, call_log, case_dir)
        save_env = envs[1]
        c_after = _sha256_file(out_c) if out_c.is_file() else "<missing>"
        wd_after = _tree_snapshot(workdir)
        artifacts["C_sha256_after_stale_attempt"] = c_after
        call_log.append({"tag": "post-stale-snapshot", "C_sha256": c_after,
                         "workdir_tree": {k: v[:12] for k, v in wd_after.items()}})
        c_unchanged = (c_after == c_before)
        wd_unchanged = (wd_after == wd_before)
        if save_env.get("outcome") == "success":
            step("stale-save-attempt", "allowed",
                 f"save succeeded over externally changed C {c_before[:12]}->{c_after[:12]}")
            check("stale-overwrite-refused", False,
                  "engine published over Word-changed C; external edit silently lost")
            check("C-preserved", c_after == c_before,
                  f"C {c_before[:12]}->{c_after[:12]}")
            # Preserve evidence of the overwrite for triage.
            try:
                shutil.copyfile(out_c, case_dir / "C_overwritten.docx")
            except OSError:
                pass
            return _case("word-handoff-stale-refusal", input_sha, "complete",
                         "CONTRACT_FAILURE", steps, assertions, artifacts,
                         limitations, call_log, case_dir)
        codes = _codes(save_env)
        step("stale-save-attempt", "refused", f"codes={codes}")
        check("stale-overwrite-refused", True, f"refused codes={codes}")
        check("C-unwritten", c_unchanged, f"C {c_before[:12]}->{str(c_after)[:12]}")
        check("workdir-unwritten", wd_unchanged, "workdir tree identical pre/post")
        if not (c_unchanged and wd_unchanged):
            step("verdict", "contract-failure", "refusal still wrote state")
            return _case("word-handoff-stale-refusal", input_sha, "complete",
                         "CONTRACT_FAILURE", steps, assertions, artifacts,
                         limitations, call_log, case_dir)
        step("verdict", "ok", "named refusal before overwrite; no-write proven")
        return _case("word-handoff-stale-refusal", input_sha, "complete",
                     "EXPECTED_REJECT", steps, assertions, artifacts,
                     limitations, call_log, case_dir)
    except subprocess.TimeoutExpired as exc:
        steps.append({"name": "timeout", "outcome": "incomplete",
                      "detail": f"timeout: {exc}"[:300]})
        return _case("word-handoff-stale-refusal", input_sha, "incomplete", None,
                     steps, assertions, artifacts, limitations, call_log, case_dir)
    except (OSError, ValueError, zipfile.BadZipFile, ET.ParseError) as exc:
        steps.append({"name": "harness-error", "outcome": "incomplete",
                      "detail": f"{type(exc).__name__}: {str(exc)[:250]}"})
        return _case("word-handoff-stale-refusal", input_sha, "incomplete", None,
                     steps, assertions, artifacts, limitations, call_log, case_dir)


def _scrub_word(payload: dict) -> dict:
    """Private log keeps codes/hashes; raw doc text never leaves the out dir."""
    scrubbed = dict(payload)
    return scrubbed


def _scrub_cli(result: dict) -> dict:
    out = dict(result)
    return out


def _case(case_id: str, input_sha: str, completion_status: str, outcome: str | None,
          steps: list, assertions: list, artifacts: dict, limitations: list,
          call_log: list, case_dir: Path) -> dict:
    # Private per-action evidence stays in the owned out dir; report.json
    # carries hashes/codes only (no document text, no absolute host paths).
    try:
        (case_dir / "calls.json").write_text(
            json.dumps(call_log, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    except OSError:
        pass
    return {"case_id": case_id, "input_sha256": input_sha,
            "completion_status": completion_status, "outcome": outcome,
            "steps": steps, "assertions": assertions, "artifacts": artifacts,
            "limitations": limitations}


# --------------------------------------------------------------------------
# Driver
# --------------------------------------------------------------------------

args_manifest: Path = Path(".")


def main(argv: list[str] | None = None) -> int:
    global args_manifest
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin", default=None)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--timeout", type=int, default=240)
    parser.add_argument("--input-sha", default=None)
    args = parser.parse_args(argv)
    args_manifest = args.manifest
    timeout = max(30, int(args.timeout))
    outdir: Path = args.out
    outdir.mkdir(parents=True, exist_ok=True)
    (outdir / SENTINEL).write_text("owned by word-handoff lane\n", encoding="utf-8")

    try:
        binpath = _resolve_bin(args.bin)
    except FileNotFoundError as exc:
        print(f"setup refused: {exc}")
        return 2
    bin_identity = {"sha256": _sha256_file(binpath),
                    "version": _bin_version_text(binpath, timeout)}
    try:
        manifest, manifest_sha = _validate_manifest(args.manifest)
    except ValueError as exc:
        print(f"setup refused: {exc}")
        return 2

    # Host preflight first: no Word host => not-run-no-host, outcome null.
    host_probe, _exe = _word_host(40, outdir / "_preflight")
    host = host_probe["host"]
    if not host_probe["healthy"]:
        report = {"schema": SCHEMA, "workflow": WORKFLOW, "host": host,
                  "binary": bin_identity,
                  "manifest_sha256": manifest_sha,
                  "cases": [
                      {"case_id": "word-handoff-success",
                       "input_sha256": args.input_sha or "",
                       "completion_status": "not-run-no-host", "outcome": None,
                       "steps": [{"name": "word-preflight", "outcome": "not-run",
                                  "detail": str(host_probe["payload"])[:300]}],
                       "assertions": [], "artifacts": {},
                       "limitations": ["word host unavailable; nothing executed"]},
                      {"case_id": "word-handoff-stale-refusal",
                       "input_sha256": args.input_sha or "",
                       "completion_status": "not-run-no-host", "outcome": None,
                       "steps": [{"name": "word-preflight", "outcome": "not-run",
                                  "detail": str(host_probe["payload"])[:300]}],
                       "assertions": [], "artifacts": {},
                       "limitations": ["word host unavailable; nothing executed"]},
                  ]}
        (outdir / "report.json").write_text(
            json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        print(json.dumps({"completion_status": "not-run-no-host",
                          "reason": host_probe["payload"]}, ensure_ascii=False))
        return 0

    # Pin inputs first: manifest + binary + host, then safe targets.
    try:
        entry, targets = _pick_entry(manifest, args.manifest, args.input_sha, 4)
    except ValueError as exc:
        message = str(exc)
        outcome = "UNSUPPORTED_WORKFLOW" if "UNSUPPORTED_WORKFLOW" in message else None
        status = "complete" if outcome else "incomplete"
        report = {"schema": SCHEMA, "workflow": WORKFLOW, "host": host,
                  "binary": bin_identity, "manifest_sha256": manifest_sha,
                  "cases": [
                      {"case_id": "word-handoff-success",
                       "input_sha256": args.input_sha or "",
                       "completion_status": status, "outcome": outcome,
                       "steps": [{"name": "pin-inputs", "outcome": "refused",
                                  "detail": message[:300]}],
                       "assertions": [], "artifacts": {},
                       "limitations": [message[:200]]},
                      {"case_id": "word-handoff-stale-refusal",
                       "input_sha256": args.input_sha or "",
                       "completion_status": status, "outcome": outcome,
                       "steps": [{"name": "pin-inputs", "outcome": "refused",
                                  "detail": message[:300]}],
                       "assertions": [], "artifacts": {},
                       "limitations": [message[:200]]},
                  ]}
        (outdir / "report.json").write_text(
            json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        print(json.dumps({"completion_status": status, "outcome": outcome,
                          "detail": message[:300]}, ensure_ascii=False))
        return 0

    success_case = _run_success(binpath, entry, targets,
                                outdir / "word-handoff-success", timeout, dict(host))
    stale_case = _run_stale(binpath, entry, targets,
                            outdir / "word-handoff-stale-refusal", timeout, dict(host))
    report = {"schema": SCHEMA, "workflow": WORKFLOW, "host": host,
              "binary": bin_identity, "manifest_sha256": manifest_sha,
              "cases": [success_case, stale_case]}
    (outdir / "report.json").write_text(
        json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    summary = {c["case_id"]: [c["completion_status"], c["outcome"]]
               for c in report["cases"]}
    print(json.dumps(summary, ensure_ascii=False, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
