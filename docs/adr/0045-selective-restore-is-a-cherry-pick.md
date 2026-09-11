# 0045 — Selective restore is a guarded source restore, not an object swap

## Status

Accepted as a **narrow v1** (version-timeline design, `docs/prd/version-timeline.md`).

## Context

Restoring a whole version is nearly free in storage terms — the new commit
points at the old tree (ADR 0043) — so "put paragraphs P39 and P44 back the
way V12 had them" looks like it should be equally free: swap two paragraph
objects. It is not, because a paragraph is not self-contained. Measured on a
real patent document:

```
typed.md          one <!--@p id="P39"--> block per paragraph
format.json       that paragraph's record carries token_ids
                  P18 references 17 tokens: revision open/close, rpr-change,
                  comment-start/end, commentReference
format.json.tokens  one GLOBAL table, 464 entries, holding raw OOXML
                    open/close byte pairs for every token
```

So a swapped paragraph can reference a token the current state does not have
(dangling `w:del`/`w:ins` bytes), can restore a `comment-start` whose
`comment-end` lives in another paragraph, or can re-anchor a range that spans
paragraphs whose other versions are elsewhere. Content would look right and the
bytes would be wrong — the exact failure mode this system refuses.

## Decision

Selective restore (`history_restore(version, paragraphs=[…])`) is a guarded
source restore. It is conceptually closest to
`git restore --source V12 -- path` followed by `git commit`, not a strict
commit-level `git cherry-pick`: the operation selects paragraph paths inside
one document and creates a new document Version.

v1 admits only the case the dependency gate can prove:

- Eligible in v1: a paragraph whose old and new states both reference **no
  tokens** (dependency-free / zero-token) and whose style ids exist in the
  current style registry.
- Refused, with `partial-restore-needs-dependent-state` naming the reason and
  the paragraph: anything with revision tokens, comment or bookmark anchors,
  ranges, structured document tags (SDTs), content controls, or table
  topology coupling.
- A refusal reports what it *would* need (the token ids, the anchor pairs, the
  counterpart paragraph) so the caller can either restore the whole version
  or patch the paragraph explicitly.
- The refused path never silently falls back to a whole-version restore.
- Coupled paragraphs use the normal editing path (a patch whose `old` is the
  target version's text), which already runs the boundary, style-ownership,
  and CAS checks — a slow, honest route instead of a fast, wrong one.

## Consequences

- The feature ships with a small, provable surface instead of a broad, unsafe
  one; the guard is cheap because token ids are already recorded per paragraph
  in `format.json`.
- The interesting cases (revision-bearing paragraphs, cross-paragraph ranges,
  comment pairs, SDTs, and table topology) are named rather than approximated,
  which keeps the fail-closed contract intact and gives the next iteration a
  precise target.
- Selective restore produces a normal Version through the normal commit lane.
  The internal `origin="cherry-pick"` value is historical wire metadata; the
  user-facing operation is a guarded source restore, not a Git commit
  cherry-pick.
