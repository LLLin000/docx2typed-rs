---
name: docx2typed
description: >
  Word DOCX text editing with locked formatting and structure (byte
  fidelity), plus a browser review console and human-to-agent handoff. Use
  when text must change while Word formatting, tracked revisions, comments,
  table structure, hyperlinks, content controls, or package parts must stay
  safe; accept or reject revisions; delete or clear comments; insert/delete
  table rows or columns; merge or split cells; edit content-control text;
  audit Unicode superscript/subscript normalization; or run any extract ->
  edit -> build -> verify DOCX workflow. MCP server available for agent tool
  calls.
---

# docx2typed — byte-fidelity Word text editing

The contract is **byte fidelity**: untouched content replays byte-identical;
only the text you change moves. Editing is typed-mode (continuous prose +
locked structural tokens), never raw XML surgery.

## Skill graph

This skill is structured as a graph of four files — a hub plus three
reference layers, each with explicit dependency edges:

```text
SKILL.md ──────────────── hub: invocation, rules, branch table, gates
├── capabilities.md ──── ATOMS (工具): every CLI command + MCP tool,
│                         exact syntax and exit contract. No dependencies.
├── composites.md ────── MOLECULES (工作流): 7 workflows that chain atoms,
│                         each with ordered steps and completion criteria;
│                         plus 3 end-to-end playbooks.
│                         depends on: capabilities.md atoms, verification.md gates
└── verification.md ──── GATES (检查): the shared acceptance contract every
                          workflow ends on. Applied by all composites.
```

The graph works by composition, not nesting: a workflow names the atoms it
uses and the gates it ends on; you never open more than one layer deep from
the hub.

## Branch table — where to start

| Task | Open | Flow |
|---|---|---|
| Change text (plain, tracked, or inside content controls) — the default path | → DEFAULT EDITING PATH below | read/search → `document_patch` → diff_preview → commit_sync |
| Accept / reject tracked revisions | → Workflow 3 | decide accept/reject → build → verify |
| Delete comments (per entry; settlement preserves them) | → Workflow 4 | decide comment-delete × N → verify |
| Patch several paragraphs in one call (replaces / insert / delete, or a unified diff) | → `document_patch` | hunks or diff → diff_preview → commit_sync |
| Insert/delete table rows or columns, merge/split cells | → Workflow 5 | decide table-* → new baseline → verify |
| Unicode superscript/subscript normalization | → Workflow 6 | audit scan → policy → approval → apply |
| End-to-end finalize / revise / agent session | `composites.md` → Playbooks | workflow chains + full gate set |
| Open the browser review console or process human decisions | `composites.md` → Playbook D | review console → human decision → agent queue → build/verify |
| Install or configure the package for an agent | `Installation.md` + the host's skill manager | authorize → install → verify → hand off |

Not sure which workflow? The atoms live in `capabilities.md`; read the
workdir state (`view --mode clean` or `edit status`) first, then pick.

## DEFAULT EDITING PATH (MCP)

```text
workdir_open
→ document_read (whole, or outline for large docs) / document_search
→ document_patch            (hunks or unified diff; one call per editing intention)
→ diff_preview
→ commit_sync               (the save boundary: preflight + sync + CAS + evidence)
→ build_docx → verify_output
```

Rules:

- `document_read` returns the editable projection with `revision=<token>`;
  pass it as `base_revision` on `document_patch` so a stale view fails
  early with `stale-document-view` instead of after context parsing.
- `document_search` returns whole matching blocks with `prev_id`/`next_id`
  anchors — you should never need `get_paragraph` to locate prose.
  Cross-region rewrites are the facade's job (the Core assigns style);
  `get_paragraph` + `batch_edit` are only for explicitly requesting exact
  per-region style ownership or for engine-refusal diagnosis.
- If `workdir_open` reports `effective_mode: "ambiguous"` (pending
  revisions, track flag off or vice versa), choose a mode BEFORE editing:
  re-open with `track=true` (revisions generated) or `track=false`
  (direct). Patches are refused with `edit-mode-ambiguous` otherwise.
- `document_search` matches the TOKEN-FREE visible text, so inline markers
  (revision edges, comment refs, bookmarks) never break a query that reads
  as continuous, and width/punctuation variants are tolerated. Each hit
  returns `matched_text` (copy-paste ready as `old`), `offset`, `region`
  and `normalized`.
- Matching is width/punctuation tolerant everywhere (patches, search,
  formatting): full-width ↔ half-width, CJK punctuation ↔ ASCII, exotic
  spaces, micro-sign ↔ mu. A tolerated match reports
  `matched-with-normalization` with the exact character pairs.
- `document_read(view="issues")` runs the document checks an editor would:
  element charges missing a superscript (with a ready `format_span` fix in
  `fix`), mixed punctuation width, a full name defined more than once, and
  comment anchors trapped inside tracked deletions.
- Formatting changes: address the text EXACTLY via `format_span` +
  `span_index` taken from `document_read(view="spans") -> style_regions[].index`
  (no matching, cannot misfire), or by `old` text when you have no map.
  The variant must already exist in the document (see `format-style-unavailable`).
- Refusals are self-diagnosing: `data.span_map`, `data.divergence` (where your
  text stopped matching + what the document says), `data.closest_spans`, and
  `data.fix` (a corrected, ready-to-send hunk). Apply `fix` verbatim instead of
  re-deriving the text.
- **Two editing intents, two tools**: `document_patch` = you know exactly
  where; `document_replace(find, replace, scope=…)` = unify every match of a
  rule (body/all/comments/part key/one paragraph id). Replace is atomic
  all-or-nothing: a match spanning a revision boundary fails the whole batch
  with a per-match plan. `expected_matches=N` fails closed on a count
  mismatch; zero matches is a success with `changed=false`.
- Search/replace hits carry `match_ref`, a version-bound address. Pass it
  straight through — `document_patch({"hunks": [{"match_ref": …, "new": …}]})`,
  `format_span(match_ref=…)` — instead of copying long `old` text. Stale refs
  fail closed (`match-ref-stale`).
- `document_state.revision_before/after` is returned by the editable-state
  mutations — `document_patch`, `document_replace`, `format_span`,
  `commit_sync` — so edits chain without re-reading (`patch A` ->
  `patch B(base_revision=<revision_after of A>)`). Other lanes may not carry
  it; read the current revision if in doubt.
- `engine_info().capabilities` = static engine manifest;
  `document_read(view="capabilities")` = what THIS document can do now. A
  closed lane's refusal carries `capability` + `fallback`, so never guess why.
- `format_span` needs a CLEAN draft (styles live in the committed AST):
  commit_sync (or revert) first, then format.
- `workdir_status` reports `draft_dirty` (`edit.md` vs canonical) separately
  from `version.dirty` (canonical tree vs `HEAD.tree`). `document_patch` first
  makes only the draft dirty; `format_span` and review decisions can make only
  the Version dirty. `commit_sync` syncs a dirty draft, then creates a Version
  only when canonical still differs from HEAD; clean/no-drift state is a no-op.
- MCP profiles keep tool selection small: set `DOCX2TYPED_MCP_PROFILE=editor`
  (27 tools) for ordinary editing plus save/history/structural transitions,
  `review` (27) when per-revision/comments and review collaboration are in
  play, and `full` for everything.
- **NEVER split hunks at style boundaries.** Style edges are not edit
  boundaries: `document_patch` accepts spans crossing style regions and the
  engine assigns ownership itself (`proportional-preserve`, flagged via
  `requires_style_review` + `style_note`). Only `replace_text` (advanced
  lane) demands one region — if it answers `cross-region-text`, resend the
  same edit through `document_patch` instead of splitting it.
- Only REVISION boundaries are hard: `document_read(view="spans", anchor=P…)`
  returns the paragraph's editable spans (copy one span's `text` verbatim as
  `old`; never cross a boundary offset). Refusals carry the same map in
  `data.span_map`, plus `data.divergence` (where your `old` stopped matching
  and what the document says there) and `data.closest_spans`.
- Batch patches report EVERY broken hunk at once (`patch-hunks-invalid`,
  `details.problems[]` with `hunk` index + paragraph + code); fix them all and
  resend in one call. A single broken hunk keeps its own code with
  `hunk #N` in the message.
- Table cell / content-control / part paragraph **text** is edited with
  `document_patch` like any prose (structure stays locked); container
  topology (rows, columns, merges, cell insert/delete) goes through the
  table/structural tools only.
- Editable-surface coverage (facade-qualified, real stdio): body, header,
  footer, footnote, endnote, text box, SDT text, table cell — all through
  `document_patch`; tracked-revision documents ride the same facade after
  the `track=true` reopen.
- `operation_id` may be omitted on every mutating tool (the server then
  generates a fresh id). NEVER reuse an id from any earlier call — success
  or failure; on `operation-id-reused`, retry with a fresh id (or omit).
- `document_patch`/`batch_edit` reject hunks whose old equals new
  (`patch-noop`) — do not re-apply an edit that already landed; check
  the current text first.
- In outline/search/plain rendering, `\u27e6?\u27e7` may denote a protected
  revision boundary (committed insert/move/delete edge). Text on the two
  sides is visually adjacent but is NOT one editable span; `old` must stay
  within one revision region (spanning spans fail with
  `edit-span-crosses-revision-boundary`).
- **NEVER modify a DOCX/ZIP/OOXML file outside docx2typed's mutation/build
  path.** Raw OOXML access (zipfile/lxml/direct XML editing) is read-only
  diagnostic access only. The source DOCX is immutable, and a fail-closed
  engine refusal is NEVER permission to bypass the engine.
- If `document_patch` and one documented recovery attempt both fail on the
  same paragraph, STOP and surface the blocker to the user. Never escalate
  to zipfile/lxml/raw OOXML mutation.
- Comment text (`comments.P*`) is technically patchable but is annotation
  content, not document prose: touch it ONLY when the user explicitly asks
  to edit a reviewer's comment text. Never let "polish the document"
  tasks silently rewrite comments; deleting a comment must go through
  `delete_comment` (entry + anchors + references), never a text replace.
- **Paragraph primitives (`list_paragraphs`, `get_paragraph`,
  `replace_text`, `batch_edit`, `insert_paragraph`, `delete_paragraph`)
  are the advanced fallback lane, not the default.** Use them only for
  explicitly requesting exact per-region style ownership, diagnosis, or
  recovery — cross-region rewrites belong to `document_patch`.
- Revision/comment/table/review-lane tools are entered only when the
  document contains those structures (revisions.json, comments, locked
  tables) — see Workflows 3–5.

## Human-facing review path

The agent owns installation, document execution, and delivery. The human uses
the browser console to inspect the continuous document, jump from the fixed
review rail to a revision or comment, accept/reject/defer a revision, add a
note, or select text for an agent patch.

The browser is a review and handoff surface, not a DOCX writer:

- `Export decisions` downloads a review decision file from a standalone page.
- `Send to agent` dispatches saved browser decisions and text-anchored patches
  into the server queue; it does not write the DOCX.
- The agent reads the queue, applies changes transactionally, refreshes the
  review snapshot, then builds and independently verifies a new DOCX.
- Comments remain by default. Deleting one requires the user's explicit
  instruction.

When a user asks for this flow, follow Playbook D in `composites.md` instead
of asking the user to edit `typed.md`, manage revision IDs, or install files
into a skill directory.

## The edit rules (apply to every editing workflow)

`typed.md` is a restricted typed source, not Markdown. Minimal document:

```text
<!--@typed schema="1" format="format.json" styles="styles.json" template="_template.docx" source="source.docx"-->

<!--@p id="P0" base="S1"-->
本发明涉及<span data-s="S2">生物医用材料</span>技术领域。

<!--@p id="P1" inherit="P0"-->
新增段落。

<!--@delete id="P2"-->
```

Rules:

- **Only text moves.** Keep the `@typed` header and every `@p` marker
  unchanged unless the operation is a paragraph insertion or deletion.
- Text inside `<span data-s="S2">…</span>` owns style `S2`; replace its
  words without touching the wrapper. Empty spans and adjacent same-style
  text merge automatically during parsing.
- New paragraph: `<!--@p id="P1" inherit="P0"-->` — inherit an existing
  paragraph, never invent a `base` style.
- Delete: `<!--@delete id="P2"-->` — never remove a marker and body
  silently (missing tombstone is a validation error).
- One paragraph = one logical source line. XML-sensitive text uses
  `&amp;` `&lt;` `&gt;`. No CommonMark, no generic HTML, no zero-width
  characters.
- Structural tokens (`<docx-inline …/>`, `<docx-anchor …/>`,
  `<docx-opaque …/>`) and revision containers are read-only. A change
  touching one: stop before `build` and report the paragraph.
- Content controls (`w:sdt`) expose their paragraphs as `S0.P0`-style ids
  and are editable like body text; the `sdtPr` structure replays byte-exact.
- Table cell paragraphs are `T0.R0.C0.P0`-style ids and editable like body
  text; table structure itself is changed only via `decide table-*`
  (Workflow 5), never by editing tokens.

## Workdir contract

`extract` creates one self-contained project; build/verify/decide consume it
as a unit — never combine sidecars from different documents.

| File | Purpose | Editable |
|---|---|---|
| `typed.md` | canonical typed source | yes |
| `edit.md` | span-free agent projection / patch input | via `edit sync` or MCP |
| `edit.state.json` | authoritative freshness binding | no |
| `format.json` | fingerprints, paragraph skeletons, token records | no |
| `styles.json` | content-addressed style registry | no |
| `_template.docx` | immutable source package | no |

## Gates (summary — full contract in `verification.md`)

- **clean gate**: `validate`, `build`, `verify` reject every non-clean edit
  state; there is no bypass flag.
- **verify is independent**: `verify` re-derives the baseline from the
  fingerprinted template and compares text, styles, tokens, protected XML
  regions, and every package part — it does not trust `build`.
- **byte fidelity**: a no-op build must be byte-identical to the input;
  untouched paragraphs replay raw bytes.
- **interop**: outputs must open in Microsoft Word without repair prompts and
  pass the Word/DOCX delivery check. LibreOffice checks are optional and
  non-gating.

## Agent setup and runtime

When this skill is invoked, use the host's normal skill manager and runtime.
If the user authorizes installation, follow `Installation.md` for the package,
MCP, and optional Tailscale setup. The user does not need to copy `SKILL.md`
or know the host's skill directory.

For a package installation, use the installed entry points:

```bash
docx2typed <command>
docx2typed mcp
docx2typed review WORKDIR --host 127.0.0.1 --port 8876
```

For a one-shot isolated command, use `uvx docx2typed <command>`. A source
checkout may use `python -m scripts <command>` only when the package is not
the intended runtime.

## Real-user session protocol

When an agent operates on behalf of a human, the agent owns setup and
execution while the human owns scope, review decisions, and final acceptance.
Keep implementation details behind the browser and the handoff summary.

1. **Intake** — identify the source DOCX, desired outcome, tracked/direct edit
   preference, comment-retention policy, and whether browser review is wanted.
2. **Set up** — when authorized, install or enable this skill and the package
   through the host's normal mechanisms; configure MCP only with permission.
3. **Protect the source** — copy the DOCX into a new workdir on a scratch
   volume; never edit or overwrite the user's original file.
4. **Baseline report** — extract once, open the workdir once, and report the
   document title, coverage, existing revisions/comments, and unsupported or
   ambiguous structures before changing text.
5. **Round loop** — state the current round's goal. For MCP, call
   `engine_info` → `workdir_open` before reading; make only region-scoped
   edits, preview and commit, and report exactly what changed and what remains.
6. **Human review** — open the browser console. The human selects revisions or
   comments, accepts/rejects/defers, or adds a source-anchored patch or note.
   `Send to agent` queues work; it is not a DOCX write.
7. **Continue** — read the review inbox and preflight, apply queued decisions
   or patches transactionally, preserve original comments, refresh the review
   surface, and report the new snapshot plus remaining queue.
8. **Delivery gate** — after the final round, build a new output DOCX, run
   independent verification, open it in Microsoft Word (and render through
   Word when a PDF is required), and return the output path with a compact
   evidence summary. LibreOffice checks are optional and non-gating.

Never call the document "finished" because the browser shows a final view or
because an event was sent. Finished means the delivery gate is green. If a
round is interrupted, resume from the persisted workdir/session snapshot and
describe the pending queue before writing.

Read `docs/rpr-reference.md` to translate rPr XML when planning style
regions.
