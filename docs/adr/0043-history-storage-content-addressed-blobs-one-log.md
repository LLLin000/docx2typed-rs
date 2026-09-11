# 0043 — History is a commit graph of content-addressed objects

## Status

Accepted (version-timeline design, `docs/prd/version-timeline.md`).
Supersedes the "no object database in v1" bullet of ADR 0042, and replaces the
`history.jsonl` layout this ADR first proposed.

## Context

The store keeps one **directory of ~20 loose files per generation**
(`.docx2typed-store/generations/<id>/…`). Measured on a 3000-paragraph document
(976 KB source):

- one generation = **5.33 MB in 20 files**; ten generations = **43.9 MB in 182
  files**, and duplication grows with every version;
- most of it is derived or duplicated: `.review/snapshots/C*.json` 0.66 MB
  *each* (accumulated into every later generation), `format.json` 1.21 MB,
  `_template.docx` 0.95 MB (identical everywhere); the actual content,
  `typed.md`, is 0.20 MB;
- a one-paragraph edit changes **1 of 3100** `format.json` records (0.3 KB) and
  **1 line** of `typed.md` (0.2 KB), yet both files are rewritten whole.

A first proposal — blobs plus an append-only `history.jsonl` as the history
authority — was prototyped and worked (2.73 MB of blobs for 23.8 MB of state,
8.5 KB of log, 5–44 KB per version, lost blobs named rather than silent), but
it keeps a **second authority**: the log says what history is, the pointer says
what is current, and the two need their own recovery story (append line → crash
→ pointer not yet swapped). That coordination problem does not need to exist.

## Decision

History **is** the version commit graph. No history file, no separate ledger:

```
.docx2typed-store/
  objects/<ab>/<sha256>     content-addressed objects (commit, tree, leaf, blob)
  generations/              INTERNAL: the Store's own materialisation lane for
                            mutations and crash recovery — it no longer carries
                            user history
  transactions/ staging/    crash journal + recovery reserve (unchanged)
  lock/ probe               writer lane + filesystem qualification (unchanged)
  workdir.json              HEAD: current_generation, head_version, head_tree, version_seq
```

- **A Version is a commit object**: `{type: "version", schema, seq, parent,
  tree, created_at, author, origin, operation_id, label, restored_from}`.
  Its parent chain *is* the history; `history_list` walks it from HEAD, so no
  index file exists to fall out of sync with the pointer.
- **A Tree is the snapshot**: a Merkle root over semantic objects, not over
  file bytes. `document_order`, `paragraphs/<id>`, `format/*`, `styles`,
  `template`, `package_metadata`. Because the keys are our paragraph
  identities, inserting a paragraph does not churn the objects of the
  paragraphs after it — a byte-offset chunking of `typed.md` would.
- **A paragraph object carries its own state together** — `{text: <sha>,
  format: <sha>}`. `typed.md`'s block and the `format.json` record that
  describes it are one unit: the record holds `token_ids` pointing into a
  global token table (`N146…N162`: revision open/close, rpr-change,
  comment anchors, commentReference — a real paragraph references 17 of them),
  so a half-swapped paragraph is exactly the dangling-reference failure to
  avoid. The token table is its own object, versioned per commit.
- **Object identity** is `sha256(object_type + schema + canonical bytes)` over
  the uncompressed canonical form. The physical layer (loose file today,
  zstd/packfile/delta later) can change without any version id changing.
- **Write order** is objects → commit object → single CAS on HEAD. A crash
  before the CAS leaves unreferenced objects (harmless, collectable); there is
  no window in which a pointer names an incomplete commit, which is the
  coordination problem the JSONL layout would have had.
- **Derived views are not object-pool assets**: `edit.md`, `regions.md`,
  `revisions.md`, and `.review/snapshots/*` are regenerated on materialisation.
  The transaction generation may retain immutable review snapshots as hard
  links (falling back to byte copies when the filesystem cannot link them), so
  review history remains available without repeating their payload bytes.
- **`Snapshot` stays as a name, not a second object.** The collaboration layer
  keeps its `C<n>` display id and its live session, but the authoritative
  binding of a snapshot becomes its **tree hash** (the record carries it), so
  there is one content truth and the review API keeps working.
- **Restore of a whole version is near zero-copy in storage terms**: the new
  commit points at the *old tree* (`parent = HEAD, tree = T18,
  restored_from = V18`). Materialising a working copy, validating, and
  regenerating derived views is work — but it is working-copy work, not history
  duplication.
- **Diff is derived from trees, not stored**: comparing two commits skips
  identical subtrees by hash and descends only into what differs. No delta
  chain, no patch log, nothing to corrupt in the middle of history.

## Measured (throwaway prototype over ten real generations)

| | loose-file layout today | objects + one commit graph |
|---|---|---|
| state payload | 23.8 MB in 88 files | **2.73 MB of objects** (8.7× dedup) |
| history index | one `generation.json` per version | **none** — the parent chain |
| bytes a version adds | ~4.4 MB | **5–44 KB** (first is the base state) |
| blob lost | silent | detected and named (version, path, chunk) |

Two corrections the prototype forced, kept here as design constraints:

1. **The index is the cost, not the content.** Inlining every chunk hash into
   each record produced a 6.4 MB log for 2 MB of content. Fanning the manifest
   into buckets took the same history to 8.5 KB of index.
2. **Bucket size is the per-version knob.** 64 entries per leaf ≈ 25 KB per
   version (one `format.json` leaf rewritten); smaller leaves cost fewer bytes
   per version and more index objects. Choose it from the measured write
   pattern.

## Consequences

- Fewer moving parts than before the change, not more: `objects/`, the
  existing pointer, and the existing transaction lane. The store's durability
  machinery (`generations/`, journals, recovery, the fault-injection tests) is
  untouched — it keeps doing transactions, while history moves to the graph.
- GC is one mark-and-sweep over the graph plus retention (ADR 0042):
  reachable objects survive, the rest are collected. Retention trims *content*,
  never commit metadata; each applied trim is recorded in the small
  `history-trim.jsonl` retention ledger (not a history authority), so trimming
  downgrades a restore/export to `version-trimmed` rather than making a version
  vanish from the list. `history_verify` treats that deliberate loss as
  healthy while still failing on an unrecorded missing object.
- Migration: existing generations are imported as objects + one commit each,
  in order, then the graph takes over.
- Deliberately still excluded: packfiles, delta chains, compression, branches.
  The object id is defined over canonical bytes precisely so those can be added
  later without touching the model.
