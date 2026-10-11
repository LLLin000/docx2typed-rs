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
