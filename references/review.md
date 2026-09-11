# Review reference

Load this file when the document has tracked revisions, comments, or a human
review queue. Read exact tool contracts in `../capabilities.md` and use the
workflow details in `../composites.md`.

## Tracked revisions

Choose the mode before editing an ambiguous document:

- `track=true`: new insertions, deletions, and replacements become Word
  revisions;
- `track=false`: changes apply directly, but revision-internal text is refused
  rather than silently rewritten.

Revision boundaries remain hard. Single accept/reject operations update
canonical state and normally fold into the next `commit_sync`; wholesale
settlement is a governed baseline transition. Existing revision identity,
author, date, ancestry, and sibling revisions remain intact unless the user
chooses a decision.

## Comments

Comments are instructions, not disposable markup. The default is:

```text
read comment → satisfy its request → preserve the comment
```

Call `delete_comment` only when the user explicitly asks to remove that
comment. Accepting or rejecting revisions does not implicitly delete comments.

## Human review queue

For browser collaboration:

```text
review_preflight / review_state
→ review_inbox
→ apply queued patch or settlement transactionally
→ review_ack
→ refresh the review surface
→ build + verify at delivery
```

The browser is a review and handoff surface, not a DOCX writer. A queued event
is not completion; the agent must apply it and prove the resulting DOCX.
