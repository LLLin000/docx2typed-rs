# capabilities.md — Rust CLI and MCP atoms

The production surface is the installed Rust binary. Install from crates.io
(all seven crates published, `0.2.0`):

```text
cargo install docx2typed --version 0.2.0
```

Commands:

```text
docx2typed <command>
docx2typed mcp
```

The Python package is an offline reference/oracle only. These atoms describe
what the Rust binary actually accepts; `--json` may appear anywhere in a CLI
invocation and returns a `docx2typed-result-1` envelope.

## CLI lifecycle atoms

| Atom | Purpose | Exit contract |
|---|---|---|
| `docx2typed extract INPUT.docx -o WORKDIR --json` | Create a typed workdir without mutating the source | 0 + clean workdir |
| `docx2typed inspect WORKDIR --json` | Read-only workdir classification and asset inventory | 0 for a readable classification |
| `docx2typed enumerate SOURCE --json` | Enumerate recursive prose leaves from a DOCX or typed workdir | 0 + leaf paths |
| `docx2typed migrate SOURCE --out TARGET --operation-id ID --json` | Copy/migrate a schema-1 workdir with explicit retry identity | 0 + target workdir |
| `docx2typed store-state WORKDIR --json` | Read store generations and mutation state | 0 + state payload |
| `docx2typed build WORKDIR -o OUTPUT.docx --json` | Build a new DOCX from a clean workdir | 0 only on successful publish |
| `docx2typed verify WORKDIR OUTPUT.docx --json` | Independently verify the output against the workdir | 0 only when every check passes |

`build` and `verify` never overwrite the source DOCX. Keep the input workdir,
output DOCX, and any `--workdir-out` baseline separate.

## Text edit atoms

```text
docx2typed edit text WORKDIR LEAF OLD NEW --json
docx2typed edit WORKDIR --json
```

`LEAF` is a leaf path returned by `enumerate`, such as `P0.0` or
`T0.R1.C1.P0.0`. `edit text` performs one island-local edit and commits a new
store generation. Ambiguous text, opaque structure, stale state, and cross-island
rewrites fail closed. The optional `--operation-id ID` makes retries explicit.

When using MCP, the equivalent region-scoped loop is:

```text
workdir_open → list_paragraphs/get_paragraph
→ replace_text or batch_edit → diff_preview → commit_sync
```

## Paragraph structure atoms

```text
insert_paragraph after_id=ID text=TEXT [inherit=ID]
delete_paragraph paragraph_id=ID
```

`insert_paragraph` synthesises a new `<w:p>` from the `inherit` paragraph
(its opening tag with ephemeral attributes stripped, its `pPr`, and its run
properties), spliced before the next surviving baseline paragraph;
`delete_paragraph` removes one. Both commit through `commit_sync` into
`structure.json` and build into a real DOCX. The `inherit` and the deleted
paragraph must be plain body prose: section-bearing paragraphs, tracked
revisions, anchors, tables, and opaque interiors refuse rather than silently
drop content.

## Revision atoms

```text
docx2typed revisions list SOURCE --json
docx2typed revisions view SOURCE accept --json
docx2typed revisions view SOURCE reject --json
```

`SOURCE` is a DOCX or typed workdir. The inventory contains the authoritative
`revision_key` and fingerprint. Apply one decision with:

```text
docx2typed decide accept REVISION_KEY --workdir WORKDIR \
  --fingerprint FINGERPRINT --json
```

The `decide` action set includes `accept`, `reject`, `reinsert`, `accept-all`,
`reject-all`, and the table operations below. Wholesale settlement requires:

```text
--output OUTPUT.docx --workdir-out NEW_WORKDIR
```

A decision never silently accepts a stale revision key. Failure leaves the
workdir bytes unchanged.

## Comment atoms

```text
docx2typed comment list WORKDIR --json
docx2typed comment delete WORKDIR COMMENT_ID --json
```

Comments remain by default. Delete only on explicit user instruction. Deletion
removes the comment entry and its document anchors while preserving other
comments and unrelated package parts.

## Table atoms

All table actions use body-level refs (`T0`, `T1`, …) and 0-based indices:

```text
docx2typed decide table-insert-row T0 --workdir WORKDIR \
  --args "AFTER" --output OUTPUT.docx --workdir-out NEW_WORKDIR --json

docx2typed decide table-delete-row T0 --workdir WORKDIR \
  --args "ROW" --output OUTPUT.docx --workdir-out NEW_WORKDIR --json

docx2typed decide table-insert-col T0 --workdir WORKDIR \
  --args "AFTER" --output OUTPUT.docx --workdir-out NEW_WORKDIR --json

docx2typed decide table-delete-col T0 --workdir WORKDIR \
  --args "COL" --output OUTPUT.docx --workdir-out NEW_WORKDIR --json

docx2typed decide table-merge-cells T0 --workdir WORKDIR \
  --args "ROW COL SPAN" --output OUTPUT.docx --workdir-out NEW_WORKDIR --json

docx2typed decide table-split-cells T0 --workdir WORKDIR \
  --args "ROW COL SPAN" --output OUTPUT.docx --workdir-out NEW_WORKDIR --json
```

Every table operation produces a new DOCX and clean workdir. It never rewrites
cell text. Merge refuses `merge-would-discard-content` unless
`--discard-content` is explicit; the first cell's content is retained.

## Unicode audit atom

```text
docx2typed audit SOURCE --json
docx2typed audit SOURCE --catalog CATALOG.json --json
```

The audit is read-only. It reports Unicode superscript/subscript candidates;
it is not an unaudited normalize command and does not mutate the workdir.
Human policy and any application step belong in the governing workflow.

## Review server atom

```text
docx2typed review WORKDIR --host 127.0.0.1 --port 8876
```

This starts the Rust single-session browser review server. The browser queues
human decisions and patches; it does not write the DOCX. The Rust binary has no
`--tailscale` option and no implicit public bind. Use an explicitly controlled
private interface only when the host network policy permits it.

## MCP transport

The server auto-detects the transport per request. Both speak the same frozen
36-tool surface:

**Standard MCP JSON-RPC 2.0** (what omp/Claude/any MCP client speaks):

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"client","version":"0"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"engine_info","arguments":{}}}
```

`tools/call` results carry the `docx2typed-result-1` envelope as
`structuredContent` plus `isError`.

**Legacy line protocol** (qualification harness):

```json
{"tool":"tools/list","args":{}}
```

Each successful reply is one `OK <json>` line; errors are `ERR <message>`.
Logs never go to stdout. The frozen 36-tool surface is:

### Session and read tools

```text
engine_info
workdir_open(workdir, author?, track?, contract_ranges?, supported_features?, required_features?)
workdir_status()
list_paragraphs()
get_paragraph(paragraph_id)
diff_preview()
list_comments()
get_comment(comment_id)
```

### Text and draft tools

```text
replace_text(paragraph_id, old, new, operation_id)
batch_edit(paragraph_id, edits, operation_id)
insert_paragraph(after_id, text, inherit?, operation_id)
delete_paragraph(paragraph_id, operation_id)
commit_sync(operation_id)
revert(operation_id)
```

### Revision, comment, and table tools

```text
accept_revision(revision_key, expected_fingerprint, operation_id)
reject_revision(revision_key, expected_fingerprint, operation_id)
reinsert_deleted_text(revision_key, expected_fingerprint, text?, operation_id)
delete_comment(comment_id, operation_id)
decide_all(action, output, workdir_out, operation_id)
table_insert_row(table_ref, after, output, workdir_out, operation_id)
table_delete_row(table_ref, row, output, workdir_out, operation_id)
table_insert_col(table_ref, after, output, workdir_out, operation_id)
table_delete_col(table_ref, col, output, workdir_out, operation_id)
table_merge_cells(table_ref, row, col, span, output, workdir_out, discard_content?, operation_id)
table_split_cells(table_ref, row, col, span, output, workdir_out, operation_id)
```

### Review collaboration tools

```text
review_preflight()
review_state()
review_external_preflight(expected_parent_snapshot, operation?)
review_settlement_plan(event_ids?)
review_settle(event_ids?, operation_id)
review_apply_patch(event_id, operation_id?)
review_apply_batch(batch_id, operation_id?)
review_inbox(include_acknowledged?)
review_ack(event_ids, operation_id)
```

### Build and verify tools

```text
build_docx(operation_id, output?)
verify_output(output)
```

The exact JSON Schema for all 36 tools is the checked-in `.mcp_schemas.json`
contract and is published unchanged by `tools/list`.
