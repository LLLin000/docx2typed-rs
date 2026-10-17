#!/usr/bin/env python
"""Independent format-preservation check for a produced DOCX.

Reads a source DOCX and a produced DOCX and reports what the edit did to the
document's formatting. It is deliberately NOT the engine's own verifier: every
rule here is re-derived from the XML, so a pass is evidence about the artifact
rather than agreement between two copies of the same code.

What it checks
--------------
1. Paragraph properties are unchanged, paragraph by paragraph (canonical XML, so
   attribute order and spelling do not matter, but a changed value does).
2. Non-target runs: the `(rPr, text)` sequence of every run OUTSIDE the target is
   unchanged on both sides. Empty runs are ignored — a whole-run replacement is
   allowed to leave one behind.
3. The first-run-wins contract: the run carrying a replacement has the `rPr` of
   the run that held the target's start in the source.
4. Protected structure is unchanged: hyperlinks, bookmarks, comment anchors,
   tracked revisions, fields, drawings, section properties, numbering.

Targets
-------
`--replaced OLD=NEW` (repeatable) says exactly what the edit replaced. That
matters: a text diff cannot see a change of RUN STRUCTURE inside the replaced
text. The `Fe3O4` case is the proof — five runs (two of them superscripts) became
one plain run, and the diff, which only sees the appended `-x`, called the
emptied runs "outside the edit". With `--replaced` the target is the span, which
is what "non-target runs" means. Without it the tool falls back to the text diff
and says so in the report.

Usage:
  python scripts/format_preservation.py <source.docx> <produced.docx> [--json] \\
      [--replaced OLD=NEW]...
"""

from __future__ import annotations

import json
import sys
import zipfile

W = "http://schemas.openxmlformats.org/wordprocessingml/2006/main"

PROTECTED = {
    "hyperlink",
    "bookmarkStart",
    "bookmarkEnd",
    "commentRangeStart",
    "commentRangeEnd",
    "commentReference",
    "ins",
    "del",
    "fldChar",
    "instrText",
    "drawing",
    "pict",
    "sectPr",
    "numPr",
    "lastRenderedPageBreak",
}


def local(tag: str) -> str:
    return tag.rsplit("}", 1)[-1]


def parse(xml):
    from lxml import etree

    return etree.fromstring(xml if isinstance(xml, bytes) else xml.encode("utf-8"))


def c14n(element) -> str:
    """Canonical XML of one element: attribute order, whitespace and prefixes
    normalised, so two spellings of the same formatting compare equal."""
    from lxml import etree

    return etree.tostring(element, method="c14n").decode("utf-8")


def readable_rpr(rpr) -> str:
    """`b;sz=24;vertAlign=superscript` — for the report, not for identity."""
    parts = []
    for child in rpr:
        name = local(child.tag)
        values = list(child.attrib.values())
        parts.append(f"{name}={values[0]}" if values else name)
    return ";".join(parts) if parts else "(empty rPr)"


def runs_of(paragraph):
    """(canonical rPr, readable rPr, text) for every run, in text order."""
    out = []
    for run in paragraph.iter(f"{{{W}}}r"):
        rpr = run.find(f"{{{W}}}rPr")
        text = "".join(t.text or "" for t in run.iter(f"{{{W}}}t"))
        out.append(
            (
                c14n(rpr) if rpr is not None else "(none)",
                readable_rpr(rpr) if rpr is not None else "(none)",
                text,
            )
        )
    return out


def paragraphs(root) -> list:
    return list(root.iter(f"{{{W}}}p"))


def visible(paragraph) -> str:
    return "".join(t.text or "" for t in paragraph.iter(f"{{{W}}}t"))


def ppr_c14n(paragraph) -> str:
    ppr = paragraph.find(f"{{{W}}}pPr")
    return c14n(ppr) if ppr is not None else "(none)"


def occurrences(text: str, needle: str) -> list[tuple[int, int]]:
    spans = []
    start = text.find(needle)
    while start != -1:
        spans.append((start, start + len(needle)))
        start = text.find(needle, start + 1)
    return spans


def diff_region(before: str, after: str):
    """The minimal changed region as (before_start, before_end, after_start, after_end)."""
    if before == after:
        return None
    start = 0
    while start < min(len(before), len(after)) and before[start] == after[start]:
        start += 1
    end_b, end_a = len(before), len(after)
    while end_b > start and end_a > start and before[end_b - 1] == after[end_a - 1]:
        end_b -= 1
        end_a -= 1
    return start, end_b, start, end_a


def rpr_at(paragraph, offset: int):
    """(canonical rPr, readable rPr) of the run holding the offset-th character."""
    position = 0
    for canonical, readable, text in runs_of(paragraph):
        if position <= offset < position + len(text):
            return canonical, readable
        position += len(text)
    return None, None


def outside(runs, spans):
    """Runs' text outside every span, in order. A run that overlaps a span keeps
    only its outside part; an empty run is ignored on both sides."""
    position = 0
    kept = []
    for canonical, _, text in runs:
        start, end = position, position + len(text)
        position = end
        if not text:
            continue
        pieces = [(start, end)]
        for span_start, span_end in spans:
            next_pieces = []
            for piece_start, piece_end in pieces:
                if piece_end <= span_start or piece_start >= span_end:
                    next_pieces.append((piece_start, piece_end))
                    continue
                if piece_start < span_start:
                    next_pieces.append((piece_start, span_start))
                if piece_end > span_end:
                    next_pieces.append((span_end, piece_end))
            pieces = next_pieces
        for piece_start, piece_end in pieces:
            kept.append((canonical, text[piece_start - start : piece_end - start]))
    return kept


def structure_counts(root) -> dict[str, int]:
    counts: dict[str, int] = {}
    for element in root.iter():
        name = local(element.tag)
        if name in PROTECTED:
            counts[name] = counts.get(name, 0) + 1
    return counts


def check(source: str, produced: str, replaced: list[tuple[str, str]] | None = None) -> dict:
    with zipfile.ZipFile(source) as z:
        before_root = parse(z.read("word/document.xml"))
    with zipfile.ZipFile(produced) as z:
        after_root = parse(z.read("word/document.xml"))

    before_paras = paragraphs(before_root)
    after_paras = paragraphs(after_root)
    report: dict = {
        "paragraphs": {"before": len(before_paras), "after": len(after_paras)},
        "target": "replaced spans (--replaced)" if replaced else "text diff (no --replaced)",
        "checks": {},
        "edited": [],
    }
    failures: list[str] = []

    report["checks"]["paragraph_count_unchanged"] = len(before_paras) == len(after_paras)
    if not report["checks"]["paragraph_count_unchanged"]:
        failures.append(
            f"paragraph count changed: {len(before_paras)} -> {len(after_paras)}"
        )

    ppr_failures = [
        index
        for index, (b, a) in enumerate(zip(before_paras, after_paras))
        if ppr_c14n(b) != ppr_c14n(a)
    ]
    report["checks"]["paragraph_properties_unchanged"] = not ppr_failures
    if ppr_failures:
        report["paragraph_property_changes"] = ppr_failures[:10]
        failures.append(f"{len(ppr_failures)} paragraph(s) changed their pPr")

    run_failures: list[dict] = []
    first_run_failures: list[dict] = []
    not_applicable = 0

    for index, (b, a) in enumerate(zip(before_paras, after_paras)):
        before_text, after_text = visible(b), visible(a)
        spans_before: list[tuple[int, int]] = []
        spans_after: list[tuple[int, int]] = []
        if replaced:
            # Every occurrence of the replaced text, in order, paired with where
            # its replacement sits in the produced text: the same kept-text
            # position, shifted by every EARLIER replacement that actually
            # happened. A target the produced file does not hold (the edit
            # replaced a subset) is dropped from both sides and counted, rather
            # than reported as a change outside the target.
            candidates = []
            for old, new in replaced:
                for start, end in occurrences(before_text, old):
                    candidates.append((start, end, new))
            candidates.sort()
            shift = 0
            for start, end, new in candidates:
                produced_start = start + shift
                produced_end = produced_start + len(new)
                if after_text[produced_start:produced_end] != new:
                    report["not_replaced"] = report.get("not_replaced", 0) + 1
                    continue
                spans_before.append((start, end))
                spans_after.append((produced_start, produced_end))
                shift += len(new) - (end - start)
        if not spans_before:
            region = diff_region(before_text, after_text)
            if region is None:
                # Untouched: the whole paragraph must be canonical-XML equal.
                if c14n(b) != c14n(a):
                    run_failures.append(
                        {
                            "paragraph": index,
                            "kind": "untouched paragraph is not byte-equivalent",
                            "before": [(r[1], r[2]) for r in runs_of(b) if r[2]],
                            "after": [(r[1], r[2]) for r in runs_of(a) if r[2]],
                        }
                    )
                continue
            b_start, b_end, _, _ = region
            spans_before = [(b_start, b_end)]
            spans_after = [(b_start, b_start + (len(after_text) - len(before_text)) + (b_end - b_start))]
            report["edited"].append(
                {
                    "paragraph": index,
                    "before": before_text[b_start:b_end],
                    "after": after_text[spans_after[0][0] : spans_after[0][1]],
                }
            )
        else:
            report["edited"].append(
                {"paragraph": index, "targets": [before_text[s:e] for s, e in spans_before]}
            )

        b_kept = outside(runs_of(b), spans_before)
        a_kept = outside(runs_of(a), spans_after)
        if b_kept != a_kept:
            run_failures.append(
                {
                    "paragraph": index,
                    "kind": "runs outside the target changed",
                    "before": b_kept,
                    "after": a_kept,
                }
            )

        for (span_start, span_end), (produced_start, produced_end) in zip(
            spans_before, spans_after
        ):
            if span_start == span_end or produced_start == produced_end:
                # A pure insertion addresses no character on one side, so "the
                # run that held the target's start" does not exist and this check
                # does not apply. Saying so beats a check that cannot fail.
                not_applicable += 1
                continue
            expected, expected_readable = rpr_at(b, span_start)
            actual, actual_readable = rpr_at(a, produced_start)
            if expected != actual:
                first_run_failures.append(
                    {
                        "paragraph": index,
                        "target": before_text[span_start:span_end],
                        "expected_rPr": expected_readable,
                        "actual_rPr": actual_readable,
                    }
                )

    report["checks"]["non_target_runs_unchanged"] = not run_failures
    if run_failures:
        report["run_changes"] = run_failures[:10]
        failures.append(f"{len(run_failures)} paragraph(s) changed runs outside the target")
    report["checks"]["first_run_wins"] = not first_run_failures
    if first_run_failures:
        report["first_run_wins_failures"] = first_run_failures[:10]
        failures.append(
            f"{len(first_run_failures)} replacement(s) did not take the first covered run's rPr"
        )
    if not_applicable:
        report["first_run_wins_not_applicable"] = not_applicable

    before_structure = structure_counts(before_root)
    after_structure = structure_counts(after_root)
    changed_structure = {
        name: (before_structure.get(name, 0), after_structure.get(name, 0))
        for name in set(before_structure) | set(after_structure)
        if before_structure.get(name, 0) != after_structure.get(name, 0)
    }
    report["checks"]["protected_structure_unchanged"] = not changed_structure
    report["structure"] = {
        "before": before_structure,
        "after": after_structure,
        "changed": changed_structure,
    }
    if changed_structure:
        failures.append(f"protected structure changed: {changed_structure}")

    report["failures"] = failures
    report["ok"] = not failures
    return report


def main() -> int:
    argv = sys.argv[1:]
    as_json = "--json" in argv
    argv = [a for a in argv if a != "--json"]
    replaced: list[tuple[str, str]] = []
    paths: list[str] = []
    index = 0
    while index < len(argv):
        if argv[index] == "--replaced":
            index += 1
            if index >= len(argv) or "=" not in argv[index]:
                print("--replaced takes OLD=NEW", file=sys.stderr)
                return 2
            old, new = argv[index].split("=", 1)
            replaced.append((old, new))
        else:
            paths.append(argv[index])
        index += 1
    if len(paths) != 2:
        print(__doc__)
        return 2

    report = check(paths[0], paths[1], replaced or None)
    if as_json:
        print(json.dumps(report, ensure_ascii=False, indent=1))
        return 0 if report["ok"] else 1

    print(f"{paths[0]}  ->  {paths[1]}")
    print(f"  target: {report['target']}")
    print(f"  paragraphs: {report['paragraphs']['before']}   edited: {len(report['edited'])}")
    for name, ok in report["checks"].items():
        print(f"  [{'PASS' if ok else 'FAIL'}] {name}")
    for failure in report["failures"]:
        print(f"  ! {failure}")
    for item in report.get("first_run_wins_failures", [])[:5]:
        print(
            f"    P{item['paragraph']} {item['target']!r}: expected {item['expected_rPr']!r}, "
            f"got {item['actual_rPr']!r}"
        )
    for item in report.get("run_changes", [])[:3]:
        print(f"    P{item['paragraph']}: {item['kind']}")
    return 0 if report["ok"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
