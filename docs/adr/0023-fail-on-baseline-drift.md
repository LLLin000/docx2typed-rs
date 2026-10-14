# 0023 — Fail on source/template drift instead of auto-merging

**Superseded in part by [ADR 0048](0048-sync-in-place-not-a-new-workdir.md).**
The guarantees here still hold — drift is never auto-merged, and pending typed
edits are never overwritten. The remedy no longer does: drift is followed by
syncing the same workdir, not by creating a new one.

If the recorded source or workdir template fingerprint changes after extract, the current workdir is stale. v2.0 refuses build and does not overwrite or three-way-merge pending typed edits; the user creates a new workdir from the latest DOCX and reapplies unfinished changes explicitly.
