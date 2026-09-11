# 0041 — A version stores editing state, not a package

## Status

Accepted (version-timeline design, `docs/prd/version-timeline.md`).

## Context

Two shortcuts were on the table for "let the user go back one version", and
both are wrong for this system:

1. **Keep the exported DOCX of every version** and re-import the one you want.
2. **Diff the DOCX packages** (say, with an OOXML differ) and reconstruct the
   old package from the current one.

Three measurements decide the question, all reproducible from a real workdir:

- **`build_docx` is deterministic.** Two builds of the same workdir, in two
  separate processes, produce the same package sha256; ZIP entry timestamps
  are pinned (`1980-01-01`). A package is therefore a *function* of the
  editing state, not an independent artifact.
- **`build → extract` is not an identity.** Re-extracting a built package does
  not reproduce the state it came from: opaque token ids are re-assigned
  (`N163 → N165`), the governed-baseline records for touched paragraphs
  (`sync_segments` / `sync_skeleton` in `format.json`) come back `null`
  (66 of 115 paragraphs in the measured document), and the source metadata
  changes. The package alone cannot reconstruct the state.
- **A generation is mostly derived data.** For a 63 KB source document a
  generation is ~1.4–2.0 MB, of which ~531 KB is a render snapshot, ~407 KB
  `format.json`, ~119 KB `typed.md` (the actual content), and the rest
  projections (`edit.md`, `regions.md`, `revisions.md`) plus a 64 KB
  `_template.docx` that is byte-identical across generations. Step deltas
  measured across nine real sessions: draft-only steps move 68–95 KB;
  a commit step moves ~1.4 MB because it rewrites the render snapshot and the
  baselines.

The DOCX ecosystem's "smart diff" tooling does not change this picture:
docx4j's `Differencer`/`diffx`, `OpenXmlDiff`, and Docxodus' IR diff engine all
produce *review artifacts* — tracked changes (`w:ins`/`w:del`) or XML patch
reports — for comparing two documents. None of them reconstructs an editing
state, which is what a restore needs.

## Decision

- A **Version references editing state**, never a package. In v1 a version adds
  no bytes at all: it references the immutable generation that already holds
  that state (ADR 0040). Packages remain derived artifacts; `build_docx`
  produces them on demand and they are never edited.
- **"Perfect restore" is defined as three checkable claims**, published as
  evidence by the restore operation:
  1. the restored canonical state files match the sha256 recorded in the
     version's generation manifest (byte-identical state, verified per asset);
  2. a rebuild from the restored state is byte-identical to a rebuild from the
     original state — guaranteed by build determinism, asserted per restore;
  3. when the version recorded an export (`export.sha256`), the rebuilt
     package is compared against it.
- **Storage work, when it becomes necessary, keeps this order**: (a) reference
  instead of copy; (b) share unchanged assets by the sha256 the manifest
  already computes (a content-addressed file pool); (c) delta-compress the
  remaining per-version text (`typed.md` line deltas; `zstd --patch-from`
  against the previous version if a compressor is wanted). No object database,
  no OOXML delta format, no per-version DOCX.
- **Hardlink sharing is not adopted now.** It would be safe only if every
  writer replaced files atomically; today `_write_edit` uses an atomic write
  but other writers (`format.json`, `revisions.json`, `.review/snapshots/*`)
  write in place, so a hardlinked generation could be corrupted retroactively.
  Making writers atomic-replace first is a prerequisite, not an assumption.
- **OOXML differs stay out of the restore path.** They become relevant only to
  the later feature "a human edited the DOCX in Word; ingest their file and
  show what changed" (PRD phase P4), where the comparison is between two
  packages and no canonical state is being reconstructed.

## Consequences

- Restore is cheap and boring: read a generation, copy canonical state
  forward, verify hashes, rebuild. Its correctness does not depend on any
  differ's fidelity.
- The determinism property is now load-bearing — it is what makes "we do not
  store the package" acceptable. Any change that makes `build_docx`
  non-deterministic (a timestamp, a set-iteration order, a locale-dependent
  sort) breaks the restore guarantee and must be treated as a regression.
- Version storage cost is decoupled from package size: a 60 KB deliverable can
  still cost ~500 KB of state if the state is large, so retention (ADR 0042)
  is expressed in generations retained, not in exports kept.
