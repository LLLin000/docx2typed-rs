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

---

# Format Engine v1 Integration Baseline

Recorded 2026-10-17, after the migration's last steps. This is the state that may
be treated as a baseline: not a claim that formatting is solved, a claim about
what is proven and what is deliberately out of scope.

## What is proven

| condition | evidence |
|---|---|
| The new engine is the actual execution path | `document_edit kind: "format"` resolves a handle to a character range and hands the request to the engine; the reply reports the plan's own shape |
| The build executes the plan and re-derives nothing | both build paths read `format_plan.json`, check `base_digest`, materialise; `apply_build` no longer takes formatting at all |
| Plan conformance is verified | `format-plan-applied` asserts each planned run exists with exactly the properties the plan names — the registry's bytes for a styled run, the baseline's own for one left alone; `edited-part-exact` compares the part modulo the planned runs |
| An INDEPENDENT check exists that does not read the plan | `scripts/format_preservation.py --styled PARAGRAPH:START-END=STYLE_ID` takes targets from the request and expected properties from the document's own registry, works per character, and reports nine checks |
| The old replay path is gone | `apply_rpr_edits`, `load_rpr_edits`, `save_rpr_edits`, `record_format_span` and `RPR_EDITS_FILE` are deleted, with the five tests that only exercised them |
| The five-tool interface and the frozen text protocol are unchanged | no surface change beyond `kind: "format"`'s routing; `core-edit-protocol-v1`'s identity, anchor and transaction contracts untouched |
| A real patent passes end to end | `find 3O` → `document_edit kind=format` → `save`, then the independent checker: `3` was already subscript (no-op), `O` gained it, and every other character kept its properties |

## What v1 deliberately does not do

- **No style inheritance.** Direct properties only. `docDefaults`, `styles.xml`,
  paragraph styles, `character styles`, `basedOn` and the paragraph mark's `rPr`
  are not computed, and the engine must not claim to compute Word's rendering.
- **No new property vocabulary.** A format edit switches between styles the
  document already carries; the attribute algebra (derive a style from a partial
  override map) still lives where it was, serving the wider engine's `format_span`
  tool, and moves here only when something needs it.
- **No widening beyond the document part's plain paragraphs.** A paragraph holding
  a revision gap, or one that is not a plain `P{n}`, is refused rather than
  guessed; a run inside a hyperlink is never restyled.

## Rules that hold from here

1. A new property, and any inheritance rule, is added in this engine. Not in
   `prose.rs`, not in `draft.rs`, not in the verifier.
2. The text lane may read, copy and preserve `rPr` BYTES — that is what keeps a
   rewrite faithful. It may not decide what those bytes mean. The distinction is
   the point: forbidding the byte handling would break the frozen guarantees,
   while allowing the semantics is what produced every failure this ADR records.
3. The frozen text guarantees may move to the engine. They may not weaken.
4. A verifier that takes its targets from the plan is not an independent check.

---

# Format Integration Baseline

Recorded 2026-10-17, after the blind-driven round that closed the interaction
layer's contract defects. The format engine's internal architecture is NOT
reopened by this record; it fixes what the AGENT-FACING contract now is.

## The contracts that hold

1. **A target is named by a handle, or by a constrained selector.** A selector
   (`{text, match: all|first, scope, expected_count}`) is enumerated by the
   surface, never by the caller; a count that disagrees, an incomplete search, or
   no match refuses before anything runs. A selector works for every kind.
2. **Format semantics and run planning are the engine's alone.** The surface
   resolves a target to a character range and hands the request over; the style
   check, the overlap rule, the run splitting and the expectation are the engine's.
   The build re-derives the plan from the recorded requests and re-checks
   `base_digest` against the generation it writes; the verifier checks the artifact
   against the plan and does not re-run it.
3. **A selector batch is probed member by member, and any member that cannot apply
   refuses the whole batch.** The probe runs the engine's own planner and throws
   the plan away — no sidecar, no projection, nothing written. It reports fields,
   not prose: `matched`, `executable`, `rejected`, `written: false`, and
   `failures: [{index, handle, paragraph_id, range, code}]`, all from the
   resolution.
4. **One generation, all-or-nothing.** A text edit and a format edit share the
   workdir's transaction; a refusal anywhere leaves the workdir exactly as it was,
   and the retry property is the store's, unchanged.
5. **Every declared field is consumed by the kind that accepts it, or refused.**
   The schema declares the union of every kind's fields, so a field that belongs to
   one kind can be sent with another; the invariant (and the test that keeps it) is
   that this is either consumed or `unsupported-combined-edit`.
6. **The scope claim is bounded.** Direct formatting only, and only combinations
   the document already carries: `attribute_request` derives a style from the
   span's base properties (refusing when the document has no such variant) and
   `registered_style` applies a named style's whole property set. Neither claims
   Word's computed styles, inheritance, or per-property fidelity: twelve spans
   reading as bold says nothing about what else they were.

## A probe is a check, not a promise

A probe answers "would this be accepted against the workdir as it stands". The
commit re-derives and re-checks the baseline; a probe that succeeded followed by an
execution that refuses is a workdir that moved in between, not a contradiction.

## Long-term regressions to keep

| regression | where |
|---|---|
| a batch with a member that cannot apply → whole batch refused, nothing written, members named | `rust-tests/agent_surface.rs::a_selector_batch_names_the_member_that_refuses_and_writes_nothing` |
| a member already holding the requested properties is satisfied, not a failure | `format_lane.rs::an_already_satisfied_member_is_skipped_not_refused`, `properties_resolve_to_the_document_own_style` |
| the plan and the output agree on characters, runs and properties | the verifier's `format-plan-applied`; `scripts/format_preservation.py --styled` (independent, reads no plan) |
| non-target properties unchanged, byte for byte for runs no split touched | `format_lane.rs::a_character_range_gets_a_style_and_nothing_else_moves` |
| a text edit and a format edit in one generation | `format_lane.rs::a_text_edit_and_a_format_edit_land_in_one_generation`, `formula_acceptance.rs::a_text_edit_and_a_format_edit_share_one_generation` |
| a probe writes and records nothing | `format_lane.rs::a_probe_refuses_what_the_planner_refuses_and_writes_nothing` |

## What is deliberately NOT next

Font inheritance, synthesised style variants, paragraph-level formatting, and any
per-property patch semantics. The evidence for the current shape is a blind agent
completing a real task on a real patent with the five tools alone; the next
questions are workdir lifecycle, real-DOCX compatibility and format-preservation
verification, not a wider format vocabulary.
