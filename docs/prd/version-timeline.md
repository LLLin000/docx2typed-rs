# PRD: Version timeline (persistent workspace, savepoints, restore)

Status: **P0–P3 implemented** · 2026-09-11 · branch `feature/agent-editor-facade`

Landed: the save boundary (`commit_sync` is the only place a version is
created), `version dirty` with its export gate, `history_list`,
`history_restore` (whole version and guarded selective restore),
`build_docx(version=…)`; the **object pool** (commits + trees + bucketed
maps, per-paragraph chunks) so a version no longer depends on a full copy of
the workspace surviving, with `history_verify` and `history_gc`; and
**baseline transitions** — `decide_all` / `table_*` without `workdir_out`
adopt their new baseline as this workspace's next version instead of
creating a sibling workdir.
Acceptance: `tests/test_version_timeline.py` (C1–C8, P2 pool, P3 adoption)
plus the full suite; all green.

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
Generation   one mutation          → Store-internal durability lane (still copied,
                                     still GC-able); not user history
Snapshot     one save boundary     → review/preflight baseline; keeps its C<n> name,
                                     bound to content by its TREE hash
Version      one user savepoint    → commit object; parent chain = history
Tree         the snapshot content  → Merkle root over semantic objects
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
| `decide_all`, `table_*` | without `workdir_out`, adopts a new baseline in this workspace | **yes** — baseline transition; with `workdir_out`, sibling-workdir compatibility |
| `build_docx` / `verify_output` | no | no (export, not save) |

**P0 is implemented.** Only the save boundary creates a Version. `format_span`,
`batch_edit`, and review decisions may update canonical state or the
collaboration snapshot, but they fold into the next `commit_sync`;
`document_patch` and `document_replace` remain draft-only. A `commit_sync` with
no draft, canonical, or publication drift is a true no-op.

### Dirty-state contract

`workdir_status` reports two independent facts:

| State | Comparison | Meaning |
|---|---|---|
| `draft_dirty` | `edit.md` projection vs canonical tree | `document_patch` and other draft edits still need `commit_sync` to sync |
| `version.dirty` | canonical tree vs `HEAD.tree` | canonical changes still need a new Version |

The save boundary follows this truth table:

| `draft_dirty` | `version.dirty` | `publish_pending` | `commit_sync` |
|---:|---:|---:|---|
| false | false | false | true no-op |
| false | false | true | publish the current snapshot; no Version |
| true | either | either | sync draft, then compare canonical tree with HEAD |
| false | true | either | create a Version |

`format_span`, review decisions, and other canonical writers may therefore
leave the draft clean while making `version.dirty` true. `document_patch` first
makes `draft_dirty` true; after the canonical tree is saved, a later draft edit
does not make `version.dirty` true until that draft is synced.

### Data contract

A version **is a content-addressed commit object**; its parent chain is the
history (ADR 0043). There is no versions file to keep in sync with the pointer:

```json
{"type": "version", "schema": "docx2typed-version-1",
 "seq": 21, "parent": "<V20 object id>", "tree": "<Merkle root>",
 "created_at": "2026-09-11T01:22:47+00:00", "author": "Lin",
 "origin": "commit_sync", "operation_id": "45d66892…",
 "label": "统一“血浆凝胶”术语", "pin": false, "restored_from": null,
 "changed_paragraph_ids": ["P5", "P7"]}
```

The tree is a Merkle root over semantic objects — `document_order`,
`paragraphs/<id>` (each carrying its `typed.md` block *and* its `format.json`
record, because the record's `token_ids` point into a global token table),
`format/tokens`, `format/global`, `styles`, `template`, `package_metadata`.
Keys are paragraph identities, so inserting a paragraph does not churn the
objects after it.

Export receipts are **evidence, not part of the record**: a version is
immutable and an export happens later, so the receipt (path + sha256 + version)
is appended to the operation evidence and joined on demand.

- `V<n>` is a display id: `seq` is the truth and is never recomputed; `V<n>`
  is derived from it so the name stays stable for a human.
- The content binding is the **tree hash**. The collaboration record keeps its
  own `typed_sha256` for the live drift check (`draft dirty`), which is a
  different question from what a version contains (ADR 0044).
- `label` is descriptive metadata; `pin=true` is the independent retention root
  (`commit_sync(label="…", pin=true)`). Naming alone does not pin, and system
  labels on restore and baseline transitions remain descriptive; commit metadata
  is retained regardless (ADR 0042).

### Tool surface (deliberately small)

Two new tools, one extension:

```text
history_list(limit=20, offset=0)          → versions (walked from HEAD) + retained/trimmed
history_restore(version, operation_id?,   → whole-version restore (near zero-copy in
                paragraphs=[…]?)             storage; paragraphs= is the guarded
                                             dependency-free selective restore
                                             of ADR 0045)
diff_preview(from_version?, to_version?)  → Merkle-accelerated: skip identical
                                             subtrees, descend only into what differs
workdir_status()                          → version.current / version.previous /
                                             draft_dirty / version_dirty /
                                             versions.retained / stale_exports
build_docx(version="V12")                 → export a historical version without
                                             restoring it
commit_sync(label="…", pin=false)           → the only place a version is created
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
 3. The version the restore creates points at the OLD TREE
    (`parent = HEAD, tree = T18, restored_from = V18`) — storage costs one
    commit object; everything else is shared. A new internal generation is
    materialised from that tree for the working copy, validated, and its
    derived views regenerated (`refresh_edit_projection(init=True)`; copying
    the bound pair edit.md + edit.state.json raw is rejected as
    edit-header-tampered — see prototype findings).
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

Git analogy: whole restore is closest to
`git restore --source V18 -- .` followed by `git commit`: it restores a forward
snapshot and advances history. It is not `git revert`, which applies an inverse
patch, and it never rewinds the pointer.

### Storage (ADR 0041/0042/0043)

Measured on the 3000-paragraph fixture (976 KB source):

| | today | ADR 0043 object graph |
|---|---|---|
| ten versions, state payload | 23.8 MB in 88 files | **2.73 MB in 6421 blobs** |
| history index | one `generation.json` per version | **none — commit parent chain** |
| bytes a version adds | ~4.4 MB | **5–44 KB** |
| blob lost | silent | detected + named (version, path, chunk) |

Where the 5.33 MB goes: `.review/snapshots/C*.json` 0.66 MB **each** (derived,
accumulated inside every later generation), `format.json` 1.21 MB,
`_template.docx` 0.95 MB (identical everywhere), `typed.md` 0.20 MB. A
one-paragraph edit changes 1 of 3100 `format.json` records (0.3 KB) and one
line of `typed.md` (0.2 KB).

History moves to content-addressed objects whose version commit parent chain
is anchored by `workdir.json` HEAD: `objects/<sha256>` plus commit, tree, map,
leaf, and blob objects. `ledger.jsonl` is only the durable idempotency plane,
not history. Derived views regenerate on materialisation; retention roots come
from the commit chain, and applied content trims are recorded separately in
`history-trim.jsonl` behind the same transaction journal so deliberate loss is
distinguishable from corruption.

## Phases

**P0 — version records at the save boundary, and the two dirty facts.**
Hook: `review_collab._publish_current_locked` (single place every publish
passes through; it already appends `history.jsonl` and persists the renderable
snapshot). Add `generation` to its inputs (the caller has it as the generation
directory name inside `Store.mutate`), append the version record, and align the
writers per the table above. Tests: one publish → one version; a two-step
save (`batch_edit` + `commit_sync`) → one version; a `commit_sync` with no
change → no version.

**P1 — version dirty + export gating, then list/status/restore.**
`history_list`, `workdir_status` extension, then `history_restore` with the
hash checks and the restore proof. `diff_preview(from,to)` last, since it is
the only piece that needs new comparison code (materialise both states in temp
via `store.read_root` and reuse the existing diff).
Failure codes: `version-not-found`, `version-trimmed`, `version-content-missing`,
`restore-draft-dirty`, `restore-review-pending`.

**P2 — the storage move of ADR 0043. DONE.** `objects/{blob,map,tree,version}`
with `sha256(type + "\0" + canonical bytes)` ids; a save writes the canonical
state as per-paragraph chunks behind bucketed maps, then a tree object, then a
commit object; `workdir.json` carries `head_commit` / `head_tree_object`.
Restore and export materialise from the pool and only fall back to a
generation for workdirs saved before the pool existed. `history_verify`
checks every retained version object by object; `history_gc` trims content
past `keep_last` (explicitly pinned versions are kept), records applied trims
in `history-trim.jsonl`, and reclaims the generations whose content the pool
already holds — commit metadata is never dropped. The trim decision is first
written to the transaction journal, then durable `retention-marked`,
`retention-swept`, and `completed` phases make crash recovery finish the same
decision rather than infer it from filesystem timestamps. A deliberate trim
remains listed as `content: trimmed`, is accepted by `history_verify`, and
refuses restore/export with `version-trimmed`.
The Store's `generations/` lane keeps its job (transactions, recovery, fault
injection).

**P3 — guarded selective restore (ADR 0045).** v1 is a dependency-free,
zero-token paragraph restore only. Revision/comment/bookmark/range anchors,
content controls/SDTs, and table topology are coupled state and refuse with
`partial-restore-needs-dependent-state`; the tool never silently falls back to
whole-version restore.

**P3 — baseline transitions. DONE.** `decide_all(action, output)` and every
`table_*` op take an optional `workdir_out`; omitting it adopts the freshly
extracted baseline into this workspace: canonical state replaced wholesale,
projection rebuilt, snapshot published with `origin="baseline-transition"`,
and a save boundary marked with the epoch bumped (`mark_save_boundary(
baseline_epoch=…)`). Passing `workdir_out` keeps the old sibling-workdir
behaviour for callers that want it.

**P5 — external DOCX ingest (out of scope now).** A human edits the DOCX in
Word; ingest their file and show what changed. This is the one place an OOXML
differ earns its keep (docx4j `Differencer`, `OpenXmlDiff`, Docxodus IR diff);
it is a comparison feature, not a restore mechanism (ADR 0041).

## Reuse ledger (researched, not invented here)

| Source | What it gives us | Used how |
|---|---|---|
| SharePoint / OneDrive versioning | major/minor semantics, "restoring a version makes it the new current version", count+age retention with automatic thinning | vocabulary + retention policy shape (ADR 0042); v1 is major-only |
| Liveblocks / editor version history | list + preview + restore-of-a-rendering, restore as an undoable change | our `.review/snapshots/C<n>.json` already *is* the renderable snapshot; reuse it as the version preview |
| Jodit Collab storage | snapshot + tail-log model, `history(from,to)`, `prune(keepSnapshots)` | confirms our generation/snapshot split; the prune contract maps to retention |
| borg / restic / casync (CDC dedup) | content-defined chunking for dedup | not adopted; P2 uses a simpler content-addressed object pool with paragraph-keyed chunks and bucketed maps |
| zstd `--patch-from` | delta a file against a previous version as a dictionary | not adopted; a future storage optimization only |
| ReFS block cloning / NTFS CoW | cheap full copies at the filesystem level | not a contract (volume-format and OS dependent); do not build on it |
| docx4j `Differencer`, `OpenXmlDiff`, Docxodus IR diff | OOXML-aware comparison producing tracked changes / patch reports | P4 only; cannot reconstruct editing state (ADR 0041) |
| Existing repo pieces | `publish_current`, `document_state`, `_persist_snapshot`, `generation.json` assets manifest, `Store.mutate`, `verify_output` byte checks, build determinism | everything above is built from these; no new history authority or commit path, no new diff engine |

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
- Object storage is adopted in P2 (ADR 0043), but packfiles, delta chains, and compression remain out of scope.
- No new tool per history verb: `diff_preview` is extended, not duplicated.
