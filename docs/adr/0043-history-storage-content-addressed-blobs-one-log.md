# 0043 — History storage: content-addressed blobs behind one append-only log

## Status

Accepted (version-timeline design, `docs/prd/version-timeline.md`).
Supersedes the "no object database in v1" bullet of ADR 0042.

## Context

The store keeps one **directory of ~20 loose files per generation**
(`.docx2typed-store/generations/<id>/…`). Measured on a 3000-paragraph
document (976 KB source):

- one generation = **5.33 MB in 20 files**; ten generations = **43.9 MB in 182
  files** — and the duplication grows with every version;
- the bytes are mostly **derivable or duplicated**: `.review/snapshots/C*.json`
  0.66 MB *each* (one per snapshot, accumulated inside every later generation),
  `format.json` 1.21 MB, `_template.docx` 0.95 MB (identical in every
  generation). The actual content, `typed.md`, is 0.20 MB;
- a one-paragraph edit changes **1 of 3100** `format.json` records (0.3 KB) and
  **1 line** of `typed.md` (0.2 KB) — but today both files are rewritten whole,
  so a version costs ~4.4 MB to record ~1 KB of change.

Loose files are also the wrong failure shape: losing one file of one generation
loses information silently, and there is no way to tell what is missing.

## Decision

Borrow Git's shape, at our scale, and drop the per-generation directory:

```
.docx2typed-store/
  objects/<ab>/<sha256>      content-addressed blobs; each unique byte stored once
  history.jsonl              append-only: one record per version = the history
  refs/current               one line: the current version id
  lock, reserve, probe.json  the writer lane and filesystem qualification (unchanged)
```

- **One log is the history.** A version record carries `id`, `parent`,
  snapshot metadata, and a pointer to its **manifest blob** — the
  path → chunk-hash mapping for the state it names. No directory per version,
  no `generation.json` per directory, no separate `versions.jsonl`, no
  snapshot files kept inside history.
- **Blobs, once.** Identical content (template, styles, unchanged paragraphs,
  a manifest that did not change) is stored once and referenced by hash; the
  measured 4.7× duplication disappears and the ratio improves as history grows.
- **Chunk at the granularity where change happens.** The two big state files
  are chunked per paragraph record (`typed.md` blocks, `format.json` records);
  small files (`styles.json`, `_template.docx`, `revisions.json`) are single
  blobs. A manifest is itself a blob, fanned out into buckets (one bucket per
  512 entries) so an edit rewrites one bucket, not the whole index — the same
  trick as Git's trees-of-trees, one level deep, and the reason per-version
  metadata stays in the hundreds of bytes rather than the hundreds of KB.
- **Derived files are never stored.** `edit.md`, `regions.md`, `revisions.md`
  and `.review/snapshots/*` are regenerated on materialisation (ADR 0041).
  This is where most of the 5.33 MB goes today.
- **Failure semantics.** A missing blob is *detectable*: the log names the
  hash it needs, verification is a hash comparison, and a restore fails closed
  with `version-content-missing` naming the version. One `history_verify`
  command sweeps the pool against the log (a `git fsck`, no new concept). The
  log is the only record that must not be lost, so it is append-only, fsynced,
  and small; the live workdir always holds the newest state, so losing it
  costs history, not the document.
- **Migration**: existing generations are imported by writing their state
  files as blobs and appending a record per generation, in order.

## Consequences

- Moving parts in the store go **down**, not up: three paths
  (`objects/`, `history.jsonl`, `refs/current`) instead of an unbounded number
  of directories, each with twenty files. Reads and writes have one shape:
  hash → store-if-absent → append record → swap ref.
- GC becomes one mark-and-sweep over the log plus the retention policy (ADR
  0042's root set is unchanged — it is simply read from the log instead of from
  per-generation manifests).
- Restore is unchanged in behaviour (ADR 0039): resolve the record, verify the
  hashes, materialise, publish. It now reads blobs instead of copying a
  directory.
- Cost model on the large fixture: a one-paragraph version adds ~1 KB of new
  content plus its metadata, against ~4.4 MB today.
- This is deliberately **not** a general-purpose object database: no packfiles,
  no delta chains, no compression, no branching. Bucketed manifests exist only
  because one file (`format.json`) dominates and changes locally.
