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

import io
import json
import pathlib
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


def rpr_signature(rpr_xml: str) -> tuple:
    """The PROPERTIES of an rPr, namespace declarations aside.

    Comparing canonical XML would compare the document's own namespace
    declarations with the registry's: two spellings of the same formatting, and a
    mismatch that has nothing to do with formatting. What matters is the children
    — each property and the attributes that give it a value.
    """
    from lxml import etree

    if not rpr_xml.strip():
        return ()
    root = parse(rpr_xml)
    out = []
    for child in root:
        name = local(child.tag)
        attributes = tuple(sorted((local(key), value) for key, value in child.attrib.items()))
        out.append((name, attributes))
    return tuple(sorted(out))


def character_properties(package: bytes, paragraph_index: int) -> list[tuple[str, str]]:
    """`(character, canonical rPr)` for every character of a paragraph.

    This is the ownership model the format checks need: a character-level edit may
    split a run, so "the run is unchanged" is the wrong question and "the
    character kept its properties" is the right one. Runs without text (a tab, a
    break) contribute no characters.
    """
    body = paragraph_body(package, paragraph_index)
    out: list[tuple[str, str]] = []
    for run in run_slices(package, paragraph_index):
        rpr = run["rpr"]
        signature = (
            rpr_signature(etree_tostring(rpr)) if rpr is not None else (("(none)", ()),)
        )
        out.extend((character, signature) for character in run["text"])
    return out


def member_bytes(package: bytes, member: str) -> bytes:
    with zipfile.ZipFile(io.BytesIO(package)) as archive:
        return archive.read(member)


def tag_positions(part: bytes) -> list[tuple[str, int, int, bool, bool]]:
    """`(name, start, end, closing, self_closing)` for every tag, in order.

    A byte scanner, because the checks need offsets: an lxml element does not
    carry where it was, and re-serialising one to find it does not survive
    namespace declarations.
    """
    out = []
    index = 0
    while index < len(part):
        if part[index] != ord("<"):
            index += 1
            continue
        if part[index : index + 4] == b"<!--":
            end = part.find(b"-->", index)
            index = len(part) if end == -1 else end + 3
            continue
        end = part.find(b">", index)
        if end == -1:
            break
        inner = part[index + 1 : end]
        closing = inner[:1] == b"/"
        body = inner[1:] if closing else inner
        self_closing = body[-1:] == b"/"
        name_end = 0
        while name_end < len(body) and not (
            body[name_end : name_end + 1].isspace() or body[name_end : name_end + 1] == b"/"
        ):
            name_end += 1
        name = body[:name_end].decode("utf-8", "replace").rsplit(":", 1)[-1]
        if name:
            out.append((name, index, end + 1, closing, self_closing))
        index = end + 1
    return out


def paragraph_element_ranges(part: bytes) -> list[tuple[int, int]]:
    """Byte ranges of the `w:p` elements, in document order."""
    ranges = []
    depth = 0
    start = 0
    for name, tag_start, tag_end, closing, self_closing in tag_positions(part):
        if name != "p":
            continue
        if not closing and not self_closing:
            if depth == 0:
                start = tag_start
            depth += 1
        elif closing:
            depth = max(0, depth - 1)
            if depth == 0:
                ranges.append((start, tag_end))
    return ranges


def etree_tostring(element) -> str:
    from lxml import etree

    return etree.tostring(element, encoding="unicode")


def run_slices(package: bytes, paragraph_index: int) -> list[dict]:
    """Every `w:r` of a paragraph: its `w:rPr` element, its text, its bytes.

    Parsed from the WHOLE document part, so the namespace declarations a run's
    canonical form carries are the document's own — a wrapper's would make every
    comparison against the registry fail for a reason that has nothing to do with
    formatting.
    """
    from lxml import etree

    part = member_bytes(package, "word/document.xml")
    root = parse(part)
    paragraph = list(root.iter(f"{{{W}}}p"))
    if paragraph_index >= len(paragraph):
        return []
    runs = []
    for run in paragraph[paragraph_index].iter(f"{{{W}}}r"):
        rpr = run.find(f"{{{W}}}rPr")
        text = "".join(t.text or "" for t in run.iter(f"{{{W}}}t"))
        runs.append(
            {
                "rpr": rpr,
                "text": text,
                "bytes": etree.tostring(run, method="c14n"),
            }
        )
    return runs


def structure_counts(root) -> dict[str, int]:
    counts: dict[str, int] = {}
    for element in root.iter():
        name = local(element.tag)
        if name in PROTECTED:
            counts[name] = counts.get(name, 0) + 1
    return counts


def check(
    source: str,
    produced: str,
    replaced: list[tuple[str, str]] | None = None,
    styled: list[tuple[str, int, int, str]] | None = None,
    styles_path: str | None = None,
) -> dict:
    with zipfile.ZipFile(source) as z:
        before_root = parse(z.read("word/document.xml"))
    with zipfile.ZipFile(produced) as z:
        after_root = parse(z.read("word/document.xml"))

    before_paras = paragraphs(before_root)
    after_paras = paragraphs(after_root)
    report: dict = {
        "paragraphs": {"before": len(before_paras), "after": len(after_paras)},
        "target": (
            "independent format targets (--styled)"
            if styled
            else "replaced spans (--replaced)"
            if replaced
            else "text diff (no --replaced)"
        ),
        "checks": {},
        "edited": [],
    }
    failures: list[str] = []

    if styled:
        styled_report = check_styled(source, produced, styled, styles_path)
        report["styled"] = styled_report["detail"]
        report["checks"].update(styled_report["checks"])
        failures.extend(styled_report["failures"])
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

    # A format edit has no text target, so the text-diff model does not apply: the
    # per-character checks above already decided what may change, and the
    # paragraph/structure checks below still run.
    for index, (b, a) in enumerate([] if styled else zip(before_paras, after_paras)):
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


def check_styled(
    source: str,
    produced: str,
    styled: list[tuple[str, int, int, str]],
    styles_path: str | None,
) -> dict:
    """The independent format check: targets from the caller's request, not from
    the plan.

    Per character, because a character-level edit may split runs:

    1. every target character carries exactly the properties of the style named;
    2. every character outside the targets keeps the properties it had;
    3. the visible text is unchanged;
    4. paragraph properties and protected structure are unchanged;
    5. a run that took part in no split is byte-identical.
    """
    before_package = pathlib.Path(source).read_bytes()
    after_package = pathlib.Path(produced).read_bytes()
    registry: dict[str, str] = {}
    if styles_path:
        value = json.loads(pathlib.Path(styles_path).read_text(encoding="utf-8"))
        for style_id, entry in value.get("styles", {}).items():
            registry[style_id] = entry.get("rPr", "")

    failures: list[str] = []
    checks: dict[str, bool] = {}
    detail: list[dict] = []

    # Paragraph indices, in document order, from the source's own paragraph ids.
    ids = paragraph_ids(before_package)
    index_of = {paragraph_id: index for index, paragraph_id in enumerate(ids)}

    targets_by_paragraph: dict[str, list[tuple[int, int, str]]] = {}
    for paragraph_id, start, end, style_id in styled:
        targets_by_paragraph.setdefault(paragraph_id, []).append((start, end, style_id))

    target_ok = True
    untargeted_ok = True
    text_ok = True
    for paragraph_id, targets in targets_by_paragraph.items():
        index = index_of.get(paragraph_id)
        if index is None:
            failures.append(f"{paragraph_id} is not in the source document")
            target_ok = False
            continue
        before = character_properties(before_package, index)
        after = character_properties(after_package, index)
        before_text = "".join(character for character, _ in before)
        after_text = "".join(character for character, _ in after)
        if before_text != after_text:
            text_ok = False
            failures.append(
                f"{paragraph_id}: a format edit changed the text "
                f"({before_text!r} -> {after_text!r})"
            )
            continue
        target_ranges = [(start, end) for start, end, _ in targets]
        for start, end, style_id in targets:
            expected = registry.get(style_id, "")
            if not expected:
                failures.append(f"{paragraph_id}: no properties for style {style_id}")
                target_ok = False
                continue
            expected_signature = rpr_signature(expected)
            for offset in range(start, min(end, len(after))):
                character, rpr = after[offset]
                conforms = bool(expected) and rpr == expected_signature
                detail.append(
                    {
                        "paragraph": paragraph_id,
                        "offset": offset,
                        "character": character,
                        "before": before[offset][1],
                        "after": rpr,
                        "target": True,
                        "conforms": conforms,
                    }
                )
                if not conforms:
                    target_ok = False
                    failures.append(
                        f"{paragraph_id}[{offset}] {character!r}: {rpr!r} is not the style's "
                        f"{style_id}"
                    )
        for offset, (character, rpr) in enumerate(after):
            if any(start <= offset < end for start, end in target_ranges):
                continue
            if before[offset][1] != rpr:
                untargeted_ok = False
                failures.append(
                    f"{paragraph_id}[{offset}] {character!r}: properties changed outside the "
                    f"target ({before[offset][1]!r} -> {rpr!r})"
                )

    checks["target_characters_carry_the_named_style"] = target_ok
    checks["characters_outside_the_target_unchanged"] = untargeted_ok
    checks["visible_text_unchanged"] = text_ok

    # 5. Byte-level: a run that took part in no split is unchanged.
    unsplit_ok = True
    for paragraph_id, targets in targets_by_paragraph.items():
        index = index_of.get(paragraph_id)
        if index is None:
            continue
        before_runs = run_slices(before_package, index)
        after_runs = run_slices(after_package, index)
        before_pairs = [(run["text"], run["bytes"]) for run in before_runs if run["text"]]
        after_pairs = [(run["text"], run["bytes"]) for run in after_runs if run["text"]]
        # A run whose text is untouched AND whose text appears exactly once on
        # both sides did not take part in a split: its bytes must match.
        before_by_text: dict[str, list[bytes]] = {}
        for text, raw in before_pairs:
            before_by_text.setdefault(text, []).append(raw)
        for text, raw in after_pairs:
            candidates = before_by_text.get(text, [])
            if len(candidates) == 1 and sum(1 for t, _ in after_pairs if t == text) == 1:
                if candidates[0] != raw:
                    unsplit_ok = False
                    failures.append(
                        f"{paragraph_id}: the run {text!r} took part in no split but its bytes "
                        "changed"
                    )
    checks["runs_outside_any_split_byte_identical"] = unsplit_ok

    return {"checks": checks, "failures": failures, "detail": detail}


def paragraph_ids(package: bytes) -> list[str]:
    """The paragraph ids the engine would mint, in document order: `P{n}` for the
    document part. Read here from the document's own structure, not from the
    engine."""
    part = member_bytes(package, "word/document.xml")
    from lxml import etree

    root = parse(part)
    return [f"P{index}" for index, _ in enumerate(root.iter(f"{{{W}}}p"))]


def paragraph_body(package: bytes, index: int) -> bytes:
    part = member_bytes(package, "word/document.xml")
    ranges = paragraph_element_ranges(part)
    if index >= len(ranges):
        return b""
    start, end = ranges[index]
    return part[start:end]


def main() -> int:
    argv = sys.argv[1:]
    as_json = "--json" in argv
    argv = [a for a in argv if a != "--json"]
    replaced: list[tuple[str, str]] = []
    styled: list[tuple[str, int, int, str]] = []
    styles_path: str | None = None
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
        elif argv[index] == "--styled":
            index += 1
            if index >= len(argv) or "=" not in argv[index] or ":" not in argv[index]:
                print("--styled takes PARAGRAPH:START-END=STYLE_ID", file=sys.stderr)
                return 2
            where, style_id = argv[index].split("=", 1)
            paragraph_id, span = where.split(":", 1)
            start, end = span.split("-", 1)
            styled.append((paragraph_id, int(start), int(end), style_id))
        elif argv[index] == "--styles":
            index += 1
            styles_path = argv[index] if index < len(argv) else None
        else:
            paths.append(argv[index])
        index += 1
    if len(paths) != 2:
        print(__doc__)
        return 2

    report = check(paths[0], paths[1], replaced or None, styled or None, styles_path)
    if as_json:
        print(json.dumps(report, ensure_ascii=False, indent=1))
        return 0 if report["ok"] else 1

    print(f"{paths[0]}  ->  {paths[1]}")
    print(f"  target: {report['target']}")
    for row in report.get("styled", [])[:12]:
        before = ",".join(name for name, _ in row["before"] if name != "(none)") or "none"
        after = ",".join(name for name, _ in row["after"] if name != "(none)") or "none"
        print(
            f"    {row['paragraph']}[{row['offset']}] {row['character']!r}: "
            f"{before} -> {after} {'ok' if row['conforms'] else 'NOT the requested style'}"
        )
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
