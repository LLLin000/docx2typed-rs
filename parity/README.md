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

```
reference  LLLin000/docx2typed    @ 1adcc9403cc830e899efe74c5ec41b11973b8c7c
target     LLLin000/docx2typed-rs @ 324cd055096e841fd5c97757def982b91ed93e15
```

The plan cited `1a4d4885` for the reference; `main` has since taken two merges
(#100 delivery evidence, #101 gate design). The lock uses `1adcc94` so the
evidence contract is included. Update only deliberately, with a new lock.

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
