# 0042 — Version retention: GC roots, not an object database

## Status

Accepted (version-timeline design, `docs/prd/version-timeline.md`).

## Context

`Store._gc_abandoned` (`scripts/store.py`) deletes every generation that the
pointer and the transaction journals do not reference — a correct rule for
transaction durability and a wrong one for user history: a generation that a
version needs would be considered abandoned.

The retention question is therefore not "how do we store history" but "what
does GC treat as a root". Sizing matters for the answer: generations measured
at 1.4–2.0 MB for a 63 KB source, with ~70 % derivable (ADR 0041), and about
one generation per mutation (a nine-call editing session produced 3–11).

## Decision

- **GC roots** become: the current pointer, active transaction journals,
  every generation referenced by a version record, every pinned version, and
  anything the review queue still refers to. Only unreferenced generations are
  reclaimed, so the "no speculative GC" contract of the store is preserved —
  the root set is merely explicit now.
- The root list is passed into the store from the caller that knows about
  versions (`mcp_server`), so `store.py` stays free of collaboration
  vocabulary: `mutate(..., keep_generations=...)` and
  `_gc_abandoned(result, keep=...)` take the roots as data.
- **Retention policy, v1**: keep the most recent 50 versions plus every named
  or pinned version, and report the retained count and disk usage in
  `workdir_status`. A version trimmed by retention is reported as trimmed, not
  silently missing (ADR 0040's missing-content contract).
- **Not in v1**: content-defined chunking, an object database, pack files,
  per-version compression. Those are only revisited if measured retention
  actually hurts; the cheap intermediate step, if it is ever needed, is the
  content-addressed file pool over the hashes the generation manifest already
  records (ADR 0041).

## Consequences

- History retention is a policy knob, not an architecture: the timeframe to
  change a retention number is an edit to one constant plus its test, while
  introducing a dedup layer would be a change to the store's identity model.
- Trimming must be explicit and reported: users learn "V12 was trimmed", never
  "V12 is gone". A restore to a trimmed version fails closed with a named
  diagnostic.
- Because the root set is explicit, a future "generation slimming" phase can
  change what is stored inside a generation without touching the retention
  contract.
