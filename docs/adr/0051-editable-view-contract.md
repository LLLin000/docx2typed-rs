# 0051 — The editable view contract

Status: accepted (2026-10-17). Companion to [0050](0050-format-engine-boundary.md);
does not reopen [0049](0049-core-edit-protocol-v1-freeze.md)'s edit semantics.

## The problem this exists to end

One document is read by three parts of the system, and each was answering the same
question — *what does this paragraph look like on the editable surface?* — with its
own rule:

| who | what it produces |
|---|---|
| the projection writer | `typed.md`, which the caller reads and edits |
| the draft parser | the paragraphs the commit compares and diffs |
| the baseline render | the template's side of that comparison |

Every defect of the last rounds is one of these three disagreeing with another.
Measured, on real patents, with the two strings that differed:

| divergence | left | right | symptom |
|---|---|---|---|
| `moveFrom` children | `[opaque:opaque][opaque:unsupported-run]` | `[opaque:opaque]` | `save` refused an untouched document (`edits-not-implemented`) |
| whitespace-only body line | `"    "` | `""` | the same refusal, one layer down |
| draft vs projection, after an edit | edited text | parsed text | `projection-mismatch`, newly on five documents |
| the structure sidecar | no user edit | `4 new + 4 deleted` | `table-structure-immutable` on a plain text replacement |

Fixing one pair moved the disagreement to the next pair — because the rule was
never stated once. This ADR states it.

## What must be equal, and what must not

Not every pair of representations is byte-equal, and pretending otherwise is its
own bug:

| comparison | requirement |
|---|---|
| source XML vs `typed.md` | NOT equal — different formats, different purposes |
| baseline `B` vs current draft `D`, no user edit | **equal**, under this contract's rule |
| `typed.md` vs the draft parsed from it | **equivalent** — the round trip is the test |
| `B` vs `D` after an edit | different, and the difference is exactly the user's edit |
| the view re-read after `save` vs `D` | **equal** |
| protected source structure vs the saved output | unchanged |

## The types, and the rule for each

For each type: how it enters the editable view, and whether it may be edited.

| type | in the editable view | editable |
|---|---|---|
| `w:ins` (insertion) | its text, inline, as a revision with author/date/id | yes |
| `w:del` (deletion) | **hidden** — a deletion is not content the caller sees | no (the text is not there to edit) |
| `w:delText` inside a `w:del` | hidden, same rule | no |
| `w:moveFrom` (a move's source) | **its children are shown**, as an addressable `move_from` revision | its text: yes; the structure: no |
| `w:moveTo` (a move's destination) | its text, inline | yes |
| opaque node (drawing, object, unknown element) | a placeholder **that carries its identity** — see below | no |
| whitespace-only paragraph body | the whitespace, exactly as stored | yes |
| inline `tab` / `br` / `cr` | a token, in place | the token is not editable; text around it is |
| paragraph-mark revision (`w:pPr/w:rPr/w:del`) | the paragraph exists; the mark's state is not the caller's business | the paragraph's text: yes |
| table-cell paragraph | its own identity (`T{n}.R{n}.C{n}.P{n}`), its text | yes, if the container allows it |
| a paragraph a round trip cannot preserve | stable identity, and edits that need the lost structure are **refused** | text: only when safe |

### Opaque nodes carry identity

A placeholder string is not an identity. Two different drawings both render as
`[opaque:…]`, so string equality cannot prove that the caller's edit addresses the
node it thinks it does. An opaque node in the editable view therefore carries:

- a stable id (the projection already mints `N{index}`);
- its source location: part, paragraph, node path;
- what it is (`kind`, `tag`), and whether it is *inside* an editable span.

An edit whose span crosses or depends on an opaque node is refused with the node
named — which is what `document_find` must report as its verdict, instead of
`editable: true` followed by a refusal at edit time (the 01/08 case).

### What "unsafe" means

A structure a round trip cannot preserve is not repaired by inventing or dropping
paragraphs. It keeps its identity and the edits that would need the lost structure
are refused. Refusing is the contract; `4 new + 4 deleted` for a document nobody
edited is not a workaround, it is the bug.

## The rule, once

1. **One renderer.** The projection writer, the draft parser, and the baseline
   render answer this contract, not three opinions. They need not share a function
   — writing and parsing are opposite directions — but they share this document,
   and a round-trip test binds them: for every fixture and every real document in
   the corpus, `parse(write(x)) == x` under the rule above.
2. **`Diff(B, D)` only.** The baseline `B` is the projection of the template, frozen
   at extract under this rule. The diff is taken between two views of the same
   kind, and a document nobody edited has an empty diff — that is the invariant the
   red tests assert.
3. **`typed.md` is the contract's artifact**, not an approximation of it: what the
   caller reads is what the parser sees, character for character.
4. **Protected structure is not negotiable.** Whatever the rule says about a node,
   the source's revisions, tables and locked interiors survive the build unchanged
   — verified against the source, not against the draft.

## Acceptance

- 02 and 13: `extract → open → save → reopen` with no edit succeeds, and the
  output's `word/document.xml` is byte-identical to the source;
- 09–12 and any other document the divergence broke: editing works again;
- 01 and 08: `document_find` reports the opaque blocker accurately — no
  `editable: true` that the edit path then refuses;
- the 14 real documents: the full task (find → edit → save → reopen) succeeds, or
  the target is refused before the edit with a reason that names it;
- the corpus fixtures and the round-trip test stay green throughout.

## Pre-merge safety closure (2026-10-18)

This review keeps the reconciliation model and the frozen edit protocol. It
changes only reproduced failures of the invariants above.

### Frozen baseline lifecycle and compatibility

`baseline.view` is initialized lazily by `ensure_projection`, before the first
edit, from the extracted template. Normal edits, commits, saves, reopenings and
refusals must not rewrite it. An existing baseline without `edit.state.json` is
a torn workdir, not a fresh one: initialization refuses rather than replacing B.
An explicit permitted extract/reset removes the old baseline with the other
old-generation assets in the same `ChangeSet`. This is not an additional
power-loss or mid-publication rollback guarantee.

New state files declare `editable_view_contract: "editable-view-1"`. With an
existing B, a missing or mismatched marker refuses as `edit-state-incompatible`
and names fresh extraction as the recovery. An explicitly foreign marker also
refuses when B is absent. Unmarked legacy workdirs with no B retain the existing
fallback; this is not evidence that arbitrary old renderers are compatible.
The marker labels a contract version, not a cryptographic digest of B.

`rust-tests/sync_guard.rs` exercises edit, commit, real save, reopen, refused
edit and inconsistent-state refusal while checking B's digest. It also extracts
the saved DOCX into a new workdir, where a new baseline is legitimate, and tests
orphaned B and missing/foreign version markers without regeneration.

### Weak text comparison is not a structural proof

`readings_disagree` remains an auxiliary, whitespace/marker-insensitive text
comparison. It does not establish opaque identity or structure. Independent
verification is responsible for those claims.

The safety audit used genuine tracked output from the core builder, then
tampered with it before calling `IndependentVerifier::verify`. Three
counterexamples originally verified as `pass`:

- an opaque node invented in an edited paragraph;
- an opening-attribute-only rewrite of an opaque run in another paragraph;
- an unchanged, ordered group of opaque nodes relocated relative to the text
  within an untouched paragraph.

The minimal corrections are in `docx2typed-verify`:

- opaque identity hashes the complete XML node, including its opening tag;
- targeted protected-node lists compare exactly: identity, count and relative
  order, including an originally empty list;
- paragraphs outside authorized text/format spans preserve their complete XML
  bytes, not merely their text and run-property sequence.

Revision wrappers remain transparent, and existing authorized textbox and
format-span exemptions are unchanged. Byte offsets may shift after a legitimate
edit; preserving source placement does not mean freezing absolute byte offsets.
The core still refuses edits into opaque paragraphs before writing.

Direct edits retain independent edited-part byte recomputation. Tracked edits
use revision-text/identity validation plus the protected-structure and
non-target-byte gates; their evidence no longer claims a byte recomputation
that did not occur. Permanent adversarial tests cover deletion, count-preserving
swap, identity/attribute replacement, duplication, invention (including no
baseline protected nodes), cross-paragraph movement and same-paragraph
non-target relocation. Legal direct and tracked outputs pass the same verifier.

### Executed closure evidence

- `cargo build --release`, `cargo clippy --workspace --all-targets -- -D warnings`
  and `cargo fmt --all --check`: pass.
- `cargo test --workspace`: **388 passed, 0 failed, 3 ignored**, 44 suites.
  This includes the existing structural, format-engine and publication gates.
- The fresh-workdir real-document regression: **14/14** inputs complete
  find, edit, save and reopen.
- `seq7`: revoke the island, save again and reopen; the final DOCX is
  **byte-identical to its source ZIP**, not just equal in visible text.
- For 02, 13 and 08, all OPC member names and non-document member bytes match
  the source. Each `word/document.xml` differs by exactly one UTF-8
  `的` → `之` substitution; every byte outside that substitution is identical.
  This proves more than equal paragraph or `proofErr` counts.

The adversarial cases live in `crates/docx2typed-verify/src/lib.rs`, including
`tracked_inventing_a_protected_node_inside_targeted_p0`,
`tracked_inventing_a_protected_node_in_empty_baseline_p0`,
`tracked_outer_opening_attribute_change_on_opaque_run_in_p1` and
`tracked_relocating_opaque_group_within_untouched_p1`. The last case keeps opaque
identity, count and order unchanged and is refused by `non-target-preserved`;
identity/order checks alone are not presented as source-position proof.

The historical **432-document** scan is not a result of this gate. Its original
manifest is unavailable in the retained records. The separately approved
**217-document** inventory defines the new `real-docx-corpus-v1`; it is neither
an equivalent replacement for the historical corpus nor a condition for this
PR's core-correctness closure.

The new run requires a frozen SHA-256 manifest and matching binary/view-contract
metadata. Its completed outcomes are `PASS`, `EXPECTED_REJECT`,
`NOT_APPLICABLE` and `CONTRACT_FAILURE`. Infrastructure or oracle uncertainty is
an incomplete scan, not a successful edit or an accurate refusal. No eligible
safe target is reported separately from a named, independently justified
unsupported target; an unproven target-occurrence linkage cannot pass.

### Issue #70 and Windows CI follow-up

The original corpus evidence remains immutable. Its six contract failures share
one reproduced defect: the builder and independent replay both used an
18-byte window to detect the 20-byte `xml:space="preserve"` attribute. Both now
derive the window length from the literal and inspect only the current text
element, not earlier runs. Existing single-quoted attributes are preserved too.
The minimized five-tool regression is mandatory; its strict xfail is removed.

Independent verification additionally strictly parses every output `.xml` and
`.rels` member before successful publication (`output-xml-wellformed`). Duplicate
attributes in either an edited target or an untouched member fail verification,
even if the byte-level text replay agrees. This closes the shared-bug escape
without coupling verification to the builder's parser or changing the edit
protocol. Package fidelity still means member names and uncompressed member
bytes, not necessarily identical ZIP container bytes.

The Windows staged `format.json` failure was caused by the source-drift test
recursively deleting `wd.parent()`, the shared temporary directory containing
other tests' in-flight commit staging. Cleanup now owns only its unique source,
sentinel and workdir; no shared-parent deletion, retries or test serialization.
The commit implementation is unchanged. Workspace tests and CI must pass before
calling PR #69 merge-ready; the corpus PR remains independently scoped.

### External-file precondition (issue #73)

Publishing replaces a file the engine does not own. The precondition names
which replacements are this workdir's: the target holding the workdir's own
last-published bytes at this baseline epoch, the canonical source still
holding the baseline bytes after an explicit re-extract, or a new path
observed absent. Anything else refuses `external-file-changed` before any
mutation and again immediately before publication — source and workdir
unchanged, sidecars beside C untouched. Success (live or recovery
roll-forward) advances `external-targets.json` to the published bytes; the
epoch keys every entry, so a re-extract re-baselines without cleanup.
Journals predating the record fail closed. Windows holds the live target
with `FILE_SHARE_READ|FILE_SHARE_DELETE` (deny writes, allow our own
rename) across hash + publication; new paths publish via no-clobber
hard-link create. The residual — an external rename-onto-target between the
final check and the rename — is documented, not claimed away. No mtimes, no
silent adoption, no CAS absolute.

