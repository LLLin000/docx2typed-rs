# 0044 — Version dirty: the working state is not HEAD

## Status

Accepted (version-timeline design, `docs/prd/version-timeline.md`).

## Context

Measured today: `commit_sync` publishes a collaboration snapshot, but
`format_span`, `batch_edit`, `accept_revision`, `reject_revision`, and
`review_settle` write the canonical state directly, and a single revision
decision does not publish at all (observed end to end: `accept_revision`
followed by `commit_sync` produced one publish, carrying the accept's net
change). So "canonical state" and "the last version" are already two different
things — the engine just has no name for the gap.

The existing `dirty` flag cannot express it: `edit.state.json` tracks the
*projection* against canonical (`draft dirty`), while a `format_span` leaves
the projection clean and canonical changed.

Without naming the gap, a deliverable can be produced from a state that no
version describes: `format_span` → `build_docx("final.docx")` yields a real
delivered file whose content is in nobody's history. That is the traceability
failure this whole design exists to prevent.

## Decision

Two independent facts, reported separately:

```
draft dirty     edit.md differs from canonical        (existing; unchanged)
version dirty   canonical tree != HEAD.tree           (new)
```

The save decision is explicit:

| `draft_dirty` | `version_dirty` | `publish_pending` | Result |
|---:|---:|---:|---|
| false | false | false | true no-op |
| true | either | either | sync draft, then compare canonical tree with HEAD |
| false | true | either | create a Version |
| false | false | true | publish the current snapshot; no Version |

Thus `document_patch` makes the draft dirty without moving the saved
canonical tree; after a clean save, another draft patch leaves `version_dirty`
false until it is synced. Conversely, `format_span`, review decisions, and
other canonical writers can leave the draft clean while making `version_dirty`
true.

- `commit_sync` becomes the only place a version is created, and its rule is
  the tree rule: draft dirty → sync it; then, **create a version only if
  `canonical_tree != HEAD.tree`**; otherwise no-op. Several canonical writers
  in a row therefore collapse into one version.
- `workdir_status` reports both, plus `version.current` / `version.previous`.
- **Export requires a saved version.** `build_docx` fails closed with
  `version-save-required` when `version dirty` is true, naming `commit_sync`
  as the fix. An export receipt (path + sha256 + the version it was built
  from) is recorded as operation evidence, **not** written into the immutable
  version record.
- **Exporting a historical version does not require restoring it**:
  `build_docx(version="V12")` materialises that tree into a scratch workdir,
  builds, and records the receipt against V12. This is exactly the case
  determinism (ADR 0041) makes safe.
- `verify_output` keeps verifying a built package against the workdir or the
  version it was built from; it is a reader, so the gate does not apply to it.

## Consequences

- "What did I deliver?" becomes answerable: every export names the version it
  came from, and every version names its tree. Nothing is delivered from an
  unnamed state.
- Agents keep a two-attempt bar: the refusal is a single named diagnostic whose
  fix is one call (`commit_sync`), and a scripted run that saves before
  exporting never sees it.
- The version record stays immutable: exports accrete as evidence, so
  `history_list` can still show "exported twice" by joining on demand without
  ever rewriting history.
- A deliberate non-goal: no auto-commit. The engine will not invent a version
  for a mutation the user did not save; it refuses the export instead.
