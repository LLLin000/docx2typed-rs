# Parity: Rust ↔ Python (Phase 1)

Phase-1 ground truth for bringing the Rust engine to behavioural parity with the
frozen Python reference. **Not** a line-by-line port: the target is equal
external behaviour, safety boundaries, verifiability, and Agent usability.

| File | What it is |
|---|---|
| `python-lock.json` | the frozen reference commit + measured surface counts + gap |
| `matrix.md` | the readable gap matrix and the ordered PR plan |
| `matrix.json` | machine-readable missing-tool list with owner + status |
| `generate.py` | regenerates both (reads the repos; never hand-edited) |
| `repro.sh` | minimal reproduction of the Phase-1 blockers |

## Frozen reference
Two references exist and must not be conflated:

```
feature reference  LLLin000/docx2typed @ 1adcc94  (current main -- behaviour to reach)
runtime oracle     LLLin000/docx2typed @ c6a17a16 (tag v0.1.0-python-final -- what CI runs)
target             LLLin000/docx2typed-rs @ 324cd05
```

The oracle is pinned in `.github/workflows/rust-ci.yml`
(`PINNED_PYTHON_REFERENCE_SHA`, fail-closed) and is **118 commits behind main**;
it predates the `w:rPr` order fix (#98). So a *correct* Rust fix can still fail
the oracle gate — that is a spec conflict to resolve by bumping the oracle, not
by reintroducing the bug. See `matrix.md` §0.

The plan cited a third, stale SHA (`1a4d4885`); `main` has since taken #100
(delivery evidence) and #101 (gate design), so the lock uses `1adcc94`.
Update only deliberately, with a new lock and a CI oracle revision if the
behaviour change reaches the oracle.

## Regenerate

```bash
python parity/generate.py          # rewrites python-lock.json + matrix.json
python parity/generate.py --json   # print the matrix, change nothing
bash parity/repro.sh               # reproduce the current red lights
```

`generate.py` fails closed: an unreadable surface or an unmapped missing tool
exits 1 rather than reporting a smaller (fake) gap.

## Rules

- Name equality is **not** functional equality. A tool counts as done only
  after input/output/error-code/permission/end-to-end evidence.
- Python is the *behavioural* reference, not always the *correct* one.
  Known reference defects (see `matrix.md` §5) must not be copied.
- Never edit the Python reference to make a Rust test pass. An Oracle bug is
  recorded, independently confirmed, and raised as a spec change.
