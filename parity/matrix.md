# Rust ↔ Python parity matrix (Phase 1, PR 1)

**Feature reference** (behaviour to reach): `LLLin000/docx2typed` @ `1adcc9403cc830e899efe74c5ec41b11973b8c7c`
**Runtime oracle** (what Rust CI compares against): tag `v0.1.0-python-final` @ `c6a17a16963d36e8860f8952a56465e94b3bb53a`
Target: `LLLin000/docx2typed-rs` @ `324cd055096e841fd5c97757def982b91ed93e15`

Machine-readable: [`matrix.json`](matrix.json), [`python-lock.json`](python-lock.json).

> ### ⚠ Two different "Python references" exist — do not conflate them
>
> This was the most consequential finding of PR 1, and the plan treated the two
> as one.
>
> | Concept | Commit | Age | Role |
> |---|---|---|---|
> | **Feature reference** | `1adcc94` = current `main` | today | the behaviour Rust must reach |
> | **Runtime oracle** | `c6a17a16` = tag `v0.1.0-python-final` | 2026-08-14 | what Rust CI actually runs against |
>
> Rust CI pins the oracle **fail-closed** (`.github/workflows/rust-ci.yml`,
> `PINNED_PYTHON_REFERENCE_SHA`). The two are **118 commits apart**, and the
> oracle **predates the `w:rPr` order fix (#98)** — its `vertical_style_variant`
> still does the buggy `root.append`.
>
> Consequences:
> 1. "Rust matches Python" is ambiguous — it must be stated *which*.
> 2. A defect fixed on `main` is **not** fixed in the oracle, so a Rust fix that
>    is *correct* can still fail the oracle gate. That is a **real conflict**,
>    not a Rust bug: the oracle must be bumped deliberately (a spec change), not
>    worked around by reintroducing the bug.
> 3. The plan's cited SHA `1a4d4885` was a third, stale value (main at plan
>    writing, now superseded by #100 delivery evidence + #101 gate design).
>
> **The oracle-bump policy is now on the critical path** — see
> [`oracle-coverage-gap.md`](oracle-coverage-gap.md). Measured: the oracle
> exposes 49 tools and lacks **exactly the 5 tools Rust still has to
> implement**, so the oracle-qualification job can never exercise the
> migration target.

## 1. Surface counts (verified, not estimated)

| Surface | Python | Rust | Gap |
|---|---|---|---|
| MCP tools | **49** | **44** | 5 missing, 0 extra |
| CLI commands | **8** | **8** | **at parity** |

The CLI is already at parity: both declare exactly
`build, decide, edit, extract, inspect, migrate, validate, verify`
(Python `scripts/protocol.py`, Rust `crates/docx2typed-protocol/src/descriptor.rs`).

## 2. The 5 missing MCP tools

Name equality is not functional equality; each row lists the Python definition
site and the Rust crate that must own the capability. Status values:
`missing` (no Rust surface), `partial` (infrastructure exists, not exposed),
`equivalent`.

| # | Tool | Python site | Rust infrastructure | Status |
|---|---|---|---|---|
| 1 | `document_replace` | `mcp_server.py:4947` | — | `missing` |
| 2 | `format_span` | `mcp_server.py:3783` | core/prose.rs (primitive landed #28; design #39) | `partial` |
| 3 | `history_gc` | `mcp_server.py:5917` | `store/store.rs` | `missing` |
| 4 | `history_restore` | `mcp_server.py:5951` | `store/store.rs` | `missing` |
| 5 | `workspace_adopt` | `mcp_server.py:2935` | identity landed (#19); resolver + tool open | `partial` |

### Landed since this matrix was first written

| Tool | PR |
|---|---|
| `history_list` | #20 |
| `document_read` | #23 (core) / #32 (tool) |
| `document_search` | #29 (core) / #31 (tool) |
| `document_patch` | #30 (core) / #32 (tool) |
| `history_diff`, `history_blame`, `history_verify` | #25 (core) / #36 (tools) |
| `workspace_fork` | #38 |

Plus `match_ref` addressing (#34), which is not a tool but closes the search →
patch loop: a caller can hand a `document_search` reference straight to
`document_patch` without copying text.

## 3. Workspace identity: substrate landed (#19)

Python's identity lane (ADR 0046) mints a permanent `f_…` family id and a
`ws_…` workspace id in `workspace.json`; `workspace_adopt`/`workspace_fork`
are built on it. Rust originally had **no equivalent at all**.

**Now landed:** `crates/docx2typed-core/src/workspace_identity.rs` mirrors
`scripts/workspace_identity.py` (`read_identity`, `write_identity`,
`ensure_identity`, `new_family_id`, `new_workspace_id`, the
`docx2typed-workspace-1` schema), verified cross-engine: Rust writes the file,
Python's `read_identity` accepts it, Python's `ensure_identity` returns
`created=false` and the bytes stay unchanged.

So tools 12–13 are no longer "substrate missing" — they are **unexposed
infrastructure**: the remaining work is the MCP surface (`workspace_adopt`
with its adoption-token flow, `workspace_fork` with origin recording).

Rust also has `review/history.rs` (`list`/`read`/`append`) and
`store/versions.rs` (the read-only chain reader behind `history_list`).

## 4. Known non-equivalences among the 44 shared tools

These must be checked per-tool, not assumed equal. Priority suspects named in
the plan plus what was verified here:

| Area | Finding | Evidence |
|---|---|---|
| `insert_paragraph` | Rust writes a **temporary id** (`N{n}`) into the draft; commits via `commit_sync`; the DOCX-build closure is complete (synthesised `<w:p>`, PR 3) | `review/draft.rs` `insert_paragraph`, `core/prose.rs` `apply_structure` |
| `delete_paragraph` | Present in both; Rust removes the paragraph for real and refuses protected structure (section properties, revisions, anchors, tables) | `review/draft.rs` `delete_paragraph`, `core/prose.rs` `structure_reason` |
| build path | `plan_build` applies `islands.json` leaf surgery plus `structure.json` new/deleted paragraphs | `core/lib.rs` `plan_build`, `core/prose.rs` `apply_build` |
| `verify_output` | Rust `verify` crate independently recomputes both edited paths; no-op only when neither sidecar exists | `verify/lib.rs` `verify_edited` |

## 5. Reference defects — must NOT be copied

Per the plan, Python's behaviour is the *behavioural* reference, not always the
*correct* one. Recorded here so they are not blindly required of Rust:

1. `w:rPr` child order on synthesized `vertAlign` — **fixed** in Python
   (#98, `typed_core.insert_rpr_child`). Rust must implement the **correct**
   order, not reproduce the old `append`.
2. Paragraph style inheritance can produce unintended styles (plan §1
   "Python 已知风险"). Listed as `known-reference-defect`; out of Phase 1 scope
   beyond compatibility.

## 6. Former CI blockers — all resolved (#18)

| Blocker | Location | Resolution |
|---|---|---|
| Clippy `too_many_arguments` (8/7) | `core/prose.rs` `tracked_run_replacement`; `review/draft.rs` `replace_text_with_options` + `record_island_operation` | parameter structs (`TextEdit`/`EditTracking`), not lint suppression |
| Python-oracle differential failures (5) | `rust-tests/migrate.rs` | Rust emitted a 15th inspect asset row (`islands.json`) the reference has no concept of; entry removed |
| `clippy::manual_contains` | `src/mcp.rs` | `TOOL_NAMES.contains(&s)` |
| `clippy::useless_format` | `store/store.rs` | literal, not `format!` |
| Local/CI clippy disagreement | toolchain | local was 1.97.1, CI `stable` is 1.99.0 → `rustup update stable` |
| `cargo check -p docx2typed-core` | — | passes |

All seven CI jobs are green as of #20/#21.

## 7. Ordered PR plan (dependency-sorted, with live status)

| PR | Scope | Depends on | Status |
|---|---|---|---|
| **1** | this lock + matrix + repro scripts | — | **done** (#17) |
| **2** | CI health: clippy, fmt, toolchain parity, inspect asset parity | 1 | **done** (#18) |
| **3** | structural closure: insert/delete paragraph → real DOCX build → verify | 2 | **done** (#27, with #26 fixed) |
| **4** | high-frequency Agent tools: `document_read`/`search`/`patch`, `format_span` | 3 | **3 of 4 done** — `document_read` #23/#32, `document_search` #29/#31, `document_patch` #30/#32, `match_ref` #34; `format_span` blocked on #33 |
| **5** | versions & workspace: `history_*`, `workspace_adopt/fork` | 3 | partial: `history_list` #20, identity substrate #19; `history_diff/blame/verify` + `workspace_adopt/fork` open |
| **6** | verification evidence + release qualification + real-DOCX acceptance | 4, 5 | pending |

Each PR carries: Python reference location, behaviour spec, positive / refusal /
misuse cases, output-diff explanation, test command, CI result.

**Toolchain note (learned in PR 2):** verify with the same compiler CI uses.
A local/CI `clippy` mismatch (1.97 vs 1.99) made green local runs fail CI
repeatedly. `rustup update stable` first, then run all three checks.
