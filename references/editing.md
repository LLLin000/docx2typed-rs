# Editing reference

Load this file when a request needs exact edit routing, format handling, or a
structured recovery. The tool schema and exit contracts remain authoritative in
`../capabilities.md`; delivery gates remain in `../verification.md`.

## Route by intent

| Request shape | Route | Keep the loop tight |
|---|---|---|
| One known phrase → another | `document_search` once, then `document_patch` with the returned `match_ref` | no whole-document read; no paragraph primitive |
| Every occurrence of a rule | `document_replace(find, replace, scope=...)` | do not search and patch each occurrence |
| A numbered occurrence | one `document_search`, choose `occurrences[n]`, then patch its `match_ref` | do not repeat narrower searches |
| Rewrite with missing context | one windowed `document_search` or `document_read`, then patch | carry the returned revision forward |
| Existing formatting change | locate the exact span, then `format_span` | do not edit typed markup |
| Several independent edits | one `document_patch` with non-overlapping hunks | commit once after the intention is complete |

`match_ref` is the preferred address. Use `old` only when the caller has no
fresh reference. Pass `document_state.revision_after` as the next
`base_revision`; a fresh read is only needed after another writer changes the
workspace or a refusal gives no usable recovery data.

## Session loop

1. Resume the trusted workdir for this logical document. Extract only for a
   first import, an explicit fork, or a different source DOCX.
2. Open the workdir once per session. Call `engine_info` only at session start,
   after a server restart, or when a capability is unclear.
3. Locate and mutate in the same round. A normal edit round is:

   ```text
   locate → mutate
   locate → mutate
   ...
   commit once
   ```

4. Use `diff_preview` before commit for multi-hunk edits, broad replacements,
   normalized matches, style review, structural transitions, or an explicit
   user request. A single exact patch with no warning may commit directly.
5. Delivery is a separate loop: `build_docx` once, `verify_output` once, then
   Microsoft Word interoperability when the output is being delivered.

## State contract

`workdir_status` exposes separate facts:

- `draft_dirty`: the projection differs from canonical state;
- `version_dirty`: canonical state differs from `HEAD.tree`;
- `publish_pending`: an output/evidence publication is pending.

`document_patch` normally starts with `draft_dirty=true` while
`version_dirty` remains false until that draft is synchronized. Canonical-only
writers such as `format_span` or a review decision can make `version_dirty=true`
with a clean draft. `commit_sync` synchronizes first, then creates a Version
only when the canonical tree differs from HEAD. Fully clean state is a no-op.

## Hard boundaries

Revision boundaries are hard edit boundaries. Style boundaries are not: let
`document_patch` assign ownership and report a style-review warning. A refusal
is a result, not permission to bypass the typed seam; use its `data.fix`,
`span_map`, `divergence`, `closest_spans`, `capability`, or `fallback`.

Text in table cells, headers, footers, footnotes, endnotes, text boxes, and
content controls uses the same facade. Container topology is a separate
structural lane. See `structure.md` for its guardrails.
