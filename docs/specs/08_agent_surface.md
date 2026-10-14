# 08 — The agent surface: five tools, not fifty-three

Status: **design settled, implementation not started.**

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

The existing suite is the acceptance, re-pointed: `qualification/facade_task_suite.py`'s 19 natural-language tasks over 8 kinds must reach the same document state **through the five-tool surface**. A task that passes by calling the old tools proves nothing about the new interface.

Additionally:

- `tools/list` returns 5.
- The four edit-looking legacy names are absent from it.
- A fresh agent that has read only the five descriptions can complete: open a real patent → find a claim term → change it → save → confirm the built file changed — with **zero** calls outside the five and zero retries caused by a guessed convention.
