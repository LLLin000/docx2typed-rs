# Diagnostics reference

Load this file after a structured refusal or when a writer may have changed the
workdir.

## Recovery order

1. Read the complete Result envelope and its `code`, `next_actions`,
   `capability`, `fallback`, `data.fix`, and `data` evidence.
2. If the result supplies `fresh_match_ref`, `divergence`, or a corrected hunk,
   use that exact recovery once with a fresh `operation_id`.
3. If the failure is stale state, re-open or re-read the workdir and carry the
   new `revision`/snapshot forward. Do not re-derive a long `old` string from
   memory.
4. A second refusal on the same paragraph is a blocker to report. Keep raw
   OOXML and private store mutation out of the recovery path.

Common safe responses:

| Signal | Response |
|---|---|
| stale document or match ref | re-read/search once, then use the fresh token/ref |
| revision-boundary refusal | narrow the edit to one editable span or choose a revision decision |
| style-region refusal | use the facade patch, not primitive region surgery |
| draft dirty at format/build | commit or revert the draft first |
| trimmed historical Version | choose a retained Version; do not resurrect from a generation |
| operation-id-reused | omit the ID or generate a new one |

A successful browser display, queued event, or partial build is not delivery.
Delivery ends with a clean state, independent verify, and the promised Word
interoperability check.
