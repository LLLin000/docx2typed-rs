# 0040 — Versions hang off the collaboration snapshot chain

## Status

Accepted (version-timeline design, `docs/prd/version-timeline.md`).

## Context

Two ledgers already exist per workdir:

- **Store** (`<wd>/.docx2typed-store/`): immutable generations, one per
  mutation, `generation.json` carrying `parent`, `operation_id`, and a
  per-asset sha256 manifest. Exists for transaction durability.
- **Collaboration** (`<wd>/.review/`): `session.json` with
  `current_snapshot` / `staged`, one renderable `snapshots/C<n>.json` per
  canonical round, and an append-only `history.jsonl`. `publish_current` runs
  at save boundaries — `commit_sync`, `batch_edit`, `format_span` — not on
  every draft patch.

A user-facing "version" could be built on either, or on a third ledger of its
own. A third ledger is the tempting answer and the wrong one: it immediately
reopens the question this project already answered once when refusing to embed
Git — which ledger is the source of truth, and what happens when they disagree.

## Decision

A **Version** is a named, listable reference to a collaboration snapshot:

- `V<n>` is a stable id recorded at creation and never recomputed; in v1 it is
  1:1 with the `C<n>` snapshot, and the record keeps both so later minor
  versions can break the 1:1 without renaming anything.
- The record pins the content by **two** hashes: the store `generation` id that
  holds the canonical state at that snapshot, and the `typed_sha256` the
  snapshot was published with. Restore verifies both; a mismatch fails closed
  (a version whose content is gone is reported as missing, never silently
  approximated).
- Versions are appended to `.review/versions.jsonl` (append-only, one record
  per version) in the same transaction that publishes the snapshot — no
  separate commit path, no second write protocol.
- `workdir_status` reports `version.current`, `version.previous`, and whether
  the draft is dirty, so the common "what am I looking at" question needs no
  history call at all.

## Consequences

- The three levels stay distinct: a **Generation** is a durability unit (many
  per task, not user language), a **Snapshot** is a canonical round (the
  review/preflight baseline), a **Version** is a user-facing savepoint.
- Listing versions is reading one file; no directory scan of
  `.docx2typed-store/generations/` ever becomes a contract (ADR 0042 keeps the
  GC honest about which generations are retained).
- A version does not own content; it references it. Content ownership stays
  with the store, which already guarantees immutability.

## Updates (2026-09-11, review)

The **binding** is now the tree hash rather than the store generation id: the
version commit object points at a Merkle root (ADR 0043), which is what a
restore verifies. The collaboration record keeps its `typed_sha256` for the
live drift check only. Keeping a version "on the snapshot chain" still holds in
the sense that matters here: a version is created at a save boundary, by the
same write path that publishes the snapshot — never by a separate ledger.
