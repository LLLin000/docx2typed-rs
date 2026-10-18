# Real workflow qualification v1

## Scope and frozen starting point

`real-workflow-qualification-v1` complements `real-docx-corpus-v1`. The latter
records package/XML fidelity under a controlled edit; this qualification records
whether an exported file remains usable in real Microsoft Word workflows.
The 217-input corpus and its original/fixed results remain unchanged.

First round: **Word bidirectional handoff** and **review handoff**, each with a
success candidate and an expected-refusal candidate. Zotero is explicitly out
of scope. Long-text editing, templates/numbering, WPS/LibreOffice and cloud
coauthoring are later, separate workflows, not prerequisites or implied support.
No Rust core modification is authorized by the existence of this framework;
only a reproducible real-host contract violation justifies a targeted fix.

## Outcome and execution status are different

Every executed case records one of:

| Outcome | Required evidence |
|---|---|
| `PASS` | Every declared user-visible assertion is exercised and satisfied. |
| `EXPECTED_REJECT` | A named, structurally justified refusal occurs before unsafe publication; pre/post source and workdir snapshots prove no undeclared write. |
| `UNSUPPORTED_WORKFLOW` | A required operation cannot be expressed through the supported product surface; name the operation and boundary. It is not a successful delivery. |
| `CONTRACT_FAILURE` | Supported/promised behavior fails reproducibly, including silently overwriting external changes or losing a revision/comment. |

An unavailable Word host is `completion_status: not-run-no-host`, `outcome: null`.
Timeouts, transport failures and insufficient observation are `incomplete`, not
accurate refusal or support evidence. Counts keep unexecuted cases separate.
A candidate intended to reject is not labelled `EXPECTED_REJECT` merely because
an arbitrary error occurred.

## External-edit state contract

1. Stage an immutable source A into an owned work area. Extract Baseline A.
2. Agent edits and exports verified B. Retain B and its evidence.
3. A Word operator edits B and saves C. Word is allowed to normalize XML,
   relationship serialization and metadata.
4. The old workdir must not silently overwrite changed C. An attempted stale
   publication is a separate negative case with content-hash no-write proof.
5. Safe continuation is explicit extraction of C into the same logical workdir,
   creating Baseline C. Existing pending/committed-version guards remain in
   force; a deliberate discard/reset is allowed only after exporting B and
   explicitly selecting C as the new source of truth. Record baseline epoch,
   template fingerprint, CLI arguments and the resulting generation.
6. Edit again and open the final output in Word. Both Agent rounds and the Word
   operator change must remain. No automatic old-generation or three-way merge.

Before Word touches a file, engine byte-level fidelity still applies. Across
Word save, compare intended content, character/paragraph formatting, sections,
revision authors/types/text, comment anchors and references semantically or by
actual host functionality. Do not demand byte-identical Word-generated XML.
A successful strict XML parse is not a Word no-repair or visual-layout result.

## Review handoff contract

Use a copy of an existing frozen real input. If the required prior revisions
and comments are absent, prepare them through real Word and record preparation
operations plus prepared-file SHA-256; never claim the original carried them.

A prior reviewer and the Agent have distinct author identities. Agent edits
with tracking enabled outside protected revision boundaries. In Word:

- inspect the text, authors, types and ranges of the pre-existing and new edits;
- accept only the Agent's revisions in one copy and reject only those revisions
  in another; reopen both and check the expected visible text;
- keep prior reviewer changes and comment text/author/anchored passage usable;
- separately attempt an edit crossing a revision boundary and require a named
  refusal with no-write evidence.

Revision counts alone are insufficient. Classic Word comments do not prove
modern threaded replies, @mentions or resolved-thread compatibility. Unexercised
comment models and UI actions are explicit limitations.

## Host and operator provenance

Record host application name, version/build, executable path in private logs,
OS, binary SHA-256 and reported descriptor; original/prepared/intermediate/final
file hashes; every tool call and Word action; assertion results; and artifact
hashes. Source originals must never be changed. Private document paths/text stay
outside committed public evidence; public rows use hashes and anonymous IDs.

`execution_mode: word-com-automated-operator` means real Word executed the action,
not that a human clicked it. Record `human_ui_review: not-run` unless a real
human observation is captured. Suppressing alerts or `OpenAndRepair=false` does
not itself prove absence of a repair prompt. UI appearance and no-repair claims
require independent UI evidence. Officecli/XML simulation cannot substitute for
Word execution; no software installation is silently performed to claim support.

## Reproducible entry points

The two lane runners accept `--manifest`, `--bin`, `--out`. They retain private
per-action evidence in the specified output directory and emit `report.json`
with schema `real-workflow-qualification-1`, workflow, host, binary and cases.
Use a fresh output directory and the unchanged corpus manifest for a new run.
Reports keep unsupported/not-run/failed cases visible rather than counting
assertions or boilerplate setup as completed user workflows.

```text
python qualification/real_workflow_word_handoff.py --manifest <archive>/manifest.json --bin <candidate> --out <fresh-word-run>
python qualification/real_workflow_review_handoff.py --manifest <archive>/manifest.json --bin <candidate> --out <fresh-review-run>
python qualification/real_workflow_qualification.py --report <fresh-word-run>/report.json --report <fresh-review-run>/report.json --out <new-summary.json>
python -m pytest tests/test_real_workflow_qualification.py -q
```

## Executed first-round evidence

Actual host: **Microsoft Word 16.0, build 16.0.20430**, Windows 11.
Execution mode: Word COM automated operator; human UI review and independently
observed no-repair prompts are **not run**. No Zotero execution. No Rust core
changes, automatic merge, corpus expansion or compatibility fixes in this round.

Binary SHA-256:
`b12f0557fbac95314108ad4c818d3f86b435cc90eb91f6f08f8bc824a5b0595a`,
descriptor `d186c3aa84c58b0ec63e68394e0d4e23f022b42e-dirty`.
Input manifest remains
`ddcd005e6ec46b52aa80d70f6d3befecf6a72060eaaecd468a36652c3b755407`.
Public anonymous assertion-level evidence:
[`qualification/evidence/real-workflow-qualification-v1/summary.json`](../../qualification/evidence/real-workflow-qualification-v1/summary.json).
Private original/prepared/intermediate files, full Word snapshots and tool calls
are retained in `C:/tmp/real-workflow-qualification-v1/` (Word run5, review run9).

| Case | Actual outcome | Observed result |
|---|---|---|
| Explicit Word → re-extract → Agent continuation | `PASS` | Both Agent edits and the Word edit survive; baseline epoch 1→2, template fingerprint matches C, real Word finds all three changes; witness format, comments, sections and references checked. |
| Old workdir attempts to overwrite Word-changed source | `CONTRACT_FAILURE` | Save succeeds and replaces C with B, losing the external change; [issue #73](https://github.com/LLLin000/docx2typed-rs/issues/73). |
| Review selective accept/reject delivery candidate | `CONTRACT_FAILURE` | Word prepares prior revision/comment, find reports editable, edit succeeds, save refuses `edited-part-exact`: deletion authors disagree; [issue #72](https://github.com/LLLin000/docx2typed-rs/issues/72). |
| Cross existing Word revision boundary | `EXPECTED_REJECT` | Named `cross-leaf-edit` refusal and successful source/workdir no-write proof, after explicitly initializing lazy projection before the probe. |

Totals: **1 PASS, 1 EXPECTED_REJECT, 0 UNSUPPORTED_WORKFLOW,
2 CONTRACT_FAILURE, 0 unexecuted cases**. A success and a rejection candidate
were exercised per workflow; their intended labels were not forced onto failed
product behavior. Review accept/reject actions after publication could not be
executed because no edited B was published; they remain unproven, not passing.
Classic comment preparation does not establish modern threaded-comment support.

The original real-input hashes are `059d686a069007e574cf82b63e3c1c5e8a4ec3d3215c2557612acbb29bd28f7c`
(Word handoff) and `62602d8f9ebd4449d29e662ece14eee36075cd6bf234012231e19c285b50a5a2`
(review). The review input was deliberately prepared through Word, not falsely
described as originally containing the controlled revisions. Word may use its
live configured reviewer identity rather than the requested alias; actual
identities stay in private evidence, and no assumed-author comparison is proof.

Earlier attempts exposed qualification-harness errors: BOM handling, PowerShell
case-insensitive parameter collisions, parameter-block ordering, selecting a
partial format-boundary span, final-view versus deleted text, Word comment-ID
normalization, and first-find lazy projection initialization. Those attempts
are retained privately and superseded, not counted as additional product bugs.
The summary collector rejects PASS/EXPECTED_REJECT with a failed assertion and
keeps no-host/incomplete cases out of executed totals; its three checks pass.

## Directed #72/#73 fix verification

The same four workflow scenarios were rerun with both independent production
fixes composed locally: [PR #75](https://github.com/LLLin000/docx2typed-rs/pull/75)
and [PR #76](https://github.com/LLLin000/docx2typed-rs/pull/76).
The qualification PR contains evidence only, not those production changes.

Pre-integration binary SHA-256 (historical evidence, not the final clean build):
`feaee847fbd6f0bff6b5b14cee07f4ec5e45f8506d4c696f39b518ba49b0e349`.
Its recorded descriptor is `31f2a57823006cb8769336b04f6bdf627457b764-dirty`;
the hash, not a claim of a clean commit build, binds these runs.
The input manifest and Word host/build are unchanged.

| Check | Observed outcome |
|---|---|
| Explicit Word → re-extract → Agent continuation | `PASS` |
| Old workdir attempts to overwrite Word-changed source | `EXPECTED_REJECT`; external edit retained |
| Review selective accept/reject delivery candidate | `PASS` |
| Cross existing Word revision boundary | `EXPECTED_REJECT` |
| Historical high-value inputs, open/find/edit/save/reopen | 14 completed, 0 failures |
| Frozen 217-input corpus | 216 `PASS`, 1 `EXPECTED_REJECT`, 0 `CONTRACT_FAILURE`; all 217 classifications unchanged |

Workflow totals: **2 PASS, 2 EXPECTED_REJECT, 0 CONTRACT_FAILURE,
0 UNSUPPORTED_WORKFLOW, 0 unexecuted**. Workspace: 407 passed, 3 ignored;
clippy with warnings denied and formatting check passed. Both production PRs'
Linux/Windows CI checks passed. The store qualification gate passed after
replacing its incidental test-count pin with the test process exit status.

Anonymous evidence is under
[`qualification/evidence/real-workflow-qualification-v1/issue72-73-fixed/`](../../qualification/evidence/real-workflow-qualification-v1/issue72-73-fixed/):
`workflow-summary.json`, `historical14.json`, `corpus-summary.json`, and
`corpus-results.jsonl`. Original failing evidence above remains historical;
it is not relabeled. Word COM automation still does not prove human visual
acceptance, independently observed absence of repair prompts, modern threaded
comments, or Zotero interoperability.

## Clean-main integration acceptance

PR #76 merged first. PR #75 was merged only after updating to that main and
passing all eight Linux/Windows CI jobs. The final acceptance binary was then
built from clean `main@32587ef925988100f41d4d27974ea675cdc0510c`,
before adding qualification-only evidence. No production code was changed for
this acceptance.

Official binary SHA-256:
`d2d5ddc73862a89cf526ffe1e5857c8b23c48bb0730bd47a972b118e6314d14c`.
Descriptor:
`docx2typed-rust 0.2.1 (32587ef925988100f41d4d27974ea675cdc0510c)`.
This clean-main record supersedes the composed dirty-build result above as
the integration acceptance reference; earlier results remain historical.

With the unchanged input manifest and Word host/build, the four existing Word
scenarios completed: **2 PASS, 2 EXPECTED_REJECT, 0 CONTRACT_FAILURE,
0 UNSUPPORTED_WORKFLOW, 0 unexecuted**. Every reported assertion passed.
The same 14 historical inputs completed open/find/edit/save/reopen with zero
failures and unchanged source hashes.

The frozen 217-input corpus completed all 217 newly scanned inputs, with zero
resumed skips and no incomplete cases: **216 PASS, 1 EXPECTED_REJECT,
0 CONTRACT_FAILURE, 0 NOT_APPLICABLE**. Every document's classification matches
the prior directed-fix run. Workflow and corpus reports bind the same official
binary hash and the unchanged manifest
`ddcd005e6ec46b52aa80d70f6d3befecf6a72060eaaecd468a36652c3b755407`.

The clean-main workspace run passed **407 tests, 3 ignored**, clippy with
warnings denied, and formatting check. The qualification report collector's
existing three tests also passed. Anonymous evidence and build/runner
provenance are under
[`qualification/evidence/real-workflow-qualification-v1/clean-main-final/`](../../qualification/evidence/real-workflow-qualification-v1/clean-main-final/).
Private Word artifacts remain in `C:/tmp/docx2typed-final-integration-v1/`.

## Frozen v1 capability boundary

This version covers only the existing four scenarios and the fixed 14/217-input
regression sets. Four scenarios meeting their contracts means two supported
successes and two accurate refusals, not four editable workflows. Further
workflow coverage belongs to a separate qualification version.

| Capability | Claim boundary |
|---|---|
| Agent → Word → explicit re-extract → Agent | Specified Word COM handoff accepted |
| Old workdir encounters external Word save | Detects and refuses the verified stale-overwrite cases |
| Existing revisions/comments + Agent tracked edit | Specified Word COM review handoff accepted, including selective accept/reject |
| Unsafe revision-boundary edit | Accurately refused |
| Human Word UI, no-repair prompts, visual layout | Not qualified |
| Zotero active citations, modern threaded comments | Not qualified |
| Absolute no-overwrite under arbitrary concurrent writers | Not established |

The external-file guard is not an absolute compare-and-swap guarantee. Windows
handle protection does not prevent every external rename-onto-target race
between the final precondition check and publication. OneDrive/SharePoint and
other multiprocess/cloud coauthoring safety must not be inferred from this
qualification; see ADR 0051's external-file precondition.

