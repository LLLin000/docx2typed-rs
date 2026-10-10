# 03 — The version writer: making `commit_sync` record a Version

Settles the four questions in #37 before any code, in the same shape as
`01_pr3_structural_closure.md` and `02_format_span_rpr_lane.md`.

The reader has existed since #20 and is wired as `history_list` / `history_diff`
/ `history_blame` / `history_verify` (#36). **The writer does not exist at all**:
`versions.rs` is documented "Object pool (read-only)", and `grep` for a writer
of `head_version` / `version_seq` finds nothing outside the reader.

The loss half is already fixed (#42): a plain mutation now carries the version
HEAD forward instead of erasing it. This document is the other half — recording
a **new** version.

## What a save boundary actually writes

Read from the reference (`scripts/store.py:1937-1975`), which is the only place
a Version is born:

```python
if generation and transaction.save_boundary is not None:
    sequence = int(predecessor.get("version_seq") or 0) + 1
    digest = _canonical_tree_digest(gen_dir)
    tree_result = build_tree(self.root, gen_dir, digest=digest)
    version_record = {
        "version": f"V{sequence}",
        "seq": sequence,
        "parent_version": predecessor.get("head_version"),
        "parent_generation": predecessor.get("head_version_generation"),
        "parent_commit": predecessor.get("head_commit"),
        "head_tree": digest,
        "tree_object": tree_result["tree"],
        "baseline_epoch": int(boundary.get("baseline_epoch")
                              or predecessor.get("baseline_epoch") or 1),
        "origin": boundary.get("origin") or "commit_sync",
        "label": boundary.get("label"),
        "pin": bool(boundary.get("pin")),
        "restored_from": boundary.get("restored_from"),
        "created_at": _now_iso(),
    }
    version_record["commit"] = write_commit(self.root, {...})
```

Then `_pointer_payload(..., version=version_record)` copies six of those into
the pointer and sets `head_version_generation` to the new generation.

So a Version is: **a tree object + a commit object in the pool, plus six
pointer fields naming them.**

## The four decisions

### 1. When does a Version get created? — when the canonical tree changed

**Decision.** On a save boundary, iff `canonical_tree_digest(workdir) !=
head_tree_before`. Nothing else creates one.

**Evidence, not recollection.** Measured on a Python-built workdir:

```
after extract        (no pointer yet)
after edit (draft)   head=None    seq=None   tree=None
after commit #1      head=V1      seq=1      tree=0bedc261b350
after commit #2      head=V1      seq=1      tree=0bedc261b350   <- nothing changed
after commit #3      head=V1      seq=1      tree=0bedc261b350   <- still nothing
```

and the exact rule from `scripts/mcp_server.py:6167`:

```python
head_tree_before = decision["head"]["tree"]
creates_version = canonical_tree_digest(target) != head_tree_before
if creates_version and tx is not None:
    tx.mark_save_boundary(origin="commit_sync", label=label, pin=pin)
```

Two things this pins down that a guess would have got wrong:

- **A clean-draft commit still advances the generation** (verified: `generation
  same? False`) while creating no version. So "commit ⇒ version" is wrong, and
  so is "no transaction ⇒ no version".
- **The digest covers the canonical assets only** (`_canonical_tree_digest` =
  `semantic_sha256({name: file_sha256(asset) for name in CANONICAL_ASSETS})`),
  where `CANONICAL_ASSETS = [_template.docx, format.json, styles.json,
  typed.md]`. Measured, because the reference asserts it and `build_docx`
  writes evidence on every run:

  ```
  after a real save           head=V1 seq=1 tree=23f27dfa7820 gen=35bc187d
  after build_docx (evidence) head=V1 seq=1 tree=23f27dfa7820 gen=35bc187d
  after another build         head=V1 seq=1 tree=23f27dfa7820 gen=35bc187d
  after history_list          head=V1 seq=1 tree=23f27dfa7820 gen=35bc187d
  ```

  So `run.evidence.json` landing does not move the version, the tree, **or
  even the generation**. A writer that digested the state directory would
  invent a version on every export.

There is also an early no-op path: a clean draft with no unsaved canonical
change returns `noop: true, reason: "already saved: draft clean, no unsaved
canonical change"` without committing at all.

### 2. Generations vs Versions — Versions are pooled commits over the canonical assets

**Decision.** Both, and they are not alternatives. Every mutation commits a
generation (that is the filesystem state the reader pins); a save boundary
**additionally** writes a tree object and a commit object into the pool. The
seven pointer fields are the join between them, with `head_version_generation`
naming which generation the version was recorded at.

**Why not "just a naming layer over generations".** Because the pool is what
makes the timeline survive the generation being reclaimed — ADR 0043's point,
stated in the reference comment: *"history lives in the object pool, not in the
generation directory: the content is deduplicated, verifiable, and survives the
generation being reclaimed."* A name without a pooled commit cannot do that.

**Consequence, and the reason this is not a small slice.** The writer needs
`build_tree` (which splits the typed/format assets into content-addressed
chunks and writes blob, tree, and version objects) and `write_commit`. The
reference's `objectstore.py` is 434 lines implementing exactly that:
`put`, `put_map`, `read_map`, `split_typed`, `join_typed`, `split_format`,
`join_format`, `build_tree`, `read_tree`, `materialize`, `verify`,
`write_commit`, `read_commit`, `sweep`, `copy_objects`. Rust has the **read**
half of this in `versions.rs` and none of the write half.

### 3. `baseline_epoch` — read-only metadata for the reader; carry it, never invent it

**Decision.** Carry it forward (already done in #42) and default to 1 on a
fresh version. It is not a timestamp and not a schema version.

**What it is.** A **retention epoch**, incremented only by a restore: the two
call sites that raise it are `mcp_server.py:6484` and `:6648`, both
`baseline_epoch=epoch_before + 1` on a restore path. Everything else carries it
forward, and every read defaults it to 1 (`store.py:613`, `:1962`).

**Does the reader need it?** It reads it into `head_version`'s output
(`versions.rs:203`) and nothing else in Rust consumes it yet. So the writer
must not leave it *absent* — the reader distinguishes `null` from `1` — but it
has no behaviour attached until `history_restore` lands. Recording it as `1` on
a first version and carrying it forward is the whole requirement.

### 4. Retention — the writer side must exist before `gc` is wired

**Decision.** `history_gc` stays unwired until the writer lands. This is
ordered, not deferred.

**Why.** Retention reads a trim ledger (`history-trim.jsonl`; `trimmed_versions`
in `versions.rs`, `_append_trim_log` in the reference) and reclaims pool
objects via `sweep(root, keep_trees=...)`. Both are meaningless while no
version is ever written: `gc` on a workdir with no versions has nothing to
reclaim, and its ledger assertions — *"an unreadable or malformed ledger is
`store-invalid`, never silently empty, which would report deliberately trimmed
content as retained"* — would be tested against a store no writer can produce.
The same trap as #20's fixture: a green test over data the engine cannot make.

So: writer first, then `history_restore` (which needs `write_commit` to record
a restore as a new version with `restored_from`), then `gc`.

## Spec for the writer

**Trigger** — in the commit path, after the generation is published, on a save
boundary where `canonical_tree_digest != head_tree_before`.

**Writes, in order:**

1. `objects/tree/…` — the tree object for the canonical assets
2. `objects/version/…` — the commit object naming `parent_commit`
3. The pointer: the six version fields plus `head_version_generation`

**Ordering matters and must match the writer's usual rule:** objects are
written *before* the pointer, because a pointer naming a missing object is
unrecoverable while an unreferenced object is merely garbage. `sweep` exists
precisely to collect the latter.

**Naming** — `V{seq}` where `seq = predecessor.version_seq + 1`. Both are
carried, and `seq` is what a reader should sort by; the name is for humans.
Python's `history_restore` copies an earlier state *forward* into a new version
rather than rewriting history, so names are append-only and a gap is a
diagnostic, not a normal state.

## Verification this design must clear

1. A first save creates `V1` with `seq=1` and a tree object that `read_tree`
   can materialise back to the same canonical assets.
2. A **no-op save creates no version** — `commit_sync` twice leaves `V1/seq=1`,
   while the generation advances. (The measured Python behaviour above.)
3. Changing only **evidence or a derived view** (a `build_docx` run) creates no
   version — the digest covers `CANONICAL_ASSETS` only.
4. A second real change creates `V2` with `parent_version=V1`,
   `parent_commit` naming `V1`'s commit object, and `seq=2`.
5. `history_list` reports the chain newest-first with `version_dirty=false`
   after a save, and `version_dirty=true` after a draft edit.
6. `history_diff V2` against its parent returns the changed paragraph ids.
7. A version recorded by Rust is **readable by the Python reference** and vice
   versa — the differential #20 could not run, because it had no Rust-built
   store to run it against.
8. Objects are written before the pointer: cutting between the two leaves an
   unreferenced object and a **recoverable** workdir, never a dangling pointer.
9. `baseline_epoch` is `1` on a fresh version, carried forward unchanged by a
   save, and the reader sees `1` — not `null`.

## What this does not decide

- `history_restore` / `history_gc` wiring (ordered after, decision 4).
- `label` / `pin` as user-facing retention roots — the fields are recorded, but
  no Rust surface sets them yet; `pin` is the only thing that survives retention
  by intent, so its tool surface deserves its own decision.
- ~~Whether `objectstore`'s chunk-level dedup is worth reproducing exactly.~~
  **Settled by test 7, which was written first, as required.** On a real
  extracted workdir Rust's whole-blob writer and the reference's chunking
  writer emit the **same tree object id**
  (`6745fdc7f656f2700e095edff5bc3a69515300754100f53680bf0ca2b7b1b6cc`), because
  `split_typed` fails its own round-trip guard on the `typed.md` this pipeline
  writes, so `_parts` returns `None` and the reference stores **every** asset
  whole. Chunking is therefore unnecessary for compatibility *and* is not what
  the reference does for this input. Re-open condition: a `typed.md` that
  survives `split_typed`'s round-trip would make the reference chunk and the
  ids diverge — the digest would still agree.
