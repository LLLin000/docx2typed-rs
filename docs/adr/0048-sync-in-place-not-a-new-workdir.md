# 0048 — Sync in place; a workdir is never replaced to follow a change

## Status

Accepted. Supersedes the "creates a new workdir" half of [ADR 0023](0023-fail-on-baseline-drift.md).

## Context

ADR 0023 settled what *not* to do when the source DOCX moves on after an
extract: do not auto-merge, and do not overwrite pending typed edits. It then
named the remedy — "the user creates a new workdir from the latest DOCX and
reapplies unfinished changes explicitly".

In practice that remedy was the wrong half. A workdir is not a snapshot of one
baseline; it is the document's editing home, carrying the store, the workspace
and family identity, the draft and the history. Making a second workdir to
absorb a change forks all of that, so the caller has to carry the old id
forward by hand and the history is orphaned from the file it belongs to. It also
contradicted what the engine already does: `extract -o <existing workdir>`
succeeds and preserves the store, `edit.md`, `edit.state.json` and
`workspace.json`, rewriting only the derived assets (`typed.md`, `format.json`,
run evidence).

It was also unsafe in the direction nobody had guarded: that in-place
re-extraction would silently discard a **pending draft**, since the derived
assets go with it and nothing checked first.

## Decision

A workdir is the permanent editing home of its document. Drift is followed by
syncing **the same workdir** — `extract <source.docx> -o <workdir>` once — never
by creating another one.

**The precondition is that the workdir sits AT its baseline**, because `extract`
is a baseline constructor, not a merge: it re-derives `_template.docx` and the
typed projection from the source, so running it over a workdir that has moved
past its baseline resets that baseline. Two conditions say it has moved, and
each names a different way out:

- **A pending draft** (`workdir-has-pending-edit`): commit it first
  (`commit_sync`). Discarding uncommitted work needs its own explicit act.
- **Committed versions the source does not carry**
  (`workdir-not-at-baseline`): export the current version first
  (`build_docx`), or declare the discard with `--discard-committed-versions`.
  History keeps the version either way, but "history still has it" is not the
  same claim as "this is still the current version", so the engine does not
  treat the two as interchangeable.

This keeps ADR 0023's actual guarantee — pending typed edits are never silently
overwritten and are never three-way merged — while replacing its remedy.

The distinguishing test is not whether the source changed. `A → extract → edit
→ commit_sync (B) → re-extract A` loses the committed text even though A never
moved; `A → extract → (someone else saves C) → extract C` is safe and is what
in-place sync is for. Both are regression tests (positive and negative).

`workdir_open` reports `source_drift` whenever it can compare the workdir's
recorded source against the file of that name beside the workdir, because
`freshness` describes the workdir against its own store and says nothing about
the world. An uncomparable source reports nothing rather than `changed: false`:
a green light nobody earned is the failure this exists to prevent.

## Atomicity

A sync publishes ONE change set: the new baseline and every removal that goes
with it travel together, because a sync that writes the new baseline and then
clears the old one's state in a second step leaves the tear this removes when it
is interrupted. Measured before the removals joined the set: after a reset the
template was the incoming document while `islands.json` still held an edit
against the old one and `workdir.json` still pointed at the old version — and
the next build refused with an escape that accepted the tear.

What the set clears is what was expressed against the OLD baseline: the edit
sidecars, the projection and its state, the version pointer, and the store lane
(a store directory without its pointer is a workdir the store refuses to open).

The review lane (`.review/`) is NOT cleared, and does not need to be: the
session anchors itself to the baseline. `.review/session.json` records the
`_template.docx` hash it belongs to, and `ensure_session` re-bootstraps when the
workdir's baseline is no longer that one, appending a `session-rebound` history
event. Two things this anchor deliberately is not:

- **Not the projection's hash.** `current_snapshot.typed_sha256` is a PROGRESS
  marker: `publish_current` gates on it having advanced, so treating a
  difference as staleness makes that gate a tautology and refuses every commit
  with `current-not-changed`. (Tried; measured; reverted.)
- **Not a cleanup step by the app.** `docx2typed-review` depends on
  `docx2typed-app`, so the app cannot be given the job without a cycle.

Measured before the anchor existed: after a reset the session still reported the
old document's origin and a changed paragraph the new document does not have,
and the next edit was refused with `agent-preflight-required` — a reset workdir
was uneditable and nothing explained why.

## What the publish guarantees, exactly

- **Refusal path**: no side effects. Guarded by test.
- **Success path**: coherent — baseline, sidecars, pointer and store lane all
  describe the incoming document. Guarded by test.
- **Mid-publish file error, or a crash**: NOT atomic. Measured by making one
  path in the set fail: the assets before it were already written and the ones
  after it were not, with no rollback. The honest claim is "one published set
  in the absence of an I/O failure", not a disk transaction.

Closing that last gap is its own gate, not part of this decision. The machinery
already exists in the port (`copy_workdir` + `publish_workdir`, which
`workspace_fork` already uses to extract into a staging directory first);
`extract` is the caller that still writes in place.

## Considered options

- **A new workdir per baseline (ADR 0023).** Rejected: forks the identity and
  history away from the file, and puts the burden on the caller.
- **Auto-re-extract on drift.** Rejected: it is the silent overwrite ADR 0023
  forbids, and it would run without asking a human who may hold the newer work.
- **Three-way merge.** Rejected, unchanged from ADR 0023: the typed model has no
  merge semantics to justify one.

## Consequences

- `extract` over an existing workdir is a supported, guarded operation — the
  guard lives in `plan_extract`, the single funnel every extract goes through.
- A caller that ignores a pending draft cannot lose it; the refusal is the
  proof.
- The drift report is advisory, not a gate. Whether a newer source should be
  taken in remains the caller's decision, because "which document is current"
  is user intent, not a fact the engine can derive.
