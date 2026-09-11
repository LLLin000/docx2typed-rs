# PRD: Version timeline (persistent workspace, savepoints, restore)

Status: draft · 2026-09-11 · branch `feature/agent-editor-facade`

## Problem Statement

The store gives every mutation durability, but no user-facing history. The
consequences show up as filesystem clutter and fear:

- Users keep parallel workdirs (`wd`, `wd2`, `final`, `final(2)`) because the
  only rollback they have is "keep the old folder".
- `build_docx` materialises a deliverable on demand, so "saving" and
  "exporting" feel like the same act and every save sheds files.
- There is no way to ask "what changed between then and now" or "put it back
  the way it was before the terminology pass".

Target sentence: **one document, one workspace, a version timeline the user can
list, diff, and restore — without storing a DOCX per version.**

## Design

Three levels, already present in the code, now made explicit (ADR 0040):

```text
Generation   one mutation          → hundreds, durability unit, not user language
Snapshot     one save boundary     → review/preflight baseline, renderable view
Version      one user savepoint    → what the user lists, diffs, restores
```

### What creates a Version

A Version is created **only at a save boundary** — the moment draft state
becomes canonical. Measured today:

| Operation | Publishes a snapshot today | Version |
|---|---|---|
| `commit_sync` | yes (`_commit_sync_impl`) | **yes** |
| `batch_edit` | yes | **no** — fold into the next save (see P0) |
| `format_span` | yes | **no** — fold into the next save (see P0) |
| `review_settle`, `accept_revision`, `reject_revision` | `review_settle` yes; single decisions write canonical without publishing | **no** — they prepare state; the next `commit_sync` versions it |
| `document_patch`, `document_replace`, `replace_text`, `insert_paragraph`, `delete_paragraph`, `revert` | no (draft) | no |
| `decide_all`, `table_*` | produces a **new workdir** | P3 (baseline transition) |
| `build_docx` / `verify_output` | no | no (export, not save) |

**P0 must align this.** Today `format_span` and `batch_edit` publish
immediately while single revision decisions do not, so one user-visible save
can produce two versions (batch → commit) or one (accept → commit). The rule
to enforce: *only the save boundary publishes a version; every other canonical
writer leaves state that the next save boundary versions.* The
collaboration-drift guard must keep working under that rule (a mutation that
leaves `typed.md` ahead of the session must still be resolvable by
`commit_sync` — the dead end fixed in `8dd135e` must not come back through a
different door).

### Data contract

`.review/versions.jsonl` — append-only, one record per version, written in the
same writer transaction that publishes the snapshot:

```json
{"schema": "docx2typed-version-1",
 "version": "V12", "snapshot": "C12", "generation": "6428591d0704…",
 "typed_sha256": "2e84d3e4…", "parent_version": "V11",
 "kind": "major", "label": "统一“血浆凝胶”术语",
 "created_at": "2026-09-11T01:22:47+00:00", "author": "Lin",
 "operation": "commit_sync", "operation_id": "45d66892…",
 "changed_paragraph_ids": ["P5", "P7"],
 "restored_from": null,
 "export": {"path": "…/final.docx", "sha256": "ade8dab8…", "built_at": "…"}}
```

- `version` numbering: `V<n>` assigned at creation (v1: `n` equals the
  snapshot ordinal), stored explicitly, never recomputed.
- `generation` + `typed_sha256` are the double binding (ADR 0040); a restore
  verifies both.
- `export` is filled when a `build_docx` of that version completes, so
  "perfect restore" is provable (ADR 0041).
- A labelled version is retained beyond the count limit (pinning without a
  second mechanism).

### Tool surface (deliberately small)

Two new tools, one extension:

```text
history_list(limit=20, offset=0)      → versions + current + retained/trimmed counts
history_restore(version, operation_id?)  → restores forward, returns the new version
diff_preview(from_version?, to_version?) → paragraph-level delta between versions
workdir_status()                      → gains version.current / version.previous /
                                        dirty / stale_exports / versions.retained
```

- `diff_preview` is extended rather than duplicated: its hunk-level diff, style
  ownership and revision-aware comparison are exactly what a version diff
  needs; a separate `history_diff` would be a second implementation of the same
  thing (and one more tool for an agent to choose between).
- `workdir_status` carrying the current/previous version means the common
  "where am I" question needs no history call.
- No `history_delete`, no `history_branch`, no `history_tag` in v1.

### Restore (ADR 0039)

```text
history_restore("V18")
 1. resolve V18 → generation G18; both hashes must verify, else fail closed
    (version-content-missing / version-trimmed)
 2. require a clean draft and a clean preflight (existing gates)
 3. Store.mutate(generation=True):
      overlay the CANONICAL files from G18 (typed.md, format.json,
      styles.json, revisions.json, _template.docx)
      then drop the bound pair edit.md + edit.state.json and regenerate it
      with refresh_edit_projection(target, init=True) — copying the pair raw
      fails closed as edit-header-tampered (see prototype findings)
      → classify_edit_state → validate_workdir
 4. publish_current(origin="restore", restored_from="V18",
                     changed_paragraph_ids=<delta vs the pre-restore state>)
    → new snapshot C<n+1>; without it the restore strands the session in
    collaboration drift
 5. append version V<n+1> {restored_from: "V18"}
 6. proof: rebuild from the restored state and compare against
    (a) the per-asset hashes recorded in G18's manifest, and
    (b) V18's export.sha256 when present; record both in evidence
```

Nothing rewinds: the pointer advances, V18 stays, every later version stays.

### Storage (ADR 0041/0042/0043)

Measured on the 3000-paragraph fixture (976 KB source):

| | today | with ADR 0043 |
|---|---|---|
| one version | 5.33 MB in 20 loose files | ~1 KB delta + metadata |
| ten versions | 43.9 MB in 182 files | shared blobs, one log |

Where the 5.33 MB goes: `.review/snapshots/C*.json` 0.66 MB **each** (derived,
accumulated inside every later generation), `format.json` 1.21 MB,
`_template.docx` 0.95 MB (identical everywhere), `typed.md` 0.20 MB. A
one-paragraph edit changes 1 of 3100 `format.json` records (0.3 KB) and one
line of `typed.md` (0.2 KB).

So history moves to content-addressed blobs behind one append-only log:
`objects/<sha256>` + `history.jsonl` + `refs/current`, derivatives regenerated
on materialisation, manifests fanned out into buckets so an edit rewrites one
bucket. Retention (ADR 0042) is unchanged — the root set is read from the log
instead of from per-generation manifests.

## Phases

**P0 — version records at the save boundary.**
Hook: `review_collab._publish_current_locked` (single place every publish
passes through; it already appends `history.jsonl` and persists the renderable
snapshot). Add `generation` to its inputs (the caller has it as the generation
directory name inside `Store.mutate`), append the version record, and align the
writers per the table above. Tests: one publish → one version; a two-step
save (`batch_edit` + `commit_sync`) → one version; a `commit_sync` with no
change → no version.

**P1 — list, status, restore.**
`history_list`, `workdir_status` extension, then `history_restore` with the
hash checks and the restore proof. `diff_preview(from,to)` last, since it is
the only piece that needs new comparison code (materialise both states in temp
via `store.read_root` and reuse the existing diff).
Failure codes: `version-not-found`, `version-trimmed`, `version-content-missing`,
`restore-draft-dirty`, `restore-review-pending`.

**P2 — the storage move of ADR 0043.** `objects/` + `history.jsonl` +
`refs/current`; import existing generations as blobs; stop storing derived
files; bucket the manifests; add `history_verify`. This is the phase that makes
long histories affordable and removes the loose-file blast radius, so it is
worth doing before P3.

**P3 — baseline transitions.** Absorb `decide_all` / `table_*` new-workdir
output into the same timeline: new generation + version with
`baseline_epoch` bumped and template/source fingerprints switched, instead of a
disjoint workdir. This is what finally removes "workspace sprawl" for
structural operations.

**P4 — external DOCX ingest (out of scope now).** A human edits the DOCX in
Word; ingest their file and show what changed. This is the one place an OOXML
differ earns its keep (docx4j `Differencer`, `OpenXmlDiff`, Docxodus IR diff);
it is a comparison feature, not a restore mechanism (ADR 0041).

## Reuse ledger (researched, not invented here)

| Source | What it gives us | Used how |
|---|---|---|
| SharePoint / OneDrive versioning | major/minor semantics, "restoring a version makes it the new current version", count+age retention with automatic thinning | vocabulary + retention policy shape (ADR 0042); v1 is major-only |
| Liveblocks / editor version history | list + preview + restore-of-a-rendering, restore as an undoable change | our `.review/snapshots/C<n>.json` already *is* the renderable snapshot; reuse it as the version preview |
| Jodit Collab storage | snapshot + tail-log model, `history(from,to)`, `prune(keepSnapshots)` | confirms our generation/snapshot split; the prune contract maps to retention |
| borg / restic / casync (CDC dedup) | content-defined chunking for dedup | **not adopted** — that is an object database; revisit only with measured need |
| zstd `--patch-from` | delta a file against a previous version as a dictionary | the escape hatch for P2 if deltas are wanted, one call, no new format |
| ReFS block cloning / NTFS CoW | cheap full copies at the filesystem level | not a contract (volume-format and OS dependent); do not build on it |
| docx4j `Differencer`, `OpenXmlDiff`, Docxodus IR diff | OOXML-aware comparison producing tracked changes / patch reports | P4 only; cannot reconstruct editing state (ADR 0041) |
| Existing repo pieces | `publish_current`, `document_state`, `_persist_snapshot`, `generation.json` assets manifest, `Store.mutate`, `verify_output` byte checks, build determinism | everything above is built from these; no new ledger, no new commit path, no new diff engine |

## Prototype findings (2026-09-11)

A throwaway prototype (`prototypes/version-restore-prototype.py`, branch
`prototype/version-restore`) drove the real engine on a real patent document:
three versions, a GC probe, a restore of V1, then six checks. All six pass:

| Check | Result |
|---|---|
| restored canonical state byte-identical to the version's recorded hashes | holds |
| rebuilt package identical to a rebuild of the pinned version | holds (`9f6d4cba1938` both sides) |
| `verify_output` passes on the restored build | holds |
| commit after a restore still works (no dead end) | holds |
| pointer advanced, V1/V2/V3 all still present and restorable | holds |
| the restore publishes its own snapshot (`origin="restore"`, delta `['P39']`) | holds |

Two corrections the prototype forced, both now in the design above:

1. **The projection/state pair is a binding, not state.** Copying
   `edit.state.json` while leaving `edit.md` (or vice versa) is rejected by the
   engine as `edit-header-tampered` — the header mirrors `base-typed-sha256` /
   `base-projection-sha256` / `segmentation` from the sidecar. Restore must
   regenerate the pair (`refresh_edit_projection(init=True)`), which is also
   the cheaper story: the pair is derived (ADR 0041).
2. **A restore that does not publish strands the session.** The first
   prototype restore produced a correct state and left
   `document_state.matches_filesystem == False` — the drift class that dead-ended
   the s5 acceptance run. Restore must publish in the same writer transaction.

And one premise confirmed: a version's content **is** reclaimable by contract
today (`_gc_abandoned` keeps only pointer + journal references — the prototype
listed every version generation as reclaimable), so the keep-list of ADR 0042
is load-bearing, not cosmetic.

## Verification plan

- **Version boundary**: table above asserted per operation kind (P0 tests).
- **Restore round trip**: edit → version A → edit → version B → restore A →
  the canonical state is byte-identical to A's recorded per-asset hashes;
  rebuild → byte-identical package (`build_docx` determinism is asserted by a
  test that builds twice in separate processes, since ADR 0041 now depends on
  it).
- **History monotonicity**: after a restore, every earlier version is still
  listed and still restorable (the invariant of ADR 0039).
- **GC safety**: with retention forced to a small number, a retained version
  restores and a trimmed version fails with `version-trimmed` — never silently
  approximating.
- **No regression of the acceptance bar**: the 8 real-document scenarios
  (list/patch/replace/format/commit/build/verify) must stay within two attempts
  per step; version records must not add a call to the happy path.

## Non-goals

- No Git: no branches, no merge, no detached states, no second history
  authority (ADR 0040).
- No per-version DOCX storage, no OOXML delta format (ADR 0041).
- No object database, no chunk-level dedup in v1 (ADR 0042).
- No new tool per history verb: `diff_preview` is extended, not duplicated.
