"""A1.1 gate: the old and new typed.md renderers must agree on the same state.

Two routes can now produce `typed.md`:

    old   re-parse the draft's `edit.md` dialect           (draft.rs)
    new   render the node tree of the current package      (prose.rs)

This runs both against ONE state and reports where they disagree, by identity
rather than by count. A count cannot tell a preserved span from a replaced one,
and `400 = 400` is exactly how the previous global projection loss stayed
invisible.

Two states, because they answer different questions:

    pristine   the template, no edits. Any difference is the old route's loss,
               with nothing else to blame.
    edited     template + a committed tracked edit. Any difference is loss that
               survives an edit — what A1 exists to remove.

Identity churn is reported separately from structural loss. The new route
allocates token ids over the edited tree, so a paragraph can legitimately
renumber (`N919 -> N921`) while carrying exactly the same structure; that is
normalised away before the comparison, and anything that survives
normalisation is a real difference.

Usage:
    python qualification/typed_renderer_diff.py <document.docx> [--json out.json]
"""

from __future__ import annotations

import argparse
import json
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

BIN = Path(__file__).resolve().parent.parent / "target/release/docx2typed.exe"

TAGS = ("span", "docx-revision", "docx-anchor", "docx-inline", "docx-opaque", "docx-range")


def call(*messages, timeout=900):
    payload = "\n".join(json.dumps(m) for m in messages) + "\n"
    proc = subprocess.run(
        [str(BIN), "mcp"], input=payload, capture_output=True, text=True, timeout=timeout
    )
    out = []
    for line in proc.stdout.splitlines():
        if line.startswith("OK "):
            out.append(json.loads(line[3:]).get("structuredContent", {}))
        elif line.startswith("ERR "):
            out.append({"error": json.loads(line[4:])})
    return out


def project_view(path: Path) -> str:
    proc = subprocess.run(
        [str(BIN), "--json", "project-view", str(path)],
        capture_output=True,
        text=True,
        timeout=900,
    )
    blob = json.loads(proc.stdout)
    if blob.get("outcome") != "success":
        raise SystemExit(f"project-view failed on {path}: {blob.get('diagnostics')}")
    return blob["data"]["typed_md"]


def paragraphs(md: str) -> dict[str, str]:
    out: dict[str, str] = {}
    current: str | None = None
    body: list[str] = []

    def flush():
        if current is not None:
            out[current] = "\n".join(body).strip()

    for line in md.splitlines():
        stripped = line.strip()
        if stripped.startswith('<!--@p id="'):
            flush()
            current = stripped.split('id="')[1].split('"')[0]
            body = []
            continue
        if stripped.startswith("<!--@part ") or stripped.startswith("<!--@typed"):
            flush()
            current, body = None, []
            continue
        if current is not None:
            body.append(line)
    flush()
    return out


def node_kinds(body: str) -> list[str]:
    """The node sequence, ids stripped: what the paragraph IS, not what it is called."""
    out = []
    for match in re.finditer(r"<(" + "|".join(TAGS) + r")\b([^>]*)>", body):
        tag, attrs = match.group(1), match.group(2)
        kind = re.search(r'kind="([^"]*)"', attrs)
        out.append(f"{tag}:{kind.group(1)}" if kind else tag)
    return out


def normalise(body: str) -> str:
    """Ids and style hashes removed; kinds, text and order kept."""
    text = re.sub(r'\bid="[^"]*"', 'id="X"', body)
    text = re.sub(r'\bdata-s="[^"]*"', 'data-s="S"', text)
    text = re.sub(r'\bstyle="[^"]*"', 'style="S"', text)
    return text


def compare(old_md: str, new_md: str) -> dict:
    old_p, new_p = paragraphs(old_md), paragraphs(new_md)
    common = sorted(set(old_p) & set(new_p))
    report: dict = {
        "paragraphs": {"old": len(old_p), "new": len(new_p)},
        "only_old": sorted(set(old_p) - set(new_p)),
        "only_new": sorted(set(new_p) - set(old_p)),
        "identical": [k for k in common if old_p[k] == new_p[k]],
    }
    # Churn: same node kinds, same order, only ids/numbering differ.
    churn, lost = [], []
    for key in common:
        if old_p[key] == new_p[key]:
            continue
        if normalise(old_p[key]) == normalise(new_p[key]):
            churn.append(key)
        else:
            lost.append(key)
    report["renumbered_only"] = churn
    report["structurally_different"] = lost
    report["structural_detail"] = {
        key: {
            "old_kinds": node_kinds(old_p[key]),
            "new_kinds": node_kinds(new_p[key]),
            "old_nodes": len(node_kinds(old_p[key])),
            "new_nodes": len(node_kinds(new_p[key])),
        }
        for key in lost[:8]
    }
    counts = {
        k: {"old": old_md.count(f'kind="{k}"'), "new": new_md.count(f'kind="{k}"')}
        for k in ("comment-start", "comment-end", "commentReference", "insert", "delete")
    }
    report["kind_counts"] = counts
    report["totals"] = {
        "spans": {"old": old_md.count("data-s="), "new": new_md.count("data-s=")},
        "revisions": {"old": old_md.count("<docx-revision"), "new": new_md.count("<docx-revision")},
        "anchors": {"old": old_md.count("<docx-anchor"), "new": new_md.count("<docx-anchor")},
        "inlines": {"old": old_md.count("<docx-inline"), "new": new_md.count("<docx-inline")},
    }
    return report


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("document")
    ap.add_argument("--json")
    ap.add_argument("--query", default="背景技术", help="term to edit in the 'edited' state")
    ap.add_argument("--replace", default="技术背景")
    args = ap.parse_args()

    src = Path(args.document).resolve()
    root = Path(tempfile.mkdtemp(prefix="a1-gate-"))
    wd = root / "wd"
    subprocess.run(
        [str(BIN), "--json", "extract", str(src), "-o", str(wd), "--operation-id", "a1"],
        capture_output=True,
        check=True,
    )

    report: dict = {"source": str(src)}

    # ---- pristine: template only -----------------------------------------
    old_pristine = (wd / "typed.md").read_text(encoding="utf-8")
    new_pristine = project_view(wd / "_template.docx")
    report["pristine"] = compare(old_pristine, new_pristine)

    # ---- edited: a real tracked edit, committed, then built ---------------
    open_track = {"tool": "workdir_open", "args": {"workdir": str(wd), "track": True}}
    located = call(open_track, {"tool": "facade_locate", "args": {"query": args.query}})
    targets = located[1]["data"]["targets"] if len(located) > 1 else []
    report["edit"] = {"query": args.query, "replace": args.replace, "located": len(targets)}
    if targets:
        handle = targets[0]["handle"]
        edited = call(
            open_track,
            {
                "tool": "facade_edit",
                "args": {
                    "edits": [{"handle": handle, "replace_with": args.replace}],
                    "track": True,
                    "author": "A1",
                },
            },
        )
        report["edit"]["outcome"] = edited[1].get("outcome") if len(edited) > 1 else None
        call(open_track, {"tool": "commit_sync", "args": {"operation_id": "a1c"}})
        built = call(
            open_track,
            {"tool": "build_docx", "args": {"output": str(root / "out.docx"), "operation_id": "a1b"}},
        )
        report["edit"]["built"] = built[1].get("outcome") if len(built) > 1 else None
        old_edited = (wd / "typed.md").read_text(encoding="utf-8")
        report["edited"] = compare(old_edited, project_view(root / "out.docx"))
    else:
        report["edited"] = None

    if args.json:
        Path(args.json).write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps(report, ensure_ascii=False, indent=2))
    shutil.rmtree(root, ignore_errors=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
