# 0039 — Restore creates a new generation; history never rewinds

## Status

Accepted (version-timeline design, `docs/prd/version-timeline.md`).

## Context

An editing session produces many generations, but the store exposes none of
them as user-facing history: the pointer is the only thing that names "the
current state", and `_gc_abandoned` treats everything else as reclaimable
scratch. Users therefore keep parallel workdirs (`wd`, `wd2`, `final`) to
hold their own rollback points.

The obvious implementation — repoint `workdir.json` at the older generation —
is a trap. A rewound pointer makes every generation after it unreachable
without a second, hidden history; it invalidates the operation ledger's
account of what happened; it desynchronizes the collaboration snapshot
(`current_snapshot` / `typed_sha256`) that review, preflight, and every
in-flight `match_ref` are bound to; and it leaves no audit trail for the
restore itself.

## Decision

Restore copies an older version's canonical state *forward* into a new
generation:

```
G20 (current) ──restore(V18)──> G21
                                 G21.content      = G18.content
                                 G21.parent       = G20
                                 G21.restored_from = G18
```

- The current pointer only ever advances; no generation is ever orphaned by a
  restore, so no version can disappear as a side effect of returning to an
  earlier one.
- The new generation is committed through the existing `Store.mutate` lane
  (Writer lock, journal phases, CAS pointer swap, materialize, evidence). No
  new commit path is introduced.
- Restore publishes a collaboration snapshot like any other canonical write,
  with `origin="restore"` and `restored_from`, and records a new Version
  whose `restored_from` names the source version.
- Restore is a user-level operation, not a rewrite: the version it restores
  from remains listed, as does every version after it.

## Consequences

- The whole restore is closest to
  `git restore --source V18 -- .` followed by `git commit`: it restores a
  forward snapshot and advances history. It is **not** `git revert`, which
  applies an inverse patch.
- The alternative — pointer rewind, or a `git checkout`-style detached
  current — was rejected: it requires a second history to be survivable, and
  it breaks the ledger/preflight contracts above. The pointer never rewinds.
- Storage-wise a restore costs one generation like any other mutation; it is
  not free to call in a loop, but it is cheap enough (see ADR 0041) that a
  user can restore, look, and restore again.
- The GC root set must include version-referenced generations (ADR 0042),
  otherwise a restore target can be collected before it is used.

## Updates (prototype 2026-09-11)

A throwaway prototype (`prototype/version-restore` branch) restored a real
version end to end and confirmed this decision; it also pinned two facts the
implementation must honour: the canonical projection pair
(`edit.md` + `edit.state.json`) is hash-bound and must be **regenerated** on
restore rather than copied, and a restore that does not publish its snapshot
leaves the session in collaboration drift (`matches_filesystem == False`) —
the dead-end class this project already fixed once. See the PRD's prototype
findings.
