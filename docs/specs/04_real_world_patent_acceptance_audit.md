# 04. Real-World Patent Acceptance & OOXML Fidelity Audit

## 1. Executive Summary

This document records the Phase 1 milestone verification for `docx2typed-rs` replacing the Python reference engine. The evaluation moves beyond synthetic unit tests and corpus extraction to an end-to-end multi-step editing workflow on a full-length, complex patent specification document.

### Phase 1 Surface & Extraction Parity

| Surface / Gate | Python Reference | Rust Target | Parity Status |
|---|---|---|---|
| **MCP Tools** | **49** | **49** | **100% (0 missing, `at_parity=true`)** |
| **CLI Commands** | **8** | **8** | **100% (`at_parity=true`)** |
| **Corpus Extraction** | **17** | **17** | **100% Byte-Identical (`0 diffs`)** |
| **CI Matrix** | GitHub Actions | GitHub Actions | **8 / 8 Gates Green** |

---

## 2. Real-World Patent Test Subject

* **Source File**: `一种用于骨关节炎治疗的双重表面化学接枝改性抗炎润滑微球材料及其制备方法（权利要求层级整体修正版）.docx`
* **Size**: 41,558 bytes
* **Document Topology**:
  * 104 original paragraphs (claims, chemical formulations, embodiments)
  * 11 multi-run paragraphs with complex nested formatting (e.g. Claim 2 carries 6 distinct `<w:r>` runs)
  * 17 distinct ZIP package parts

---

## 3. End-to-End Workflow & Execution

Both engines executed the identical sequence via MCP JSON-RPC stdio protocol:
1. `extract` (CLI): Unpack document into typed workdir representation.
2. `workdir_open` (MCP): Initialize session and read baseline state.
3. `insert_paragraph` (MCP): Insert new claim paragraph after P0.
4. `document_replace` (MCP): Global atomic replacement of domain keyword across document (34 occurrences).
5. `commit_sync` (MCP): Settle projection edits and write canonical typed representation.
6. `build_docx` (MCP): Replay and compile back to production DOCX.
7. `verify_output` (MCP): Independent structural and signature verification.
8. `history_restore` (MCP): Forward version recovery to milestone snapshot.

---

## 4. P0 Risk Discovery & Resolution: Run-Level `<w:rPr>` Preservation

### The Defect in Coarse Structural Replacement
Early iterations fell back to structural synthesis (`DeletedParagraph` + `NewParagraph`) when handling multi-run paragraphs. While this allowed text replacement to proceed without single-leaf substring errors, it introduced unacceptable format loss:
* OOXML paragraph-level properties (`w:pPr`) were inherited, but **all internal `<w:r>` runs were flattened into a single run**.
* Run-level properties (`w:rPr`: sub/superscripts, chemical bolding, local fonts) inside multi-run paragraphs were stripped.
* Coarse structural anchoring (`insert_before: next_live`) caused consecutive replaced paragraphs (e.g. Claim 1 and Claim 2) to share identical splice anchors, risking paragraph order inversion.

### The Leaf-Level Atomic `<w:t>` Architecture (Commit `8243360`)
The engine was updated to enforce strict leaf-level atomic updates:
1. `document_replace` maps matches down to individual `ProseLeaf` runs in `_template.docx`.
2. Each modified run emits an `IslandEdit` whose `old` text is the **entire text of that `<w:t>` leaf**, and `new` is that leaf's replaced text.
3. Because the replacement replaces the exact entire `<w:t>` content:
   * `find_old_byte_range` has zero ambiguity (`prose-edit-ambiguous` is mathematically impossible).
   * Surrounding `<w:rPr>` (bold, italic, font, color, sub/superscript) is **100% untouched**.
   * Untouched sibling runs in the same paragraph are **100% untouched**.
   * Paragraph order and `<w:pPr>` are **100% preserved**.

---

## 5. Independent Oracle Deep Audit Results

A standalone, shared verification Oracle (`audit_oracle.py`) inspected all 105 paragraphs in `word/document.xml` across Original, Python output, and Rust output:

```text
================================================================================
INDEPENDENT ORACLE: DOCUMENT.XML DEEP AUDIT REPORT
Original: 104 paragraphs | Python: 105 paragraphs | Rust: 105 paragraphs
================================================================================

[Audit 1: Inserted Paragraph Verification]
  Python P[1]: 本发明所制备的微球具有优异的靶向抗炎及缓释润滑双重响应特性。
  Rust   P[1]: 本发明所制备的微球具有优异的靶向抗炎及缓释润滑双重响应特性。
  ✓ Inserted paragraph text and position (index 1) 100% MATCH!

[Audit 2: Paragraph Properties (w:pPr) Preservation]
  Exact pPr matches across 104 baseline paragraphs: 104 / 104 (100.0%)
  ✓ All 104 baseline paragraphs preserved exact original w:pPr!

[Audit 3: Run Properties (w:rPr) & Run Count Preservation]
  Exact run-level preservation: 104 / 104 paragraphs (100.0%)
  ✓ All 104 baseline paragraphs preserved exact Run count and w:rPr!
  ✓ Claim 2 (6 distinct runs) preserved all 6 runs with original formatting intact!

[Audit 4: Semantic Text Equivalence & Unintended Changes]
  Intended replacements ('润滑' -> '长效润滑'): Python=34, Rust=34
  Unintended text deviations: 0
  ✓ ZERO unintended text deviations across all 105 paragraphs in Rust!

================================================================================
AUDIT VERDICT: 100% PASS (ALL 5 AUDIT GATES FULLY SATISFIED)
================================================================================
```

### Package Part Byte-Fidelity

| Package Part | Status | Notes |
|---|---|---|
| `[Content_Types].xml` | **Identical** | Byte-for-byte identical |
| `_rels/.rels` | **Identical** | Byte-for-byte identical |
| `customXml/*` (3 parts) | **Identical** | Byte-for-byte identical |
| `docProps/app.xml` | **Identical** | Byte-for-byte identical |
| `docProps/core.xml` | **Identical** | Byte-for-byte identical |
| `docProps/thumbnail.jpeg`| **Identical** | Byte-for-byte identical |
| `word/_rels/document.xml.rels` | **Identical** | Byte-for-byte identical |
| `word/fontTable.xml` | **Identical** | Byte-for-byte identical |
| `word/numbering.xml` | **Identical** | Byte-for-byte identical |
| `word/settings.xml` | **Identical** | Byte-for-byte identical |
| `word/styles.xml` | **Identical** | Byte-for-byte identical |
| `word/stylesWithEffects.xml` | **Identical** | Byte-for-byte identical |
| `word/theme/theme1.xml` | **Identical** | Byte-for-byte identical |
| `word/webSettings.xml` | **Identical** | Byte-for-byte identical |
| `word/document.xml` | **Modified** | Intended edits only (Audit 1-4 verified) |

---

## 6. Statistical Performance Benchmark (5 Trials)

Measured across 5 consecutive trials (Trial 1 = Cold Start, Trials 2–5 = Warmed Runs):

| Operation | Python Cold (ms) | Python Med (ms) | Rust Cold (ms) | Rust Med (ms) | Rust P95 (ms) | Median Speedup |
|---|---|---|---|---|---|---|
| **extract** | 2226.6 | 2226.6 | 70.9 | **76.6** | 76.8 | **29.05x** |
| **workdir_open** | 151.1 | 160.2 | 38.2 | **38.2** | 38.2 | **4.20x** |
| **insert_paragraph** | 2975.5 | 2854.1 | 2318.2 | **2186.7** | 2297.4 | **1.31x** |
| **document_replace** (34 hits) | 3666.1 | 3922.3 | 1358.1 | **1569.7** | 1595.5 | **2.50x** |
| **commit_sync** | 2788.7 | 2788.7 | 1613.0 | **1750.0** | 2052.5 | **1.59x** |
| **build_docx** | 598.0 | 598.0 | 508.8 | **508.8** | 523.8 | **1.18x** |
| **verify_output** | 120.7 | 126.3 | 28.1 | **42.9** | 50.8 | **2.94x** |
| **Total End-to-End** | **14180.5** | **14574.3** | **5949.6** | **6283.6** | **6511.6** | **2.32x** |

---

## 7. Strategic Architecture Decision

1. **Production Cutover**: `docx2typed-rust` is validated for primary production use. All 49 MCP tools and 8 CLI verbs operate with verified byte fidelity on real-world patent documents.
2. **Oracle Retention**: The Python engine is frozen and retained in CI as an offline verification Oracle.
3. **Roadmap Transition**: Foundation layer editing semantics are frozen. Future development transitions from Rust/Python parity to Agent operational ergonomics (semantic targeting, claim dependency verification, patent-drafting Skill integration).

---

## 8. Post-Audit Follow-Up: Three Semantics the Audit Settled

The first audit proved byte fidelity for the paragraphs it touched. It could
not prove the *reach* of a replace, and a deeper probe found three boundaries,
all measured on real inputs.

### 8.1 A replace did not reach paragraphs inserted in the session

`document_replace`'s scope (`scope_paragraph_ids`) names only `<!--@p` blocks.
A paragraph inserted earlier is a `<!--@new temp="N1"-->` block, so a
document-wide replace reported `matches=0` on it and the build kept the
original keyword. **The Python reference has the same gap**
(`mcp_server.py:5024` filters `kind == "p"`), so this was a shared defect.
Rust now unions the draft's own blocks: for the patent sample the inserted
paragraph reads `…缓释长效润滑双重响应特性。` where the reference leaves
`…缓释润滑双重响应特性。`.

### 8.2 A keyword split across two runs

Visible text is the concatenation of `<w:t>` leaves, so `("长效润", "滑材料")`
presents a `润滑` that no leaf contains. On that input:

| engine | outcome |
|---|---|
| Python | rewrites the paragraph; the build then fails with `built document XML is invalid: unbound prefix` |
| Rust (before) | recorded an island for a leaf without the text; reported `changed=35` while 34 were real |

Both are now refused with `replace-cross-leaf-matches` before any write,
naming `word.text.replace.cross-leaf` as the unimplemented capability.

### 8.3 A reported match must be recordable

Each plan entry must resolve to a baseline leaf carrying it, a structure
record, or a draft `@new` block the commit synthesises. Otherwise
`replace-unrecorded-match` fails the call closed.

### 8.4 The lost commit baseline

`commit_sync` bootstrapped the review session lazily, after
`apply_projection` rewrote `typed.md`, so the C0 baseline pinned the
POST-commit hash and `publish_current` refused with `current-not-changed` on a
draft that had edits. `ensure_session` now runs first.

### 8.5 Verification status

| check | result |
|---|---|
| `rust-tests/replace_semantics.rs` (7 boundary cases) | 7 / 7 pass |
| Corpus extraction parity | 17 / 17 byte-identical |
| MCP schema gate | 49 tools, exact parity |
| Real-patent oracle (`qualification/patent_acceptance_oracle.py`) | 100 % pass, stricter than the reference on inserted paragraphs |

---

## 9. Count Audit and Timing Coverage (follow-up)

### 9.1 Three counts, kept separate

A single "matches" number cannot distinguish a complete replacement from one
that scanned a smaller document. `facade_verify` now reports, for a named
query:

| count | source |
|---|---|
| `baseline_matches` | the template (`_template.docx`), the pre-edit state |
| `draft_matches` | the projection the agent edits (`edit.md`) |
| `built_matches` | the exported DOCX, when one is named |

Measured on the real patent **with a paragraph inserted mid-session**:

```
baseline 34 -> draft 35 -> built 35      residual bare occurrences: 0
```

The `+1` is the inserted paragraph. Before this audit the same flow reported
**34/34** with one occurrence left unreplaced — four filters each hid the
`<!--@new temp="N1"-->` block in the window between `insert_paragraph` and the
`commit_sync` that reallocates the temp id.

Rust now reports **35** where the Python reference reports **34**: the reference
filters on `kind == "p"` too (`mcp_server.py:5024`) and misses the same
paragraph. Rust is deliberately stricter.

### 9.2 Timing coverage: the uncounted cost is Python's

Summed medians never equal the total median. Recording every round exposes what
the phase rows do not cover — process start, module import, orchestration:

| engine | sum of phases (ms) | measured total (ms) | **unaccounted (ms)** |
|---|---|---|---|
| Python | 13 148 – 14 941 | 14 888 – 16 772 | **1 739 – 1 831** |
| Rust | 6 154 – 6 700 | 6 167 – 6 712 | **12 – 15** |

Python carries **~1.78 s per round** the phase rows never see (interpreter start
plus `mcp_server` import); Rust carries **~13 ms**. Raw rows are written to
`I:/docx2typed_patent_benchmark/timing_raw.csv`.

### 9.3 Where Rust's remaining time goes

| phase | Rust median (ms) | share of total |
|---|---|---|
| `insert_paragraph` | 2 378 | **36.9 %** |
| `commit_sync` | 1 914 | 29.7 % |
| `document_replace` | 1 497 | 23.2 % |
| `build_docx` | 590 | 9.1 % |
| `extract` | 79 | 1.2 % |
| `verify_output` | 34 | 0.5 % |
| `workdir_open` | 35 | 0.5 % |

`insert_paragraph` is the largest single phase but is not obviously
optimisable: it shares the session-initialisation and validation path with
`commit_sync`. The remaining work is transaction-shaped, so a batch-edit entry
point would help more than tuning any one function. At 6.45 s for a full real
patent round-trip, this is not on the critical path for agent usability.

## 10. Independent `real-docx-corpus-v1` compatibility baseline

This is the separately approved **217 SHA-256-distinct inputs**, not the
historical 432-document corpus (whose original manifest is unavailable). It is
not an equivalent replacement for that corpus or an added merge condition for
PR #69. No Rust core changes were made for this scan.

### Frozen inputs and executed identity

- Source bytes: **249,587,007** total; only exact-byte SHA-256 deduplication.
  Similar document versions remain separate. The saved inventory was consumed
  directly, without reselection or another filesystem sweep.
- Immutable manifest SHA-256:
  `ddcd005e6ec46b52aa80d70f6d3befecf6a72060eaaecd468a36652c3b755407`.
- Binary SHA-256:
  `9cb7cce1d2e7561fc5da033d4df0a35089322fb50dbfd9498bb7266ea5dd89ba`;
  reported descriptor: `docx2typed-rust 0.2.1
  (7d919010e711eff00f515c554c6327edd1814851-dirty)`. The dirty descriptor is
  retained, not rewritten as a clean release claim.
- Final scanner rules: `real-docx-scan-rules-4`; scanner SHA-256:
  `9f6ad2c5bd4f36e6077f3573dfed5517a04e1b7f554cc70b69e6c36a8448f0b9`.
- Anonymous frozen manifest, per-document results and feature-stratified summary:
  [`qualification/evidence/real-docx-corpus-v1/`](../../qualification/evidence/real-docx-corpus-v1/).
  Original paths, call arguments, text and full MCP envelopes remain private
  under `C:/tmp/real-docx-corpus-v1/`; archive bytes are not committed.

### Workflow and final results

Every input first completed a fresh-workdir pristine open → save → reopen:
**217/217** had identical uncompressed OPC member names and bytes. Eligible
edits then used a separate fresh copy with `track:false`. Target selection
resolved the source XML namespace and byte range independently, before find;
the query was unique under the surface's case-insensitive search semantics.
The expected `word/document.xml` was sealed before calling the engine.
Passing required that exact expected XML plus unchanged non-target members,
not merely a success response or a single diff at an unspecified occurrence.

| Completed classification | Documents |
|---|---:|
| `PASS` | 210 |
| `EXPECTED_REJECT` | 1 |
| `NOT_APPLICABLE` | 0 |
| `CONTRACT_FAILURE` | 6 |
| Infrastructure/oracle incomplete | 0 |

The expected rejection names `P0`, `opaque-paragraph-mutated`, with independently
matched unsupported structure and a pre/post-probe no-write snapshot. It is
not counted as an edit. No-match or an unprovable oracle is never a successful
edit or an accurate refusal. Features overlap: inputs include 25 with tables,
119 with revisions, 10 with opaque candidates, 1 with SDT and 63 with media;
`opaque_candidates` is an inventory heuristic, not an engine support verdict.

All six failures share the **+21-byte duplicate `xml:space="preserve"`
attribute** after an accepted direct edit. They are recorded as failures even
though edit/save/reopen reported success. See [issue #70](https://github.com/LLLin000/docx2typed-rs/issues/70).
The source hashes begin `15171069`, `62602d8f`, `77a6027a`, `84a10489`,
`a704ca45`, `b7493b22`; full hashes are in the committed rows.
`tests/test_real_docx_contract_regressions.py` is the minimized real-binary
reproducer: `--runxfail` fails on exact output XML. Its strict xfail records a
known defect, **not a passing correctness check**. The suspected root cause is
the 18-byte window compared against the 20-byte `xml:space="preserve"` literal
in `maybe_inject_xml_space`; no speculative core rewrite was attempted.

Earlier scanner rules exposed harness defects (pagination metadata placement,
case-insensitive uniqueness, math `m:t` versus Word `w:t`, and snapshot timing).
Those runs are superseded, not combined with the final totals. The final
tooling checks passed **37 tests**; the separate known-defect regression
reported **1 strict xfail**. Archive validation after scanning still returned
217 matching inputs and the same manifest digest.

### Reproduction and retention

```text
python qualification/real_docx_corpus.py verify --manifest <archive>/manifest.json
python qualification/real_docx_scan.py --manifest <archive>/manifest.json --out <new-run> --bin <pinned-binary> --workers 4 --keep-failures
python -m pytest tests/test_real_docx_corpus.py tests/test_real_docx_scan.py -q
DOCX2TYPED_BIN=<pinned-binary> python -m pytest tests/test_real_docx_contract_regressions.py --runxfail -q
```

The archive stores `originals/<sha256>.docx`; manifest IDs are `doc-<sha256>`.
`verify` rejects changed/missing inputs without relocking. Resume binds the
manifest, binary, scanner and engine-metadata hashes plus rules version;
incomplete rows are retried, completed rows skipped. Disk preflight enforces
2 GiB free; workers are bounded at four. Cleanup only removes descendants of
the scanner-owned sentinel directory. Successful scratch is removed; failure
evidence is retained with `--keep-failures`. This is package/XML fidelity and
engine-reopen evidence, **not a Microsoft Word rendering/no-repair test**.

PR #69's package jobs now both pass after the qualification count fix, but
run `39099908345` contains a separate Windows workspace failure in
`store_mutation_replays_byte_exact_and_rejects_reuse` (missing staged
`format.json`). It is not hidden by the corpus report or a rerun-to-green.

## 11. Issue #70 closure: unchanged corpus, rules and oracle

The original section 10 report and its manifest/results/summary are retained
unchanged. A fresh execution with the corrected binary is published under
[`issue70-fixed/`](../../qualification/evidence/real-docx-corpus-v1/issue70-fixed/),
including a document-ID comparison rather than an edited old report.

| Classification | Original | Fixed binary |
|---|---:|---:|
| `PASS` | 210 | 216 |
| `EXPECTED_REJECT` | 1 | 1 |
| `NOT_APPLICABLE` | 0 | 0 |
| `CONTRACT_FAILURE` | 6 | 0 |
| Incomplete | 0 | 0 |

All 217 document IDs, the manifest digest and scanner digest are identical to
the original run. Rules remain `real-docx-scan-rules-4`; no query/oracle relaxation
or corpus expansion. Exactly the original six failures changed to `PASS`; every
other classification is unchanged. Pristine round-trip remains **217/217 exact
OPC member names and uncompressed bytes**, not a whole-ZIP byte claim.

The new binary SHA-256 is
`6e8ac8e169e7a9d60a6584dfaf337523abbe98a678aa2567f5f1e086530469a4`;
its build descriptor is `8e8db0ab91323974013405a74b730093e366528f-dirty`
(fixes built before commit `8285d27`; the actual descriptor is retained).
The root fix changes both the builder and independent expected replay to
derive attribute-window lengths from the byte literal, scoped to the current
text opening tag. A separate strict XML parser now rejects malformed output
`.xml` and `.rels`, including duplicate attributes in an edited target or an
untouched part, before publication. The real five-tool minimal regression now
passes without xfail. Verifier tests: **29 passed**; workspace: **393 passed,
3 opt-in fixture emitters ignored**; clippy warnings denied passes. Final
Windows review and commit tests pass **48 + 3** after unique-source isolation.

The Windows `format.json` disappearance was caused by a test deleting its
shared temporary parent, not by the commit state machine. That deletion is
removed and the test source name is unique; no global lock, retry or serialized
test runner was added. Core fixes are also on PR #69 as `eb996c5`; this corpus
comparison remains on PR #71. Remote CI is recorded separately from local proof.

## 12. Issue #72 note: tracked verification beside another author's prior revision

review-run9 prepares a prior tracked replace and a classic comment in real Word
under one maker (`照轩 林`), then agent-edits another paragraph's text under its
own sidecar identity. The old whole-part identity walk joined the prior
paragraph's deletion into the agent's deletion run and refused the correct
output with `the runs of one deletion disagree about who made it` — a
cross-paragraph grouping error, not an emission error: the builder wraps each
covered run in place and never rewrites prior revisions.

The fix scopes the check to the edit's own paragraph (`verify_tracked_paragraph`
in `crates/docx2typed-verify/src/lib.rs`): the template's prior revisions must
still stand byte-identical inside the edited paragraph (dropped or rewritten
priors refuse here, where no other gate sees revision wrappers), and the edit's
own run is the candidate deletions whose `delText` bodies join to `old`, with
verified priors allowed to stand between covered runs. The identity agreement
is unchanged — one maker's runs agree, the insertion names the same maker, and
the sidecar's author/date are checked on the revision the edit produced.

Permanent unit regressions (real core `apply_build` output, real verifier, no
hand-shaped XML): an edit beside another author's prior revision verifies with
the prior paragraph byte-identical; an edit inside a paragraph with an untouched
prior verifies; rewriting or dropping the prior inside the edited paragraph
refuses; restamping one run of the edit's own deletion or its insertion still
refuses with the exact disagreement strings.

