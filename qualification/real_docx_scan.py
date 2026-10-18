#!/usr/bin/env python3
"""Resumable classified sweep over the frozen real-DOCX corpus (v1).

Reads the pinned 217-document manifest (owned by qualification/real_docx_corpus.py;
local fallbacks below implement the same schema when that module is absent),
drives the real Rust binary through the five-tool MCP surface, and classifies
each document into exactly one completed outcome:

- PASS:             pristine open/save/reopen is byte-identical AND a
                    source-mapped single-char edit reproduces the independently
                    precomputed word/document.xml byte-for-byte.
- EXPECTED_REJECT:  an unsupported target/doc was correctly refused: the
                    refusal names a location, nothing was written, and the
                    source structure justifies the refusal (codes alone never
                    decide). A zero-match find is NEVER an EXPECTED_REJECT.
- NOT_APPLICABLE:   only when no safe eligible test target exists (genuine
                    absence of an eligible target, kept separate).
- CONTRACT_FAILURE: find said editable but the deterministic edit refused; an
                    accepted edit but the deterministic save refused; pristine
                    drift, output corruption, or state mismatch.

Timeouts, process failures, and insufficient disk yield completion_status
INCOMPLETE with category null and an explicit stage -- never a false PASS or
a false refusal. Manifest drift stops the whole scan.

Strict eligible-target rule (NOT_APPLICABLE iff no safe target): a safe
w:t element must sit in a paragraph whose ancestors are only w:r/w:hyperlink/
w:p, whose paragraph carries no opaque, revision, field, or SDT structure,
whose literal alphanumeric/CJK substring (8-24 chars, no XML-escapable bytes)
occurs exactly once across all w:t text of word/document.xml, exactly once
across every paragraph concatenation (catches duplicate spanning runs), zero
times across the other word/*.xml parts, and exactly once in the raw member
bytes; the replacement changes one equal-byte-length char. The expected
output member is computed from the source node byte range BEFORE document_find
runs, so a wrong-occurrence engine edit cannot look exact.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import hashlib
import json
import os
import re
import shutil
import subprocess
import threading
import time
import xml.etree.ElementTree as ET
import zipfile
from pathlib import Path

SCAN_SCHEMA = "real-docx-scan-1"
RULES_VERSION = "real-docx-scan-rules-4"
CORPUS_ID = "real-docx-corpus-v1"
EXPECTED_MANIFEST_SCHEMA = "real-docx-corpus-manifest-1"
EXPECTED_DOCUMENT_COUNT = 217

PASS = "PASS"
EXPECTED_REJECT = "EXPECTED_REJECT"
NOT_APPLICABLE = "NOT_APPLICABLE"
CONTRACT_FAILURE = "CONTRACT_FAILURE"
COMPLETED = (PASS, EXPECTED_REJECT, NOT_APPLICABLE, CONTRACT_FAILURE)

SURFACE_TOOLS = ("document_open", "document_find", "document_edit", "document_save", "document_status")
FIND_PAGE_LIMIT = 200
W_NS = "http://schemas.openxmlformats.org/wordprocessingml/2006/main"
QUERY_RE = re.compile(r"[0-9A-Za-z\u4e00-\u9fff]{8,24}")
SENTINEL = ".real-docx-scan-owned"
MIN_FREE_BYTES = 2 * 1024**3
EVIDENCE_MAX_BYTES = 32 * 1024**2
# Paragraph-level locks: any descendant with one of these local names makes
# the paragraph ineligible for a safe target (strict: field runs included).
OPAQUE_LOCALS = frozenset({
    "drawing", "pict", "object", "fldSimple", "oMath", "oMathPara",
    "sdt", "smartTag", "proofErr", "txbxContent", "AlternateContent",
})
FIELD_LOCALS = frozenset({"fldChar", "instrText", "fldSimple"})
REVISION_LOCALS = frozenset({"ins", "del", "moveFrom", "moveTo", "delText"})
# Run children that keep a run on the editable surface (rPr allowed separately).
KNOWN_RUN_INLINE = frozenset({
    "t", "delText", "tab", "br", "cr", "noBreakHyphen", "softHyphen", "sym",
    "commentReference", "footnoteRef", "endnoteRef", "annotationRef",
    "separator", "continuationSeparator", "lastRenderedPageBreak",
})


class ManifestDrift(RuntimeError):
    """The frozen archive changed under the scan; stop everything."""


# --------------------------------------------------------------------------
# Corpus interface (owned by qualification/real_docx_corpus.py; fallbacks)
# --------------------------------------------------------------------------

try:  # owned by the CorpusFreeze sibling; prefer it when present
    from qualification.real_docx_corpus import (  # type: ignore
        load_manifest as _shared_load,
        validate_manifest as _shared_validate,
        resolve_source as _shared_resolve,
    )
except ImportError:
    try:
        from .real_docx_corpus import (  # type: ignore
            load_manifest as _shared_load,
            validate_manifest as _shared_validate,
            resolve_source as _shared_resolve,
        )
    except ImportError:
        _shared_load = _shared_validate = _shared_resolve = None


def _fallback_load(path: Path) -> dict:
    return json.loads(path.read_text(encoding="utf-8"))
def _fallback_validate(path: Path) -> dict:
    try:
        manifest = _fallback_load(path)
    except (OSError, json.JSONDecodeError) as exc:
        return {"ok": False, "problems": [f"unreadable manifest: {exc}"],
                "document_count": 0, "manifest_sha256": ""}
    problems: list[str] = []
    if manifest.get("schema") != EXPECTED_MANIFEST_SCHEMA:
        problems.append(f"schema={manifest.get('schema')!r}")
    if manifest.get("corpus_id") != CORPUS_ID:
        problems.append(f"corpus_id={manifest.get('corpus_id')!r}")
    docs = manifest.get("documents") or []
    if manifest.get("document_count") != EXPECTED_DOCUMENT_COUNT or len(docs) != EXPECTED_DOCUMENT_COUNT:
        problems.append(f"document_count={manifest.get('document_count')}/{len(docs)}")
    seen: set[str] = set()
    for entry in docs:
        did = entry.get("document_id", "")
        if not did or did in seen:
            problems.append(f"duplicate/empty document_id: {did!r}")
            break
        seen.add(did)
        sp = entry.get("source_path", "")
        if not sp or os.path.isabs(sp) or ".." in Path(sp).parts:
            problems.append(f"bad source_path for {did}")
            break
        if not entry.get("sha256") or not entry.get("size_bytes"):
            problems.append(f"missing hash/size for {did}")
            break
    digest = hashlib.sha256(path.read_bytes()).hexdigest()
    return {"ok": not problems, "problems": problems,
            "document_count": len(docs), "manifest_sha256": digest}


def _fallback_resolve(manifest_path: Path, entry: dict) -> Path:
    base = manifest_path.parent.resolve()
    candidate = (base / entry["source_path"]).resolve()
    if candidate != base and base not in candidate.parents:
        raise ValueError(f"source_path escapes archive: {entry['source_path']!r}")
    return candidate


def load_manifest(path: Path) -> dict:
    if _shared_load is not None:
        return _shared_load(path)
    return _fallback_load(path)


def validate_manifest(path: Path) -> dict:
    if _shared_validate is not None:
        return _shared_validate(path)
    return _fallback_validate(path)


def resolve_source(manifest_path: Path, entry: dict) -> Path:
    if _shared_resolve is not None:
        return _shared_resolve(manifest_path, entry)
    return _fallback_resolve(manifest_path, entry)


# --------------------------------------------------------------------------
# Hashing / packages / binary
# --------------------------------------------------------------------------

def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def member_shas(docx_path: Path) -> dict[str, str]:
    """SHA-256 of every member's uncompressed bytes, keyed by member name."""
    with zipfile.ZipFile(docx_path) as archive:
        return {n: hashlib.sha256(archive.read(n)).hexdigest()
                for n in sorted(archive.namelist())}


def compare_packages(src: dict[str, str], out: dict[str, str]) -> dict:
    changed = sorted(n for n in src if n in out and src[n] != out[n])
    return {"same": src == out, "added": sorted(set(out) - set(src)),
            "removed": sorted(set(src) - set(out)), "changed": changed}


def _resolve_bin(explicit: str | None) -> Path:
    if explicit:
        candidate = Path(explicit)
        if candidate.is_file():
            return candidate
        raise FileNotFoundError(f"--bin not found: {explicit}")
    env = os.environ.get("DOCX2TYPED_BIN")
    if env and Path(env).is_file():
        return Path(env)
    found = shutil.which("docx2typed")
    if found:
        return Path(found)
    repo = Path(__file__).resolve().parents[1] / "target" / "release"
    for name in ("docx2typed", "docx2typed.exe"):
        if (repo / name).is_file():
            return repo / name
    raise FileNotFoundError("no docx2typed binary: pass --bin or set DOCX2TYPED_BIN")


def _bin_version_text(binpath: Path, timeout: int) -> str:
    try:
        proc = subprocess.run([str(binpath), "--version"], capture_output=True,
                              text=True, encoding="utf-8", errors="replace", timeout=timeout)
        return ((proc.stdout or "") + (proc.stderr or "")).strip()[:300] or "unavailable"
    except (OSError, subprocess.TimeoutExpired, ValueError):
        return "unavailable"


def bin_identity(binpath: Path, timeout: int) -> dict:
    return {"path": str(binpath), "sha256": _sha256_file(binpath),
            "version": _bin_version_text(binpath, timeout)}


# --------------------------------------------------------------------------
# MCP transport (legacy line protocol; one process == one session)
# --------------------------------------------------------------------------

def mcp_session(binpath: Path, calls: list[tuple[str, dict]], timeout: int) -> tuple[list[dict], dict]:
    """Run calls in one process; returns (envelopes aligned to calls, log)."""
    lines = [json.dumps({"tool": name, "args": args}, ensure_ascii=True) for name, args in calls]
    started = time.time()
    proc = subprocess.run([str(binpath), "mcp"], input="\n".join(lines) + "\n",
                          capture_output=True, text=True, encoding="utf-8",
                          errors="replace", timeout=timeout)
    elapsed = time.time() - started
    outs: list[dict] = []
    for line in proc.stdout.splitlines():
        text = line.strip()
        if text.startswith("OK "):
            try:
                outs.append(json.loads(text[3:]).get("structuredContent", {}))
            except json.JSONDecodeError:
                outs.append({"operation": "?", "outcome": "transport-error",
                             "diagnostics": [{"code": "bad-json", "message": text[3:103]}]})
        elif text.startswith("ERR"):
            outs.append({"operation": "?", "outcome": "transport-error",
                         "diagnostics": [{"code": "transport", "message": text[:300]}]})
    while len(outs) < len(calls):
        outs.append({"operation": calls[len(outs)][0], "outcome": "transport-error",
                     "diagnostics": [{"code": "missing-reply", "message": "no reply line"}]})
    log = {"seconds": round(elapsed, 1), "returncode": proc.returncode,
           "stderr_tail": (proc.stderr or "")[-2000:]}
    return outs[:len(calls)], log


def _codes(envelope: dict) -> list[str]:
    return [str(d.get("code", "")) for d in (envelope.get("diagnostics") or []) if isinstance(d, dict)]


def _is_transport_error(envelope: dict) -> bool:
    if (envelope.get("outcome") or "") == "transport-error":
        return True
    return any(str(d.get("code", "")) in ("transport", "bad-json", "missing-reply")
               for d in (envelope.get("diagnostics") or []) if isinstance(d, dict))


def _evidence_head(envelope: dict) -> str:
    # ponytail: public rows stay anonymous/hash-only; diagnostic text (which
    # may quote document content) lives in the private per-doc log only.
    diags = envelope.get("diagnostics") or []
    first = diags[0] if diags and isinstance(diags[0], dict) else {}
    return f"{first.get('code', '')}"[:120]


def _hash_map_digest(mapping: dict[str, str]) -> str:
    canonical = json.dumps(mapping, sort_keys=True, separators=(",", ":"),
                           ensure_ascii=True)
    return hashlib.sha256(canonical.encode("ascii")).hexdigest()

# --------------------------------------------------------------------------
# Source XML analysis: safe targets, unsupported probes, features
# --------------------------------------------------------------------------

def _local(tag: object) -> str:
    text = tag.tag if isinstance(tag, ET.Element) else tag
    assert isinstance(text, str)
    if "}" in text:
        return text.rsplit("}", 1)[-1]
    return text.rsplit(":", 1)[-1]


def _scan_tags(xml: bytes) -> list[dict]:
    """Minimal structural tag scan: comments/PIs/CDATA skipped, quoted attrs
    honored, self-closing detected, prefix-agnostic local names + byte ranges.
    Mirrors the engine xml_walker discipline (never str offsets)."""
    tags: list[dict] = []
    pos = 0
    size = len(xml)
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
        cursor = start + 1
        quote: int | None = None
        end = -1
        while cursor < size:
            byte = xml[cursor]
            if quote is not None:
                if byte == quote:
                    quote = None
            elif byte in (34, 39):
                quote = byte
            elif byte == 62:
                end = cursor
                break
            cursor += 1
        if end < 0:
            break
        raw = xml[start + 1:end]
        closing = raw.startswith(b"/")
        body = raw[1:] if closing else raw
        body = body.strip()
        self_closing = (not closing) and body.endswith(b"/")
        if self_closing:
            body = body[:-1].rstrip()
        name = body.split(None, 1)[0] if body else b""
        try:
            name_s = name.decode("ascii")
        except UnicodeDecodeError:
            pos = end + 1
            continue
        if not name_s or name_s.startswith("!"):
            pos = end + 1
            continue
        local = name_s.split(":", 1)[-1]
        tags.append({"name": local, "closing": closing,
                     "self_closing": self_closing, "start": start,
                     "open_end": end + 1, "end": end + 1})
        pos = end + 1
    return tags


def _parse_xml(data: bytes) -> ET.Element | None:
    try:
        return ET.fromstring(data)
    except ET.ParseError:
        return None


def _wt_ranges_expat(docxml: bytes, want: int) -> list[tuple[int, int]] | None:
    """Byte ranges of w-namespace w:t elements via expat namespace callbacks.

    Proper prefix->namespace resolution (no hardcoded `w` assumption beyond
    resolving whatever prefix is BOUND to the WordprocessingML namespace at
    each element): expat reports the resolved (uri, localname) pair, so an
    `m:t` (math namespace) never pairs with a `w:t` even though the local
    name is identical. Byte offsets come from CurrentByteIndex at each
    callback, measured on the same bytes -- no third-party dependency."""
    import xml.parsers.expat as expat
    ranges: list[tuple[int, int]] = []
    stack: list[int] = []
    failed: list[str] = []
    parser = expat.ParserCreate(namespace_separator=" ")

    def start_handler(name: str, _attrs: dict) -> None:
        uri, _, local = name.rpartition(" ")
        if uri == W_NS and local == "t":
            # CurrentByteIndex at StartElementHandler is the `<` of the open
            # tag; inner start is just past its `>`.
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
            # CurrentByteIndex at EndElementHandler is the `<` of the close
            # tag (== inner end). A self-closing element ALSO fires EndElement
            # immediately after Start; skip it (already recorded empty range).
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
    opaque = bool(tags & OPAQUE_LOCALS)
    revision = bool(tags & REVISION_LOCALS)
    field = bool(tags & FIELD_LOCALS)
    sdt = "sdt" in tags
    return {"has_opaque": opaque, "has_revision": revision,
            "has_field": field, "has_sdt": sdt}


def _run_children_ok(run: ET.Element) -> bool:
    for child in run:
        if not _wns(child):
            continue
        name = _local(child.tag)
        if name == "rPr":
            continue
        if name not in KNOWN_RUN_INLINE:
            return False
    return True


def replacement_for(query: str) -> str:
    """One equal-byte-length char swap (ASCII alnum preferred, else CJK)."""
    for index, char in enumerate(query):
        if "0" <= char <= "9":
            return query[:index] + str((int(char) + 1) % 10) + query[index + 1:]
        if "a" <= char <= "z":
            return query[:index] + chr((ord(char) - 97 + 1) % 26 + 97) + query[index + 1:]
        if "A" <= char <= "Z":
            return query[:index] + chr((ord(char) - 65 + 1) % 26 + 65) + query[index + 1:]
    last = query[-1]
    swap = "\u4e4b" if last != "\u4e4b" else "\u7684"
    candidate = query[:-1] + swap
    assert len(candidate.encode("utf-8")) == len(query.encode("utf-8"))
    return candidate


def _candidate_queries(text: str) -> list[str]:
    if QUERY_RE.fullmatch(text or ""):
        return [text]
    runs = [m.group(0) for m in QUERY_RE.finditer(text or "")]
    out: list[str] = []
    for run in runs:
        out.append(run[:24] if len(run) > 24 else run)
    return out


def _xpath(element: ET.Element, parents: dict) -> str:
    parts: list[str] = []
    node: ET.Element | None = element
    while node is not None:
        name = _local(node.tag)
        parent = parents.get(node)
        if parent is None:
            parts.append(f"/w:{name}")
            break
        same = [c for c in list(parent) if _local(c.tag) == name]
        parts.append(f"/w:{name}[{same.index(node) + 1}]" if len(same) > 1 else f"/w:{name}")
        node = parent
    return "".join(reversed(parts))


def _locate_paragraphs(docxml: bytes) -> tuple[list[dict] | None, str]:
    """Byte-exact paragraph locator: direct P{n}, table T/R/C/P, sdt S{n}.P{p}.
    Mirrors engine scan_document: direct ids count ONLY direct body children;
    table/box/sdt paragraphs carry their own ids and never shift P{n}."""
    tags = _scan_tags(docxml)
    body_open = body_close = -1
    depth = 0
    stack: list[str] = []
    located: list[dict] = []
    tid = -1
    open_tables: list[int] = []
    sid = -1
    direct_count = 0
    pending_p: list[tuple[int, int, int]] = []  # (start, open_end, body_children_index)
    body_children = 0
    for tag in tags:
        name = tag["name"]
        if tag["closing"]:
            if stack and (stack[-1] == name or stack[-1].startswith("sdt:")):
                top = stack.pop()
                depth -= 1
                if top == "tbl" and open_tables:
                    open_tables.pop()
            if name == "body":
                body_close = tag["start"]
        elif name == "body" and body_open < 0:
            body_open = tag["start"]
            depth += 1
            stack.append(name)
        elif body_open >= 0 and body_close < 0:
            is_direct_child = len(stack) == 1 and stack[0] == "body"
            if name == "tbl" and is_direct_child and not tag["self_closing"]:
                tid += 1
                open_tables.append(tid)
                stack.append(name)
                depth += 1
            elif name == "sdt" and not tag["self_closing"]:
                sid += 1
                stack.append(f"sdt:{sid}")
                depth += 1
            elif name == "p":
                if is_direct_child:
                    ident = f"P{direct_count}"
                    direct_count += 1
                    located.append({"id": ident, "start": tag["start"],
                                    "open_end": tag["open_end"], "table": None,
                                    "sdt": None, "direct": True,
                                    "body_index": body_children})
                    body_children += 1
                else:
                    chain_tbl = [entry for entry in stack if entry == "tbl"]
                    chain_sdt = [entry for entry in stack if entry.startswith("sdt:")]
                    located.append({"id": None, "start": tag["start"],
                                    "open_end": tag["open_end"],
                                    "table": open_tables[-1] if chain_tbl and open_tables else None,
                                    "sdt": chain_sdt[-1].split(":", 1)[1] if chain_sdt else None,
                                    "direct": False, "body_index": None})
                if tag["self_closing"]:
                    located[-1]["end"] = tag["open_end"]
                else:
                    pending_p.append((len(located) - 1, tag["start"], depth))
                    stack.append(name)
                    depth += 1
            elif not tag["self_closing"]:
                stack.append(name)
                depth += 1
    # Close paragraphs: self-closing already ended; pair opens with closes
    # in order (scanner guarantees nesting order for well-formed XML).
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


def select_target(docxml: bytes, other_texts: list[str]) -> tuple[dict | None, str]:
    """Independent safe-target selection. Returns (target, reason)."""
    root = _parse_xml(docxml)
    if root is None:
        return None, "document.xml unparseable"
    located, why = _locate_paragraphs(docxml)
    if located is None:
        return None, f"paragraph locator failed: {why}"
    parents = {child: parent for parent in root.iter() for child in list(parent)}
    wt_all = _all_wt(root)
    # Byte ranges via expat namespace callbacks: only elements BOUND to the
    # WordprocessingML namespace pair as w:t (an m:t math element never
    # pairs, though its local name is identical); offsets are CurrentByteIndex
    # on the same bytes. A w:tab token is local name "tab", never "t".
    wt_ranges = _wt_ranges_expat(docxml, len(wt_all))
    if wt_ranges is None:
        # ponytail: namespace/parse oddity; fail closed, no guessing.
        return None, "w:t byte/element count mismatch; not mappable"
    wt_range_of = {id(element): wt_ranges[i] for i, element in enumerate(wt_all)}
    texts = [(e.text or "") for e in wt_all]
    paras = _paragraphs(root)
    direct_entries = [e for e in located if e["direct"]]
    if len(direct_entries) != len(paras):
        return None, "direct paragraph count mismatch; not mappable"
    for ordinal, para in enumerate(paras):
        flags = _paragraph_flags(para)
        if any(flags.values()):
            continue
        entry = direct_entries[ordinal]
        for run in list(para.iter()):
            if not (_wns(run) and _local(run.tag) == "r"):
                continue
            if not _run_children_ok(run):
                continue
            for text_el in list(run):
                if not (_wns(text_el) and _local(text_el) == "t"):
                    continue
                chain_ok = True
                node = parents.get(text_el)
                while node is not None and node is not para:
                    if not _wns(node) or _local(node.tag) not in ("r", "hyperlink"):
                        chain_ok = False
                        break
                    node = parents.get(node)
                if not chain_ok or node is not para:
                    continue
                for query in _candidate_queries(text_el.text or ""):
                    qb = query.encode("utf-8")
                    folded = query.casefold()
                    # Engine find is case-INSENSITIVE by default (document_find
                    # has no case_sensitive param): a source-unique exact token
                    # with a case variant elsewhere is a two-occurrence engine
                    # query, not a safe EXACT1 target. Expected query stays the
                    # exact first token; matching counts use casefold.
                    if sum(t.casefold().count(folded) for t in texts) != 1:
                        continue
                    if sum(("".join((e.text or "") for e in _all_wt(p))).casefold().count(folded) for p in paras) != 1:
                        continue  # duplicate spanning runs elsewhere
                    if any(folded in other.casefold() for other in other_texts):
                        continue
                    if docxml.count(qb) != 1:
                        continue
                    inner_start, inner_end = wt_range_of[id(text_el)]
                    inner = docxml[inner_start:inner_end]
                    if inner.count(qb) != 1:
                        continue
                    query_start = inner_start + inner.index(qb)
                    query_end = query_start + len(qb)
                    replacement = replacement_for(query)
                    rb = replacement.encode("utf-8")
                    assert len(rb) == len(qb)
                    expected = docxml[:query_start] + rb + docxml[query_end:]
                    return {
                        "kind": "safe", "part": "word/document.xml",
                        "xpath": _xpath(text_el, parents),
                        "paragraph_id": entry["id"],
                        "paragraph_ordinal": ordinal,
                        "wt_byte_start": inner_start, "wt_byte_end": inner_end,
                        "query_start": query_start, "query_end": query_end,
                        "query_sha256": hashlib.sha256(qb).hexdigest(),
                        "query_len": len(query), "query_bytes": len(qb),
                        "replacement_sha256": hashlib.sha256(rb).hexdigest(),
                        "expected_sha256": hashlib.sha256(expected).hexdigest(),
                        "_query": query, "_replacement": replacement, "_expected": expected,
                    }, "safe target mapped"
    return None, "no safe eligible test target"


def select_unsupported(docxml: bytes) -> tuple[dict | None, str]:
    """One unsupported probe target with exact structural proof.
    Returns the target paragraph identity (direct P{n} id or table/sdt
    locator), the expected refusal code family, and byte location, all
    mapped INDEPENDENTLY from source bytes -- same linkage discipline as
    the safe target so the edit oracle still catches wrong-target writes."""
    root = _parse_xml(docxml)
    if root is None:
        return None, "document.xml unparseable"
    located, why = _locate_paragraphs(docxml)
    if located is None:
        return None, f"paragraph locator failed: {why}"
    parents = {child: parent for parent in root.iter() for child in list(parent)}
    wt_all = _all_wt(root)
    texts = [(e.text or "") for e in wt_all]
    paras = _paragraphs(root)
    direct_entries = [e for e in located if e["direct"]]
    if len(direct_entries) != len(paras):
        return None, "direct paragraph count mismatch; not mappable"
    # Mirror select_target's byte pairing: expat namespace-resolved w:t only.
    wt_ranges = _wt_ranges_expat(docxml, len(wt_all))
    if wt_ranges is None:
        return None, "w:t byte/element count mismatch; not mappable"
    wt_range_of = {id(element): wt_ranges[i] for i, element in enumerate(wt_all)}
    for ordinal, para in enumerate(paras):
        flags = _paragraph_flags(para)
        if not any(flags.values()):
            continue
        # Expected refusal family from the SOURCE structure (not the code).
        reason_family = ("opaque-paragraph-mutated" if flags.get("has_opaque")
                         else "prose-edit-unsupported" if flags.get("has_field")
                         else "revision-text-mutated-in-direct-mode" if flags.get("has_revision")
                         else "prose-edit-unsupported")
        entry = direct_entries[ordinal]
        for text_el in _all_wt(para):
            text = (text_el.text or "").strip()
            if len(text) < 2:
                continue
            query = text[:32]
            qb = query.encode("utf-8")
            folded = query.casefold()
            # Narrow unique probe: exactly once casefolded in w:t text, once
            # in raw bytes, once in its w:t node range -- the engine must
            # refuse THIS span (case variants would make it multi-match).
            if sum(t.casefold().count(folded) for t in texts) != 1:
                continue
            if docxml.count(qb) != 1:
                continue
            inner_start, inner_end = wt_range_of[id(text_el)]
            if docxml[inner_start:inner_end].count(qb) != 1:
                continue
            query_start = inner_start + docxml[inner_start:inner_end].index(qb)
            return {"kind": "unsupported", "part": "word/document.xml",
                    "paragraph_id": entry["id"], "paragraph_ordinal": ordinal,
                    "xpath": _xpath(text_el, parents),
                    "wt_byte_start": inner_start, "wt_byte_end": inner_end,
                    "query_start": query_start, "query_end": query_start + len(qb),
                    "flags": flags, "reason_family": reason_family,
                    "structural_why": ";".join(k for k, v in flags.items() if v),
                    "_query": query}, "unsupported probe mapped"
    return None, "no unsupported probe target"


REFUSAL_CODES = {
    "opaque-paragraph-mutated": ("has_opaque",),
    "prose-edit-unsupported": ("has_field", "has_sdt"),
    "revision-text-mutated-in-direct-mode": ("has_revision",),
    "match-spans-leaves": ("has_field", "has_opaque"),
    "edit-span-crosses-revision-boundary": ("has_revision",),
    "cross-leaf-edit": ("has_opaque", "has_field", "has_revision", "has_sdt"),
}


def _code_matches_structure(code: str, flags: dict) -> bool:
    need = REFUSAL_CODES.get(code or "")
    if need is None:
        return False  # unknown code alone never justifies; fail closed
    return any(flags.get(key) for key in need)


def refusal_justified(probe: dict, engine_para: str, code: str) -> bool:
    """EXPECTED_REJECT iff the engine names the PROBED paragraph id AND the
    code matches the SOURCE structure. No paragraph identity or no matching
    reason => INCOMPLETE, never EXPECTED_REJECT."""
    if not probe or engine_para != probe.get("paragraph_id"):
        return False
    return _code_matches_structure(code, probe.get("flags") or {})


def fallback_features(docxml: bytes, members: list[str]) -> dict:
    return {"tables": docxml.count(b"<w:tbl") > 0,
            "revisions": docxml.count(b"<w:ins") + docxml.count(b"<w:del") > 0,
            "opaque_candidates": sum(docxml.count(tag) for tag in
                (b"<w:drawing", b"<w:pict", b"<w:fldSimple", b"<w:object",
                 b"oMath", b"<w:sdt", b"proofErr")) > 0,
            "sdt": docxml.count(b"<w:sdt") > 0,
            "media": any(not n.endswith((".xml", ".rels", ".json"))
                         and n not in ("[Content_Types].xml",) for n in members)}


# --------------------------------------------------------------------------
# Verification
# --------------------------------------------------------------------------

def verify_edit(_src_docxml: bytes, out_docxml: bytes | None, expected_docxml: bytes,  # noqa: ARG001
                src_map: dict, out_map: dict, query: str, replacement: str) -> dict:
    src_bytes = _src_docxml
    cmp = compare_packages(src_map, out_map)
    non_target = [n for n in cmp["changed"] if n != "word/document.xml"]
    if cmp["added"] or cmp["removed"]:
        return {"verdict": "mismatch",
                "reason": f"package member set changed added={cmp['added']} removed={cmp['removed']}"}
    if non_target:
        return {"verdict": "mismatch",
                "reason": f"non-target package changes: {non_target}"}
    if out_docxml is None:
        return {"verdict": "mismatch", "reason": "word/document.xml unreadable in output"}
    if out_docxml == expected_docxml:
        return {"verdict": "exact", "reason": ""}
    if out_docxml == src_bytes:
        return {"verdict": "mismatch", "reason": "output identical to source; edit lost"}
    hint = ""
    if out_docxml.count(replacement.encode("utf-8")) == 1 \
            and out_docxml.count(query.encode("utf-8")) >= 1:
        hint = "wrong-occurrence: replacement landed elsewhere; source-mapped byte range not honored"
    else:
        first = next((i for i, (a, b) in enumerate(zip(out_docxml, expected_docxml)) if a != b),
                     min(len(out_docxml), len(expected_docxml)))
        hint = (f"first output-vs-expected diff at byte {first}; "
                "opaque/non-target drift or corrupted write")
    return {"verdict": "mismatch", "reason": hint}

# --------------------------------------------------------------------------
# Scratch discipline
# --------------------------------------------------------------------------

def _ensure_scratch(root: Path) -> Path:
    root.mkdir(parents=True, exist_ok=True)
    (root / SENTINEL).write_text("owned by real-docx-scan\n", encoding="utf-8")
    return root


def safe_remove_tree(path: Path, scratch_root: Path) -> None:
    """Remove only a path strictly inside a sentinel-guarded scratch root."""
    root = scratch_root.resolve()
    if not (root / SENTINEL).is_file():
        raise ValueError(f"scratch guard missing sentinel: {root}")
    target = path.resolve()
    if target == root or root not in target.parents:
        raise ValueError(f"cleanup boundary refused: {target} outside {root}")
    shutil.rmtree(target, ignore_errors=True)


def _tree_snapshot(stage: Path) -> dict[str, str]:
    out: dict[str, str] = {}
    if not stage.is_dir():
        return out
    for path in sorted(stage.rglob("*")):
        if path.is_file():
            try:
                out[str(path.relative_to(stage))] = _sha256_file(path)
            except OSError:
                out[str(path.relative_to(stage))] = "<unreadable>"
    return out


def _only_expected_creation(before: dict[str, str], after: dict[str, str]) -> bool:
    for key, value in after.items():
        if key not in before:
            if not any(part.endswith(".docx2typed") for part in Path(key).parts):
                return False
        elif before[key] != value:
            return False
    return not any(key not in after for key in before)


# --------------------------------------------------------------------------
# Resume binding
# --------------------------------------------------------------------------

def check_resume_binding(stored: dict, current: dict) -> None:
    for key in ("bin_sha256", "manifest_sha256", "rules_version",
                "scanner_sha256", "engine_metadata_sha256"):
        if stored.get(key) != current.get(key):
            raise ValueError(f"resume refused: {key} changed "
                             f"({stored.get(key)} -> {current.get(key)})")

def _load_rows(results_path: Path) -> dict[str, dict]:
    rows: dict[str, dict] = {}
    if not results_path.is_file():
        return rows
    with open(results_path, encoding="utf-8") as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            try:
                record = json.loads(line)
                rows[record["document_id"]] = record
            except (json.JSONDecodeError, KeyError):
                continue
    return rows


# --------------------------------------------------------------------------
# Per-document run
# --------------------------------------------------------------------------

def run_one(entry: dict, ctx: dict) -> tuple[dict, dict]:
    """Run pristine + edit lifecycles for one manifest entry.

    Returns (public row, private log). Never raises except ManifestDrift.
    """
    manifest_path: Path = ctx["manifest_path"]
    binpath: Path = ctx["bin"]
    timeout: int = ctx["timeout"]
    scratch: Path = ctx["scratch"]
    keep_failures: bool = ctx["keep_failures"]
    outdir: Path = ctx["out"]
    log: dict = {"document_id": entry.get("document_id", ""), "calls": []}
    row: dict = {"schema": SCAN_SCHEMA, "rules_version": RULES_VERSION,
                 "document_id": entry.get("document_id", ""),
                 "completion_status": "COMPLETE", "category": None, "stage": "init"}

    def _mark_incomplete(stage: str, detail: str) -> tuple[dict, dict]:
        row.update(completion_status="INCOMPLETE", category=None,
                   stage=stage, detail=detail)
        return row, log

    def call(calls: list[tuple[str, dict]], tag: str) -> list[dict]:
        row["stage"] = tag  # current call tag retained on INCOMPLETE paths
        envelopes, transport = mcp_session(binpath, calls, timeout)
        log["calls"].append({"tag": tag, "transport": transport,
                             "requests": [{"tool": name, "args": args}
                                          for name, args in calls],
                             "envelopes": envelopes})
        return envelopes

    def need(row_envs: list[dict]) -> str | None:
        """Central transport-error gate: missing/bad replies are INCOMPLETE."""
        for env in row_envs:
            if _is_transport_error(env):
                codes = _codes(env)
                return f"transport error codes={codes}"
        return None
    try:
        try:
            archived = resolve_source(manifest_path, entry)
        except ValueError as exc:
            row.update(category=CONTRACT_FAILURE, stage="resolve", detail=f"source_path refused: {exc}")
            return row, log
        if not archived.is_file():
            raise ManifestDrift(f"archived file missing: {entry.get('document_id')}")
        actual_sha = _sha256_file(archived)
        if actual_sha != entry.get("sha256"):
            raise ManifestDrift(f"archive hash changed for {entry.get('document_id')}")
        row["sha256"] = actual_sha
        row["size_bytes"] = archived.stat().st_size
        row["binary_sha256"] = ctx["bin_sha256"]
        row["binary_version"] = ctx.get("bin_version", "unavailable")
        row["manifest_sha256"] = ctx["manifest_sha256"]
        row["view_contract_tools"] = list(ctx.get("tools") or [])
        row["engine_metadata_sha256"] = ctx.get("engine_metadata_sha256", "")

        if shutil.disk_usage(scratch).free < MIN_FREE_BYTES:
            return _mark_incomplete("disk-precheck", "insufficient disk before doc")
        stage = scratch / entry["document_id"]
        if stage.exists():
            safe_remove_tree(stage, scratch)
        stage.mkdir(parents=True)

        # ---- pristine lifecycle (fresh copy) ----
        pristine_src = stage / "pristine.docx"
        shutil.copyfile(archived, pristine_src)
        if _sha256_file(pristine_src) != actual_sha:
            return _mark_incomplete("stage-copy", "staged copy hash mismatch")
        pristine_out = stage / "pristine-out.docx"
        pre_tree = _tree_snapshot(stage)
        envs = call([("document_open", {"source": str(pristine_src), "track": False}),
                     ("document_save", {"output": str(pristine_out)})], "pristine")
        failed = need(envs)
        if failed is not None:
            return _mark_incomplete("pristine", failed)
        opened, saved = envs[0], envs[1]
        row["pristine"] = {"open": opened.get("outcome"), "open_codes": _codes(opened),
                           "save": saved.get("outcome"), "save_codes": _codes(saved)}
        if opened.get("outcome") != "success":
            return _classify_open_failure(row, log, opened, pristine_src, pristine_out,
                                          pre_tree, stage, entry, ctx)
        if saved.get("outcome") != "success":
            row.update(category=CONTRACT_FAILURE, stage="pristine-save",
                       detail=f"pristine save refused codes={_codes(saved)}")
            return row, log
        envs = call([("document_open", {"source": str(pristine_out)}),
                     ("document_status", {})], "pristine-reopen")
        failed = need(envs)
        if failed is not None:
            return _mark_incomplete("pristine-reopen", failed)
        row["pristine"]["reopen"] = envs[0].get("outcome")
        if envs[0].get("outcome") != "success":
            row.update(category=CONTRACT_FAILURE, stage="pristine-reopen",
                       detail="pristine output failed to reopen")
            return row, log
        src_map, out_map = member_shas(pristine_src), member_shas(pristine_out)
        cmp = compare_packages(src_map, out_map)
        row["pristine"]["members_same"] = cmp["same"]
        row["pristine"]["source_sha256"] = _sha256_file(pristine_src)
        row["pristine"]["output_sha256"] = _sha256_file(pristine_out)
        row["pristine"]["source_member_digest"] = _hash_map_digest(src_map)
        row["pristine"]["output_member_digest"] = _hash_map_digest(out_map)
        log["pristine_members"] = {"source": src_map, "output": out_map}
        if not cmp["same"]:
            row.update(category=CONTRACT_FAILURE, stage="pristine-verify",
                       detail=f"pristine drift changed={cmp['changed']} "
                              f"added={cmp['added']} removed={cmp['removed']}")
            return row, log

        # ---- independent target selection from source bytes ----
        with zipfile.ZipFile(pristine_src) as archive:
            names = archive.namelist()
            docxml = archive.read("word/document.xml")
            others = [archive.read(n).decode("utf-8", errors="replace")
                      for n in names if n.startswith("word/") and n.endswith(".xml")
                      and n != "word/document.xml"]
        features = entry.get("features") or {}
        row["features"] = {k: bool(features.get(k)) for k in
                           ("tables", "revisions", "opaque_candidates", "sdt", "media")} \
            if features else fallback_features(docxml, names)
        target, reason = select_target(docxml, others)
        if target is None:
            return _unsupported_or_na(row, log, docxml, entry, ctx, stage,
                                      pre_tree, reason)
        query, replacement = target["_query"], target["_replacement"]
        expected = target["_expected"]
        row["target"] = {k: v for k, v in target.items() if not k.startswith("_")}
        log["target_detail"] = {"query": query, "replacement": replacement,
                                "reason": reason}

        # ---- edit lifecycle (separate fresh copy) ----
        if shutil.disk_usage(scratch).free < MIN_FREE_BYTES:
            return _mark_incomplete("disk-pre-edit",
                                    "insufficient disk before edit")
        edit_src = stage / "edit.docx"
        shutil.copyfile(archived, edit_src)
        edit_out = stage / "edit-out.docx"
        envs = call([("document_open", {"source": str(edit_src), "track": False}),
                     ("document_find", {"query": query, "limit": 200})], "find-0")
        failed = need(envs)
        if failed is not None:
            return _mark_incomplete("find-0", failed)
        if envs[0].get("outcome") != "success":
            row.update(category=CONTRACT_FAILURE, stage="edit-open",
                       detail="edit-lifecycle open failed after pristine open succeeded")
            return row, log
        handle, found, pages, paged_transport = _collect_single(
            envs[1], binpath, edit_src, query, timeout, call, log)
        row["find"] = found
        if paged_transport or _is_transport_error(envs[1]):
            return _mark_incomplete("find", "find reply transport error")
        if found["occurrences"] != 1:
            # Zero or multi matches for a source casefold-unique query is an
            # engine search/state mismatch -- NEVER an EXPECTED_REJECT.
            # (document_find has no case_sensitive param; matching is
            # case-insensitive, so source uniqueness is casefolded too.)
            row.update(category=CONTRACT_FAILURE, stage="find",
                       detail=f"find occurrences={found['occurrences']} for source-unique "
                              f"query (pages={pages}); not a refusal")
            return row, log
        if not found["editable"]:
            row.update(category=CONTRACT_FAILURE, stage="find",
                       detail=f"safe source-mapped target refused "
                              f"code={found['code']}; structure shows no lock")
            return row, log
        if found.get("paragraph_id") != target.get("paragraph_id"):
            row.update(category=CONTRACT_FAILURE, stage="find",
                       detail="paragraph identity mismatch: engine "
                       f"{found.get('paragraph_id')} vs source {target.get('paragraph_id')}")
            return row, log
        envs = call([("document_open", {"source": str(edit_src), "track": False}),
                     ("document_edit", {"edits": [{"handle": handle,
                                                   "replace_with": replacement}]}),
                     ("document_save", {"output": str(edit_out)})], "edit-save")
        failed = need(envs)
        if failed is not None:
            return _mark_incomplete("edit-save", failed)
        row["edit"] = {"outcome": envs[1].get("outcome"), "codes": _codes(envs[1]),
                       "message": _evidence_head(envs[1])}
        row["save"] = {"outcome": envs[2].get("outcome"), "codes": _codes(envs[2]),
                       "message": _evidence_head(envs[2])}
        if envs[1].get("outcome") != "success":
            row.update(category=CONTRACT_FAILURE, stage="edit",
                       detail=f"find reported editable but deterministic edit refused "
                              f"codes={_codes(envs[1])}")
            return row, log
        if _is_transport_error(envs[1]):
            return _mark_incomplete("edit", "edit reply transport error")
        if envs[2].get("outcome") != "success":
            row.update(category=CONTRACT_FAILURE, stage="save",
                       detail=f"accepted edit but deterministic save refused "
                              f"codes={_codes(envs[2])}")
            return row, log
        envs = call([("document_open", {"source": str(edit_out)}),
                     ("document_status", {})], "edit-reopen")
        failed = need(envs)
        if failed is not None:
            return _mark_incomplete("edit-reopen", failed)
        row["reopen"] = envs[0].get("outcome")
        if envs[0].get("outcome") != "success":
            row.update(category=CONTRACT_FAILURE, stage="reopen",
                       detail="edited output failed to reopen")
            return row, log
        edit_src_map, edit_out_map = member_shas(edit_src), member_shas(edit_out)
        with zipfile.ZipFile(edit_out) as archive:
            out_docxml = archive.read("word/document.xml")
        check = verify_edit(docxml, out_docxml, expected, edit_src_map,
                            edit_out_map, query, replacement)
        row["verify"] = {k: v for k, v in check.items()}
        row["edit_output"] = {"source_sha256": _sha256_file(edit_src),
                              "output_sha256": _sha256_file(edit_out),
                              "source_member_digest": _hash_map_digest(edit_src_map),
                              "output_member_digest": _hash_map_digest(edit_out_map)}
        log["edit_members"] = {"source": edit_src_map, "output": edit_out_map}
        if check["verdict"] == "exact":
            row.update(category=PASS, stage="done",
                       detail="source-mapped single-char edit reproduced exactly")
        else:
            row.update(category=CONTRACT_FAILURE, stage="verify", detail=check["reason"])
        if _sha256_file(archived) != actual_sha:
            raise ManifestDrift(f"archive changed during scan of {entry.get('document_id')}")
        row["archive_unchanged"] = True
        return row, log
    except ManifestDrift:
        raise
    except subprocess.TimeoutExpired:
        row.update(completion_status="INCOMPLETE", category=None,
                   detail="mcp timeout")
        return row, log
    except (OSError, ValueError, zipfile.BadZipFile, ET.ParseError) as exc:
        # ponytail: harness/infra failure is data, never a crash; engine
        # verdicts arrive as envelopes, so an exception cannot be a refusal.
        if isinstance(exc, ValueError) and "escapes archive" in str(exc):
            row.update(category=CONTRACT_FAILURE, stage="resolve", detail=str(exc)[:200])
            return row, log
        row.update(completion_status="INCOMPLETE", category=None,
                   detail=f"{type(exc).__name__}: {str(exc)[:280]}")
        return row, log
    finally:
        _finish_stage(row, log, ctx)


def _collect_single(first: dict, binpath: Path, edit_src: Path, query: str,
                    timeout: int, call, log: dict) -> tuple[str | None, dict, int, bool]:
    """Page find until complete; require exactly one occurrence.

    Contract (src/surface/find.rs): page shape lives at data.counts
    (offset/returned_blocks/complete; limit bounds BLOCKS). The authoritative
    resume cursor is data.next.call (query+offset); offset math is fallback.
    Limit bounds BLOCKS, so EXACT1 harvests every handle across pages with
    dedupe by handle (a repeated page must not double-count one span)."""
    pages = 1
    data = first.get("data") or {}
    _box = data.get("counts") or {}
    # ponytail: counts.occurrences is per-PAGE here (see find.rs), so EXACT1
    # needs every handle across pages, not one page's counter.
    handles: list[tuple[str, bool, str, str, str]] = []  # handle, editable, code, para, why
    seen: set[str] = set()
    def _harvest(entries: list) -> None:
        for entry in entries:
            occs = entry.get("occurrences") or []
            para = str(entry.get("paragraph_id") or "")
            if occs:
                for occ in occs:
                    handle = str(occ.get("handle") or "")
                    if handle and handle in seen:
                        continue
                    if handle:
                        seen.add(handle)
                    verdict = occ.get("verdict") or {}
                    handles.append((handle,
                                    verdict.get("editable") is True,
                                    str(verdict.get("code") or ""),
                                    para, str(verdict.get("why") or "")[:200]))
            else:
                handle = str(entry.get("handle") or "")
                if handle and handle in seen:
                    continue
                if handle:
                    seen.add(handle)
                verdict = entry.get("verdict") or {}
                handles.append((handle,
                                verdict.get("editable") is True,
                                str(verdict.get("code") or ""), para,
                                str(verdict.get("why") or "")[:200]))
    _harvest(data.get("matches") or [])
    def _page_state(payload: dict) -> tuple:
        box = payload.get("counts") or {}
        nxt = payload.get("next")
        nxt_call = (nxt or {}).get("call") if isinstance(nxt, dict) else None
        return (box.get("complete"), int(box.get("offset") or 0),
                int(box.get("returned_blocks") or 0),
                box.get("total_matches"),
                nxt_call if isinstance(nxt_call, dict) else None)
    complete, offset, returned, documented_total, nxt_call = _page_state(data)
    if complete is None and nxt_call is None:
        complete = True  # legacy shape without paging fields: single page
    while complete is not True and pages < 10:
        # ponytail: authoritative cursor is next.call; offset math is fallback.
        if nxt_call is not None and nxt_call.get("query", query) == query \
                and isinstance(nxt_call.get("offset"), int):
            find_args: dict = {"query": query, "limit": FIND_PAGE_LIMIT,
                               "offset": nxt_call["offset"]}
        else:
            find_args = {"query": query, "limit": FIND_PAGE_LIMIT,
                         "offset": offset + returned}
        envs = call([("document_open", {"source": str(edit_src), "track": False}),
                     ("document_find", find_args)], f"find-{pages}")
        if any(_is_transport_error(env) for env in envs):
            return None, {"occurrences": len(handles), "editable": False,
                          "code": "", "paragraph_id": "",
                          "why": "page transport error"}, pages + 1, True
        pages += 1
        data = envs[1].get("data") or {}
        state_total = (data.get("counts") or {}).get("total_matches")
        if state_total != documented_total and documented_total is not None:
            break  # document changed between pages; state mismatch, no EXACT1 claim
        _harvest(data.get("matches") or [])
        complete, offset, returned, documented_total, nxt_call = _page_state(data)
        if complete is None and nxt_call is None:
            complete = True
    total = len(handles)
    if total == 1:
        handle, editable, code, para, why = handles[0]
        return handle or None, {"occurrences": 1, "editable": editable,
                                "code": code, "paragraph_id": para, "why": why}, pages, False
    return None, {"occurrences": total, "editable": False, "code": "",
                  "paragraph_id": "", "why": ""}, pages, False


def _classify_open_failure(row: dict, log: dict, opened: dict, staged: Path,
                           output: Path, pre_tree: dict, stage: Path,
                           entry: dict, ctx: dict) -> tuple[dict, dict]:
    clean = (_sha256_file(staged) == row.get("sha256")
             and _only_expected_creation(pre_tree, _tree_snapshot(stage))
             and not output.exists())
    if not clean:
        # Dirty read phase: the scan wrote something (or lost the tree), so
        # no refusal claim is possible -- hard CONTRACT_FAILURE, never a pass.
        row.update(completion_status="INCOMPLETE", category=None, stage="open",
                   detail="open failed and read phase was not write-free; "
                   f"state mismatch, no refusal claim codes={_codes(opened)}")
        log["open_failure"] = {"codes": _codes(opened), "clean": False}
        return row, log
    if _is_transport_error(opened):
        row.update(completion_status="INCOMPLETE", category=None, stage="open",
                   detail="open reply transport error")
        log["open_failure"] = {"codes": _codes(opened), "clean": True,
                               "transport_error": True}
        return row, log
    # Open-time refusal needs the same proof as a probe: a named paragraph id
    # plus a structure-matching reason. Without both, INCOMPLETE, never a pass.
    diags = opened.get("diagnostics") or []
    para = ""
    code = ""
    for diag in diags:
        if isinstance(diag, dict) and diag.get("paragraph_id"):
            para = str(diag["paragraph_id"])
            code = str(diag.get("code", ""))
            break
    if not para or not code:
        row.update(completion_status="INCOMPLETE", category=None, stage="open",
                   detail=f"open failed without named paragraph+reason codes={_codes(opened)}")
        log["open_failure"] = {"codes": _codes(opened), "clean": True}
        return row, log
    flags: dict = {}
    try:
        with zipfile.ZipFile(staged) as archive:
            docxml = archive.read("word/document.xml")
        root = _parse_xml(docxml)
        located, _ = _locate_paragraphs(docxml)
        paras = _paragraphs(root) if root is not None else []
        direct = [e for e in (located or []) if e["direct"]]
        match_ordinal = None
        if re.fullmatch(r"P\d+", para) and len(direct) == len(paras):
            match_ordinal = int(para[1:])
        if root is not None and match_ordinal is not None and match_ordinal < len(paras):
            flags = _paragraph_flags(paras[match_ordinal])
    except (OSError, zipfile.BadZipFile, ET.ParseError, ValueError):
        flags = {}
    proved = bool(flags) and any(flags.values()) and _code_matches_structure(code, flags)
    log["open_failure"] = {"codes": _codes(opened), "paragraph_id": para,
                           "code": code, "clean": True, "proved": proved}
    if proved:
        row.update(category=EXPECTED_REJECT, stage="open",
                   detail=f"unsupported doc refused at named {para}: {code}")
    else:
        row.update(completion_status="INCOMPLETE", category=None, stage="open",
                   detail=f"open refusal unproven at {para or '?'} code={code or '?'}")
    return row, log


def _unsupported_or_na(row: dict, log: dict, docxml: bytes, entry: dict,
                       ctx: dict, stage: Path, pre_tree: dict, reason: str) -> tuple[dict, dict]:
    """No safe target: probe one unsupported target for a justified refusal."""
    binpath, timeout = ctx["bin"], ctx["timeout"]
    probe, _ = select_unsupported(docxml)
    if probe is None:
        row.update(category=NOT_APPLICABLE, stage="target",
                   detail=f"{reason}; no unsupported probe either")
        return row, log
    staged = stage / "pristine.docx"
    before_sha = _sha256_file(staged)
    # Snapshot immediately BEFORE the probe: the pristine lifecycle's own
    # outputs (pristine-out.docx + sidecars) are harness-own and must not
    # count as probe writes. Only the probe workdir subtree + staged-source
    # hash + the probe session's clean state prove the probe wrote nothing.
    probe_tree = _tree_snapshot(stage)
    calls = [("document_open", {"source": str(staged), "track": False}),
             ("document_find", {"query": probe["_query"], "limit": 50})]
    first, transport = mcp_session(binpath, calls, timeout)
    row["stage"] = "unsupported-probe"
    log["calls"].append({"tag": "unsupported-probe", "transport": transport,
                         "requests": [{"tool": name, "args": args} for name, args in calls],
                         "envelopes": first})
    if any(_is_transport_error(env) for env in first):
        row.update(completion_status="INCOMPLETE", category=None,
                   stage="unsupported-probe", detail="probe transport error")
        return row, log
    find = first[1]
    data = find.get("data") or {}
    matches = data.get("matches") or []
    proved = False
    code = ""
    if find.get("outcome") == "success" and len(matches) == 1:
        entry_m = matches[0]
        engine_para = str(entry_m.get("paragraph_id") or "")
        occs = entry_m.get("occurrences") or []
        if len(occs) == 1:
            verdict = occs[0].get("verdict") or {}
            if verdict.get("editable") is not True:
                code = str(verdict.get("code") or "")
                proved = refusal_justified(probe, engine_para, code)
    clean = (_sha256_file(staged) == before_sha
             and _only_expected_creation(probe_tree, _tree_snapshot(stage)))
    log["probe"] = {"codes": _codes(find), "engine_paragraph_id": engine_para,
                    "code": code, "expected_paragraph_id": probe.get("paragraph_id"),
                    "reason_family": probe.get("reason_family"), "clean": clean,
                    "query_start": probe.get("query_start")}
    if proved and clean:
        row.update(category=EXPECTED_REJECT, stage="probe",
                   detail="unsupported target correctly refused at "
                   f"{engine_para}: {code}")
    elif not clean:
        row.update(category=CONTRACT_FAILURE, stage="probe",
                   detail="probe phase wrote state; refusal unclaimable")
    else:
        row.update(completion_status="INCOMPLETE", category=None, stage="probe",
                   detail=f"{reason}; unsupported probe inconclusive "
                   f"(engine_para={engine_para or '?'} clean={clean})")
    return row, log


def _finish_stage(row: dict, log: dict, ctx: dict) -> None:
    """Persist the private log; clean owned scratch per policy."""
    outdir: Path = ctx["out"]
    scratch: Path = ctx["scratch"]
    try:
        logs = outdir / "logs"
        logs.mkdir(parents=True, exist_ok=True)
        with open(logs / f"{row.get('document_id', 'unknown')}.json", "w",
                  encoding="utf-8") as handle:
            json.dump(log, handle, ensure_ascii=False)
    except OSError:
        pass
    stage = scratch / str(row.get("document_id", ""))
    try:
        succeeded = row.get("category") in (PASS, EXPECTED_REJECT, NOT_APPLICABLE)
        incomplete = row.get("completion_status") == "INCOMPLETE"
        failed = incomplete or row.get("category") == CONTRACT_FAILURE
        if failed and ctx["keep_failures"]:
            ev = outdir / "evidence" / str(row.get("document_id", ""))
            ev.mkdir(parents=True, exist_ok=True)
            for name in ("pristine-out.docx", "edit-out.docx"):
                src = stage / name
                try:
                    if src.is_file() and src.stat().st_size <= EVIDENCE_MAX_BYTES:
                        shutil.copyfile(src, ev / name)
                except OSError:
                    pass
        # ponytail: completed outcomes release scratch; INCOMPLETE keeps its
        # stage dir so --resume can rerun it in place, and CONTRACT_FAILURE
        # keeps scratch only under --keep-failures for triage.
        if succeeded or (row.get("category") == CONTRACT_FAILURE and not ctx["keep_failures"]):
            if stage.exists():
                safe_remove_tree(stage, scratch)
    except (OSError, ValueError):
        pass


# --------------------------------------------------------------------------
# Scan driver
# --------------------------------------------------------------------------

def _preflight(binpath: Path, timeout: int) -> dict:
    identity = bin_identity(binpath, timeout)
    envelopes, _ = mcp_session(binpath, [("tools/list", {})], timeout)
    engine = (envelopes[0].get("data") or {}) if envelopes else {}
    present = {t.get("name") for t in (engine.get("tools") or [])
               if isinstance(t, dict) and t.get("name")}
    missing = [t for t in SURFACE_TOOLS if t not in present]
    if missing:
        raise RuntimeError(f"binary viewcontract missing tools: {missing}")
    scanner_sha = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
    engine_meta = hashlib.sha256(json.dumps(engine, sort_keys=True,
                                             ensure_ascii=True).encode("ascii")).hexdigest()
    return {"binary": identity, "engine": engine, "tools": sorted(present),
            "scanner_sha256": scanner_sha, "engine_metadata_sha256": engine_meta}


def scan(manifest: Path, outdir: Path, binpath: Path, workers: int = 2,
         resume: bool = False, limit: int = 0, keep_failures: bool = False,
         timeout: int = 240, scratch: Path | None = None) -> dict:
    validation = validate_manifest(manifest)
    if not validation.get("ok"):
        raise ValueError(f"manifest invalid: {validation.get('problems')}")
    manifest_data = load_manifest(manifest)
    entries = manifest_data.get("documents") or []
    if limit:
        entries = entries[:limit]
    outdir.mkdir(parents=True, exist_ok=True)
    scratch = _ensure_scratch(scratch or (outdir / "_scratch"))

    flight = _preflight(binpath, timeout)
    binding = {"bin_sha256": flight["binary"]["sha256"],
               "manifest_sha256": validation.get("manifest_sha256", ""),
               "rules_version": RULES_VERSION,
               "scanner_sha256": flight["scanner_sha256"],
               "engine_metadata_sha256": flight["engine_metadata_sha256"]}
    meta_path = outdir / "run_meta.json"
    results_path = outdir / "results.jsonl"
    if resume:
        if not meta_path.is_file():
            raise FileNotFoundError(f"no run to resume: {meta_path}")
        check_resume_binding(json.loads(meta_path.read_text(encoding="utf-8")).get("binding", {}), binding)
    else:
        if results_path.exists():
            raise FileExistsError(f"results exist; pass --resume: {results_path}")
        meta_path.write_text(json.dumps({"schema": SCAN_SCHEMA, "binding": binding,
                                         "preflight": {k: v for k, v in flight.items()
                                                       if k != "engine"},
                                         "corpus_id": CORPUS_ID}, ensure_ascii=False, indent=2)
                             + "\n", encoding="utf-8")
    done = _load_rows(results_path) if resume else {}
    # Resume retries INCOMPLETE rows: completed rows skip, incomplete rerun.
    todo = [e for e in entries
            if e.get("document_id") not in done
            or done[e["document_id"]].get("completion_status") == "INCOMPLETE"
            or done[e["document_id"]].get("category") not in COMPLETED]
    # Preflight the pinned 217 hashes: any drift stops the scan, not one row.
    for entry in todo:
        archived = resolve_source(manifest, entry)
        if not archived.is_file() or _sha256_file(archived) != entry.get("sha256"):
            raise ManifestDrift(f"archive drift for {entry.get('document_id')}")

    ctx = {"manifest_path": manifest, "bin": binpath, "timeout": timeout,
           "scratch": scratch, "keep_failures": keep_failures, "out": outdir,
           "bin_sha256": binding["bin_sha256"], "manifest_sha256": binding["manifest_sha256"],
           "bin_version": flight["binary"].get("version", "unavailable"),
           "tools": flight["tools"],
           "engine_metadata_sha256": binding["engine_metadata_sha256"]}
    lock = threading.Lock()

    def one(entry: dict) -> dict:
        row, _ = run_one(entry, ctx)
        public = {k: v for k, v in row.items()}
        with lock:
            merged = _load_rows(results_path)
            merged[public["document_id"]] = public
            with open(results_path, "w", encoding="utf-8") as handle:
                for record in merged.values():
                    handle.write(json.dumps(record, ensure_ascii=False) + "\n")
            print(f"[{row.get('category') or row.get('completion_status')}] "
                  f"{row.get('document_id')} {row.get('stage')}: "
                  f"{row.get('detail', '')}"[:220], flush=True)
        return public

    with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as pool:
        futures = {pool.submit(one, entry): entry for entry in todo}
        try:
            for future in concurrent.futures.as_completed(futures):
                future.result()  # exceptions are row data, except ManifestDrift
        except ManifestDrift:
            pool.shutdown(wait=False, cancel_futures=True)
            raise
    return summarize(outdir, manifest, binding, flight, len(todo), len(done))


def summarize(outdir: Path, manifest: Path, binding: dict, flight: dict,
              scanned_new: int, resumed_skips: int) -> dict:
    rows = _load_rows(outdir / "results.jsonl")
    counts = {name: 0 for name in COMPLETED}
    incomplete: list[str] = []
    strata: dict[str, dict] = {}
    for record in rows.values():
        category = record.get("category")
        if record.get("completion_status") == "INCOMPLETE" or category not in counts:
            incomplete.append(record.get("document_id", "?"))
            continue
        counts[category] += 1
        for flag, value in (record.get("features") or {}).items():
            cell = strata.setdefault(flag, {"with": {n: 0 for n in COMPLETED},
                                            "without": {n: 0 for n in COMPLETED}})
            cell["with" if value else "without"][category] += 1
    summary = {"schema": SCAN_SCHEMA, "rules_version": RULES_VERSION,
               "corpus_id": CORPUS_ID, "binary": flight["binary"],
               "binding": binding, "documents": len(rows),
               "scanned_new": scanned_new, "resumed_skips": resumed_skips,
               "counts": counts, "incomplete": sorted(incomplete),
               "feature_stratification": strata, "results": "results.jsonl"}
    (outdir / "summary.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2) + "\n",
                                         encoding="utf-8")
    return summary


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--bin", default=None)
    parser.add_argument("--workers", type=int, default=2)
    parser.add_argument("--resume", action="store_true")
    parser.add_argument("--limit", type=int, default=0)
    parser.add_argument("--keep-failures", action="store_true")
    parser.add_argument("--timeout", type=int, default=240)
    parser.add_argument("--scratch", type=Path, default=None)
    args = parser.parse_args(argv)
    workers = max(2, min(4, args.workers))
    try:
        binpath = _resolve_bin(args.bin)
        summary = scan(args.manifest, args.out, binpath, workers, args.resume,
                       args.limit, args.keep_failures, args.timeout, args.scratch)
    except (FileNotFoundError, FileExistsError, ValueError) as exc:
        print(f"scan setup refused: {exc}")
        return 2
    except ManifestDrift as exc:
        print(json.dumps({"manifest_drift": str(exc)}))
        return 3
    print(json.dumps(summary, ensure_ascii=False, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
