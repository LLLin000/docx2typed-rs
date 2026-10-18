#!/usr/bin/env python3
"""Review-handoff lane for real-workflow-qualification-v1 (Word COM only).

Real Microsoft Word executes every document mutation through
qualification/real_workflow_review_handoff.ps1 (fresh COM instance per action,
quits only its own instance). Python never mutates OOXML bytes (readonly zip
reads for deterministic target selection are diagnostics only).

Flow per run:
  1. Pin a clean frozen input (revisions==0) -> stage copy (original SHA kept).
  2. Word prepare: prior-author tracked replacement + classic comment on known
     unique passages -> prepared SHA + preparation ops recorded.
  3. Agent (FIVE MCP tools only + explicit CLI extract lifecycle): extract
     prepared file to baseline A, tracked edit as WorkflowAgent on a distinct
     passage, save B.
  4. Word inspect B: prior + agent revisions (author/type/text/range), comments
     (author/text/scope). Semantic identity check, not counts.
  5. Word accept-ONLY-agent -> copy 1; Word reject-ONLY-agent -> copy 2
     (backwards walk, prior untouched). Reopen both in Word: final visible text
     must show agent new (accept) / agent old (reject); prior revision and
     comment anchor must remain usable.
  6. Negative case: query crossing a revision boundary -> named refusal at find
     or edit with pre/post no-write proof.

Report: report.json {schema:'real-workflow-qualification-1',
workflow:'review-handoff', host, binary, cases:[{case_id,input_sha256,
completion_status,outcome,steps,assertions,artifacts,limitations}]}.
Raw document text and absolute paths stay in private evidence files under --out;
report.json carries hashes, codes, counts and booleans only.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import shutil
import subprocess
import sys
import time
import zipfile
from pathlib import Path

try:
    from qualification.real_docx_scan import (  # type: ignore
        mcp_session as _mcp_session,
        bin_identity as _bin_identity,
    )
    try:
        from qualification.real_docx_scan import _resolve_bin as _resolve_bin_fn  # type: ignore
    except ImportError:
        _resolve_bin_fn = None  # type: ignore
    try:
        from qualification.real_docx_scan import (  # type: ignore
            load_manifest as _load_manifest,
            validate_manifest as _validate_manifest,
            resolve_source as _resolve_source,
        )
    except ImportError:
        _load_manifest = _validate_manifest = _resolve_source = None  # type: ignore
except ImportError:
    try:
        from real_docx_scan import (  # type: ignore
            mcp_session as _mcp_session,
            bin_identity as _bin_identity,
        )
        try:
            from real_docx_scan import _resolve_bin as _resolve_bin_fn  # type: ignore
        except ImportError:
            _resolve_bin_fn = None  # type: ignore
        try:
            from real_docx_scan import (  # type: ignore
                load_manifest as _load_manifest,
                validate_manifest as _validate_manifest,
                resolve_source as _resolve_source,
            )
        except ImportError:
            _load_manifest = _validate_manifest = _resolve_source = None  # type: ignore
    except ImportError:
        _mcp_session = _bin_identity = None  # type: ignore
        _resolve_bin_fn = None  # type: ignore
        _load_manifest = _validate_manifest = _resolve_source = None  # type: ignore

SCHEMA = "real-workflow-qualification-1"
WORKFLOW = "review-handoff"
AGENT_AUTHOR = "WorkflowAgent"
PRIOR_AUTHOR = "PriorReviewer"
PRIOR_COMMENT_TEXT = "Prior reviewer note: please verify this passage."
PS1 = Path(__file__).with_name("real_workflow_review_handoff.ps1")
EXECUTION_MODE = "word-com-automated-operator"

PASS = "PASS"
EXPECTED_REJECT = "EXPECTED_REJECT"
UNSUPPORTED_WORKFLOW = "UNSUPPORTED_WORKFLOW"
CONTRACT_FAILURE = "CONTRACT_FAILURE"


class _HarnessError(RuntimeError):
    """Word/COM/transport/setup failure: observation incomplete, never a product verdict."""


class _ProductError(RuntimeError):
    """A checked product assertion failed after the Word host fully exercised."""


def _raise_word(action: str, payload: dict) -> None:
    raise _HarnessError(f"word {action} failed: {payload.get('error')}")

# --------------------------------------------------------------------------
# Small utilities (no document mutation here)
# --------------------------------------------------------------------------

def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _sha256_text(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def _tree_hashes(root: Path) -> dict[str, str]:
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


def _functional_diff(before: dict[str, str], after: dict[str, str]) -> dict[str, list[str]]:
    """Tree diff excluding proven-transient session/projection sidecars.

    Allowed (ignored): per-session `*.word.json` result files this lane writes
    itself, `calls.json`-style logs, and lock files (`*.lock`, `*.tmp`).
    Everything else — workdir JSON, staged docx bytes, prepared.docx — is
    functional state: any change there is a real write, not a transient.
    """
    ignore_suffix = (".word.json", ".lock", ".tmp", ".log")
    changed = [k for k in after if k not in before and not k.endswith(ignore_suffix)]
    changed += [k for k in after if k in before and before[k] != after[k]
                and not k.endswith(ignore_suffix)]
    removed = [k for k in before if k not in after and not k.endswith(ignore_suffix)]
    return {"changed_or_added": sorted(changed), "removed": sorted(removed)}

def _resolve_bin(explicit: str | None) -> Path:
    if _resolve_bin_fn is not None:
        return _resolve_bin_fn(explicit)
    if explicit:
        candidate = Path(explicit)
        if candidate.is_file():
            return candidate
        raise FileNotFoundError(f"--bin not found: {explicit}")
    raise FileNotFoundError("no docx2typed binary: pass --bin")


def _load_manifest_fallback(path: Path) -> dict:
    return json.loads(path.read_text(encoding="utf-8"))


def _run_ps(action_args: list[str], timeout: int) -> dict:
    proc = subprocess.run(
        ["powershell", "-NoProfile", "-ExecutionPolicy", "Bypass",
         "-File", str(PS1)] + action_args,
        capture_output=True, text=True, encoding="utf-8",
        errors="replace", timeout=timeout)
    if proc.returncode not in (0, 1):
        raise _HarnessError(f"word operator crashed rc={proc.returncode}: "
                            f"{(proc.stderr or '')[-500:]}")
    return proc


def _ps_json(out_json: Path) -> dict:
    # ponytail: PS WriteAllText UTF8 emits a BOM; utf-8-sig tolerates it.
    return json.loads(out_json.read_text(encoding="utf-8-sig"))


def _word_call(action: str, params: dict, workdir: Path, name: str,
               timeout: int, calls_log: list) -> dict:
    out_json = workdir / f"{name}.word.json"
    if out_json.exists():
        out_json.unlink()
    args = ["-Action", action, "-OutJson", str(out_json)]
    for key, value in params.items():
        args += [f"-{key}", str(value)]
    started = time.time()
    try:
        proc = _run_ps(args, timeout)
    except (OSError, subprocess.TimeoutExpired) as exc:
        calls_log.append({"tool": "word-com", "action": action,
                          "error": f"host-error: {exc}"})
        raise
    elapsed = round(time.time() - started, 1)
    try:
        payload = _ps_json(out_json)
    except (OSError, json.JSONDecodeError) as exc:
        calls_log.append({"tool": "word-com", "action": action,
                          "seconds": elapsed, "error": f"bad-result-json: {exc}",
                          "stderr_tail": (proc.stderr or "")[-1000:]})
        raise _HarnessError(f"word {action} produced no result JSON: {exc}")
    calls_log.append({"tool": "word-com", "action": action,
                      "seconds": elapsed, "ok": payload.get("ok"),
                      "returncode": proc.returncode})
    return payload


def _mcp(binpath: Path, calls: list[tuple[str, dict]], timeout: int,
         calls_log: list, tag: str) -> list[dict]:
    if _mcp_session is None:
        raise RuntimeError("mcp transport unavailable: "
                           "qualification.real_docx_scan not importable")
    envelopes, transport = _mcp_session(binpath, calls, timeout)
    calls_log.append({"tag": tag, "transport": transport,
                      "requests": [{"tool": n, "args": a} for n, a in calls],
                      "envelopes": envelopes})
    return envelopes


def _is_transport(envelope: dict) -> bool:
    if (envelope.get("outcome") or "") == "transport-error":
        return True
    return any(str(d.get("code", "")) in ("transport", "bad-json", "missing-reply")
               for d in (envelope.get("diagnostics") or []) if isinstance(d, dict))


def _codes(envelope: dict) -> list[str]:
    return [str(d.get("code", "")) for d in (envelope.get("diagnostics") or [])
            if isinstance(d, dict)]


def _cli(binpath: Path, argv: list[str], timeout: int,
         calls_log: list, tag: str) -> dict:
    started = time.time()
    proc = subprocess.run([str(binpath)] + argv, capture_output=True,
                          text=True, encoding="utf-8", errors="replace",
                          timeout=timeout)
    elapsed = round(time.time() - started, 1)
    try:
        envelope = json.loads((proc.stdout or "").strip())
    except json.JSONDecodeError:
        envelope = {"outcome": "transport-error", "stdout_tail": (proc.stdout or "")[-500:]}
    calls_log.append({"tag": tag, "argv": argv, "seconds": elapsed,
                      "returncode": proc.returncode, "envelope": envelope,
                      "stderr_tail": (proc.stderr or "")[-1000:]})
    return envelope


def _read_baseline_epoch(workdir: Path) -> int | None:
    try:
        value = json.loads((workdir / "baseline.json").read_text(encoding="utf-8"))
        epoch = value.get("epoch")
        return int(epoch) if isinstance(epoch, int) else None
    except (OSError, ValueError, json.JSONDecodeError):
        return None


def _read_template_fingerprint(workdir: Path) -> str:
    try:
        value = json.loads((workdir / "format.json").read_text(encoding="utf-8"))
        for key in ("template_sha256", "source_sha256", "document_xml_sha256"):
            text = value.get(key)
            if isinstance(text, str) and text:
                return text
    except (OSError, json.JSONDecodeError):
        pass
    return ""


# --------------------------------------------------------------------------
# Deterministic target selection (readonly diagnostics, no mutation)
# --------------------------------------------------------------------------
def _leaf_runs(docx_path: Path) -> list[tuple[int, list[tuple[str, str]]]]:
    """Body paragraphs as (paragraph index, [(leaf w:t text, canonical rPr key)])."""
    import xml.etree.ElementTree as ET
    ns = "{http://schemas.openxmlformats.org/wordprocessingml/2006/main}"
    with zipfile.ZipFile(docx_path) as archive:
        raw = archive.read("word/document.xml")
    root = ET.fromstring(raw)
    body = root.find(f"{ns}body")
    if body is None:
        return []
    out: list[tuple[int, list[tuple[str, str]]]] = []
    for index, para in enumerate(body.findall(f"{ns}p")):
        leaves: list[tuple[str, str]] = []
        for run in para.findall(f"{ns}r"):
            if run.find(f"{ns}delText") is not None:
                continue
            for node in run.findall(f"{ns}t"):
                text = node.text or ""
                if not text:
                    continue
                rpr = run.find(f"{ns}rPr")
                key = (ET.tostring(rpr, encoding="unicode")
                       if rpr is not None else "")
                leaves.append((text, key))
        out.append((index, leaves))
    return out


def _paragraph_texts(docx_path: Path) -> tuple[list[str], bytes]:
    with zipfile.ZipFile(docx_path) as archive:
        raw = archive.read("word/document.xml")
    import xml.etree.ElementTree as ET
    ns = "{http://schemas.openxmlformats.org/wordprocessingml/2006/main}"
    root = ET.fromstring(raw)
    paragraphs: list[str] = []
    for para in root.iter(f"{ns}p"):
        texts = [node.text or "" for node in para.iter(f"{ns}t")]
        paragraphs.append("".join(texts))
    return paragraphs, raw


def _leaf_candidate(leaf: str) -> list[str]:
    words = leaf.split()
    out: list[str] = []
    for width in (4, 3):
        for i in range(len(words) - width + 1):
            cand = " ".join(words[i:i + width])
            if 12 <= len(cand) <= 40 and all(ch.isalnum() or ch.isspace()
                                            or '\u4e00' <= ch <= '\u9fff' for ch in cand):
                # Inside one w:t leaf (no internal run split possible).
                out.append(cand)
    # ponytail: dedupe preserving order; first deterministic hit wins.
    return list(dict.fromkeys(out))


def _pick_targets(docx_path: Path) -> tuple[str, str, str, int, int, int]:
    """Three unique passages in three paragraphs, each inside one w:t leaf."""
    paragraphs, raw = _paragraph_texts(docx_path)
    full = "\n".join(paragraphs)
    runs = {index: leaves for index, leaves in _leaf_runs(docx_path)}
    picked: list[tuple[str, int]] = []
    for index, para in enumerate(paragraphs):
        if len(para) < 40:
            continue
        leaves = runs.get(index, [])
        if not leaves:
            continue
        claimed: set[str] = set()
        for leaf, _key in leaves:
            if len(leaf) < 12:
                continue
            for cand in _leaf_candidate(leaf):
                if cand in claimed:
                    continue
                # Globally unique visible text AND inside a single leaf copy:
                # raw count guards against a visible-unique but XML-split span.
                if full.count(cand) != 1:
                    continue
                if raw.count(cand.encode("utf-8")) != 1:
                    continue
                if any(cand in p[0] or p[0] in cand for p in picked):
                    continue
                picked.append((cand, index))
                claimed.add(cand)
                break
            if picked and picked[-1][1] == index:
                break
        if len(picked) == 3:
            break
    if len(picked) != 3:
        raise _HarnessError("no three leaf-exact unique passages in candidate document")
    (prior_old, pi), (agent_old, ai), (anchor, ci) = picked
    if len({pi, ai, ci}) != 3:
        raise _HarnessError("leaf-exact passages must sit in three distinct paragraphs")
    return prior_old, agent_old, anchor, pi, ai, ci


def _mutate_middle(text: str) -> str:
    pos = len(text) // 2
    while pos < len(text) and not text[pos].isalnum():
        pos += 1
    if pos >= len(text):
        pos = len(text) // 2
    replacement = "X" if text[pos] != "X" else "Y"
    return text[:pos] + replacement + text[pos + 1:]


# --------------------------------------------------------------------------
# Lane
# --------------------------------------------------------------------------

def _host_info(workdir: Path, timeout: int, calls_log: list) -> dict:
    payload = _word_call("hostinfo", {}, workdir, "hostinfo", timeout, calls_log)
    if not payload.get("ok"):
        raise _HarnessError(f"word hostinfo failed: {payload.get('error')}")
    return payload


def run_lane(manifest: Path, outdir: Path, binpath: Path,
             timeout: int, document_id: str | None) -> dict:
    if _bin_identity is None or _mcp_session is None:
        raise RuntimeError("lane requires qualification.real_docx_scan transport")
    outdir.mkdir(parents=True, exist_ok=True)
    evidence = outdir / "private"
    evidence.mkdir(parents=True, exist_ok=True)
    calls_log: list = []

    if _validate_manifest is not None:
        validation = _validate_manifest(manifest)
        if not validation.get("ok"):
            raise ValueError(f"manifest invalid: {validation.get('problems')}")
        manifest_sha = validation.get("manifest_sha256", "")
    else:
        manifest_sha = hashlib.sha256(manifest.read_bytes()).hexdigest()
    manifest_data = (_load_manifest(manifest) if _load_manifest
                     else _load_manifest_fallback(manifest))
    entries = manifest_data.get("documents") or []
    clean = [e for e in entries
             if (e.get("features") or {}).get("revisions") == 0
             and (e.get("features") or {}).get("tables") == 0
             and (e.get("features") or {}).get("sdt") == 0
             and (e.get("features") or {}).get("parse_error") is None]
    clean.sort(key=lambda e: e.get("document_id", ""))
    if document_id:
        clean = [e for e in entries if e.get("document_id") == document_id]
        if not clean:
            raise ValueError(f"document_id not in manifest: {document_id}")
    binary = _bin_identity(binpath, timeout)

    # Word host preflight: only a genuine launch/connect absence (COM missing,
    # Word not installed/blocked, powershell missing) is not-run-no-host.
    # Healthy Word that answers but whose result JSON cannot be read is a
    # tool/harness failure -> incomplete, never a fabricated PASS or no-host.
    try:
        host = _host_info(evidence, timeout, calls_log)
    except Exception as exc:
        message = str(exc)
        lowered = message.lower()
        no_host_markers = ("could not load file or assembly 'microsoft",
                           "retrieving the com class factory",
                           "cannot be found", "not installed",
                           "powershell", "80040154", "80080005",
                           "80070002", "word.application")
        genuine_absence = any(marker in lowered for marker in no_host_markers)
        status = "not-run-no-host" if genuine_absence else "incomplete"
        report = {"schema": SCHEMA, "workflow": WORKFLOW,
                  "host": {"error": message[:300],
                           "classification": ("genuine-absence" if genuine_absence
                                              else "result-decode-failure")},
                  "binary": {"sha256": binary["sha256"],
                             "version": binary.get("version", "unavailable")},
                  "manifest_sha256": manifest_sha,
                  "cases": [{"case_id": "review-handoff-selective",
                             "completion_status": status,
                             "outcome": None},
                            {"case_id": "review-handoff-cross-boundary",
                             "completion_status": status,
                             "outcome": None}]}
        (outdir / "report.json").write_text(
            json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        (evidence / "calls.json").write_text(
            json.dumps(calls_log, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        return report

    host_record = {"application": host.get("application", "Microsoft Word"),
                   "version": host.get("version"), "build": host.get("build"),
                   "executable_exists": host.get("executableExists"),
                   "os": host.get("os"),
                   "execution_mode": EXECUTION_MODE,
                   "human_ui_review": "not-run"}
    limitations = [
        "execution_mode word-com-automated-operator: real Word executed the "
        "actions; no human UI observation or visual-layout acceptance.",
        "DisplayAlerts silencing / OpenAndRepair=false do not prove absence of "
        "a repair prompt; no-repair is not claimed.",
        "Classic comments only: comment anchor/author/text verified via Word "
        "COM; modern threaded replies, @mentions and resolve states are "
        "unexercised and unsupported here.",
        "Host normalization is semantic (text/authors/types/anchors), never "
        "raw Word XML byte equality.",
    ]

    cases: list[dict] = []
    # Pin the first clean input that deterministically yields three passages.
    pinned = None
    pin_targets = None
    pin_probe = evidence / "probe.docx"
    pin_errors: list[str] = []
    for entry in clean:
        try:
            archived = (_resolve_source(manifest, entry) if _resolve_source
                        else manifest.parent / entry["source_path"])
        except ValueError as exc:
            pin_errors.append(f"{entry.get('document_id')}: {exc}")
            continue
        if not Path(archived).is_file():
            continue
        if _sha256_file(Path(archived)) != entry.get("sha256"):
            raise ValueError(f"archive drift for {entry.get('document_id')}")
        shutil.copyfile(archived, pin_probe)
        try:
            pin_targets = _pick_targets(pin_probe)
            pinned = entry
            break
        except (_HarnessError, ValueError) as exc:
            pin_errors.append(f"{entry.get('document_id')}: {exc}")
            continue
    if pinned is None or pin_targets is None:
        raise _HarnessError("no clean input yields three passages: "
                            + "; ".join(pin_errors[:5]))
    if pin_probe.exists():
        pin_probe.unlink()
    prior_old, agent_old, anchor, _, _, _ = pin_targets
    prior_new = _mutate_middle(prior_old)
    agent_new = _mutate_middle(agent_old)
    assert prior_new != prior_old and agent_new != agent_old

    # ---- success case -----------------------------------------------------
    case_id = "review-handoff-selective"
    steps: list[dict] = []
    assertions: list[dict] = []

    def _step(name: str, ok: bool, detail: str = "") -> None:
        steps.append({"name": name, "ok": ok, "detail": detail[:300]})

    def _assert(ident: str, passed: bool, detail: str = "") -> None:
        assertions.append({"id": ident, "result": "pass" if passed else "fail",
                           "detail": detail[:300]})

    completion = "COMPLETE"
    outcome: str | None = None
    artifacts: dict = {}
    try:
        input_sha = pinned["sha256"]
        stage = evidence / "selective"
        if stage.exists():
            shutil.rmtree(stage)
        stage.mkdir(parents=True)
        input_copy = stage / "input.docx"
        shutil.copyfile((_resolve_source(manifest, pinned) if _resolve_source
                         else manifest.parent / pinned["source_path"]), input_copy)
        assert _sha256_file(input_copy) == input_sha
        _step("stage-original-copy", True, f"sha {input_sha[:12]}")
        artifacts["input_sha256"] = input_sha
        artifacts["input_document_id"] = pinned.get("document_id", "")

        prepared = stage / "prepared.docx"
        payload = _word_call("prepare",
                             {"InputPath": str(input_copy), "Out": str(prepared),
                              "PriorAuthor": PRIOR_AUTHOR, "PriorOld": prior_old,
                              "PriorNew": prior_new, "CommentAnchor": anchor,
                              "CommentText": PRIOR_COMMENT_TEXT,
                              "AgentAuthor": AGENT_AUTHOR},
                             stage, "prepare", timeout, calls_log)
        if not payload.get("ok"):
            _raise_word("prepare", payload)
        prepared_sha = _sha256_file(prepared)
        _step("word-prepare-prior-revision-comment", True,
              f"sha {prepared_sha[:12]} revs={payload.get('revisionCount')} "
              f"comments={payload.get('commentCount')}")
        artifacts["prepared_sha256"] = prepared_sha
        # Actual host identity: prepare is allowed to record whatever author
        # Word reports (host resets UserName); never alias to PRIOR_AUTHOR.
        prep_revs = payload.get("revisions") or []
        prep_comments = payload.get("comments") or []
        prep_rev_set = sorted(
            (str(r.get("author")), str(r.get("type")), str(r.get("text")))
            for r in prep_revs if isinstance(r, dict))
        prep_comment_set = sorted(
            (str(c.get("author")), str(c.get("text"))) for c in prep_comments
            if isinstance(c, dict))
        prep_comment_anchors = sorted(
            str(c.get("anchor") or c.get("scope")) for c in prep_comments
            if isinstance(c, dict))
        artifacts["preparation"] = {
            "requested_prior_author": PRIOR_AUTHOR,
            "actual_revision_identities": prep_rev_set,
            "actual_comment_identities": prep_comment_set,
            "prior_old_sha256": _sha256_text(prior_old),
            "prior_new_sha256": _sha256_text(prior_new),
            "comment_anchor_sha256": _sha256_text(anchor),
            "ops": ["range-Find tracked replace (prior author)",
                    "classic comment add on known passage"],
        }
        if not prep_rev_set:
            raise _HarnessError("prepare produced no tracked revisions")
        if not prep_comment_set:
            raise _HarnessError("prepare produced no comments")

        workdir = stage / "prepared.docx2typed"
        envelope = _cli(binpath, ["--json", "extract", str(prepared),
                                  "-o", str(workdir)], timeout, calls_log,
                        "extract-baseline-A")
        if envelope.get("outcome") != "success":
            _step("extract-baseline-A", False, f"codes={_codes(envelope)}")
            raise _HarnessError(f"extract baseline A failed: {_codes(envelope)}")
        epoch_a = _read_baseline_epoch(workdir)
        template_fp = _read_template_fingerprint(workdir)
        _step("extract-baseline-A", True, f"epoch={epoch_a}")
        artifacts["baseline_epoch_A"] = epoch_a
        artifacts["template_fingerprint_A"] = template_fp

        opened = _mcp(binpath, [("document_open",
                                 {"source": str(workdir), "author": AGENT_AUTHOR,
                                  "track": True})], timeout, calls_log, "agent-open")[0]
        if _is_transport(opened) or opened.get("outcome") != "success":
            raise _HarnessError(f"agent open failed: {_codes(opened)}")
        found = _mcp(binpath, [("document_open",
                                {"source": str(workdir), "author": AGENT_AUTHOR,
                                 "track": True}),
                               ("document_find", {"query": agent_old, "limit": 200})],
                     timeout, calls_log, "agent-find")[1]
        if _is_transport(found):
            raise _HarnessError("agent find transport error")
        matches = (found.get("data") or {}).get("matches") or []
        occs = [o for m in matches for o in (m.get("occurrences") or [m])
                if isinstance(o, dict)]
        single = [o for o in occs if (o.get("verdict") or {}).get("editable") is True]
        if len(occs) != 1 or len(single) != 1 or not single[0].get("handle"):
            _step("agent-find", False, f"occurrences={len(occs)}")
            _assert("agent-target-unique-editable", False, f"occurrences={len(occs)}")
            raise _HarnessError("agent target is not a single editable span")
        handle = single[0]["handle"]
        _step("agent-find", True, "one editable occurrence")
        edited = _mcp(binpath, [("document_open",
                                 {"source": str(workdir), "author": AGENT_AUTHOR,
                                  "track": True}),
                                ("document_edit",
                                 {"edits": [{"handle": handle,
                                             "replace_with": agent_new}]}),
                                ("document_save",
                                 {"output": str(stage / "agent_edited.docx")})],
                      timeout, calls_log, "agent-edit-save")
        if any(_is_transport(e) for e in edited):
            raise _HarnessError("agent edit/save transport error")
        if edited[1].get("outcome") != "success":
            _step("agent-edit", False, f"codes={_codes(edited[1])}")
            raise _ProductError(f"agent edit refused: {_codes(edited[1])}")
        if edited[2].get("outcome") != "success":
            save_env = edited[2] if isinstance(edited[2], dict) else {}
            diags = save_env.get("diagnostics") or []
            artifacts["agent_edit_accepted"] = True
            artifacts["agent_edit_codes"] = _codes(edited[1])
            artifacts["agent_save_codes"] = _codes(edited[2])
            artifacts["agent_save_diagnostic_codes"] = [
                str(d.get("code")) for d in diags if isinstance(d, dict)]
            artifacts["agent_save_output_exists"] = (stage / "agent_edited.docx").exists()
            raise _ProductError(
                f"agent save refused after accepted edit: {_codes(edited[2])} "
                f"(fixture preserved; core triage, not harness)")
        agent_path = stage / "agent_edited.docx"
        agent_sha = _sha256_file(agent_path)
        _step("agent-edit-save-B", True, f"sha {agent_sha[:12]}")
        artifacts["agent_output_sha256"] = agent_sha
        artifacts["agent_old_sha256"] = _sha256_text(agent_old)
        artifacts["agent_new_sha256"] = _sha256_text(agent_new)

        inspected = _word_call("inspect", {"InputPath": str(agent_path)},
                               stage, "inspect-B", timeout, calls_log)
        if not inspected.get("ok"):
            _raise_word("inspect B", inspected)
        (stage / "inspect-B.word.json").write_text(
            json.dumps(inspected, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        revs = inspected.get("revisions") or []
        comments = inspected.get("comments") or []
        b_rev_set = sorted(
            (str(r.get("author")), str(r.get("type")), str(r.get("text")))
            for r in revs if isinstance(r, dict))
        b_comment_set = sorted(
            (str(c.get("author")), str(c.get("text"))) for c in comments
            if isinstance(c, dict))
        b_anchors = sorted(
            str(c.get("anchor") or c.get("scope")) for c in comments
            if isinstance(c, dict))
        _step("word-inspect-B", True,
              f"revs={len(revs)} comments={len(comments)}")
        # Prior set must survive exactly (semantic multiset, prepared identity);
        # agent adds exactly its own distinct-authored revision(s).
        _assert("prior-revisions-preserved-exact-multiset",
                all(item in b_rev_set for item in prep_rev_set),
                f"prep={len(prep_rev_set)} in_B={len(b_rev_set)}")
        agent_revs = [r for r in revs
                      if str(r.get("author")) == AGENT_AUTHOR and isinstance(r, dict)]
        _assert("agent-revisions-distinct-author-present",
                bool(agent_revs)
                and all(str(r.get("author")) not in {a for a, _, _ in prep_rev_set}
                        for r in agent_revs)
                and any(agent_new in (r.get("text") or "")
                        or agent_new in (r.get("context") or "") for r in agent_revs),
                f"agent_revs={len(agent_revs)}")
        _assert("agent-author-distinct-from-prepared",
                AGENT_AUTHOR not in {a for a, _, _ in prep_rev_set}, "")
        _assert("prior-comments-preserved-anchor",
                all(item in b_comment_set for item in prep_comment_set)
                and all(a in b_anchors for a in prep_comment_anchors),
                f"comments={len(comments)}")
        if not (all(item in b_rev_set for item in prep_rev_set) and agent_revs
                and all(item in b_comment_set for item in prep_comment_set)):
            raise _ProductError("word inspect B identity mismatch")

        accept_path = stage / "accept_agent_only.docx"
        accepted = _word_call("accept",
                              {"InputPath": str(agent_path), "Out": str(accept_path),
                               "AgentAuthor": AGENT_AUTHOR},
                              stage, "accept-agent", timeout, calls_log)
        reject_path = stage / "reject_agent_only.docx"
        rejected = _word_call("reject",
                              {"InputPath": str(agent_path), "Out": str(reject_path),
                               "AgentAuthor": AGENT_AUTHOR},
                              stage, "reject-agent", timeout, calls_log)
        if not accepted.get("ok") or not rejected.get("ok"):
            raise _HarnessError("selective Word decide failed: "
                              f"{accepted.get('error')} / {rejected.get('error')}")
        _step("word-selective-accept-reject", True,
              f"touched={accepted.get('touched')}/{rejected.get('touched')}")
        _assert("selective-touched-agent-only",
                accepted.get("touched") == len(agent_revs)
                and rejected.get("touched") == len(agent_revs),
                f"agent_revs={len(agent_revs)}")
        artifacts["accept_sha256"] = _sha256_file(accept_path)
        artifacts["reject_sha256"] = _sha256_file(reject_path)
        (stage / "accept.word.json").write_text(
            json.dumps(accepted, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        (stage / "reject.word.json").write_text(
            json.dumps(rejected, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")

        # Real Word REOPEN of the actual saved copies in new Word instances:
        # inspect text/revisions/comments from disk, never the pre-save memory
        # snapshot. Semantic proof, not counts.
        accept_re = _word_call("inspect", {"InputPath": str(accept_path)},
                               stage, "reopen-accept", timeout, calls_log)
        reject_re = _word_call("inspect", {"InputPath": str(reject_path)},
                               stage, "reopen-reject", timeout, calls_log)
        if not accept_re.get("ok") or not reject_re.get("ok"):
            _raise_word("reopen accept/reject",
                        accept_re if not accept_re.get("ok") else reject_re)
        (stage / "reopen-accept.word.json").write_text(
            json.dumps(accept_re, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        (stage / "reopen-reject.word.json").write_text(
            json.dumps(reject_re, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        accept_text = accept_re.get("text") or ""
        reject_text = reject_re.get("text") or ""
        _assert("accept-copy-shows-agent-new",
                agent_new in accept_text and agent_old not in accept_text, "")
        _assert("reject-copy-shows-agent-old",
                agent_old in reject_text and agent_new not in reject_text, "")
        a_rev_set = sorted(
            (str(r.get("author")), str(r.get("type")), str(r.get("text")))
            for r in (accept_re.get("revisions") or []) if isinstance(r, dict))
        r_rev_set = sorted(
            (str(r.get("author")), str(r.get("type")), str(r.get("text")))
            for r in (reject_re.get("revisions") or []) if isinstance(r, dict))
        _assert("prior-revisions-preserved-exact-both-copies",
                all(item in a_rev_set for item in prep_rev_set)
                and all(item in r_rev_set for item in prep_rev_set),
                f"prep={len(prep_rev_set)} accept={len(a_rev_set)} reject={len(r_rev_set)}")
        a_comment_set = sorted(
            (str(c.get("author")), str(c.get("text")))
            for c in (accept_re.get("comments") or []) if isinstance(c, dict))
        r_comment_set = sorted(
            (str(c.get("author")), str(c.get("text")))
            for c in (reject_re.get("comments") or []) if isinstance(c, dict))
        a_anchors = sorted(
            str(c.get("anchor") or c.get("scope"))
            for c in (accept_re.get("comments") or []) if isinstance(c, dict))
        r_anchors = sorted(
            str(c.get("anchor") or c.get("scope"))
            for c in (reject_re.get("comments") or []) if isinstance(c, dict))
        _assert("comments-anchor-match-prepared-both-copies",
                all(item in a_comment_set for item in prep_comment_set)
                and all(item in r_comment_set for item in prep_comment_set)
                and all(a in a_anchors for a in prep_comment_anchors)
                and all(a in r_anchors for a in prep_comment_anchors),
                f"prep_comments={len(prep_comment_set)}")
        # Stale protection probe (distinct goal): refusal is safe-protection
        # evidence recorded in artifacts, NOT a gate on the selective outcome.
        stale_outcome: str | None = None
        stale_completion = "COMPLETE"
        try:
            word_c = stage / "word_operator_C.docx"
            touched = _word_call("touch",
                                 {"InputPath": str(agent_path), "Out": str(word_c),
                                  "PriorAuthor": PRIOR_AUTHOR,
                                  "CommentText": "Operator follow-up sentence for stale check."},
                                 stage, "operator-C", timeout, calls_log)
            if not touched.get("ok"):
                _raise_word("operator C", touched)
            artifacts["word_operator_C_sha256"] = _sha256_file(word_c)
            stale_pre_tree = _tree_hashes(stage)
            stale_pre_workdir = _tree_hashes(workdir)
            stale_env = _cli(binpath, ["--json", "extract", str(word_c),
                                       "-o", str(workdir)], timeout, calls_log,
                             "stale-extract-must-refuse")
            stale_codes = _codes(stale_env)
            stale_post_tree = _tree_hashes(stage)
            stale_post_workdir = _tree_hashes(workdir)
            stale_clean = (stale_post_tree == stale_pre_tree
                           and stale_post_workdir == stale_pre_workdir
                           and stale_env.get("outcome") != "success"
                           and any("workdir-not-at-baseline" in code
                                   or "not-at-baseline" in code for code in stale_codes))
            artifacts["stale_protection"] = {
                "refused": bool(stale_clean), "codes": stale_codes,
            }
            _step("stale-publication-refused", stale_clean, f"codes={stale_codes}")
            if stale_clean:
                cont_env = _cli(binpath, ["--json", "extract", str(word_c),
                                          "-o", str(workdir),
                                          "--discard-committed-versions"],
                                timeout, calls_log, "explicit-continue-C")
                epoch_c = _read_baseline_epoch(workdir)
                fp_c = _read_template_fingerprint(workdir)
                artifacts["safe_continue"] = {
                    "epoch_C": epoch_c, "template_fingerprint_C": fp_c,
                    "cli": ["extract", "--discard-committed-versions"],
                }
                _step("explicit-continue-C", cont_env.get("outcome") == "success",
                      f"epoch={epoch_c}")
                stale_outcome = PASS if (
                    cont_env.get("outcome") == "success"
                    and (epoch_c or 0) > (epoch_a or 0)
                    and _sha256_file(word_c) == artifacts["word_operator_C_sha256"]
                ) else CONTRACT_FAILURE
            else:
                stale_outcome = CONTRACT_FAILURE
        except (_HarnessError, TimeoutError, OSError) as exc:
            _step("stale-lane-error", False, f"harness: {exc}"[:300])
            stale_completion = "incomplete"
            stale_outcome = None
        artifacts["stale_case"] = {"completion_status": stale_completion,
                                   "outcome": stale_outcome}
        if all(a.get("result") == "pass" for a in assertions):
            outcome = PASS
        else:
            outcome = CONTRACT_FAILURE
        _step("reopen-verify", outcome == PASS, f"outcome={outcome}")
    except _ProductError as exc:
        _step("lane-error", False, str(exc)[:300])
        outcome = CONTRACT_FAILURE
    except _HarnessError as exc:
        _step("lane-error", False, f"harness: {exc}"[:300])
        completion = "incomplete"
        outcome = None
    except Exception as exc:
        _step("lane-error", False, str(exc)[:300])
        completion = "incomplete"
        outcome = None
    cases.append({"case_id": case_id, "input_sha256": pinned["sha256"],
                  "completion_status": completion, "outcome": outcome,
                  "steps": steps, "assertions": assertions,
                  "artifacts": artifacts, "limitations": limitations})

    # ---- negative case: cross-boundary edit must refuse --------------------
    case_id = "review-handoff-cross-boundary"
    steps = []
    assertions = []
    artifacts_nb: dict = {"input_sha256": pinned["sha256"]}
    completion_nb = "COMPLETE"
    outcome_nb: str | None = None

    def _step2(name: str, ok: bool, detail: str = "") -> None:
        steps.append({"name": name, "ok": ok, "detail": detail[:300]})

    def _assert2(ident: str, passed: bool, detail: str = "") -> None:
        assertions.append({"id": ident, "result": "pass" if passed else "fail",
                           "detail": detail[:300]})

    try:
        stage = evidence / "cross_boundary"
        if stage.exists():
            shutil.rmtree(stage)
        stage.mkdir(parents=True)
        # Independent of the positive agent edit/save: the negative case needs
        # only the Word-prepared file (prior tracked change + normal text).
        # The run4 save-side core failure cannot block this refusal case.
        input_copy = stage / "input.docx"
        shutil.copyfile((_resolve_source(manifest, pinned) if _resolve_source
                         else manifest.parent / pinned["source_path"]), input_copy)
        prepared = stage / "prepared.docx"
        payload = _word_call("prepare",
                             {"InputPath": str(input_copy), "Out": str(prepared),
                              "PriorAuthor": PRIOR_AUTHOR, "PriorOld": prior_old,
                              "PriorNew": prior_new, "CommentAnchor": anchor,
                              "CommentText": PRIOR_COMMENT_TEXT,
                              "AgentAuthor": AGENT_AUTHOR},
                             stage, "prepare", timeout, calls_log)
        if not payload.get("ok"):
            _raise_word("prepare", payload)
        workdir = stage / "prepared.docx2typed"
        envelope = _cli(binpath, ["--json", "extract", str(prepared),
                                  "-o", str(workdir)], timeout, calls_log,
                        "extract-baseline-A")
        if envelope.get("outcome") != "success":
            raise _HarnessError(f"extract failed: {_codes(envelope)}")
        _step2("boundary-setup", True, "Word-prepared prior revision present")

        # Crossing query anchored on the Word-created PRIOR tracked revision:
        # Word nests w:ins/w:del as DIRECT children of w:p (siblings of w:r),
        # so iterate paragraph children, not just w:r runs. Bridge prior_new
        # (inside Word's w:ins) + adjacent regular run text in the SAME
        # paragraph. No guessing: the boundary is Word's own w:del/w:ins.
        import xml.etree.ElementTree as ET
        wns = "{http://schemas.openxmlformats.org/wordprocessingml/2006/main}"

        def _local(tag: str) -> str:
            return tag.rsplit("}", 1)[-1] if "}" in tag else tag.rsplit(":", 1)[-1]

        with zipfile.ZipFile(prepared) as archive:
            prep_xml = archive.read("word/document.xml")
        root = ET.fromstring(prep_xml)
        crossing = ""
        crossing_context = ""
        for para in root.iter(f"{wns}p"):
            kids = list(para)
            for i, kid in enumerate(kids):
                kind = _local(kid.tag)
                if kind == "ins":
                    ins_text = "".join((n.text or "") for n in kid.iter(f"{wns}t"))
                elif kind == "del":
                    continue  # deleted text is not visible; the bridge uses w:ins
                else:
                    continue
                if prior_new not in ins_text:
                    continue
                neighbor = ""
                for j in list(range(i - 1, -1, -1)) + list(range(i + 1, len(kids))):
                    other = kids[j]
                    other_kind = _local(other.tag)
                    if other_kind in ("ins", "del"):
                        continue
                    if other_kind != "r":
                        continue
                    joined = "".join((n.text or "") for n in other.findall(f"{wns}t"))
                    if len(joined.strip()) >= 4:
                        neighbor = joined.strip()
                        break
                if not neighbor:
                    continue
                tail = neighbor[-4:] if len(neighbor) >= 4 else neighbor
                # FULL inserted run text: last covered run ends exactly where
                # the query ends, so no partial-tail format predicate. The span
                # still crosses normal+revision (prefix from the regular run +
                # the entire Word w:ins body), which is the revision boundary
                # under test.
                crossing = (tail + ins_text).strip()
                if prior_new not in crossing:
                    crossing = ""
                    continue
                crossing_context = "".join(
                    "".join((n.text or "") for n in k.iter(f"{wns}t")) for k in kids)
                break
            if crossing:
                break
        if not crossing:
            raise _HarnessError("cannot build revision-anchored crossing span fixture")
        artifacts_nb["crossing_query_sha256"] = _sha256_text(crossing)
        artifacts_nb["crossing_context_sha256"] = _sha256_text(crossing_context)
        artifacts_nb["crossing_source"] = "word-prior-w:ins"
        prepare_revs = payload.get("revisions") or []
        artifacts_nb["prepared_revision_authors"] = sorted({str(r.get("author")) for r in prepare_revs})
        _step2("crossing-fixture", True, f"span_chars={len(crossing)}")

        # Projection initialization (known ADR: first find materializes
        # baseline.view/edit.md/edit.state.json): run a SAFE find on the known
        # leaf-exact agent_old target (no edit) BEFORE the proof snapshot, so
        # the refusal probe's delta excludes lazy-projection writes. A bare
        # warm open alone does not initialize the projection.
        _mcp(binpath, [("document_open",
                        {"source": str(workdir), "author": AGENT_AUTHOR,
                         "track": True}),
                       ("document_find", {"query": agent_old, "limit": 200})],
             timeout, calls_log, "warm-safe-find")
        pre_tree = _tree_hashes(stage)
        pre_source = _sha256_file(prepared)
        find_env = _mcp(binpath, [("document_open",
                                  {"source": str(workdir), "author": AGENT_AUTHOR,
                                   "track": True}),
                                 ("document_find", {"query": crossing, "limit": 200})],
                        timeout, calls_log, "cross-boundary-find")[1]
        if _is_transport(find_env):
            completion_nb = "incomplete"
            outcome_nb = None
            _step2("cross-boundary-find", False, "transport error: no refusal claim")
        else:
            matches = (find_env.get("data") or {}).get("matches") or []
            occs = [o for m in matches for o in (m.get("occurrences") or [m])
                    if isinstance(o, dict)]
            named = ""
            for occ in occs:
                verdict = occ.get("verdict") or {}
                if verdict.get("editable") is False and verdict.get("code"):
                    named = str(verdict.get("code"))
                    break
            codes = _codes(find_env)
            boundary_named = bool(named) and ("revision" in named or "boundary" in named
                                             or "cross-leaf" in named or "cross_leaf" in named
                                             or "match-spans" in named or "unsafe" in named)
            if boundary_named:
                _step2("cross-boundary-find-refusal", True, f"code={named}")
                post_tree = _tree_hashes(stage)
                post_source = _sha256_file(prepared)
                diff = _functional_diff(pre_tree, post_tree)
                proof = (not diff["changed_or_added"] and not diff["removed"]
                         and post_source == pre_source
                         and not (stage / "crossing_out.docx").exists())
                _assert2("named-refusal-at-find", True, f"code={named}")
                _assert2("no-write-proof", proof,
                         f"diff={diff}"[:300] if not proof else "")
                artifacts_nb["refusal_tree_diff"] = diff
                artifacts_nb["refusal_source_stable"] = (post_source == pre_source)
                if proof:
                    outcome_nb = EXPECTED_REJECT
                else:
                    _step2("refusal-mutated-state", False, f"diff={diff}"[:300])
                    outcome_nb = CONTRACT_FAILURE
            elif named:
                _step2("cross-boundary-find-refusal", False,
                       f"non-boundary code={named} is not boundary proof")
                _assert2("non-boundary-code-is-not-refusal", False,
                         f"code={named}")
                outcome_nb = CONTRACT_FAILURE
            else:
                # Find did not refuse: a single editable claim must be
                # challenged at edit; an uneditable-without-code or zero-match
                # is NEVER an EXPECTED_REJECT.
                edit_handle = None
                for occ in occs:
                    if isinstance(occ.get("handle"), str) and occ.get("handle"):
                        edit_handle = occ["handle"]
                        break
                if edit_handle is None:
                    _step2("cross-boundary-find", False,
                            f"no named refusal and no handle codes={codes}")
                    _assert2("zero-or-codeless-find-is-not-refusal", False,
                             f"occurrences={len(occs)}")
                    outcome_nb = CONTRACT_FAILURE
                else:
                    pre_tree2 = _tree_hashes(stage)
                    edit_env = _mcp(
                        binpath,
                        [("document_open",
                          {"source": str(workdir), "author": AGENT_AUTHOR,
                           "track": True}),
                         ("document_edit",
                          {"edits": [{"handle": edit_handle,
                                     "replace_with": prior_new + "Z"}]}),
                         ("document_save",
                          {"output": str(stage / "crossing_out.docx")})],
                        timeout, calls_log, "cross-boundary-edit")[1]
                    data = edit_env.get("data") or {}
                    blocked = data.get("blocked_targets") or []
                    reason = ""
                    if isinstance(blocked, list) and blocked:
                        first = blocked[0] if isinstance(blocked[0], dict) else {}
                        reason = str(first.get("reason") or first.get("code") or "")
                    named_edit = reason.split(":")[0] if reason else ""
                    post_tree = _tree_hashes(stage)
                    clean = (post_tree == pre_tree2
                             and _sha256_file(prepared) == pre_source
                             and not (stage / "crossing_out.docx").exists())
                    boundary_edit = bool(named_edit) and ("revision" in reason or "boundary" in reason
                                                         or "cross-leaf" in reason or "match-spans" in reason
                                                         or "unsafe" in reason)
                    if edit_env.get("outcome") != "success" and boundary_edit and clean:
                        _step2("cross-boundary-edit-refusal", True,
                               f"reason={named_edit}")
                        _assert2("named-refusal-at-edit", True,
                                 f"reason={named_edit}")
                        _assert2("no-write-proof", clean,
                                 "" if clean else "edit probe mutated state")
                        outcome_nb = EXPECTED_REJECT if clean else CONTRACT_FAILURE
                    elif edit_env.get("outcome") == "success":
                        _step2("cross-boundary-edit", False,
                               "crossing edit published: boundary unguarded")
                        _assert2("crossing-edit-must-refuse", False, "")
                        outcome_nb = CONTRACT_FAILURE
                    else:
                        _step2("cross-boundary-edit", False,
                               f"non-boundary failure codes={_codes(edit_env)}")
                        _assert2("non-boundary-failure-is-not-refusal", False,
                                 f"codes={_codes(edit_env)}")
                        outcome_nb = CONTRACT_FAILURE
    except (_HarnessError, TimeoutError, OSError) as exc:
        _step2("lane-error", False, f"harness: {exc}"[:300])
        completion_nb = "incomplete"
        outcome_nb = None
    except Exception as exc:
        _step2("lane-error", False, str(exc)[:300])
        completion_nb = "incomplete"
        outcome_nb = None
    cases.append({"case_id": case_id, "input_sha256": pinned["sha256"],
                  "completion_status": completion_nb, "outcome": outcome_nb,
                  "steps": steps, "assertions": assertions,
                  "artifacts": artifacts_nb, "limitations": limitations})

    report = {"schema": SCHEMA, "workflow": WORKFLOW, "host": host_record,
              "binary": {"sha256": binary["sha256"],
                         "version": binary.get("version", "unavailable")},
              "manifest_sha256": manifest_sha,
              "cases": cases}
    (outdir / "report.json").write_text(
        json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    (evidence / "calls.json").write_text(
        json.dumps(calls_log, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    return report


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--bin", default=None)
    parser.add_argument("--timeout", type=int, default=240)
    parser.add_argument("--document-id", default=None)
    args = parser.parse_args(argv)
    if args.out.exists() and (args.out / "report.json").exists():
        print(f"refused: --out already holds a report: {args.out}")
        return 2
    try:
        binpath = _resolve_bin(args.bin)
        summary = run_lane(args.manifest, args.out, binpath,
                           args.timeout, args.document_id)
    except (FileNotFoundError, ValueError) as exc:
        print(f"review-handoff setup refused: {exc}")
        return 2
    print(json.dumps({"workflow": WORKFLOW,
                      "cases": [(c.get("case_id"), c.get("completion_status"),
                                 c.get("outcome")) for c in summary.get("cases", [])]},
                     ensure_ascii=False, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
