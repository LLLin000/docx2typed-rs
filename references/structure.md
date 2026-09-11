# Structure reference

Load this file when a request touches tables, content controls, text boxes,
headers/footers, footnotes/endnotes, or protected tokens.

## Text versus topology

Text inside a table cell, SDT/content control, text box, header, footer,
footnote, or endnote uses `document_patch` like ordinary prose. The container
and its XML topology remain locked.

Rows, columns, merges, and splits use the dedicated table tools. They create a
new baseline/version according to the active surface; inserted rows and
columns are empty, and existing cell text is not copied or rewritten.

A merge that would discard non-empty spanned cells refuses unless the caller
sets the explicit discard option. Table references are body-level ordinals;
read the raw/table view when the ordinal matters.

## Protected structures

Structural tokens, bookmarks, comment anchors, fields, drawings, math,
revision containers, relationship IDs, and opaque interiors are not ordinary
text. Leave them to the engine's typed or byte-level lane. Never modify raw
OOXML to bypass a refusal.

## Selective restore boundary

Selective restore v1 is only for dependency-free, zero-token paragraphs. Any
paragraph coupled to revision tokens, comment/bookmark ranges, SDT/content
control state, table topology, or text-box topology refuses with
`partial-restore-needs-dependent-state`. The refusal must not become a whole
restore by accident.
