# The oracle coverage gap (blocking)

Measured 2026-10-10 against the pinned oracle. Machine-readable:
[`oracle-coverage.json`](oracle-coverage.json).

## Finding

| Surface | Tools |
|---|---|
| Feature reference (`main` @ `1adcc94`) | **49** |
| Pinned runtime oracle (`v0.1.0-python-final` @ `c6a17a16`) | **36** |
| In the reference but **not** in the oracle | **13** |

The 13:

```
document_patch    document_read     document_replace  document_search
format_span       history_blame     history_diff      history_gc
history_list      history_restore   history_verify    workspace_adopt
workspace_fork
```

## Why this is blocking, not cosmetic

**That list is the migration target.** Rust has landed `history_list` (#20), so
Rust is missing **12** tools — and **every one of those 12 is also absent from
the oracle**.

Consequence: the `python-oracle-qualification` CI job, which is the project's
declared correctness gate for Rust (`cargo test --features python-oracle
--test differential --test migrate --test tracer`), can **never** exercise any
of the work this migration exists to do. It compares against a reference that
predates the entire agent-editor surface.

Two things follow:

1. **New features need an independent qualification path.** For
   `document_read` (#23) that was a differential run against the *feature
   reference* — same workdir, both engines, byte-compare the output. That is
   strictly weaker than a frozen oracle (the reference can move), so it must be
   treated as provisional.
2. **The oracle pin must be bumped** before the migration target can be
   qualified at all. `parity/matrix.md` §0 recorded this as an open question;
   it is now on the critical path.

## What a bump involves

`PINNED_PYTHON_REFERENCE_SHA` in `.github/workflows/rust-ci.yml` pins
`v0.1.0-python-final` fail-closed, and `v0.1.0-python-final` is a **tag**, so a
bump means either a new tag on the reference repo or a switch to a commit SHA.

It is **not** a mechanical edit. The oracle predates fixes that `main` now
carries, and one of them is exactly the class of thing a naive bump would
re-break:

> the `w:rPr` child-order defect on synthesized `vertAlign` — fixed on `main`
> in #98 (`typed_core.insert_rpr_child`), still present in the oracle.

So the bump must be paired with a decision for each divergence:

| Divergence | Resolution |
|---|---|
| A bug fixed on `main`, still in the oracle | Rust implements the **correct** behaviour; the oracle must move forward, or the case must be explicitly exempted with a reason |
| A behaviour genuinely changed on `main` | the oracle moves; Rust follows the new contract |
| A case the oracle covers that `main` dropped | confirm intentional before losing the coverage |

## Concrete instances already hit

- **#23 `document_read`**: no oracle coverage. Verified instead by differential
  against the reference (7 fields byte-exact across 8 window/view cases).
- **#20 `history_list`**: same — the oracle has no `history_list`, so its CI
  job passing says nothing about it.

## Proposal

Do not bump the pin blindly. Sequence it as its own PR:

1. Diff the oracle's 36-tool surface against the reference's 49, per tool.
2. For each of the 13 absent tools, state whether the oracle *should* cover it
   (feature the oracle predates) or *must not* (removed on purpose).
3. For the shared 36, diff behaviour and list every divergence; classify each as
   fixed-bug / changed-contract / unexplained.
4. Move the pin, update `PINNED_PYTHON_REFERENCE_SHA`, and re-run the
   qualification job — expecting the previously-green tests to be re-derived,
   not merely still-green.

Until then, any Rust feature whose Python counterpart postdates 2026-08-14 is
**provisional**: it has independent evidence, but no frozen reference.
