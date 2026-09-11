# Advanced typed-mode reference

Load this file only for engine diagnosis, an unavailable facade, or explicit
maintenance of the typed projection. Ordinary Agent editing should stay on
MCP document tools.

`typed.md` is a restricted typed serialization, not CommonMark or a generic
HTML document. Its header, paragraph markers, style spans, deletion tombstones,
and structural tokens are governed by the typed parser and sidecars.

The workdir binding is:

```text
typed.md + format.json + styles.json + _template.docx
edit.md + edit.state.json
```

`edit.md` is a derived, span-free projection. Its sidecar is authoritative for
freshness; the visible header is only a mirror. Do not copy or hand-edit one
member of the pair. Use `edit refresh`, `edit sync`, or the MCP facade.

Untouched paragraphs replay source bytes. Touched paragraphs are synthesized
from canonical text/style spans. Unknown nodes, changed skeletons, relationship
changes, or protected tokens fail closed at validation/build.

Paragraph primitives and raw XML/ZIP access are diagnosis-only lanes. A refusal
from the facade is a contract boundary, not permission to bypass it.
