# verification.md — gates (检查说明)

The shared acceptance contract. Every workflow in
[`composites.md`](composites.md) ends on these gates; every claim about an
output is only as good as the gate that proved it. Atoms:
[`capabilities.md`](capabilities.md).

## The seam

```text
source DOCX → extract workdir → edit → build output DOCX → independent verify
```

`verify` (and the MCP `verify_output`) returns structured evidence — checks, revision counts/authors, and surviving comment ids — so agents do not need to unzip the output to confirm tracked edits or comment state.

`verify` is the acceptance gate: it independently re-derives the template
baseline, parses the typed source and the output DOCX, and compares text,
styles, structural tokens, protected XML regions, and every non-document
package part. **It does not trust `build`'s intermediate results** — the
output must stand on its own.

For MCP, effectful mutation attempts enter the operation-id/evidence ledger;
preflight and argument failures return before mutation with a structured
operation ID and recovery action. Canonical-wide writes run the conservative
review preflight, and paragraph-local edits scope the queued-patch check to
the addressed paragraph. `verify_output` binds the operation to the current
snapshot, edit freshness, and output SHA-256; reusing its ID after any of
those inputs changes fails closed instead of replaying stale verification.

## Freshness gate (edit state)

`edit.state.json` is the authoritative freshness binding; the `edit.md`
header is a visible mirror only. Header/sidecar disagreement fails closed as
`edit-header-tampered`.

| State | Meaning | Allowed to build? |
|---|---|---|
| `clean` | edit.md matches typed.md | yes |
| `dirty` | edit.md edited, not synced | no — run `edit sync` |
| `stale-clean` | typed.md changed without refresh | no — run `edit refresh` |
| `conflict` | both changed | no — resolve via `edit refresh --discard` or manual merge |

`validate`, `build`, and `verify` reject every non-clean state. **There is
no bypass flag.**

## Build fail-closed list

`build` writes a temporary DOCX, runs package checks and independent
verification, then atomically publishes. It refuses (non-exhaustive):

- malformed typed grammar; missing deletion tombstones; invalid inheritance;
- style/structure changes (styles.json, skeleton, tokens, anchors);
- source/template fingerprint drift (`baseline drift`);
- protected package part changes (template package manifest mismatch);
- any edit-state other than `clean`.

## Byte-fidelity checks

- A no-op build (extract → build, no edits) must be byte-identical to the
  input DOCX — `document.xml` content hash equal, every untouched part
  replayed verbatim.
- Untouched paragraphs replay raw bytes; only touched paragraphs are
  synthesized from the typed AST.
- Decision paths that create a new baseline (`accept-all`, `reject-all`,
  `table-*`) never mutate the source workdir — they produce a new DOCX and
  a fresh clean-baseline workdir.

## Interop check (Microsoft Word / DOCX)

Microsoft Word is the release interoperability target. The contract is the
official DOCX/OOXML package as opened by Word; LibreOffice is an optional
additional check and is not a release blocker.

Before delivering any output:

1. Open the generated DOCX in Microsoft Word (COM automation or the GUI) and
   confirm it opens without repair prompts.
2. When PDF evidence is required, export/render the DOCX through Word and
   confirm the export completes without repair warnings.

Page count may change (layout reflow is expected — text length changes reflow);
structural damage is not. The demo corpus outputs and every structural op output
are held to this bar. If LibreOffice is available, its conversion is useful
additional evidence but its absence does not fail this gate.

## Dev gates (repository)

Applied after any change to the tool itself (not for document work):
```bash
python -m pytest -q --basetemp=D:/L/AppData/pytest-tmp          # full suite
python -m scripts.acceptance_corpus --corpus corpus/release --workdir D:/L/AppData/...  # real-doc corpus
python -m scripts.tool_smoke --workdir D:/L/AppData/...         # CLI + MCP
```

Corpus covers pathological real documents (57 MB manuscript with 199
revisions, nested pPr, freestanding anchors, CJK text) — a change that
passes the corpus is allowed to touch byte machinery; one that doesn't, is
not.

## Where each gate is enforced

| Gate | Enforced by |
|---|---|
| Freshness | `validate` / `build` / `verify` entry |
| Fingerprint + manifest | `build` (package_guard) |
| Text/style/structure parity | `verify` (independent re-derivation) |
| Byte identity (no-op) | `verify` + dev corpus |
| Interop | human-run Microsoft Word open/render check (above) |
| Tool surface | dev smoke suite |
