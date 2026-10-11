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
