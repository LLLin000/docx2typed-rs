"""Differential projection audit: Rust engine vs the Python reference.

Answers one question per document: *where do the two engines disagree about the
same file?* Not "do they differ" — a byte diff says nothing about which side is
wrong — but which layer disagrees, and in what way.

Four layers, checked separately because they fail separately:

    source OOXML   the ground truth both engines read
    format.json    the model (tokens, paragraphs, styles)
    styles.json    style canonicalization
    typed.md       the readable typed projection
    edit.md        the editable projection

The revision inventory is built from the source package, not from either
engine, so it can act as the baseline for the fidelity invariant: an edit may
add revisions and must not disturb the ones already there.

Usage:
    python qualification/projection_audit.py <document.docx> [--json out.json]
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
from collections import Counter
from pathlib import Path

RUST_BIN = Path("target/release/docx2typed.exe")
PY_MODULE = "docx2typed.mcp_server"
PY_EXE = Path("D:/programs/Python/python.EXE")


# --------------------------------------------------------------------------
# Ground truth: the revision inventory, read straight from the source package
# --------------------------------------------------------------------------

REVISION_TAGS = {"ins": "insert", "del": "delete", "moveFrom": "move_from", "moveTo": "move_to"}


def revision_inventory(docx: Path) -> list[dict]:
    """Every tracked revision in the source, with its identity and content.

    This is the fidelity baseline. A build is faithful when each of these still
    exists with the same identity and the same text, regardless of how many
    `<w:ins>` elements the output happens to contain — a count proves nothing
    about which revisions survived.
    """
    with zipfile.ZipFile(docx) as z:
        xml = z.read("word/document.xml").decode("utf-8")
    out: list[dict] = []
    for tag, kind in REVISION_TAGS.items():
        # Non-greedy, and revisions do not nest in these documents.
        for m in re.finditer(rf"<w:{tag}\b([^>]*)>(.*?)</w:{tag}>", xml, re.S):
            attrs_raw, inner = m.group(1), m.group(2)
            attrs = dict(re.findall(r'w:([A-Za-z]+)="([^"]*)"', attrs_raw))
            # `w:t` for insertions, `w:delText` for deletions.
            text = "".join(re.findall(r"<w:(?:t|delText)[^>]*>(.*?)</w:(?:t|delText)>", inner, re.S))
            out.append(
                {
                    "kind": kind,
                    "w:id": attrs.get("id", ""),
                    "author": attrs.get("author", ""),
                    "date": attrs.get("date", ""),
                    "text": text,
                }
            )
    out.sort(key=lambda r: (r["kind"], int(r["w:id"]) if r["w:id"].isdigit() else -1))
    return out


# --------------------------------------------------------------------------
# Extraction, both engines
# --------------------------------------------------------------------------


def rust_extract(src: Path, wd: Path) -> None:
    subprocess.run(
        [str(RUST_BIN.resolve()), "--json", "extract", str(src), "-o", str(wd), "--operation-id", "audit"],
        capture_output=True,
        check=True,
    )


def py_extract(src: Path, wd: Path) -> None:
    subprocess.run(
        [str(PY_EXE), "-m", "docx2typed", "extract", str(src), "-o", str(wd)],
        capture_output=True,
        check=True,
    )


def paragraphs_of(md: str) -> dict[str, str]:
    return dict(re.findall(r'<!--@p id="([^"]+)"[^\n]*\n(.*?)(?=\n<!--@|\Z)', md, re.S))


def strip_placeholders(s: str) -> str:
    return re.sub(r"\u27e6[^\u27e7]*\u27e7", "", s)


def strip_span_tags(s: str) -> str:
    return re.sub(r"</?span[^>]*>", "", s)


# --------------------------------------------------------------------------
# The audit
# --------------------------------------------------------------------------


def compare(src: Path, workdir: Path, json_out: Path | None) -> dict:
    workdir.mkdir(parents=True, exist_ok=True)
    rust_wd, py_wd = workdir / "rust", workdir / "python"
    for d in (rust_wd, py_wd):
        if d.is_dir():
            shutil.rmtree(d)
    rust_extract(src, rust_wd)
    py_extract(src, py_wd)

    report: dict = {"source": str(src), "layers": {}, "divergences": []}

    # ---- layer: the model -------------------------------------------------
    rfmt = json.loads((rust_wd / "format.json").read_text(encoding="utf-8"))
    pfmt = json.loads((py_wd / "format.json").read_text(encoding="utf-8"))
    rt, pt = rfmt.get("tokens", {}), pfmt.get("tokens", {})
    token_diff = [k for k in sorted(set(rt) | set(pt)) if rt.get(k) != pt.get(k)]
    by_kind = Counter((rt.get(k) or pt.get(k) or {}).get("kind", "<absent>") for k in token_diff)
    rp, pp = rfmt.get("paragraphs", []), pfmt.get("paragraphs", [])
    para_diff = [p for p in rp if p not in pp]

    report["layers"]["format.json"] = {
        "tokens": {"rust": len(rt), "python": len(pt), "differing": len(token_diff), "by_kind": dict(by_kind)},
        "paragraphs": {"rust": len(rp), "python": len(pp), "differing": len(para_diff)},
        "styles_sha256_equal": rfmt.get("styles_sha256") == pfmt.get("styles_sha256"),
        "vertical_promotion": {
            "rust": rfmt.get("vertical_promotion"),
            "python": pfmt.get("vertical_promotion"),
        },
        "scalar_keys_differing": sorted(
            k
            for k in set(rfmt) | set(pfmt)
            if k not in {"tokens", "paragraphs"} and rfmt.get(k) != pfmt.get(k)
        ),
    }
    if token_diff:
        sample = token_diff[0]
        report["divergences"].append(
            {
                "layer": "format.json/tokens",
                "count": len(token_diff),
                "kinds": dict(by_kind),
                "sample": {"id": sample, "rust": rt.get(sample), "python": pt.get(sample)},
            }
        )
    if para_diff:
        a = para_diff[0]
        same = next((x for x in pp if x.get("id") == a.get("id")), None)
        report["divergences"].append(
            {
                "layer": "format.json/paragraphs",
                "count": len(para_diff),
                "sample": {"id": a.get("id"), "rust": a, "python": same},
            }
        )

    # ---- layer: styles ----------------------------------------------------
    rsty = json.loads((rust_wd / "styles.json").read_text(encoding="utf-8"))
    psty = json.loads((py_wd / "styles.json").read_text(encoding="utf-8"))
    rkeys, pkeys = set(json.dumps(rsty)), set(json.dumps(psty))
    report["layers"]["styles.json"] = {
        "equal": rsty == psty,
        "rust_bytes": len(json.dumps(rsty)),
        "python_bytes": len(json.dumps(psty)),
        "rust_only_ids": sorted(rkeys - pkeys)[:5],
        "python_only_ids": sorted(pkeys - rkeys)[:5],
    }

    # ---- layer: typed.md --------------------------------------------------
    rmd = (rust_wd / "typed.md").read_text(encoding="utf-8")
    pmd = (py_wd / "typed.md").read_text(encoding="utf-8")
    rpar, ppar = paragraphs_of(rmd), paragraphs_of(pmd)
    common = sorted(set(rpar) & set(ppar))
    only_py = sorted(set(ppar) - set(rpar))
    only_rs = sorted(set(rpar) - set(ppar))
    span_only = [k for k in common if rpar[k].strip() != ppar[k].strip()
                 and strip_span_tags(rpar[k].strip()) == strip_span_tags(ppar[k].strip())]
    real = [k for k in common if rpar[k].strip() != ppar[k].strip() and k not in span_only]
    report["layers"]["typed.md"] = {
        "paragraphs": {"rust": len(rpar), "python": len(ppar)},
        "only_python": len(only_py),
        "only_python_prefixes": sorted({k.split(".")[0] for k in only_py}),
        "only_rust": len(only_rs),
        "differ_only_by_span": len(span_only),
        "differ_otherwise": len(real),
        "rust_span_count": rmd.count("data-s="),
        "python_span_count": pmd.count("data-s="),
    }
    for k in real[:2]:
        report["divergences"].append(
            {"layer": "typed.md", "paragraph": k, "rust": rpar[k].strip()[:220], "python": ppar[k].strip()[:220]}
        )

    # ---- layer: edit.md ---------------------------------------------------
    red = (rust_wd / "edit.md").read_text(encoding="utf-8") if (rust_wd / "edit.md").is_file() else ""
    ped = (py_wd / "edit.md").read_text(encoding="utf-8") if (py_wd / "edit.md").is_file() else ""
    rpe, ppe = paragraphs_of(red), paragraphs_of(ped)
    ed_common = sorted(set(rpe) & set(ppe))
    ed_diff = [k for k in ed_common if strip_placeholders(rpe[k]).strip() != strip_placeholders(ppe[k]).strip()]
    report["layers"]["edit.md"] = {
        "present": {"rust": (rust_wd / "edit.md").is_file(), "python": (py_wd / "edit.md").is_file()},
        "paragraphs": {"rust": len(rpe), "python": len(ppe)},
        "only_python": len(set(ppe) - set(rpe)),
        "only_rust": len(set(rpe) - set(ppe)),
        "differ_after_stripping_placeholders": len(ed_diff),
    }

    # ---- layer: sidecar inventory ----------------------------------------
    # A workdir is a contract about what a consumer can read. An artifact one
    # engine writes and the other does not is a capability gap, not a format
    # difference, and it has to be named before anything downstream relies on it.
    rust_files = sorted(x.name for x in rust_wd.iterdir() if x.is_file())
    py_files = sorted(x.name for x in py_wd.iterdir() if x.is_file())
    report["layers"]["sidecars"] = {
        "rust": rust_files,
        "python": py_files,
        "missing_in_rust": sorted(set(py_files) - set(rust_files)),
        "missing_in_python": sorted(set(rust_files) - set(py_files)),
    }

    # ---- layer: the fidelity baseline ------------------------------------
    inv = revision_inventory(src)
    report["layers"]["source_revisions"] = {
        "total": len(inv),
        "by_kind": dict(Counter(r["kind"] for r in inv)),
        "authors": dict(Counter(r["author"] for r in inv)),
        "with_text": sum(1 for r in inv if r["text"]),
        "without_text": sum(1 for r in inv if not r["text"]),
    }

    # ---- layer: the revision inventories, compared as SETS ---------------
    # Counts are not fidelity. Two inventories can both hold 985 revisions with
    # 250 of them different, and two revisions can swap identities while the
    # count still balances. Everything below is set membership plus per-field
    # agreement over the keys that DO overlap.
    def load_revisions(path: Path) -> dict:
        if not path.is_file():
            return {}
        blob = json.loads(path.read_text(encoding="utf-8"))
        return {x["revision_key"]: x for x in (blob.get("revisions") or [])}

    rrev = load_revisions(rust_wd / "revisions.json")
    prev = load_revisions(py_wd / "revisions.json")
    layer = {
        "rust": len(rrev),
        "python": len(prev),
        "overlap": len(set(rrev) & set(prev)),
        "rust_only": len(set(rrev) - set(prev)),
        "python_only": len(set(prev) - set(rrev)),
        "rust_only_sample": sorted(set(rrev) - set(prev))[:3],
        "python_only_sample": sorted(set(prev) - set(rrev))[:3],
    }
    if rrev and prev:
        shared = set(next(iter(rrev.values()))) & set(next(iter(prev.values())))
        fields = Counter()
        for key in set(rrev) & set(prev):
            for name in shared:
                if rrev[key].get(name) != prev[key].get(name):
                    fields[name] += 1
        layer["shared_fields_differing"] = dict(fields)
        layer["rust_only_fields"] = sorted(set(next(iter(rrev.values()))) - shared)
    report["layers"]["revisions.json"] = layer
    if layer["rust_only"] or layer["python_only"]:
        report["divergences"].append({"layer": "revisions.json/sets", **layer})

    if json_out:
        json_out.write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
    return report


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("document")
    ap.add_argument("--json")
    ap.add_argument("--workdir")
    args = ap.parse_args()

    src = Path(args.document)
    wd = Path(args.workdir) if args.workdir else Path(tempfile.mkdtemp(prefix="proj-audit-"))
    report = compare(src, wd, Path(args.json) if args.json else None)

    print(f"source: {report['source']}")
    print()
    for layer, data in report["layers"].items():
        print(f"[{layer}]")
        print(json.dumps(data, ensure_ascii=False, indent=2))
        print()
    if report["divergences"]:
        print(f"[divergences] {len(report['divergences'])}")
        for d in report["divergences"]:
            print(json.dumps(d, ensure_ascii=False)[:400])
    return 0


if __name__ == "__main__":
    sys.exit(main())
