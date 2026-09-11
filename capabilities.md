# capabilities.md — atoms (工具使用说明)

Atoms are single commands, near-deterministic, with no dependencies. Two
surfaces: the CLI and the MCP server — same engine, same gates. Workflows
that compose them: [`composites.md`](composites.md). Shared acceptance
contract: [`verification.md`](verification.md).

## Workdir lifecycle atoms (CLI)

| Atom | Purpose | Exit contract |
|---|---|---|
| `python -m docx2typed extract <input.docx> -o <workdir>` | Create a typed workdir from a DOCX (never mutates the source) | 0 + workdir; source/template fingerprints recorded |
| `python -m docx2typed validate <workdir>` | Grammar/skeleton/style/template integrity check | 0 only when workdir is valid AND edit state is clean |
| `python -m docx2typed view <workdir> --mode clean` | Read-only continuous-prose projection | stdout prose; 0 |
| `python -m docx2typed view <workdir> --mode style` | Read-only diagnostic projection showing style regions | stdout with style labels; 0 |
| `python -m docx2typed view <workdir> --mode raw` | Read-only projection with all typed tokens visible | stdout typed markup; 0 |

## Edit atoms (CLI)

| Atom | Purpose | Exit contract |
|---|---|---|
| `python -m docx2typed edit status <workdir>` | Freshness: `clean` / `dirty` / `stale-clean` / `conflict` | 0 for all four states |
| `python -m docx2typed edit refresh <workdir> [--init] [--discard]` | Regenerate `edit.md` from `typed.md` after a raw typed change; `--init` for legacy workdirs, `--discard` replaces a dirty draft | 0; every non-clean build gate uses the sidecar, not the header |
| `python -m docx2typed edit sync <workdir>` | Apply an edited `edit.md` draft to the canonical typed AST: unchanged text keeps style, single-region rewrites inherit exactly, cross-region rewrites follow the explicit `proportional-preserve` policy (reason + warning recorded), insertions inherit caret context | 0 + new canonical state; every hunk recorded in `edit.state.json.run.json` |

Before syncing, read `regions.md` in the workdir — it lists style regions
with indices and auto-updates after every edit. Plan region-scoped edits
from it.

## Decision atoms (CLI)

`python -m docx2typed decide <action> --workdir <workdir> [options]`

| Action | Purpose | Key options |
|---|---|---|
| `accept <revision_key>` / `reject <revision_key>` / `reinsert <revision_key>` | Decide ONE tracked revision (key: `part|kind|w:id|fingerprint` from `revisions.json`); mutates the typed AST, publishes transactionally | `--fingerprint` defensive check, `--author`, `--text` |
| `apply --workdir <workdir> --file <review-decisions.json>` | Apply a review-console decisions export in one pass; `accept`/`reject` entries publish one by one, `defer`/comment-only entries are skipped, per-entry failures are reported without rolling back published entries (exit 1 if any failed) | `--file`; schema `docx2typed-review-decisions-1` |
| `accept-all` / `reject-all` | Settle every tracked revision at byte level; builds a new DOCX and re-extracts a fresh clean-baseline workdir | `--output <after.docx>`, `--workdir-out <new-wd>` — source workdir never mutated |
| `comment-delete <id>` | Delete one Word comment: `comments.xml` entry, all `commentRangeStart/End` anchors, `commentReference`s; other comments untouched | `--workdir`; publishes in place |
| `table-insert-row <T0>` / `table-delete-row <T0>` / `table-insert-col <T0>` / `table-delete-col <T0>` / `table-merge-cells <T0>` / `table-split-cells <T0>` | Row/col insert/delete, horizontal merge (gridSpan), split; table refs are `T0`, `T1`, … (see `view --mode raw`) | `--args '<index> [<index> <span>]'` (0-based), `--output`, `--workdir-out`; new clean baseline, source untouched |

## Normalization atoms (CLI)

| Atom | Purpose | Exit contract |
|---|---|---|
| `python -m docx2typed audit scan <workdir> -o <scan.json>` | Read-only: hash-bound candidate artifact for Unicode superscript/subscript vertical candidates | 0 + scan.json + run evidence; never mutates |
| `python -m docx2typed audit apply <workdir> --scan <scan.json> --policy <policy.json> -o <normalized.docx> --workdir-out <normalized-workdir>` | Apply an approved policy to a NEW DOCX + NEW workdir with `normalization.audit.json`; stale bindings fail before transform | 0 only with complete approved policy and matching fingerprints |
| `python -m docx2typed normalize <workdir> --legacy-policy-1 …` | Unaudited compatibility path; emits `governance_status="legacy-unaudited"` | Use `audit scan/apply` when approval matters |

## MCP atoms (server: `python -m docx2typed.mcp_server`)
All mutating MCP tools accept an optional `operation_id`. If omitted, the
server generates one and returns it in the Result envelope. Pass the returned
ID on a retry when byte-exact replay is required.

Session tools:

| Tool | Purpose |
|---|---|
| `engine_info()` | Protocol descriptor, schema/capability hashes, and the complete MCP tool list; call before opening a workdir. |
| `workdir_open(workdir, author?, track?)` | Open the session document; validates, reports freshness + effective edit mode. |
| `workdir_status()` | Freshness state of the opened workdir |
| `list_comments()` | Comment inventory: id, author, date, text, anchor paragraphs |
| `get_comment(comment_id)` | One comment with its anchors |
| `revert()` | Discard the uncommitted draft, regenerate from canonical typed source |

Document surface (default editing path):

| Tool | Purpose |
|---|---|
| `document_read(anchor?, before?, after?, view?)` | Editable projection as one virtual text file: whole / outline / windowed; carries the opaque `revision` token |
| `document_search(query, context_chars?)` | Full-text matches as whole blocks with `prev_id`/`next_id` anchors |
| `document_patch(hunks | diff, base_revision?)` | One atomic batch: non-overlapping replaces (multi-hunk per paragraph), inserts, deletes — or a unified diff against the projection |

Paragraph primitives (advanced fallback — diagnosis, same-paragraph
exact per-region style ownership, diagnosis, recovery; not the default
editing path):

| Tool | Purpose |
|---|---|
| `list_paragraphs()` | Draft paragraphs: id, visible-text summary, token count, deletions |
| `get_paragraph(paragraph_id)` | Draft text + style regions (region-scoped editing basis) |
| `diff_preview()` | Dry-run of `commit_sync`: hunks with style ownership, warnings |

Edit tools (region-scoped, zero guessing):

| Tool | Purpose |
|---|---|
| `replace_text(paragraph_id, old, new)` | Replace exactly one occurrence in one style region |
| `batch_edit(paragraph_id, edits)` | Multi-region edit, atomic, immediate |
| `insert_paragraph(after_id, text, inherit?)` | Insert a new paragraph in the draft |
| `delete_paragraph(paragraph_id)` | Mark a paragraph deleted (protected structure rejected at commit) |
| `commit_sync(label?, pin?)` | Save boundary: sync a dirty draft, create a Version only when canonical differs from HEAD; `label` is metadata and `pin` is independent retention policy |

Build/verify tools:

| Tool | Purpose |
|---|---|
| `build_docx(output?)` | Build the DOCX from the committed workdir (clean state required) |
| `verify_output(output)` | Independently verify a built DOCX against the workdir |

Decision tools:

| Tool | Purpose |
|---|---|
| `accept_revision(revision_key, expected_fingerprint)` / `reject_revision(revision_key, expected_fingerprint)` / `reinsert_deleted_text(revision_key, expected_fingerprint)` | One revision decision, fingerprint-defended |
| `decide_all(action, output, workdir_out)` | accept-all / reject-all byte settlement + new baseline |
| `delete_comment(comment_id)` | Delete one comment (entry + anchors + references) — user-instructed only; comments are kept during agent review |

Collaboration tools:

| Tool | Purpose |
|---|---|
| `review_state()` / `review_preflight()` | Read `review_base`, `current_snapshot`, `staged_snapshot`, drift, and queued human work before an Agent write |
| `review_inbox(include_acknowledged=False)` / `review_ack(event_ids)` | Consume the summary-first review queue and acknowledge events idempotently |
| `review_apply_patch(event_id)` / `review_apply_batch(batch_id)` | Apply a semantic human patch batch through the typed edit seam; anchors, fingerprints, style regions, parent snapshots, and overlaps fail closed |
| `review_settlement_plan(event_ids?)` / `review_settle(event_ids?)` | Inspect or atomically settle mixed accept/reject/defer decisions; deferred items carry forward to the next review base |
| `review_external_preflight(expected_parent_snapshot, operation?, operation_id?)` | Issue an idempotent CAS guard before an external import or rollback writer |

Table tools:

| Tool | Purpose |
|---|---|
| `table_insert_row(table_ref, after, output, workdir_out)` | Insert empty row after `after` (0-based) |
| `table_delete_row(table_ref, row, output, workdir_out)` | Delete a row |
| `table_insert_col(table_ref, after, output, workdir_out)` | Insert empty column after `after` in every row |
| `table_delete_col(table_ref, col, output, workdir_out)` | Delete a column from every row |
| `table_merge_cells(table_ref, row, col, span, output, workdir_out, discard_content=False)` | Merge `span` cells horizontally via gridSpan; fail-closed: spanned cells with text refuse (`merge-would-discard-content`) unless `discard_content=true` |
| `table_split_cells(table_ref, row, col, span, output, workdir_out)` | Split one cell into `span` cells |

All table tools produce a new DOCX + clean-baseline workdir; the source
workdir is never mutated.

## Files that act as tools

- `regions.md` — style regions with indices (read to plan region-scoped
  edits; auto-updated after every edit).
- `revisions.json` / `revisions.md` — read-only tracked-revision inventory
  (type/author/date/text/location/editable); the source of `revision_key`s.
- `edit.state.json.run.json` — run evidence for extract/refresh/sync.
- `docs/rpr-reference.md` — rPr XML → style translation dictionary.
