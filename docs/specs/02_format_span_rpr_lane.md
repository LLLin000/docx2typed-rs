# 02 — `format_span` and the run-property lane

Settles the four questions in #33 before any code, in the same shape as
`01_pr3_structural_closure.md`. The primitive already exists (#28); what is
missing is a way for the build to **apply** it.

## BLOCKED: the style registry this lane depends on does not exist

Added after starting the implementation. This section corrects the premise the
rest of the document was written on, so nobody builds from a plan that cannot
work.

`restyle_run_properties` refuses `format-style-unavailable` unless the derived
`style_id` is already in `styles.json`. **Rust's extract writes `styles.json`
with an empty `styles` object**, hardcoded:

```rust
// crates/docx2typed-core/src/lib.rs:220
let styles_bytes = serde_json::to_vec(&serde_json::json!({
    "schema": STYLES_SCHEMA,
    "canonicalizer_version": CANONICALIZER_VERSION,
    "styles": {},
}))
```

Measured, same document (`corpus/release/styled.docx`):

| engine | `styles.json` entries |
|---|---|
| Python | **3** (`s_9f5c04f8681ebd4e` normal, `s_22a1dd1510eee8bd` 宋体, `s_adcd924781ac8b4e` Times New Roman) |
| Rust | **0** |

So `format_span` as specified here can never succeed on a Rust-extracted
workdir: every restyle would fail `format-style-unavailable`, including one
reusing a style that plainly exists in the document. The lane would be inert
exactly as `history_*` was before #37 — a mechanism with nothing feeding it.

### And the two engines derive style ids differently

`content_style_id` is `s_` + `sha256(canonical_bytes)[:16]`, and Rust's
`canonical_rpr_bytes` emits **raw XML bytes**. Python's canonical is a
**JSON-list form**, and its id hashes that string. Verified against the three
ids above:

```
Python: sha256('["w:rPr",[],"",[]]')[:16] = 9f5c04f8681ebd4e   == its own id  ✓
Rust:   sha256(b"")[:16]                  = e3b0c44298fc1c14   != that id     ✗
```

Each scheme is self-consistent, so a registry populated with Rust's ids would
work *within* Rust. It would not agree with Python's ids for the same run
properties — and `styles.json` is in `CANONICAL_ASSETS`, so that difference
also moves the tree digest and therefore the version chain.

That is a decision, not a mechanical fix, which is why this is filed rather
than guessed: adopt Python's JSON-list canonical so ids and digests agree
across engines, or keep Rust's and record the divergence as a known
non-equivalence.

### What is already built and tested

`prose.rs` now carries the mechanism this document specifies, with five tests
passing (restyle in place, insert onto a run with no `w:rPr`, one span across
several runs, a stale paragraph reported not dropped, sidecar round-trip with
empty-removes-file), and `ProseLeaf` gained the `rpr_start`/`rpr_end`/
`run_open_end` ranges the in-place rewrite needs. The negative control fires:
removing the insertion branch fails two of the tests.

It is **not** committed on its own, because with no registry it has no caller —
a mechanism nothing invokes is a stub, and this repository has enough of those
recorded as open issues.

## The gap, precisely

Every build-time edit Rust can express today is one of:

| mechanism | lane | what it changes |
|---|---|---|
| `islands.json` | draft | **text bytes** inside an existing leaf |
| `structure.json` | draft | whole paragraphs, synthesised from an inherit anchor or removed |

A format change is neither: it rewrites an existing run's `<w:rPr>`. Verified
in `crates/docx2typed-core/src/prose.rs`: `apply_edits` reads `w:rPr` only to
clone the owning run's properties into replacement runs, and `IslandEdit`
addresses `text_start..text_end`, which excludes the run's `w:rPr` — the
properties sit before the text.

`grep -rn "rpr_edit\|RprEdit" crates/*/src/*.rs` → nothing. The lane is absent,
not incomplete.

### What the primitive gives us (all present)

| symbol | line | role |
|---|---|---|
| `rpr_with_overrides(base, attributes)` | `prose.rs:2748` | apply a partial override map to a base `w:rPr`; `None` when nothing changes |
| `insert_rpr_child(rpr, child)` | `prose.rs:2682` | order-correct child insertion (`RPR_ORDER`) |
| `restyle_run_properties(base, attributes, available)` | `prose.rs:2852` | → `RunRestyle { rpr, canonical_rpr, style_id }` |
| `RunRestyle::register(&mut styles)` | `prose.rs:2440` | idempotent, content-addressed |

### The hard constraint the primitive already enforces

`restyle_run_properties` returns `format-style-unavailable` unless the derived
`style_id` is **already in `styles.json`**:

> *styles.json must mirror the source document... Reuse an existing variant by
> passing `style_id=...`, or apply the formatting once in Word and re-extract*

That is a deliberate refusal to invent style variables, and it decides more of
this design than any of the four questions: `format_span` **switches between
run properties the document already carries**; it does not synthesise new ones.
`{"vertAlign": "superscript"}` on a document that has never held a superscript
fails with a diagnostic that says so — not by writing a new style into
`styles.json`.

## The four decisions

### 1. Addressing — span-anchored, resolved to runs at build time

**Decision.** The record addresses `{paragraph_id, text_start, text_end}`, the
same anchor `IslandEdit` uses. **Not** a run index.

**Why.** Python's contract is `old` (text match, exactly like `document_patch`)
XOR `span_index` (a style-region index from `document_read view=spans`); both
name *text*. A run index would be an index into a structure the user never
sees, and it goes stale the moment any text edit changes run boundaries —
precisely the failure class #26 named. Text anchors survive edits to other
parts of the paragraph; run indices do not.

**Consequence.** One span may intersect **several** runs. Each intersecting run
derives its own style from **its own** base `w:rPr`, because the same
`attributes` on two differently-formatted runs produce two different `w:rPr`.
So the record is one span → N derived styles, and the build reports the count
it restyled rather than pretending the span was one run.

**Consequence.** Ambiguity is refused, not guessed: an `old` that occurs more
than once in the leaf fails exactly as the island lane does
(`prose-edit-ambiguous`, `prose.rs:349`). Reusing that rule is the point —
the same selector must not mean two different things in two lanes.

### 2. Interaction with a text edit on the same run — refuse, require a clean draft

**Decision.** `format_span` requires a **clean draft** and fails
`format-requires-clean-draft` otherwise, matching Python
(`scripts/mcp_server.py:3933-3939`).

**Why.** The two lanes write different things (bytes vs properties) to the same
object, and the order would be observable: an island edit clones the run's
*current* rPr into its replacement, so formatting-then-editing and
editing-then-formatting give different results. Both orders are defensible,
which is exactly why the choice must not be implicit. Refusing removes the
question instead of answering it with a precedence rule nobody can predict.

**Why not compose them.** It costs one `commit_sync` in the user's flow — the
cost Python users already pay — and it buys a lane whose result is provable
from the record alone. `structure.json` already works this way: it rewrites
the committed tree, not the draft.

### 3. Interaction with the structure path — make the collision unrepresentable

**Decision.** The addressed `paragraph_id` must exist in the **committed**
tree. A synthesised paragraph has no committed identity, so an rPr edit cannot
address one: it fails `paragraph-not-found` like any other unknown id.

**Why.** A synthesised paragraph's runs take their properties from the inherit
anchor, so "restyle a run of a paragraph that does not exist yet" has two
answers (restyle the anchor, or restyle the product) and no way to pick. Making
it unaddressable is cheaper than making it well-defined.

**The residual risk, stated rather than hidden.** A structure edit applied
*after* an rPr edit can remove a paragraph the rPr edit addressed. The record
then points at nothing. The build must treat that as **stale, not as an
error to swallow and not as content to resurrect**:

- If the build's paragraph set no longer contains the record's `paragraph_id`,
  the rPr edit is **dropped and the drop is reported** in the build diagnostics
  (`rpr-edit-stale`), never silently ignored.
- This is #26's class, so it gets #26's treatment: silence is the defect.

### 4. Tracked mode — out of scope for the first landing

**Decision.** The direct (in-place) restyle ships first. Whether tracked mode
emits a delete+insert pair (Python's choice) is **not** decided here, and
`track: true` fails `format-tracked-unsupported` rather than silently
restyling in place.

**Why.** Python emits a delete+insert pair because Word's own `rPrChange` is
not synthesised; that is a *workaround*, and copying a workaround before the
direct path is proven adds a second mechanism to debug at once. Rust's tracked
island lane already exists (`prose.rs:55-61`), so the follow-up is cheap when
wanted. Refusing loudly is the honest default: a `track: true` call that
silently restyled in place would produce a document a reviewer cannot see
through.

## Spec

**Record** — a new sidecar `rpr_edits.json` (name mirrors `islands.json` /
`structure.json`), written by the draft lane:

```json
{
  "schema": "docx2typed-rpr-edits-1",
  "edits": [
    {
      "paragraph_id": "P12",
      "text_start": 41,
      "text_end": 45,
      "attributes": { "vertAlign": "superscript" },
      "style_id": "s_9f2c1a0b7e4d6538"
    }
  ]
}
```

`attributes` is retained for the audit trail and for `diff_preview`;
`style_id` is what the build applies. An `attributes` entry with no resolvable
`style_id` is a bug, not a fallback.

**Build** — after `structure.json` establishes the paragraph set, before the
`w:rPr` of each addressed run is emitted: resolve `paragraph_id` → the runs
intersecting `text_start..text_end` → for each, `restyle_run_properties(base_rpr_of_that_run, attributes, available_styles)` → write the derived `w:rPr` in place. Empty record ⇒ sidecar removed, matching
`an_empty_structure_sidecar_is_removed` (`prose.rs:2920`).

**Tool surface** — mirrors Python, minus `span_index` until a caller needs it:

```
format_span(paragraph_id, old, attributes|style_id, match_ref?, operation_id?)
```

| condition | code |
|---|---|
| neither/both of `attributes`/`style_id` | `format-invalid-attribute` (Python parity) |
| `old` absent or ambiguous | `format-invalid-argument` / `prose-edit-ambiguous` |
| draft not clean | `format-requires-clean-draft` |
| derived style not in `styles.json` | `format-style-unavailable` (already emitted by the primitive) |
| `track: true` | `format-tracked-unsupported` |
| no such paragraph | `paragraph-not-found` |

`operation_id` is optional and replays identically, like every other mutation.

## Verification this design must clear

1. A superscript applied to `Cu2+` survives `build_docx` and re-reads as
   superscript through `document_read view=spans`.
2. Two runs under one span each get **their own** derived style (the N-run
   case in decision 1), not one style shared.
3. `format_span` on a dirty draft fails `format-requires-clean-draft`.
4. `format_span` on a paragraph `structure.json` synthesises fails
   `paragraph-not-found`.
5. An rPr edit whose paragraph a later structure edit removes is reported
   `rpr-edit-stale` — and the paragraph does **not** come back.
6. `track: true` fails `format-tracked-unsupported` and leaves the document
   untouched.
7. The derived style is registered content-addressably, so two identical
   restyles produce one entry (`RunRestyle::register` already guarantees this;
   the test proves the build path uses it).

## What this does not decide

- **The style-id scheme** (see the blocked section above). It gates everything
  else here: the registry cannot be populated until the canonical form is
  chosen, and the choice moves `styles.json`'s bytes and so the tree digest.

- `span_index` addressing (Python has it; no Rust caller needs it yet).
- `rPrChange` synthesis (decision 4).
- Formatting **paragraph** properties or table cells — this is the character
  lane only.
