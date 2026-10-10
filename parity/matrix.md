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
> **PR 2 must decide the oracle-bump policy before touching any behaviour that
> `main` fixed after 2026-08-14.**

## 1. Surface counts (verified, not estimated)

| Surface | Python | Rust | Gap |
|---|---|---|---|
| MCP tools | **49** | **36** | 13 missing, 0 extra |
| CLI commands | **8** | **8** | **at parity** |

The CLI is already at parity: both declare exactly
`build, decide, edit, extract, inspect, migrate, validate, verify`
(Python `scripts/protocol.py`, Rust `crates/docx2typed-protocol/src/descriptor.rs`).

## 2. The 13 missing MCP tools

Name equality is not functional equality; each row lists the Python definition
site and the Rust crate that must own the capability. Status values:
`missing` (no Rust surface), `partial` (infrastructure exists, not exposed),
`equivalent`.

| # | Tool | Python site | Rust infrastructure | Status |
|---|---|---|---|---|
| 1 | `document_read` | `mcp_server.py:3462` | `core/document_projection.rs`, `core/inspect.rs` | `partial` |
| 2 | `document_search` | `mcp_server.py:3580` | `core/govern.rs`, `document_projection.rs` | `partial` |
| 3 | `document_replace` | `mcp_server.py:4947` | — | `missing` |
| 4 | `document_patch` | `mcp_server.py:5232` | region/leaf machinery in `review/draft.rs` | `partial` |
| 5 | `format_span` | `mcp_server.py:3783` | run-properties in `core/prose.rs` | `partial` |
| 6 | `history_list` | `mcp_server.py:5934` | `review/history.rs:163 list()` | `partial` |
| 7 | `history_diff` | `mcp_server.py:5865` | — | `missing` |
| 8 | `history_blame` | `mcp_server.py:5886` | — | `missing` |
| 9 | `history_verify` | `mcp_server.py:5905` | — | `missing` |
| 10 | `history_restore` | `mcp_server.py:5951` | `store/store.rs` (generations) | `missing` |
| 11 | `history_gc` | `mcp_server.py:5917` | `store/store.rs` | `missing` |
| 12 | `workspace_adopt` | `mcp_server.py:2935` | **none** (see §3) | `missing` |
| 13 | `workspace_fork` | `mcp_server.py:2984` | **none** (see §3) | `missing` |

## 3. Structural finding: workspace identity is absent from Rust

`grep -rn "family_id\|workspace_id\|workspace.json" crates/` returns **nothing**.

Python's identity lane (ADR 0046) mints a permanent `f_…` family id and a
`ws_…` workspace id in `workspace.json`, and `workspace_adopt`/`workspace_fork`
are built on it. Rust has **no equivalent**, so tools 12–13 are not "unexposed
infrastructure" — the substrate itself is missing. This is larger than a tool
addition and should be scoped as its own slice.

Rust does have `review/history.rs` (`list`/`read`/`append`) — the *version*
lane exists, but only as history records, not as family/observation identity.

## 4. Known non-equivalences among the 36 shared tools

These must be checked per-tool, not assumed equal. Priority suspects named in
the plan plus what was verified here:

| Area | Finding | Evidence |
|---|---|---|
| `insert_paragraph` | Rust writes a **temporary id** (`N{n}`) into the draft; commits via `commit_sync`; the plan reports the DOCX-build closure is incomplete | `review/draft.rs:574` returns `{"temp_id": …, "next": "commit_sync allocates the formal paragraph ID"}` |
| `delete_paragraph` | Present in both; Rust must be checked for real removal + protected-structure refusal | `review/draft.rs` |
| build path | `plan_build` is reported to treat `islands.json` local text edits as the main supported path | `core/lib.rs:344 plan_build`, `core/prose.rs:30 ISLANDS_FILE` |
| `verify_output` | Rust `verify` crate is documented as no-op without `islands.json` | `verify/lib.rs:10` |

## 5. Reference defects — must NOT be copied

Per the plan, Python's behaviour is the *behavioural* reference, not always the
*correct* one. Recorded here so they are not blindly required of Rust:

1. `w:rPr` child order on synthesized `vertAlign` — **fixed** in Python
   (#98, `typed_core.insert_rpr_child`). Rust must implement the **correct**
   order, not reproduce the old `append`.
2. Paragraph style inheritance can produce unintended styles (plan §1
   "Python 已知风险"). Listed as `known-reference-defect`; out of Phase 1 scope
   beyond compatibility.

## 6. Red CI / first blockers (PR 2 targets)

| Blocker | Location | Status |
|---|---|---|
| Clippy `too_many_arguments` (8/7) | `core/prose.rs:1330` `tracked_run_replacement` | **reproduced** (`cargo clippy -p docx2typed-core -- -D warnings`) |
| Python-oracle differential failures | `rust-tests/migrate.rs`, `differential.rs` | reported (Linux 4 / Windows 5); needs structured JSON-path diff, not assertion deletion |
| `cargo check -p docx2typed-core` | — | **passes** (11 s) |

## 7. Ordered PR plan (dependency-sorted)

| PR | Scope | Depends on |
|---|---|---|
| **1** | this lock + matrix + repro scripts | — |
| **2** | CI health: clippy fix, migrate/differential normalisation (no semantic change) | 1 |
| **3** | structural closure: insert/delete paragraph → real DOCX build → verify | 2 |
| **4** | high-frequency Agent tools: `document_read/search/patch/replace`, `format_span` | 3 |
| **5** | versions & workspace: `history_*`, `workspace_adopt/fork` (+ identity substrate) | 3 |
| **6** | verification evidence + release qualification + real-DOCX acceptance | 4, 5 |

Each PR carries: Python reference location, behaviour spec, positive / refusal /
misuse cases, output-diff explanation, test command, CI result.
