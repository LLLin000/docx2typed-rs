# 05. Agent Facade Transaction Contract

## 1. Why this exists

The three replace defects found by the real-patent audit were not three bugs in
one lane. They were three different layers failing to agree on one thing: what
"the edit happened" means.

| defect found | architectural fault | owning layer |
|---|---|---|
| an inserted paragraph was invisible to `document_replace` | the edit view was not unified | Document Model |
| a cross-run match was silently skipped | the operation's promise outran its effect | Edit Engine |
| the review baseline pinned the post-edit hash | transaction init order | Transaction / History |

So the fix is not another patch. It is an explicit lifecycle with
preconditions and postconditions, and an interaction layer whose `success`
means what it says.

## 2. The layer boundary

```
Agent / Patent Writing Skill      intent, planning, feedback
        |
Agent Facade                      Locate / Edit / Verify
        |
Rust Document Engine              Projection Record OOXML History Build
```

The Facade talks about the **logical document**. It must not expose
`typed.md`, `ProseLeaf`, `⦃token⦄`, or XML nodes to the agent. The inserted
paragraph omission was exactly this boundary being crossed: the logical
document and the physical projection had drifted, and only one of them was
searched.

Locate / Edit / Verify are three *capability domains*, not three new modules.
They must not re-implement the engine's editing, history, or verification —
they wrap it.

## 3. The four contracts, measured against the current engine

Each contract was probed against the engine as merged, not argued from design.

### Contract 1 — Atomicity. `HOLDS`

Input: one paragraph with a cross-run split keyword plus three runs with safe
single-run hits, so a replace finds **4** matches of which **1** is unsafe.

```
outcome: failure
code:    replace-cross-leaf-matches
message: "1 of 4 matches span more than one text run ... nothing was written"
```

All four matches were discovered and classified before any write; `edit.md`
and `islands.json` were never created. This is all-or-nothing, not
last-writer-abort.

### Contract 2 — Idempotency. `HOLDS`

| call | result |
|---|---|
| same `operation_id`, identical args | byte-exact replay of the first result |
| same `operation_id`, different args | `operation-id-reused` refusal |
| stale `base_revision` | `stale-document-view` refusal |

`"润滑" → "长效润滑"` retried does not become `"长效长效润滑"`.

### Contract 3 — Stable, version-checked handles. `HOLDS`

A `match_ref` carries `revision` + `paragraph_id` + `start`/`end` + a content
`sha256`. It is anchored to **content**, not to position:

| intervening change | behavior |
|---|---|
| a paragraph inserted elsewhere | the old ref still applies — the anchor is intact |
| the referenced text itself edited | `match-ref-stale`, with `current_text`, `current_revision` and a `fresh_match_ref` |

An agent never has to re-derive an anchor by paragraph number, and a stale
anchor fails closed instead of editing the wrong span.

### Contract 4 — Explicit coverage. `CLOSED THIS ROUND`

`document_replace` returned `scope: "body"` and nothing else. An agent reading
`matches: 0` could not distinguish "the phrase is absent" from "the phrase is
in a part this call never opens".

Measured: the editable projection carries **only the main document story**.
`parts.docx` holds `header1`, `footer1`, `footnotes` and `endnotes` paragraphs
(in `typed.md`), and **none of them appear in `edit.md`** — so no replace can
reach them, and `scope: "all"` means "all of the body", not "all of the file".

Every replace response now carries:

```json
"coverage": {
  "scanned_parts": ["document"],
  "unscanned_parts": ["header1", "footer1", "footnotes", "endnotes", "comments"],
  "note": "...a `0` match count does not mean the phrase is absent from the file"
}
```

This does not widen the engine; it stops the engine from implying a reach it
does not have.

## 4. The lifecycle the Facade must present

Any Facade operation runs as one transaction. It does not need a new engine —
`review session`, `commit_sync`, `verify_output` and History already supply the
mechanism — but the phases must be named, with their conditions.

```
C0 confirmed baseline
  |
Stage      locate, pre-check, build the edit plan
  |        (all matches classified; unsupported ones abort here)
Validate   match coverage, expected diff, OOXML fidelity
  |        (nothing published yet)
Commit     publish C1 atomically, register history
```

Failure at **any** phase publishes nothing and leaves C0 intact. Contract 1 is
the Stage-phase guarantee; Contract 4 is the Validate-phase report.

## 5. Capability vocabulary, not free text

Every Facade operation returns a capability verdict per target, never a
prose error the agent must parse:

| verdict | meaning |
|---|---|
| `supported` | planned, validated, can commit |
| `unsupported` | named capability missing (e.g. `word.text.replace.cross-leaf`) |
| `ambiguous` | more than one candidate; the agent must disambiguate |
| `stale_revision` | the handle no longer matches; a fresh one is provided |

The single rule that matters most:

> `success` means every modification the request promised has been applied and
> accepted — not merely that the underlying function did not raise.

## 6. What is deliberately NOT built yet

`document_replace` reaches the main story only; the other parts are extracted
and rebuilt but not editable by it. That is an accepted limit for this phase,
now reported rather than implied. Widening OOXML coverage is regression work
that must not block the Facade.

## 7. Build order

1. **Locate** — unified object lookup, disambiguation, stable reference.
2. **Edit** — wrap replace / insert / delete; dry-run, conflict detection,
   transactional commit.
3. **Verify** — structured diff, blocking reasons, completion state, version
   reference the agent can act on.

Build the Facade thin and against real patent documents first. Only after a
full round-trip (natural-language request → locate → preview → execute → diff
review → build → version restore) succeeds should the patent-domain rules
(claim structure, description consistency, terminology unification, embodiment
logic) be layered on. Keeping these apart is the boundary worth holding:
`docx2typed` decides *how a document changes safely*; the patent Skill decides
*what the document should say*.

---

## 8. The Completion Contract (implemented)

The Facade previously proved `reported == applied`. That is the engine's
question. The agent's question is larger, and it has three layers:

```
operation complete   the engine performed the specified edit
document complete    the exported DOCX contains the expected change
task complete        the user's natural-language request was satisfied
```

The third does not follow from the second. An agent told to "unify every
material name" may locate the 34 occurrences in the body and miss a table, a
caption or a text box. Perfect execution of 34 edits proves nothing about the
35th place it never looked.

So `facade_verify` returns **two evidence blocks, kept apart**.

### Execution Evidence — did each planned change land?

```json
"execution_evidence": {
  "planned": 28, "applied": 28, "recorded": 28,
  "unrecorded": [],
  "persisted": true,
  "build_verified": { "output": "…", "paragraphs_checked": 28, "readable": true },
  "rows": [ { "paragraph_id": "P0", "applied": true, "recorded": true,
              "uncommitted": false } ],
  "complete": true
}
```

Rows come from the **sidecar set** (what the build carries), not from the
current draft delta: after `commit_sync` the delta is empty while the edits are
still real, and deriving rows from it would report an empty execution at the
least useful moment. `unrecorded` names draft changes with no sidecar — the
ones a build would drop.

### Coverage Evidence — how far does the claim reach?

```json
"coverage_evidence": {
  "scanned_parts": ["document"],
  "unscanned_parts": ["header1", "footer1", "footnotes", "endnotes", "comments"],
  "claim_reaches": "the main document story only",
  "note": "…a `0` count does not mean the phrase is absent from the file"
}
```

### Three counts, never one

| count | source |
|---|---|
| `baseline_matches` | the template — the pre-edit state |
| `draft_matches` | the projection the agent edits |
| `built_matches` | the exported DOCX, when one is named |

Measured on the real patent with a paragraph inserted mid-session:

```
baseline 34 -> draft 35 -> built 35      residual bare occurrences: 0
```

A single number cannot distinguish a complete replacement from one that
scanned a smaller document. Three side by side make "34 of 35" visible instead
of "34, done".

### The rule this encodes

> An agent may claim a task complete only when the coverage evidence covers the
> task's scope — not merely when the execution evidence is clean.

`complete` is therefore about execution; the coverage block is what the agent
must read before telling the user the job is done.

---

## 9. Acceptance: the task suite, not the tool calls

Tool-call success is not the metric an agent is judged on. `success` on every
call can still leave the document wrong — that is the failure this whole line
of work exists to remove. So acceptance is measured on the DOCUMENT:

> A task passes only when the exported file reaches the expected state. A call
> that reports success while the file keeps the old text is a FAILURE.

`qualification/facade_task_suite.py` runs 19 natural-language tasks over 8
kinds against a real patent, each asserting either the built DOCX content or
specific payload fields. A task that only checks "no error" proves nothing, so
none does.

| kind | what it pins |
|---|---|
| unify-terminology | the whole-document rewrite lands, and the coverage block names the unreachable parts |
| targeted-claim | one handle, one paragraph; the count of other targets is unchanged |
| insert | a new dependent claim reaches the build; an inserted paragraph is itself editable |
| parameter | a dosage edit locates, and an absent term reports zero rather than guessing |
| refusal | empty query, bogus handle, cross-run span — each refused, nothing written |
| atomicity | a batch with a bad target blocks with `staged: 0` |
| idempotency | same `operation_id` replays byte-exactly; a repeat before commit refuses rather than doubling |
| completion | execution and coverage evidence both present; counts agree with the built file |
| history | a version is listed after a commit; restore works |

```
first-pass task success: 19/19 = 100%
```

### What building the suite found

Two defects that no unit test had reached:

1. **`locate` classified spans in the wrong coordinate space.** Offsets came
   from the draft while the verdict walked the template's runs. They agree only
   until an edit changes a paragraph's length — after which a healthy span read
   as `match-spans-leaves`. The verdict now comes from the search's own
   draft-space analysis, the same question the edit path enforces.

2. **A repeat before commit refused unhelpfully.** When the replacement
   contains the search term (`材料` -> `复合材料`), a naive second pass would
   produce `复合复合材料`. It refused safely but as a bare `invalid-edit`. The
   real cause is that draft offsets stop addressing the template's runs once an
   uncommitted edit changed the length, so it now says so:
   `paragraph-has-uncommitted-length-change`, naming `commit_sync` as the fix.

### The remaining boundary, stated plainly

A second edit to a paragraph that already carries an **uncommitted**
length-changing edit is refused. That is deliberate and safe: the alternative
is mapping draft offsets onto template runs whose lengths no longer correspond.
The workflow is edit -> `commit_sync` -> edit again, and the refusal says so.

## 10. Where the work now sits

| layer | responsibility | state |
|---|---|---|
| Rust Document Engine | correct execution, fidelity, persistence, versioning | frozen |
| Agent Facade | discoverable operations, locatable targets, explainable failures, evidenced completion | 19/19 acceptance |
| Patent Writing Skill | intent, claim logic, description support, professional phrasing | not started |

The boundary to hold: the Skill must not touch OOXML, and the engine must not
make patent-domain judgements.
