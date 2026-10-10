---
name: docx2typed
description: >
  Structure-preserving DOCX text editing with a signed Rust CLI, 36-tool MCP
  review lane, byte-preserving builds, tracked revisions, comments, tables,
  content controls, and independent verification. Use when a DOCX must change
  without losing formatting or untouched package parts, when a human review
  console is needed, or when an agent must install/configure docx2typed.
---

# docx2typed — Rust DOCX editing skill

The contract is **structure and byte fidelity**: untouched content and package
parts stay unchanged; only requested text or explicit structure operations move.
The production runtime is the signed Rust binary. The Python package is an
offline reference/oracle for development qualification only.

## Skill graph

```text
SKILL.md ─────────────── hub: invocation, rules, branch table, gates
├── capabilities.md ── CLI + MCP atoms and exact syntax
├── composites.md ──── ordered document workflows and playbooks
└── verification.md ── shared acceptance and repository gates
```

Read one layer at a time: choose a branch here, then open the referenced
runtime document. Keep the source DOCX, workdir, and output DOCX distinct.

## Branch table

| Task | Start here | Flow |
|---|---|---|
| Install or configure an agent | `Installation.md` | authorize → install Rust binary → configure MCP → verify |
| Edit ordinary text | `composites.md` Workflow 1 | extract → enumerate/MCP edit → build → verify |
| Edit with tracked revisions | Workflow 2 | open with track mode → edit → build → verify |
| Accept/reject revisions | Workflow 3 | `revisions` → `decide` → new baseline → verify |
| Review or delete comments | Workflow 4 | `comment`/MCP review → explicit deletion only → verify |
| Change table structure | Workflow 5 | `decide table-*` or MCP table tools → new baseline → verify |
| Audit Unicode vertical forms | Workflow 6 | `audit` scan → human policy → apply path → verify |
| Edit content-control text | Workflow 7 | enumerate leaf → scoped edit → build → verify |
| Human browser review | Playbook D | `review` server → queue decisions → MCP apply → build/verify |

If the target paragraph or leaf is unclear, use `docx2typed enumerate <source>`
or MCP `list_paragraphs`/`get_paragraph`. The Rust binary has no Python CLI
fallback and no separate `--help` command; use `capabilities.md` for syntax.

## Model-tier guidance (适配所有模型)

The MCP loop is designed so a small (≤8B) model can complete it. Match the
surface to the model:

| Model tier | Surface | Rules |
|---|---|---|
| Small (≤8B, e.g. qwen3:4b) | MCP only, subset of tools | Expose ONLY the loop tools: `workdir_open`, `workdir_status`, `list_paragraphs`, `get_paragraph`, `replace_text`, `diff_preview`, `commit_sync`, `build_docx`, `verify_output`. Hide revision/table/review tools — they need fingerprint judgment a small model does not have. |
| Mid (30–70B) | MCP full surface | All 36 tools; still one tool call per step. |
| Large / frontier agent | CLI + MCP | May batch reads, but edits stay on the MCP lane. |

Prompt rules that make small models reliable (verified with qwen3:4b on a
real patent DOCX):

1. State the task as ONE paragraph replacement: paragraph id, exact old
   text, exact new text. Never "improve this section".
2. Require the fixed order: read → edit → diff_preview → commit_sync →
   build_docx → verify_output.
3. Tell it operation_id must be unique per call; when it hits
   `operation-id-reused`, the fix is a NEW id, not a retry.
4. Tell it failures are informative: report the diagnostic code, do not
   invent success.

## Engine consistency (引擎一致性 — hard rule)

One workdir, one engine for its whole life. The Rust and Python engines
write different sidecar grammars; cross-engine reuse fails closed:

- Extract AND edit AND build through the same engine. If the workdir was
  extracted by the Rust binary (`run.evidence.json` present), drive it only
  through the Rust MCP/CLI. A Python-extracted workdir (no
  `run.evidence.json`) goes to the Python server.
- Symptom of violation: `template part fingerprints changed after extract`
  or `edit-grammar-invalid` on open.
- The source DOCX changing after extract is baseline drift: re-extract into
  a NEW workdir; never edit a stale one.

## Runtime and installation

Use the host's normal skill manager for this file. When installation is
authorized, follow `Installation.md`; preserve existing skills and MCP entries.
Production MCP must point to the installed binary's absolute path:

```json
{
  "mcpServers": {
    "docx2typed": {
      "command": "C:\\Users\\<you>\\AppData\\Local\\docx2typed\\bin\\docx2typed.exe",
      "args": ["mcp"]
    }
  }
}
```

Do not configure `python`, `uvx`, a relative executable, or a repository import
as the production server. The source checkout's Python implementation remains
an offline oracle and diagnostic rollback asset.

## Workdir contract

`extract` creates a self-contained workdir. Treat it as one unit:

| File | Purpose | Editable |
|---|---|---|
| `typed.md` | canonical typed source | only through supported edit seams |
| `format.json` | paragraph skeleton, fingerprints, token records | no |
| `styles.json` | content-addressed style registry | no |
| `_template.docx` | immutable source package | no |
| `edit.md` / `edit.state.json` | draft projection and freshness binding, when present | through edit seam |
| `islands.json` | committed text edits the build applies and verify re-checks | no (written by `commit_sync`) |
| `structure.json` | committed new/deleted paragraphs (synthesis + removal) the build applies and verify re-checks | no (written by `commit_sync`) |

Never mix sidecars from different documents. Never overwrite the original DOCX.
Use a new output path and, for decisions/table operations, a new workdir-out.

## Editing rules

- Keep typed markers, structural tokens, opaque nodes, anchors, and revision
  containers unchanged unless the requested operation explicitly targets them.
- Use leaf paths from `enumerate` (for example `P0.0` or
  `T0.R1.C1.P0.0`) for `edit text`; edits are island-local and reject opaque or
  ambiguous ranges.
- Existing comments remain by default. Delete one only on explicit user
  instruction.
- Table operations change structure only. They do not copy or rewrite cell
  text. Merge fails closed if it would discard text unless the user explicitly
  supplies `--discard-content` or `discard_content=true`.
- `build` and `verify` consume the same clean workdir. A dirty, stale, or
  conflicting edit state is a hard failure; there is no bypass flag.

## Human review surface

The local Rust review server is a review and handoff surface, not a DOCX writer:

```powershell
docx2typed review workdir --host 127.0.0.1 --port 8876
```

The human can inspect revisions/comments, accept/reject/defer, and submit
source-anchored patches. The browser queues decisions; the agent reads
`review_inbox`, checks `review_preflight`, applies the queue transactionally,
refreshes the review snapshot, and reports remaining work. There is no Rust
`--tailscale` flag; remote access requires an explicitly controlled private
interface and network ACLs.

## Core execution protocol

1. **Intake** — identify source DOCX, requested outcome, direct/tracked mode,
   comment policy, table scope, and whether browser review is wanted.
2. **Protect** — copy or reference the source through a new workdir; record the
   starting inspect/inventory result before editing.
3. **Plan** — enumerate the document, select exact paragraph/leaf paths, and
   state the intended change set.
4. **Edit** — use `edit text` for one leaf or MCP region-scoped tools for an
   agent session. Keep each round small and inspect the diff before commit.
5. **Review** — when requested, start the local review server and process human
   decisions through the MCP review lane.
6. **Deliver** — build a new DOCX, run independent `verify`, and report output
   path, changed scope, remaining revisions/comments, and any unavailable
   Office check.

Finished means the independent verification gate passes. A browser final view,
clean store generation, or successful `build` alone is not completion.

## Workflow gates

- **Freshness:** `inspect`/MCP status identifies stale or dirty state; build
  refuses non-clean state.
- **Independent verification:** `verify <workdir> <output.docx>` re-derives
  baseline and checks text, style/structure, protected XML, revisions/comments,
  and package-part identity.
- **Byte fidelity:** no-op and untouched parts remain byte-identical; decisions
  and table operations publish a new clean baseline without mutating source.
- **Office boundary:** Word/LibreOffice save/reopen is host-dependent. Run it
  when an actual host exists; otherwise report `not-run-no-host`. This project
  does not automate Office COM.

## Quick command surface

```text
docx2typed --version --json
docx2typed extract INPUT.docx -o WORKDIR --json
docx2typed inspect WORKDIR --json
docx2typed enumerate WORKDIR --json
docx2typed edit text WORKDIR LEAF OLD NEW --json
docx2typed build WORKDIR -o OUTPUT.docx --json
docx2typed verify WORKDIR OUTPUT.docx --json
docx2typed mcp
docx2typed review WORKDIR --host 127.0.0.1 --port 8876
```

MCP uses the frozen 36-tool surface. The normal edit loop is:

```text
workdir_open → list_paragraphs/get_paragraph → replace_text/batch_edit
→ diff_preview → commit_sync → build_docx → verify_output
```

Read [`capabilities.md`](capabilities.md) for operation-specific options and
[`verification.md`](verification.md) before claiming delivery.
