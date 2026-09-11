---
name: docx2typed
description: >
  Word DOCX text editing with locked formatting and structure (byte
  fidelity), plus a browser review console and human-to-agent handoff. Use
  when text, tracked revisions, comments, tables, content controls, or
  package parts must stay safe; the MCP server is the preferred Agent surface.
---

# docx2typed — Agent editing policy

The target is a Word-compatible DOCX whose untouched content remains byte
faithful. Keep the source DOCX immutable. Use the typed engine through the
installed MCP/CLI surface; raw OOXML is a read-only diagnostic view.

## Fast-path router

Choose the smallest route that matches the user's intent:

| User intent | First action | Next action | Do not add |
|---|---|---|---|
| One known phrase → another | `document_search(query)` once | `document_patch` with `match_ref` | whole-document read |
| Every occurrence of X → Y | `document_replace(find, replace, scope=...)` | inspect result, then commit | search + one patch per hit |
| The second/nth occurrence | one search, choose `occurrences[n]` | patch its `match_ref` | repeated narrower searches |
| Rewrite a section with missing context | windowed search/read once | patch the returned context | paragraph-by-paragraph reads |
| Change existing formatting | locate the exact span | `format_span(match_ref=...)` | typed markup edits |
| Several independent changes | collect non-overlapping hunks | one `document_patch` call | one mutation call per sentence |
| Restore a saved version | `history_list` only if the name is unclear | `history_restore(version)` | a new workdir or pointer rewind |
| Export an old version | `build_docx(version=...)` | verify the artifact | restore first |
| Revisions, comments, or topology | load the matching reference below | use the governed lane | guessing XML |

The normal edit loop is:

```text
locate → mutate
locate → mutate
...
commit once
```

Delivery is a separate loop:

```text
clean committed state → build once → verify once → Word check
```

## Session lifecycle

1. If a trusted workdir already exists for this logical DOCX, resume it.
   Create a new workdir only for a first import, an explicit fork, or a
   different source document.
2. Protect the source by copying it to scratch storage before the first
   `extract`. Never overwrite the user's original DOCX.
3. Call `engine_info` at session start, after a server restart, or when a
   requested capability is unclear. Do not call it before every edit.
4. Call `workdir_open` once for the session, choose `track=true` or
   `track=false` when the document is ambiguous, and use `workdir_status` when
   the state matters.
5. Keep the same workspace across rounds. Chain each mutation's
   `document_state.revision_after` into the next `base_revision`; re-read only
   after another writer changes the workspace or a refusal provides no usable
   recovery.

## Edit decisions

- Use `document_search` for an addressable phrase or occurrence. Pass its
  `match_ref` directly to `document_patch` or `format_span`.
- Use `document_replace` for a deliberate global rule. Use
  `expected_matches` when the count is part of the request; zero matches is a
  successful no-op.
- Use `document_patch` for one or more exact replacements, insertions, or
  deletions. Let the facade assign style ownership across style spans.
- Use `format_span` only for an existing style variant. If the draft is dirty,
  commit or revert it before formatting.
- A single exact patch with no normalization or style warning may commit
  directly. Preview first for multi-hunk edits, broad replacements, normalized
  matches, style review, structural transitions, or an explicit user request.
- Use a fresh operation ID, or omit it and let the server create one. A retry
  after `operation-id-reused` gets a new ID.

## Dirty state and save

Treat these as separate facts:

| State | Meaning |
|---|---|
| `draft_dirty` | `edit.md` projection differs from canonical typed state |
| `version_dirty` | canonical tree differs from `HEAD.tree` |
| `publish_pending` | an output/evidence publication still needs completion |

The save boundary is:

| `draft_dirty` | `version_dirty` | `publish_pending` | `commit_sync` |
|---:|---:|---:|---|
| false | false | false | true no-op |
| false | false | true | publish current snapshot; no Version |
| true | any | any | sync draft, compare canonical tree with HEAD |
| false | true | any | create a Version |

`document_patch` normally starts only `draft_dirty`. Canonical writers such as
`format_span` or review decisions can leave the draft clean while making
`version_dirty` true. `commit_sync` synchronizes first and creates an ordinary
Version only when the canonical tree differs from HEAD.

## Savepoints, restore, and export

- `commit_sync` is the ordinary save boundary. Multiple draft mutations can
  become one Version; a fully clean save is a no-op.
- `label` describes a Version. `pin=true` independently protects it from
  retention; do not use labels as implicit pins.
- `history_restore` creates a new forward Version from an old tree. It never
  rewinds HEAD, deletes later history, or silently falls back from a refused
  selective restore.
- Selective restore v1 is restricted to dependency-free, zero-token
  paragraphs. Coupled revision/comment/range/SDT/table/text-box state refuses
  with `partial-restore-needs-dependent-state`.
- `build_docx(version=...)` exports a historical state without moving HEAD.
- `history_gc` may trim content while retaining Version metadata. A trimmed
  Version remains visible but restore/export fail with `version-trimmed`.

## Hard safety invariants

1. Source DOCX, template package, styles, anchors, relationships, and opaque
   structures remain protected unless the selected governed operation owns them.
2. Revision boundaries are hard edit boundaries. Narrow the edit or choose the
   revision decision; do not cross them by rewriting raw XML.
3. Comments remain unless the user explicitly requests `delete_comment`.
4. A refusal is a contract boundary. Follow its structured recovery data once;
   if the same paragraph refuses again, report the blocker.
5. Build and verify only from a clean committed state. Verify independently
   re-derives the template baseline and package invariants.
6. Delivery targets Microsoft Word / official DOCX-OOXML: require a Word open
   without repair prompts, and Word rendering when a PDF is required.

## Progressive references

Load only the branch-specific reference needed for the current request:

| Branch | Reference |
|---|---|
| Default patch/search/format route | [`references/editing.md`](references/editing.md) |
| Save, history, restore, export, or GC | [`references/history.md`](references/history.md) |
| Tracked revisions, comments, or human review | [`references/review.md`](references/review.md) |
| Tables, SDTs, text boxes, parts, or protected structure | [`references/structure.md`](references/structure.md) |
| A refusal, stale view, or recovery decision | [`references/diagnostics.md`](references/diagnostics.md) |
| Typed grammar or engine diagnosis explicitly requested | [`references/advanced-typed-mode.md`](references/advanced-typed-mode.md) |
| Installation or browser-console administration | [`references/admin.md`](references/admin.md) |

Use [`capabilities.md`](capabilities.md) for exact schemas, exit contracts,
and tool names; [`composites.md`](composites.md) for long workflows; and
[`verification.md`](verification.md) for the final acceptance gates. Their
machine-facing tool and verification contracts outrank prose summaries here.

## Human review and delivery

The browser console is a review and handoff surface, not a DOCX writer.
Humans decide revision/comment actions; the Agent applies queued work
transactionally and reports the resulting snapshot.

Before calling a document finished:

```text
clean workdir
→ build_docx
→ verify_output
→ Microsoft Word open without repair
→ Word PDF render when requested
```

Never present a browser view, queued event, or partial build as the final
document. If a session is interrupted, resume the persisted workdir and report
the pending queue before writing.
