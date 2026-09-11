# 0045 — Selective restore is a semantic cherry-pick, not an object swap

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

Selective restore (`history_restore(version, paragraphs=[…])`) is a
**semantic cherry-pick** with a dependency gate, and v1 admits only the case
the gate can prove:

- Eligible in v1: a paragraph whose old and new states both reference **no
  tokens** (no revision nodes, no anchors, no ranges, no format-history
  records) and whose style ids exist in the current style registry.
- Refused, with `partial-restore-needs-dependent-state` naming the reason and
  the paragraph: anything with revision tokens, comment/bookmark anchors,
  ranges, content controls, or table structure coupling.
- A refusal reports what it *would* need (the token ids, the anchor pairs, the
  counterpart paragraph) so the caller can either restore the whole version or
  patch the paragraph explicitly.
- The refused path never silently falls back to a whole-version restore.
- Complex paragraphs get their cherry-pick by composing the normal editing
  path (a patch whose `old` is the target version's text), which already runs
  the boundary, style-ownership, and CAS checks — a slow, honest route instead
  of a fast, wrong one.

## Consequences

- The feature ships with a small, provable surface instead of a broad, unsafe
  one; the guard is cheap because the token ids are already recorded per
  paragraph in `format.json`.
- The interesting cases (revision-bearing paragraphs, cross-paragraph ranges,
  comment pairs) are named rather than approximated, which keeps the
  fail-closed contract intact and gives the next iteration a precise target.
- Cherry-pick is not a restore primitive: it produces a normal version through
  the normal commit lane, published with `origin="cherry-pick"` and
  `restored_from` naming both the version and the paragraphs it took.
