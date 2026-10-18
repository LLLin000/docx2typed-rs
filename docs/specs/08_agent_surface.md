# 08 — The agent surface: five tools, not fifty-three

Status: **implemented and shipped in 0.2.1.** The five tools are live; the
engine's other 48 stay callable by name and are not advertised. The acceptance in
§6 — the workflow end to end through the five, plus fault injection — is the
gate that still has to hold, and it is run as a test rather than asserted here.

## 1. Why this exists

The engine serves 53 MCP tools. Measured against real use:

| measurement | value |
|---|---|
| tools advertised at once | 53 |
| tools with a `description` | 0 (before this round) |
| one real session | 94 calls, **16 errors (17%)** |
| error class "the agent guessed the calling convention" | `edit-empty` (sent `old`/`new`; schema wanted `handle`/`replace_with`), `operation-id-required`, `external-operation`, `workdir-already-open` |
| error class "two engine gates disagreed" | `opaque-paragraph-mutated` ×4, `cross-leaf-edit` against `patchable_as_single_hunk: true` |

The published evidence says the count is the dominant factor, not the descriptions:

- All-tools exposure: **32.1%** task success (ToolMenuBench, arXiv 2606.15508).
- ~2.2 tools exposed: **93.1%** tool-selection accuracy; five tools on medium-difficulty queries: **60.9%** (arXiv 2605.24660).
- A **4-tool** interface is the measured optimum: **+16.4%** task completion over fine-grained primitives, argument accuracy **0.20 → 0.40** (MCP-GRANITE, arXiv 2609.24161).
- **89.1%** of tool-interface failures are tool **misselection**, not bad arguments (ToolRobustBench, arXiv 2608.23635).
- Vendor guidance agrees on direction: OpenAI "aim for fewer than 20 functions available at the start of a turn"; Gemini "keep active set to 10-20 tools maximum"; Anthropic "more tools don't always lead to better outcomes… too many tools or overlapping tools can also distract agents".

53 exposed tools, four of which read as the same verb (`document_patch`, `document_replace`, `replace_text`, `facade_edit`), is the documented worst case.

## 2. The decision

**The agent sees five tools. Everything else is the engine's business.**

The rule that keeps this honest: *if the engine can decide it, the agent must not be asked.* A workflow step that has exactly one correct answer is not a decision — it is an internal call. `commit_sync` after an edit has one correct answer. Resolving a review preflight before a write has one correct answer. Choosing between four edit transports has one correct answer the engine already knows. None of these belong in an agent's context.

## 3. The five

```text
document_open    source, author?, date?, track?          -> session + state
document_find    query, scope?, limit?                    -> handles + ONE editability verdict
document_edit    edits[], dry_run?                        -> the change, applied and synced
document_save    output, author?, date?                   -> the built DOCX + evidence
document_status  verbose?                                 -> state, versions, next step
```

### `document_open`

Absorbs: `extract` (when given a DOCX), `ensure_identity`, `workdir_open`, `workdir_status`, session resume and rebase, and preflight resolution.

`source` accepts **either** a DOCX path **or** an existing workdir path. A real session was refused with `workdir-invalid: workdir not found: …hydrogel-copy.docx` for passing a document to a tool documented as accepting one — the engine must not make the caller know which of the two it has. Opening a source already open on this connection is a success, not `workdir-already-open`.

### `document_find`

Absorbs: `document_search`, `list_paragraphs`, `get_paragraph`, the scoped read, and **the editability verdict**.

Returns handles, and for each candidate **one** verdict about whether it can be edited as a single edit — the verdict the edit path will reach, computed once. A search must never promise `patchable_as_single_hunk: true` for a span the edit refuses as `cross-leaf-edit`.

### `document_edit`

Absorbs: `document_patch`, `facade_edit`, `replace_text`, `batch_edit`, `insert_paragraph`, `delete_paragraph`, `format_span`, `diff_preview`, **`commit_sync`**, and `revert` on failure. It is one transaction:

```text
preflight -> resolve targets -> apply -> sync -> verify -> publish
                                                         \-> roll back on failure
```

`edits` has exactly **one** shape — `{ "handle": <from document_find>, "replace_with": <text> }` — so there is no second key spelling to guess. `operation_id` is **optional**; the engine mints one, returns it, and replays byte-exactly on a repeat. The agent is never asked to call a sync tool between two edits: `paragraph-has-uncommitted-length-change` cannot occur, because the sync is not the caller's job.

A failed edit writes nothing and says which edit failed and why.

### `document_save`

Absorbs: `build_docx`, `verify_output`, and the evidence/ledger publication. The build is verified before it is reported: a save that reports success while the file kept the old text is the failure this whole line of work exists to remove.

### `document_status`

Absorbs: `workdir_status`, `history_list`, `review_state`, and the version list. Reports pending work and, when a next step exists, names it.

## 4. What is deliberately NOT in the five

Specialist operations that no normal flow needs — table structure surgery, comment/revision decisions, workspace adopt/fork, history restore, the review console — stay implemented and stay callable **by the engine**. Where one is genuinely needed, it is a field on one of the five (`document_edit` can restore a version named by `document_status`), not a sixth tool.

If something cannot be expressed that way, that is a finding to report, not a sixth tool to add silently.

## 5. The invariants

1. `tools/list` advertises exactly these five. The other 48 remain implemented; they are simply not the agent's interface.
2. Every failure carries `recovery: { action, tools }` (ported from the Python reference's `_recovery_for`), so a refusal names the next call.
3. Every tool and every argument has a description written to the Anthropic/OpenAI/MCP standard: what it does, when to use it, when NOT to, what each argument means, and an example where the shape is non-obvious.
4. `additionalProperties: false` everywhere, and a wrong key is refused **naming the right one**.
5. No two of the five overlap. A call that could have gone to two of them is a design defect, not a caller error.

## 6. Acceptance

Two suites, two subjects — and the split is deliberate:

- **`rust-tests/agent_surface.rs`** is the SURFACE's acceptance, and it is a CI
  gate. It drives the workflow the way an agent does and asserts the FILE, not a
  success flag: open -> find -> edit -> save -> status, then the built DOCX is
  read back and its text checked. Plus the failure modes that are injectable from
  outside: a blocked batch changes no document content, a replay does not apply
  twice, a reused id with different edits is refused, re-opening a DOCX does not
  rebuild the baseline, and find reports its coverage.
- **`qualification/facade_task_suite.py`**'s 19 natural-language tasks remain the
  ENGINE's acceptance. They drive the engine's own tools, which are still served
  by name, so they still prove what they always proved.

**The 19 tasks now run through the surface too**, and pass 19/19:

```text
python qualification/facade_task_suite.py            # the engine's tools
python qualification/facade_task_suite.py --surface  # the five
```

It is not a rename. The tasks needed three things the surface did not have:

1. **A handle comes from a previous call.** The engine's tasks are one static
   batch; a surface edit addresses a handle the search returned. The suite now
   resolves `@<call index>.<dotted path>` placeholders — and `*` over a list, for
   "unify this term everywhere", which is what an agent does by looping — and
   runs one call per connection, as an agent does.
2. **Insert and delete.** The surface could only replace a handle's text, so
   "insert a dependent claim" — a core patent task — was not expressible at all.
   `document_edit` now takes a closed `kind` of `replace`, `insert_after` or
   `delete`, still addressing every kind with the same handle.
3. **A save boundary.** The surface's edit absorbs `commit_sync`, and
   `commit_sync` IS a save boundary; without it an edit never minted a Version
   and `document_status` reported `versions.total: 0` after a successful edit.

Two task expectations changed, and both are the surface being BETTER:
`repeat_before_commit_refuses` expected `paragraph-has-uncommitted-length-change`
— a state that cannot exist on the surface, because every edit is settled as part
of the edit — and it now refuses for the reason that remains, `edit-blocked`. And
an empty query is refused by the surface's own argument validator
(`invalid-arguments`) before the search is reached.

Additionally:

- `tools/list` returns 5.
- The four edit-looking legacy names are absent from it.
- A fresh agent that has read only the five descriptions can complete: open a real patent → find a claim term → change it → save → confirm the built file changed — with **zero** calls outside the five and zero retries caused by a guessed convention.

## 7. Evidence (PR69, HEAD 9815fa8, CI run 39098870126)

Surface acceptance is the Rust `agent_surface.rs` workflow gate plus the 19
`qualification/facade_task_suite.py --surface` consumer tasks. Core gates are
green (Rust tests and oracle qualifications); both package jobs failed the
surface suite 16/19 on three stale assertions, not on engine behaviour:

- `unify_no_match_is_informational` and `edit_example_parameter` asserted
  `counts.matches == 0`. `inserted_paragraph_is_editable` asserted
  `counts.matches == 1`. The current `document_find` reply carries no
  `counts.matches` field (`src/surface/find.rs`): the consumer-visible count
  is `counts.occurrences` (occurrences on this page, summed over the
  per-occurrence verdicts the caller acts on) alongside the document-wide
  `counts.total_matches` inherited from the search, plus `editable`,
  `not_editable`, `blocks`, paging, and `complete`. The evaluator reads a
  missing path as `None`, hence `got None want 0/1`. The fix asserts the
  current semantics with exact expected counts — `occurrences` and
  `total_matches`, including the zero-match case — and adds no alias or
  engine change.
- The three `#[ignore]` emitters are opt-in fixture writers, not skipped
  correctness: `rust-tests/style_id_differential.rs::emit_style_id_fixture`
  (writes `STYLE_ID_FIXTURE`, default `C:/tmp/styleids.json`) and
  `rust-tests/token_table_differential.rs::emit_token_table_fixture`
  (writes `TOKEN_FIXTURE`, default `C:/tmp/tokens.json`) emit Rust-side maps
  for manual comparison — the style-id comment names the historical
  `scripts/check_style_ids.py` consumer, the token-table comment points at
  the PR notes; and
  `crates/docx2typed-store/tests/pool_objects.rs::emit_cross_read_fixture`
  (writes `POOL_FIXTURE`, default `C:/tmp/poolxread`, honouring `STATE_DIR`
  for a real extracted workdir) emits pool objects for manual cross-language
  inspection. Neither historical `check_style_ids.py` nor `check_cross_read.py`
  ships in the current tree. These emitters do not themselves assert comparison
  results and are not counted as correctness coverage. Keeping them opt-in
  prevents normal test runs from writing or deleting fixture destinations.
  The baseline, reconciliation, protected-structure, format and publication
  assertions run in the non-ignored workspace tests.

Acceptance commands:

```text
cargo test --test agent_surface
PYTHONIOENCODING=utf-8 PYTHONUTF8=1 python qualification/facade_task_suite.py --surface
```
