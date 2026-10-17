# 0051 — The editable view contract

Status: accepted (2026-10-17). Companion to [0050](0050-format-engine-boundary.md);
does not reopen [0049](0049-core-edit-protocol-v1-freeze.md)'s edit semantics.

## The problem this exists to end

One document is read by three parts of the system, and each was answering the same
question — *what does this paragraph look like on the editable surface?* — with its
own rule:

| who | what it produces |
|---|---|
| the projection writer | `typed.md`, which the caller reads and edits |
| the draft parser | the paragraphs the commit compares and diffs |
| the baseline render | the template's side of that comparison |

Every defect of the last rounds is one of these three disagreeing with another.
Measured, on real patents, with the two strings that differed:

| divergence | left | right | symptom |
|---|---|---|---|
| `moveFrom` children | `[opaque:opaque][opaque:unsupported-run]` | `[opaque:opaque]` | `save` refused an untouched document (`edits-not-implemented`) |
| whitespace-only body line | `"    "` | `""` | the same refusal, one layer down |
| draft vs projection, after an edit | edited text | parsed text | `projection-mismatch`, newly on five documents |
| the structure sidecar | no user edit | `4 new + 4 deleted` | `table-structure-immutable` on a plain text replacement |

Fixing one pair moved the disagreement to the next pair — because the rule was
never stated once. This ADR states it.

## What must be equal, and what must not

Not every pair of representations is byte-equal, and pretending otherwise is its
own bug:

| comparison | requirement |
|---|---|
| source XML vs `typed.md` | NOT equal — different formats, different purposes |
| baseline `B` vs current draft `D`, no user edit | **equal**, under this contract's rule |
| `typed.md` vs the draft parsed from it | **equivalent** — the round trip is the test |
| `B` vs `D` after an edit | different, and the difference is exactly the user's edit |
| the view re-read after `save` vs `D` | **equal** |
| protected source structure vs the saved output | unchanged |

## The types, and the rule for each

For each type: how it enters the editable view, and whether it may be edited.

| type | in the editable view | editable |
|---|---|---|
| `w:ins` (insertion) | its text, inline, as a revision with author/date/id | yes |
| `w:del` (deletion) | **hidden** — a deletion is not content the caller sees | no (the text is not there to edit) |
| `w:delText` inside a `w:del` | hidden, same rule | no |
| `w:moveFrom` (a move's source) | **its children are shown**, as an addressable `move_from` revision | its text: yes; the structure: no |
| `w:moveTo` (a move's destination) | its text, inline | yes |
| opaque node (drawing, object, unknown element) | a placeholder **that carries its identity** — see below | no |
| whitespace-only paragraph body | the whitespace, exactly as stored | yes |
| inline `tab` / `br` / `cr` | a token, in place | the token is not editable; text around it is |
| paragraph-mark revision (`w:pPr/w:rPr/w:del`) | the paragraph exists; the mark's state is not the caller's business | the paragraph's text: yes |
| table-cell paragraph | its own identity (`T{n}.R{n}.C{n}.P{n}`), its text | yes, if the container allows it |
| a paragraph a round trip cannot preserve | stable identity, and edits that need the lost structure are **refused** | text: only when safe |

### Opaque nodes carry identity

A placeholder string is not an identity. Two different drawings both render as
`[opaque:…]`, so string equality cannot prove that the caller's edit addresses the
node it thinks it does. An opaque node in the editable view therefore carries:

- a stable id (the projection already mints `N{index}`);
- its source location: part, paragraph, node path;
- what it is (`kind`, `tag`), and whether it is *inside* an editable span.

An edit whose span crosses or depends on an opaque node is refused with the node
named — which is what `document_find` must report as its verdict, instead of
`editable: true` followed by a refusal at edit time (the 01/08 case).

### What "unsafe" means

A structure a round trip cannot preserve is not repaired by inventing or dropping
paragraphs. It keeps its identity and the edits that would need the lost structure
are refused. Refusing is the contract; `4 new + 4 deleted` for a document nobody
edited is not a workaround, it is the bug.

## The rule, once

1. **One renderer.** The projection writer, the draft parser, and the baseline
   render answer this contract, not three opinions. They need not share a function
   — writing and parsing are opposite directions — but they share this document,
   and a round-trip test binds them: for every fixture and every real document in
   the corpus, `parse(write(x)) == x` under the rule above.
2. **`Diff(B, D)` only.** The baseline `B` is the projection of the template, frozen
   at extract under this rule. The diff is taken between two views of the same
   kind, and a document nobody edited has an empty diff — that is the invariant the
   red tests assert.
3. **`typed.md` is the contract's artifact**, not an approximation of it: what the
   caller reads is what the parser sees, character for character.
4. **Protected structure is not negotiable.** Whatever the rule says about a node,
   the source's revisions, tables and locked interiors survive the build unchanged
   — verified against the source, not against the draft.

## Acceptance

- 02 and 13: `extract → open → save → reopen` with no edit succeeds, and the
  output's `word/document.xml` is byte-identical to the source;
- 09–12 and any other document the divergence broke: editing works again;
- 01 and 08: `document_find` reports the opaque blocker accurately — no
  `editable: true` that the edit path then refuses;
- the 14 real documents: the full task (find → edit → save → reopen) succeeds, or
  the target is refused before the edit with a reason that names it;
- the corpus fixtures and the round-trip test stay green throughout.
