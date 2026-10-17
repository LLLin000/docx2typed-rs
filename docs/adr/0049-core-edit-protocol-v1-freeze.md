# 0049 — Freeze: Core Edit Protocol v1

## Status

Accepted, and **frozen** at commit `64d2b2d`.

This record does not claim the engine has no defects. It claims that the
consistency work this branch exists for has reached the point where further
fuzzing of the same contracts has diminishing returns, and that the contracts
below may now be treated as a baseline: changes to them need evidence, not
curiosity.

## Scope

**In the freeze.**

- Paragraph identity and the anchor-resolution contract: one handle addresses one
  span; the offset is read in the projection the caller reads; a new paragraph's
  position and its inherit source are separate questions, and the engine picks the
  source only when the caller named a position alone.
- Pending structure and island semantics: one island per leaf across calls; a
  later edit composes into the island already pending there; a composed edit whose
  text equals the template's records nothing; `insert_before` anchors resolve
  through chains, fall forward past deleted paragraphs, and refuse cycles.
- Draft → build → projection identity mapping: the build hands out the paragraph
  ids it produced, in document order, and the projection is keyed by them — never
  by re-numbering a scanned result.
- Refusal atomicity: a batch the engine refuses changes nothing (`edit.md`,
  `islands.json`, `structure.json` and the source bytes all unchanged).
- Staging, verification, publication and recovery: build to a staged path, verify,
  hash again against the verification's own evidence, publish the evidence before
  the document, and never leave a half-published document; recovery from a
  stranded journal is reachable and idempotent.

**Out of the freeze.**

- The five-tool agent protocol's workdir lifecycle (close / switch / reopen on one
  connection). The engine refuses a second workdir per connection and the surface
  has no close verb; that is a known gap, not a freeze term. Work on it must not
  reopen the editing contracts above.
- Story coverage beyond the main document body (headers, footers, footnotes,
  endnotes, comments).
- Strict semantic equivalence between the two dialect reducers. See "Known limits".
- Crash guarantees at every journal boundary, and strong protection against
  malicious concurrent tampering.

## Gates

Run from the repository root; all of them are required, and their result is what
"frozen" refers to:

```text
cargo test --workspace                     # 332 passed / 0 failed (40 suites) at the
                                           # tagged baseline; 333 after this record's
                                           # added limit test
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

The generated sequence sweep, which is part of `cargo test --workspace` at three
seeds, and — when widening — over two DISJOINT ranges:

```text
SEQUENCE_FUZZ_SEEDS=100 SEQUENCE_FUZZ_STEPS=12 \
  cargo test --release --test agent_surface sequence_fuzz          # seeds 1..=100
SEQUENCE_FUZZ_SEED_BASE=10001 SEQUENCE_FUZZ_SEEDS=100 \
  SEQUENCE_FUZZ_STEPS=12 cargo test --release --test agent_surface sequence_fuzz
```

Both ranges are green at this baseline, and no failure artifact was written. A
failure is shrunk to its shortest failing prefix and saved under
`target/sequence-fuzz/seed-<n>.json` with the call log and the replay command
(`SEQUENCE_FUZZ_SEED=<n>`).

## Known limits

These are limitations of the guarantees, not open defects. Each is stated where a
reader will hit it, and each is checked by a test that would fail if the limit
changed silently.

- **Process termination is a smoke test, not a durability proof.** The kill test
  (a child process terminated the moment its staging file appears) proves that
  what a kill leaves is either a whole version or a state the next process
  repairs, and that repairing is idempotent. It does not prove that every journal
  boundary was hit, and it is not a power-loss claim.
- **The published-bytes check is a check, not a transaction.** The staged file is
  re-hashed against the verification's evidence before the rename, so a change at
  any point BEFORE that read is refused. A change between the read and the rename
  is not detected; closing that would take a file-handle protocol, which this tool
  does not have.
- **The projection/draft comparison is whitespace-insensitive, not semantic.**
  `readings_disagree` removes whitespace and compares the character sequence, so
  it catches a paragraph wearing another paragraph's body but not a displacement
  that only moves spaces (`AB C` and `A BC` compare equal). It is an additional
  consistency check; the identities it guards are also checked by the build's own
  id list. Making it semantic means one shared reducer for both dialects, which is
  deliberately NOT part of this freeze — see below.

## Why the shared reducer is deferred

The two dialects spell the same document differently: the typed view renders an
inline tab as a tab character, the draft as a token with no text; revision and
range tokens are markup in one and absent from the other. Building a shared
*visible-atom* stream (`Text` / `Space` / `Tab` / `LineBreak`, with invisible
structural tokens excluded) would give a recomputable notion of "the visible
content", which is worth more than removing duplicated code.

It is deferred because nothing in the frozen contracts needs it yet. The trigger
for doing it is evidence: a real document whose visible text the projection and
the draft disagree about while both are individually correct. Until then, the
workaround is the whitespace-insensitive comparison above, and its limit is
recorded.

## The rule after the freeze

The frozen contracts change for exactly three reasons, each of which must leave a
minimal reproduction and a regression test:

1. a correctness defect that has been reproduced;
2. the contract and the implementation disagree (a documentation bug is a bug);
3. evidence that the frozen architecture blocks the next stage of work.

"More fuzzing found something" is reason 1 only with a reproduction. "It would be
cleaner" is not one of the three.
