# A1: projection closure

Status: **complete**. `typed.md` is rendered from the current node tree; the
route that derived it by re-parsing the draft dialect is out of the commit path.

## The architecture

```
Canonical Node Tree + verified patches
        |
        v
current logical state          apply_build(template, islands, structure, rpr, styles)
        |
        v
current node tree              paragraph_nodes(package)
        |
        v
typed.md                       typed_paragraph_block  (shared with extract)
```

`commit_sync`'s order, every step load-bearing:

```
recording -> saves -> render -> write -> rebind
```

`saves` must precede `render`: `apply_build` folds those records into the
template, so rendering first builds from the PREVIOUS sidecar and the splicer
gives one paragraph another's body. `rebind` must follow `write`: it re-pins
`edit.state.json` and canonicalizes the draft, or the workdir stays dirty and
the build refuses with `edit-conflict`.

## Why the old route could not be repaired

The draft dialect spells a revision as a token with nowhere to put the text, the
author or the date. Measured on a real patent (13 comments, 641 tracked
insertions), one tracked edit to P41:

| | nodes in the projection of P41 |
|---|---|
| old route | `[comment-start, comment-end, commentReference]` — 3 |
| new route | `[comment-start, delete, insert, comment-end, commentReference]` — 5 |

The old projection of an edited paragraph carried **no revision at all**. Not a
lossy one, not a smaller one: absent. No converter work could recover what the
intermediate never held, which is why the fix is a different source, not a
better converter.

## Fidelity, measured

Frozen identity baseline: 1113 tokens, 99 paragraphs, 985 revisions, 99 blocks.

| gate | result |
|---|---|
| no-op commit | `no drift: every identity holds its value` |
| one tracked edit | drift is exactly `typed.P41` — 0 tokens, 0 paragraphs, 0 revisions |
| build / verify | `success` / `success` |
| shape | spans 400, revisions 984, anchors 28, inlines 100 |
| comments | 13 / 13 / 13, no orphans |
| the edit | delete `背景技术` + insert `技术背景`, author AND date |
| built DOCX | 642 `<w:ins>`, 345 `<w:del>`, the advisor's marks preserved |
| projection gaps | NONE — the tree covered every paragraph |

Against the Python reference: tokens 1113 = 1113 with **0 differing**, and the
revision inventory agrees on **985/985** `revision_key`s with no shared field
disagreeing. The one remaining divergence is a documented reference defect
(`06_reference_divergences.md`), deliberately not reproduced.

## The refusal path

The lossy converter survives only where the tree cannot address a paragraph, and
every such branch reports `DOCX2TYPED_REPORT_PROJECTION_GAP` first. A capability
gap is reported, never guessed at, and never taken silently. On the real patent
the reporter was armed and wrote nothing.

## Naming: two systems for a synthesised paragraph

The defect class this work kept tripping over, recorded because it will recur:

| side | name |
|---|---|
| `structure.json` | the id the DRAFT allocated — next free `P<n>` over the draft's ids |
| the tree | POSITIONAL — `P0`, `P1`, ... over the output document |

The splicer inserts raw bytes at the `insert_before` anchor and never uses the
stored id, so the two agree only when the insert lands where the draft's
numbering happened to line up. On `parts.docx` an insert after `P0` was stored
as `P3` while the tree called the inserted paragraph `P1` and the endnote slot
`P3` — looking up by id alone returned the ENDNOTE's markup for a body
paragraph.

Resolution, in order: the id when it also carries the right body, else the
position the anchor names. The allocator also counts the OUTPUT's ids now, which
is what the splicer does.

## Identity is sticky, and every lane must carry it

`workdir_open` updates `author`, `date` and `track` only when the call supplies
them. It used to reset `track` unconditionally, so a session opened with
`track: true` went silently untracked the moment any later call omitted it.

An island states who made a revision and when. Four sites can write or re-record
one — `replace_text`, `document_patch`, `document_replace`, `commit_sync` — and
all four captured the author but passed `None` for the date. Measured: the raw
lane recorded `date=""`, the commit preserved it, and the revision reached the
document undated while every reply said `success`.

## Tests

- `projection_invariants` — no-op / target-only / revision identity
- `edited_projection` — an edit appears with its identity; comments stay
  balanced; untouched paragraphs are not thinned
- `a_tracked_edit_keeps_its_author_and_date_through_the_commit` — drives the lane
  that was broken, and was confirmed to FAIL when that lane is reverted
- `qualification/projection_audit.py` — identity-level freeze and drift check
- `qualification/typed_renderer_diff.py` — old vs new renderer on one state

267 workspace tests, clippy 0, CI green on both platforms.
