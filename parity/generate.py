#!/usr/bin/env python3
"""Regenerate parity/matrix.json and parity/python-lock.json from both repos.

Reads the real surfaces rather than a hand-kept list:

* Python MCP tools  -- ``@mcp.tool()`` decorators in ``scripts/mcp_server.py``
  (plus ``workdir_open``, registered through a distinct decorator form).
* Rust MCP tools    -- keys of ``crates/docx2typed-app/assets/mcp_schemas.json``
  minus the non-tool schema entries.
* CLI commands      -- ``PROTOCOL_COMMANDS`` in each repo.

Usage:
    python parity/generate.py [--python <path>] [--rust <path>]

Exit code 1 if a surface cannot be read, so a broken reader is never mistaken
for "no gap".
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
from datetime import date
from pathlib import Path
from typing import Any

# Schema keys that are not tools.
_NON_TOOL_KEYS = {"table_ref", "workdir_out"}

#: Python definition sites for the tools Rust does not expose yet, and the
#: Rust crate that must own each capability. Kept here (not in the JSON) so the
#: mapping is reviewed in code, and checked against the computed gap in BOTH
#: directions by the check below.
_CAPABILITY_OWNERS = {
    # Still missing from Rust. Entries are removed as each lands --
    # `history_diff`, `history_blame`, `history_verify` and `workspace_fork` went
    # with the commits that wired them, which the both-directions check forces.
    "document_replace": ("mcp_server.py:4947", None, "missing"),
    "history_restore": ("mcp_server.py:5951", "store/store.rs", "missing"),
    "history_gc": ("mcp_server.py:5917", "store/store.rs", "missing"),
    "workspace_adopt": ("mcp_server.py:2935", "identity landed (#19); resolver + tool open", "partial"),
}


def sync_matrix_md(rs: Path, p_tools: set[str], r_tools: set[str], missing: list[str]) -> None:
    """Rewrite the count-bearing lines of matrix.md from the computed surface.

    matrix.json is generated and matrix.md is prose, so every number quoted in
    the prose was hand-maintained and free to drift -- it said "36 tools" and
    "9 missing" while the computed surface was 44 and 5. The prose stays
    hand-written; the numbers are substituted, so they cannot disagree with
    the JSON sitting next to them.
    """
    path = rs / "parity" / "matrix.md"
    text = path.read_text(encoding="utf-8")
    n_missing, n_shared = len(missing), len(r_tools & p_tools)
    subs = [
        # | MCP tools | **49** | **40** | 9 missing, 0 extra |
        (r"(\| MCP tools \| \*\*\d+\*\* \| \*\*)\d+(\*\* \| )[\d]+ missing",
         lambda m: f"{m.group(1)}{len(r_tools)}{m.group(2)}{n_missing} missing"),
        (r"exposes \d+ tools and lacks \*\*exactly the \d+ tools",
         f"exposes {len(p_tools)} tools and lacks **exactly the {n_missing} tools"),
        (r"## 2\. The \d+ missing MCP tools", f"## 2. The {n_missing} missing MCP tools"),
        (r"among the \d+ shared tools", f"among the {n_shared} shared tools"),
    ]
    for pattern, replacement in subs:
        text, count = re.subn(pattern, replacement, text)
        if count != 1:
            raise SystemExit(
                f"error: matrix.md pattern {pattern!r} matched {count} times, expected 1 "
                "-- the prose moved and the substitution would silently stop applying"
            )

    # The table rows come from the same map the check above validates, so a
    # landed tool cannot leave a row behind: the row set IS the map.
    def owner_cell(value: str | None) -> str:
        if not value:
            return "—"
        # Bare paths read as code; prose stays prose.
        return f"`{value}`" if " " not in value else value

    rows = "\n".join(
        f"| {i} | `{name}` | `{_CAPABILITY_OWNERS[name][0]}` | "
        f"{owner_cell(_CAPABILITY_OWNERS[name][1])} | `{_CAPABILITY_OWNERS[name][2]}` |"
        for i, name in enumerate(sorted(missing), 1)
    )
    start = text.index("| # | Tool | Python site | Rust infrastructure | Status |")
    header_end = text.index("\n", text.index("|---|", start)) + 1
    table_end = text.index("\n\n", header_end)
    text = text[:header_end] + rows + text[table_end:]
    path.write_text(text, encoding="utf-8")


def python_tools(repo: Path) -> set[str]:
    src = (repo / "scripts/mcp_server.py").read_text(encoding="utf-8")
    tools = set(re.findall(r"@mcp\.tool\(\)\s*\ndef ([a-z_]+)", src))
    tools.add("workdir_open")  # distinct decorator form
    return tools


def rust_tools(repo: Path) -> set[str]:
    schemas = json.loads(
        (repo / "crates/docx2typed-app/assets/mcp_schemas.json").read_text(encoding="utf-8")
    )
    return {k for k in schemas if k not in _NON_TOOL_KEYS}


def python_cli(repo: Path) -> list[str]:
    src = (repo / "scripts/protocol.py").read_text(encoding="utf-8")
    match = re.search(r"PROTOCOL_COMMANDS = \(([^)]+)\)", src)
    return re.findall(r'"([a-z]+)"', match.group(1)) if match else []


def rust_cli(repo: Path) -> list[str]:
    src = (repo / "crates/docx2typed-protocol/src/descriptor.rs").read_text(encoding="utf-8")
    match = re.search(r"PROTOCOL_COMMANDS[^=]*=\s*\[([^\]]+)\]", src)
    return re.findall(r'"([a-z]+)"', match.group(1)) if match else []


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--python", default="C:/Users/Lin/dev/docx2typed-installed")
    parser.add_argument("--rust", default="C:/Users/Lin/dev/docx2typed-rs")
    parser.add_argument("--json", action="store_true", help="print the matrix and exit")
    args = parser.parse_args()

    py, rs = Path(args.python), Path(args.rust)
    for label, path in (("python", py), ("rust", rs)):
        if not path.is_dir():
            print(f"error: {label} repo not found: {path}", file=sys.stderr)
            return 1

    p_tools, r_tools = python_tools(py), rust_tools(rs)
    p_cli, r_cli = python_cli(py), rust_cli(rs)
    if not p_tools or not r_tools or not p_cli or not r_cli:
        print("error: a surface read returned empty; refusing to report a fake gap", file=sys.stderr)
        return 1

    missing = sorted(p_tools - r_tools)
    untracked = [t for t in missing if t not in _CAPABILITY_OWNERS]
    if untracked:
        print(f"error: missing tools without a capability mapping: {untracked}", file=sys.stderr)
        return 1
    # The other direction, which was unchecked: a mapping whose tool has since
    # landed. Such an entry claims a gap that is already closed, and it is
    # exactly the entry a reader consults to decide what to build next.
    lingering = sorted(t for t in _CAPABILITY_OWNERS if t not in missing)
    if lingering:
        print(
            "error: capability mappings for tools that are no longer missing: "
            f"{lingering} -- remove them; the gap is now {len(missing)}",
            file=sys.stderr,
        )
        return 1

    lock: dict[str, Any] = {
        "schema": "docx2typed-parity-lock-1",
        "created": str(date.today()),
        "reference": {
            "repo": "LLLin000/docx2typed",
            "commit": "1adcc9403cc830e899efe74c5ec41b11973b8c7c",
            "role": "feature baseline: the behaviour Rust must reach (current main)",
        },
        "oracle": {
            "repo": "LLLin000/docx2typed",
            "tag": "v0.1.0-python-final",
            "commit": "c6a17a16963d36e8860f8952a56465e94b3bb53a",
            "role": "CI-pinned oracle: what Rust CI compares against at runtime",
            "pinned_in": ".github/workflows/rust-ci.yml PINNED_PYTHON_REFERENCE_SHA",
            "note": (
                "DISTINCT from the feature reference: 118 commits behind main and "
                "predates the rPr fix (#98). A defect fixed on main is NOT fixed in "
                "the oracle; Rust must implement the correct behaviour and the "
                "oracle must be bumped deliberately."
            ),
        },
        "target": {
            "repo": "LLLin000/docx2typed-rs",
            "commit": "324cd055096e841fd5c97757def982b91ed93e15",
        },
        "surfaces": {
            "mcp_tools": {"python": len(p_tools), "rust": len(r_tools)},
            "cli_commands": {"python": len(p_cli), "rust": len(r_cli)},
        },
        "mcp_gap": {
            "missing_in_rust": missing,
            "only_in_rust": sorted(r_tools - p_tools),
        },
        "cli_gap": {
            "missing_in_rust": sorted(set(p_cli) - set(r_cli)),
            "only_in_rust": sorted(set(r_cli) - set(p_cli)),
        },
        "evidence_hashes": {
            "rust_mcp_schemas": sha256(rs / "crates/docx2typed-app/assets/mcp_schemas.json"),
            "rust_core_prose": sha256(rs / "crates/docx2typed-core/src/prose.rs"),
        },
    }

    matrix: dict[str, Any] = {
        "schema": "docx2typed-parity-matrix-1",
        "reference_commit": lock["reference"]["commit"],
        "target_commit": lock["target"]["commit"],
        "missing_tools": [
            {
                "tool": tool,
                "python_site": _CAPABILITY_OWNERS[tool][0],
                "rust_owner": _CAPABILITY_OWNERS[tool][1],
                "status": _CAPABILITY_OWNERS[tool][2],
            }
            for tool in missing
        ],
    }

    if args.json:
        print(json.dumps(matrix, ensure_ascii=False, indent=2))
        return 0

    (rs / "parity/python-lock.json").write_text(
        json.dumps(lock, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    sync_matrix_md(rs, p_tools, r_tools, missing)
    (rs / "parity/matrix.json").write_text(
        json.dumps(matrix, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(f"python tools={len(p_tools)} rust tools={len(r_tools)} missing={len(missing)}")
    print(f"python cli={len(p_cli)} rust cli={len(r_cli)} at_parity={set(p_cli)==set(r_cli)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
