# Reference divergences

Places where the Rust engine deliberately does **not** reproduce the Python
reference. Each entry names the evidence, the rule the reference states, the
rule it actually applies, and why we keep ours.

A parity gate that demands byte equality would fail on every one of these. That
is the point: they are recorded so the gate can whitelist them explicitly
instead of being loosened until it stops meaning anything.

---

## 1. `factor_vertical_style` compares two different canonicalizations

**Symptom.** On a real patent (13 advisor comments, 641 tracked insertions,
982 revisions), the engines disagree about vertical alignment:

| | Rust | Python |
|---|---|---|
| `vertical_promotion.promoted` | **6** | 4 |
| `styles.json` | 29 styles | 30 (`s_f8d4ccf853cae72d` extra) |
| `format.json` paragraphs differing | — | 1 (`P14`) |

The 4 superscripts agree exactly. The disagreement is 2 subscripts inside a
revision: Rust promotes them to the paragraph's base style, Python keeps them
as spans and registers a subscript style variant.

**The rule the reference states** (`typed_core.py:522`):

> Eligible iff removing **exactly one** `w:vertAlign` from the region's run
> properties yields run properties canonically equal to the paragraph's base
> style:
>
>     canonical(region.rPr - exactly_one(w:vertAlign)) == canonical(base.rPr)
>
> Canonical identity, not a feature subset.

**The rule it applies.** The two sides of that comparison go through different
canonicalizers:

```python
def _canonical_element(element, *, rpr=False):
    ...
    if rpr:
        children.sort(key=repr)      # sorts ONLY when rpr=True

def canonical_rpr(rpr_xml):          # produces base.canonical
    return canonical_xml(etree_xml(root), rpr=True)          # SORTED

def factor_vertical_style(registry, style_id, base_style_id):
    remainder = canonical_xml(etree_xml(root))               # NOT sorted
    if remainder != base.canonical:
        return None
```

`remainder` keeps the children in document order; `base.canonical` is sorted by
`repr`. They are equal only when the region's children already happen to be in
sorted order — which for a real `rPr` (`rFonts, bCs, sz, szCs`) they are not.

**Consequence.** The reference's promotion is effectively a no-op for most
regions. Verified directly on this document: removing the `vertAlign` from
`s_e3cca057107f0664` yields

```
["w:rPr",[],"",[["w:rFonts",…],["w:bCs",…],["w:sz",…],["w:szCs",…]]]   remainder
["w:rPr",[],"",[["w:bCs",…],["w:rFonts",…],["w:sz",…],["w:szCs",…]]]   base
```

Same elements, different order, `!=`.

**Our position.** We apply the stated rule with one canonicalizer on both
sides, so we promote where the reference refuses. We do not reproduce the
defect: matching it would mean writing a document whose alignment is spelled
two ways, and would make `^{…}`/`_{…}` depend on the order Word happened to
write the children in.

**If the reference is fixed**, the expected outcome is that Python promotes
those 2 subscripts, `promoted` becomes 6, and `s_f8d4ccf853cae72d` stops being
registered. That is the change to watch for.

---

## 2. `typed.md` omits the `comments` part

Python's `typed.md` carries the comment bodies as paragraphs (`comments.P0` …
`comments.P19` on the patent above); Rust's does not.

Rust reports the omission instead of hiding it: `facade_verify` returns
`comments` in `coverage_evidence.unscanned_parts`, and `facade_locate` carries
the same note. The reference has no equivalent coverage report, so its
consumers cannot tell a `0` count from "not looked at".

Not yet decided which way this should go. Reading the advisor's comment text
from the canonical projection is genuinely useful for a revision workflow, and
the current split — readable via `list_comments`/`get_comment`, absent from
`typed.md` — means a consumer that only reads `typed.md` cannot see them.

---

## 3. A delete revision loses its attributes and text in `typed.md`

Python renders `<docx-revision id="N77" kind="delete" w:author=… w:id=…>钠</docx-revision>`.
Rust renders `<docx-revision id="N77" kind="delete"></docx-revision>`.

Root cause is architectural, not a bug in either renderer: Python projects
`edit.md` and `typed.md` independently from the node tree, while Rust derives
`typed.md` by re-parsing `edit.md`. The draft spells a deletion as a point
token (`⟦revision-gap id="N77" kind="delete"⟧`) that carries neither the
attributes nor the deleted text, so the information is not there to recover.

`format.json` is unaffected — the token model matches the reference exactly
(1113 tokens, 0 differing) — so nothing downstream that reads the model loses
anything. Only the text projection does.

Closing this means rendering `typed.md` from the node tree, which is the
projection-closure work, not a patch to the renderer.
