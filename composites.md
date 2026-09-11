# composites.md — workflows (工作流说明)

Molecules chain atoms into scoped tasks; every workflow ends on the shared
gates. Atoms: [`capabilities.md`](capabilities.md). Gates:
[`verification.md`](verification.md). Hub: [`SKILL.md`](SKILL.md).

Each workflow lists ordered steps with a **completion criterion** — the
checkable condition that tells you the workflow is done. Do not skip to the
next step until the criterion holds.

---

## Workflow 1 — Clean text edit (普通文本编辑)

Edit ordinary prose; formatting, structure, anchors stay locked.

1. `extract <input.docx> -o <workdir>` — creates typed.md + edit.md + sidecars.
2. Plan: `view <workdir> --mode clean` ONCE for the full document; use
   `get_paragraph`/`regions.md` only for paragraphs you are about to change
   or that carry complex structure. Do not read the document paragraph by
   paragraph.
   Content-control paragraphs (`S0.P0`) and table-cell paragraphs
   (`T0.R0.C0.P0`) are editable exactly like body text.
3. Edit. Two allowed surfaces, never both at once:
   - **edit.md draft**: rewrite prose (single-region edits inherit
     exactly; cross-region rewrites follow the explicit
     `proportional-preserve` policy with a recorded warning), then
     `edit sync <workdir>`; or
   - **MCP draft (default)**: `workdir_open` → `document_read` /
     `document_search` → `document_patch` (hunks or unified diff, one call
     per editing intention) → `diff_preview` → `commit_sync`.
     Paragraph primitives (`get_paragraph`/`batch_edit`/…) are the
     advanced fallback: only for explicitly requesting exact per-region
     style ownership, post-refusal region diagnosis, or recovery.
     Every mutating MCP call runs its preflight automatically: paragraph-local
     edits block only intersecting queued human patches; commit, decision, and
     table-wide operations use the conservative document-wide gate. Failures
     are structured Result envelopes; follow `data.recovery` and the
     diagnostic `next_actions`.
   Raw `typed.md` edits are allowed but must be followed by
   `edit refresh <workdir>`.
4. `build <workdir> -o <output.docx>` — fails closed on any rule violation.
5. `verify <workdir> <output.docx>` — independent re-derivation.
6. Interop: open the output in Microsoft Word; when a rendered artifact is
   required, export it through Word (see `verification.md`).

**Completion criterion**: verify PASS, Microsoft Word opens the output without
repair prompts, and every intended text change is present with its original style;
nothing else in the document changed.

## Workflow 2 — Tracked edit (修订式编辑)

Same as Workflow 1, but insertions/deletions/replacements become real
`w:ins`/`w:del` revisions.

1. `extract` the source. If the source carries pending revisions but
   `settings.xml` has no `w:trackChanges` (or vice versa), extraction
   succeeds but revision-generating calls are refused until you choose a
   mode — pass `track=true` (or `--track`) to open in track mode.
2. Steps 2–4 of Workflow 1 with `edit sync --track` (or MCP `workdir_open`
   with `track=true`). Replace = delete + insert revision.
3. `build` + `verify` + Word interoperability check as in Workflow 1.

**Completion criterion**: the output carries the new revisions with session
author/date; existing revisions are untouched; verify PASS.

## Workflow 3 — Revision decisions (修订决策)

Accept or reject tracked revisions, singly or wholesale.

1. `extract` → read `revisions.json` (inventory with `revision_key`s).
2. Single decision:
   `decide accept <key> --workdir <wd> [--fingerprint <fp>]` (or `reject`,
   `reinsert`). The typed AST mutates and publishes transactionally; the
   fingerprint defends against stale keys.
3. Wholesale settlement:
   `decide accept-all --workdir <wd> --output after.docx --workdir-out <wd2>`
   (or `reject-all`). Byte-level settlement of every revision in every part;
   produces a new DOCX + fresh clean-baseline workdir; the source workdir is
   never mutated. Paragraph-mark revisions settle too (the paragraph itself
   is never removed by settlement).
4. `verify <wd2> after.docx` + Microsoft Word check.

**Completion criterion**: `revisions.json` in the new baseline lists 0
pending revisions (for accept-all/reject-all) or the decided key is gone and
its siblings remain; verify PASS; `after.docx` opens cleanly.

## Workflow 4 — Comment review (批注返修)

Comments are the teacher's instructions: work through their content, then
LEAVE them in place. Comment deletion is the teacher's own decision — an
agent must not delete a comment merely because edits were made. Only delete
when the user explicitly instructs it (or confirms the teacher resolved it).

1. `list_comments` (MCP) — inventory with id, author, date, text, and the
   body paragraphs carrying each anchor. This is the first-class entry
   point; do not infer comments from `comments.P0`-style paragraph ids.
2. For every comment, decide what content change satisfies it; make those
   changes in track mode (Workflow 2). A broad comment ("全文润色") is
   satisfied only by a complete, checklist-based pass — partial edits are
   NOT completion.
3. Leave the comment untouched. `delete_comment` exists for the user; an
   agent calls it only on explicit instruction.
4. `build` + `verify` (the structured evidence includes surviving comment
   ids) + Microsoft Word check.

**Completion criterion**: for every comment, the requested editing task
has either (a) corresponding tracked edits, or (b) a reported reason no
edit was made. The original comment remains present (id/author/date/text/
anchor unchanged) unless the user explicitly ordered deletion; verify PASS.
docx2typed does not vouch for content correctness — whether the teacher is
satisfied is the upper-layer reviewer's call.

## Workflow 5 — Table structure operations (表格结构操作)

Row/column insert/delete and cell merge/split — structure only, cell text
never rewritten.

1. `extract` → `view --mode raw` to learn table ordinals (`T0`, `T1`, …).
2. `decide table-insert-row T0 --workdir <wd> --args '<after>' --output <out.docx> --workdir-out <wd2>`
   — same shape for `table-delete-row`, `table-insert-col`, `table-delete-col`;
   `table-merge-cells T0 --args '<row> <col> <span>'` and
   `table-split-cells` for cells (0-based indices; `span` ≥ 2).
3. Every table op produces a new DOCX + clean-baseline workdir; the source
   workdir is never mutated. Inserted rows/columns are synthesized empty
   (structure preserved, no text copied).

> **Table ref semantics**: `T0`/`T1` in `decide table-*` and MCP table tools are
> BODY-LEVEL ordinals — nested tables are excluded from the numbering (a
> document whose typed.md shows `T0/T1/T2` with a nested `T1` addresses the
> second body table as `T1`). Read `view --mode raw` for body tables.
> Merge is fail-closed: spanned cells carrying text refuse with
> `merge-would-discard-content` unless `--discard-content` (CLI) or
> `discard_content=true` (MCP) is explicit; the first cell's content is
> always kept. Split restores empty cells with gridSpan cleared.
4. `verify <wd2> <out.docx>` + Microsoft Word check.

**Completion criterion**: the new baseline has the expected row/col/cell
counts, inserted structure is empty (never duplicates existing text), all
original cell content is byte-intact, verify PASS, and Microsoft Word opens
without repair prompts.

## Workflow 6 — Unicode normalization audit (归一化审计)

Convert Unicode superscript/subscript code points to Word `vertAlign`
styles under explicit governance. Classification is a suggestion, never a
decision.

1. `audit scan <workdir> -o <scan.json>` — read-only; show candidates to the
   user before changing anything.
2. Build the policy from the scan artifact (copy snapshot fields,
   `scan_artifact_sha256`, each `occurrence_id` and `candidate_fingerprint`
   exactly — never invent hashes). Skeleton:

```json
{
  "schema": "vertical-normalization-policy-2",
  "status": "draft",
  "approval_requirement": "human",
  "audit_schema": "vertical-normalization-audit-2",
  "scanner_contract_version": 1,
  "project_id": "<scan.snapshot.project_id>",
  "baseline_sha256": "<scan.snapshot.baseline_sha256>",
  "draft_snapshot_sha256": "<scan.snapshot.draft_snapshot_sha256>",
  "model_sha256": "<scan.snapshot.model_sha256>",
  "catalog_sha256": "<scan.snapshot.catalog_sha256>",
  "scan_artifact_sha256": "<scan.scan_artifact_sha256>",
  "decisions": {
    "<candidate.occurrence_id>": {
      "decision": "convert|preserve",
      "actor": "<reviewer>",
      "candidate_fingerprint": "<candidate.candidate_fingerprint>",
      "rationale": "<required for risky classifications>"
    }
  }
}
```

3. Decide every occurrence exactly once (`convert` only for `approved`
   classifications with a non-empty `proposed_target`; ambiguous/manual/
   unsupported/non-reversible/conflicting require a rationale; when
   uncertain, preserve and ask). Keep status `draft`/`reviewed` while
   pending.
4. Approve: set `status="approved"` with an explicit approval object
   (`approved_by`, `approval_time`). Use `approval_requirement="human"` by
   default; `self` only when the user explicitly authorizes agent
   self-approval — otherwise stop and request approval.
5. `audit apply <workdir> --scan <scan.json> --policy <policy.json> -o <normalized.docx> --workdir-out <normalized-workdir>`
   — writes a NEW DOCX/workdir + `normalization.audit.json`; the source
   workdir is never mutated. Stale workdir/model/catalog/scanner/scan
   bindings fail before transformation.
6. `verify` + Microsoft Word check.

**Completion criterion**: every scan candidate has a decision, the policy is
`approved` with a valid approval record, `audit apply` succeeded, the
normalized DOCX opens cleanly, and the source workdir is unchanged.

## Workflow 7 — Content-control text edit (内容控件文本)

`w:sdt` content controls expose their paragraphs (`S0.P0`, …) in the
editable surface. Edit them exactly like Workflow 1 — the `sdtPr` structure
(alias, lock, tag) replays byte-exact; only text inside `sdtContent` moves.
Structural edits to the control itself (adding/removing an sdt) are out of
scope.

**Completion criterion**: control text changed, `<w:sdtPr>` and its
properties byte-identical to the template, verify PASS.

---

# Playbooks (compounds — end-to-end scenarios)

Compounds chain workflows; you (the human) stay in the driver's seat. Each
playbook ends on the full gate set in `verification.md`.

## Playbook A — Finalize a manuscript (定稿)

Settle every revision and clear comments, then export and prove the result.

1. Workflow 3: `decide accept-all --workdir <wd> --output after.docx --workdir-out <wd2>`.
2. Workflow 4: if the user wants comments gone too, verify the new baseline
   carries none (`revisions.json` / comments inventory) or delete
   individually.
3. Workflow 1 tail: `build <wd2> -o final.docx` + `verify` + Microsoft Word check.
4. Report: settled count, remaining revisions = 0, comments = 0, verify
   PASS, Word interoperability check clean.

**Completion criterion**: every gate green, and the counts you report match
what the inventory files say.

## Playbook B — Tracked revision (修订返修)

Revise a document the user will review in Word with changes visible.

1. Workflow 2 end-to-end with `track=true` (choose session author).
2. Deliver the built DOCX; tell the user the author/date stamped on the new
   revisions and that pre-existing revisions are untouched.

**Completion criterion**: new revisions carry the session identity, old
revisions intact, verify PASS.

## Playbook C — Agent editing session (MCP 会话)

Drive the whole edit loop through the MCP server.

1. `workdir_open` → `workdir_status` (clean required).
2. `document_read` (whole; `view="outline"` first for large documents) and
   `document_search` for locating targets — whole blocks with
   `prev_id`/`next_id` anchors come back, no per-paragraph reads.
3. `document_patch` (hunks or unified diff, `base_revision` from the
   read/search token) → `diff_preview` → `commit_sync`.
4. `build_docx` → `verify_output` → Microsoft Word check.
   Fallback only: `get_paragraph` + `batch_edit` for explicitly
   requesting exact per-region style ownership or refusal diagnosis.

**Completion criterion**: committed state is clean, output verified, every
intended change present with its original style.


## Playbook D — Human-led multi-round session (真实用户全流程)

This is the default human experience. The agent owns execution; the human
owns scope, review decisions, and final acceptance.

1. **Open the agent**: collect the DOCX, desired result, tracked/direct edit
   preference, and comment-retention policy. Repeat the plan in one sentence.
2. **Create the baseline**: copy the source to a new scratch workdir, run
   `extract`, validate it, and report the source fingerprint and initial
   inventory. Do not modify the original DOCX.
3. **Run round 1**: open the workdir once, read the full clean projection once,
   inspect only target paragraphs/regions, make the smallest valid edits, and
   commit them. Report changed paragraph IDs, edit mode, and unresolved items.
4. **Human review**: open the review console. The human selects revisions or
   source comments, accepts/rejects/defers, or adds a source-anchored patch or
   note. The console's “send to agent” action only queues work; it is not a
   DOCX write.
5. **Run round N**: the agent reads the review inbox and preflight, applies
   queued decisions/patches transactionally, preserves original comments,
   refreshes the review surface, and reports the new snapshot plus remaining
   queue. Repeat step 4 until the human's requested scope is satisfied.
6. **Deliver**: require a clean workdir, build a new DOCX, run independent
   `verify`, open it in Microsoft Word, and render through Word when a PDF is
   required. Return the output path with a compact evidence summary. LibreOffice
   checks are optional and non-gating.
   round loop and do not present a partial DOCX as final.

**Completion criterion**: the human can tell what the agent is doing, what is
waiting for them, and what remains before delivery; every round is resumable
from the persisted workdir/session state; final output passes verify and
interop with the promised comment/revision policy.