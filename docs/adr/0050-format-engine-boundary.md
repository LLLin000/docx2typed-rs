# 0050 — The format engine: an independent module, in the same process

Status: accepted (2026-10-17). Supersedes nothing; sits beside
[0049](0049-core-edit-protocol-v1-freeze.md), which freezes the TEXT lane's
contract and explicitly leaves formatting outside it.

## The decision

Formatting gets its own engine: `crates/docx2typed-format`, in the same process,
with its own contract and its own plan. The agent surface stays the five tools,
and a workdir change stays one generation — a format edit and the save that
publishes it are one transaction, which a second process or a second tool surface
would break.

The engine has three exclusive powers. Nothing else may have them:

1. **The only interpretation of format semantics.** What a property means, which
   styles the document carries, what a request for one of them does.
2. **The only plan from characters to runs.** A character-level request can change
   the number, the boundaries and the ownership of runs. Who splits, merges or
   keeps a run is decided here and nowhere else.
3. **The only derivation of the format expectation.** One derivation, consumed by
   the build and checked by the verifier, instead of two modules each replaying
   the same edit records.

## Why: five measured failures, all of them boundary failures

| what happened | what it actually was |
|---|---|
| the verifier did not know about `rpr_edits` and refused its own restyle | two lanes each maintaining "what counts as changed" |
| the verifier resolved a restyle span in TEMPLATE coordinates while the record was in DRAFT coordinates | a coordinate contract nobody had written down |
| two restyle spans overlapping one run, with no rule | no span/run model |
| the text lane silently losing the properties of text it did not replace | span→run policy with no owner |
| styling two characters (`3O`) turned a whole eight-character run subscript | no concept of character-versus-run granularity |

None of these is a parsing problem. Each is a module that had to know something
about formatting to do its own job — which is what "the text lane does not touch
formatting" is supposed to prevent.

## The contract

```rust
FormatEngine::plan(
    snapshot: &Snapshot,        // template runs + draft text + registry + baselines
    intents: &[FormatIntent],   // SetStyle { paragraph_id, range, style_id }
) -> Result<FormatPlan, FormatError>
```

`FormatPlan` carries:

| field | what it is for |
|---|---|
| `generation_id` | binds the plan to one transaction |
| `base_digest` | binds the text, structure and format baselines the plan was derived from, so a build against another document is refusable |
| `unit` | the range unit, declared: `UnicodeScalars`. A range can never split a character; byte offsets appear only inside the materialiser, where the bytes are the engine's own |
| `paragraph_id` | the frozen paragraph identity |
| `segments` (`ExpectedSegment`) | each character range and the DIRECT properties it is expected to have |
| `runs` (`PlannedRun`) | the run structure DERIVED from the segments: `range`, `style`, `source_run` |
| `protected` | the ranges this edit may not change |

The authoritative representation is therefore **not** "what `w:rPr` should run
*i* have": it is paragraph + character range + expected direct properties. The run
structure is derived, because a character-level request legitimately changes it.

## Verification: two kinds, and the reason for both

If the plan is wrong, a builder that executes it faithfully and a verifier that
compares faithfully both pass:

```
Plan   PASS
Build  PASS
Verify PASS
intent FAIL
```

So the verifier keeps two jobs, not one:

- **Contract conformance** — the built DOCX matches the approved plan. It does not
  re-derive spans, ordering or property algebra.
- **Independent safety invariants** — derived from the original snapshot, the
  intent and the actual output: non-target characters keep their properties,
  paragraph properties and protected structure are unchanged, and every format
  change lies strictly inside a target range. This is the check that can find a
  wrong plan, and it must not be a re-reading of the plan.

## Scope of v1: direct formatting only

A run's own `w:rPr` is what the engine reads, plans and writes. Three things are
NOT equal and must not be conflated:

- **direct** properties — what the run stores;
- **computed** properties — what style inheritance yields;
- **rendered** properties — what Word finally shows.

v1 claims the first only. `docDefaults`, `styles.xml`, paragraph styles, character
styles, `basedOn` and the paragraph mark's own `rPr` may be read or registered,
but the engine must not claim to compute Word's final rendering. Inheritance is a
later, separately-decided step.

## First acceptance: `Fe3O4-x`

A real case from a Chinese patent application: after a text edit collapsed the
formula into one run of seven characters, the caller wants the digits subscript.
`crates/docx2typed-format/tests/formula_acceptance.rs` holds every row:

| check | result |
|---|---|
| original | one run, seven characters |
| intent | digits `3` and `4` subscript |
| plan | two non-overlapping character ranges |
| build | five runs |
| non-target text (`Fe`, `O`, `-x`) | properties byte-identical |
| text content | `Fe3O4-x` exactly |
| second execution | no further splits |
| overlapping requests | refused up front (different styles), merged (same style) |
| a run holding more than properties and text | refused, not guessed |
| a snapshot that disagrees with the part | refused |
| a text edit and a format edit in one generation | both land, no coordinate drift |

## Migration

1. **The contract and the acceptance** (this ADR's commit). Nothing else moves.
2. **Item by item**, the old implementation migrates: `rpr_with_overrides`,
   `restyle_run_properties`, `apply_rpr_edits`, the style registry, the surface's
   `kind: "format"`, the verifier's restyle checks.
3. **When it is done, the text core may not read or write `rPr` at all** — that is
   the condition under which the old path truly exits, and the point of the whole
   split.

Rules that keep the boundary from eroding:

- A new property, and any new inheritance rule, goes into the format engine. Not
  into `prose.rs`, not into `draft.rs`, not into the verifier.
- The text lane may know that runs exist and that their properties must survive a
  rewrite byte for byte. It may not know what those properties mean.
- The frozen text guarantees (`format-boundary-crossed`, and every other refusal
  the text protocol has promised) may MOVE to the engine. They may not weaken.
  A split that quietly relaxes one of them is not a refactor.
