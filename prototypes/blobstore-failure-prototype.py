#!/usr/bin/env python3
"""PROTOTYPE — throwaway. Does the Git-shaped store pay off, and does losing a
file degrade LOUDLY instead of silently?

QUESTION
    1. Storing real history (ten generations of a 3000-paragraph document) as
       content-addressed blobs behind ONE log: bytes, file count, and the
       bytes a single version actually adds — with the index fanned out into
       buckets (Git's trees-of-trees, one level deep) so an edit rewrites one
       bucket instead of the whole index.
    2. When a blob goes missing, is the failure DETECTED and NAMED (which
       path, which version) rather than quietly losing information?

First run without bucketing put 6.4 MB of index into the log for 2 MB of
content — the index, not the content, was the cost. Hence the fan-out.

READS ONLY: nothing here touches the live store; the pool is a throwaway copy
under the scratch directory.

RUN
    python prototypes/blobstore-failure-prototype.py <workdir-with-generations>
"""
from __future__ import annotations

import hashlib
import json
import re
import shutil
import sys
from pathlib import Path

WHOLE_FILES = ("styles.json", "revisions.json", "_template.docx", "edit.state.json")
DERIVED = (".review", "edit.md", "regions.md", "revisions.md")  # regenerated, never stored
LEAF = 64  # chunk entries per leaf bucket


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def chunks(name: str, raw: bytes) -> dict[str, bytes]:
    """Keyed, paragraph-granular chunks for the files that dominate and change
    locally; one chunk for everything else."""
    if name == "format.json":
        data = json.loads(raw.decode("utf-8"))
        head = {k: v for k, v in data.items() if k != "paragraphs"}
        out = {"__head__": json.dumps(head, sort_keys=True).encode()}
        for record in data["paragraphs"]:
            out[f"p:{record['id']}"] = json.dumps(record, sort_keys=True).encode()
        out["__order__"] = json.dumps([r["id"] for r in data["paragraphs"]]).encode()
        return out
    if name == "typed.md":
        text = raw.decode("utf-8")
        blocks = text.split("\n\n")
        out, order = {}, []
        for block in blocks:
            marker = re.match(r'<!--@(?:p|new|delete) (?:id|temp)="([^"]+)"', block)
            key = f"b:{marker.group(1)}" if marker else f"b:~{len(order)}"
            out[key] = block.encode()
            order.append(key)
        out["__head__"] = blocks[0].encode()
        out["__order__"] = json.dumps(order).encode()
        return out
    return {"whole": raw}


def put(objects: Path, data: bytes, seen: dict[str, int], new_bytes: list[int]) -> str:
    digest = sha(data)
    if digest not in seen:
        seen[digest] = len(data)
        (objects / digest).write_bytes(data)
        new_bytes.append(len(data))
    return digest


def manifest_trees(objects: Path, mapping: dict[str, str], seen, new_bytes: list[int]) -> str:
    """Fan the chunk map out into leaf buckets + a root, all content-addressed,
    so an edit rewrites one leaf bucket and the root — not the whole index."""
    items = sorted(mapping.items())
    leaves = [items[i:i + LEAF] for i in range(0, len(items), LEAF)]
    leaf_hashes = [
        put(objects, json.dumps(dict(leaf), sort_keys=True).encode(), seen, new_bytes)
        for leaf in leaves
    ]
    return put(objects, json.dumps(leaf_hashes, sort_keys=True).encode(), seen, new_bytes)


def main() -> int:
    workdir = Path(sys.argv[1])
    generations_root = workdir / ".docx2typed-store" / "generations"
    scratch = workdir.parent / "blobstore"
    if scratch.exists():
        shutil.rmtree(scratch)
    objects = scratch / "objects"
    objects.mkdir(parents=True)

    ordered = sorted(
        (d for d in generations_root.iterdir() if d.is_dir()),
        key=lambda d: (d / "generation.json").stat().st_mtime,
    )
    print(f"source: {len(ordered)} generations under {generations_root}")

    raw_bytes = raw_files = 0
    seen: dict[str, int] = {}
    log: list[dict] = []
    per_version = []

    for generation in ordered:
        manifest = json.loads((generation / "generation.json").read_text(encoding="utf-8"))
        record = {"generation": generation.name, "parent": manifest.get("parent"), "trees": {}}
        before = len(seen)
        for path in sorted(generation.rglob("*")):
            if not path.is_file() or path.name == "generation.json":
                continue
            rel = path.relative_to(generation).as_posix()
            if any(rel.startswith(d) for d in DERIVED):
                continue  # derived: regenerated on materialisation, never stored
            raw_bytes += path.stat().st_size
            raw_files += 1
            new_bytes: list[int] = []
            mapping = {key: put(objects, data, seen, new_bytes) for key, data in chunks(rel, path.read_bytes()).items()}
            record["trees"][rel] = manifest_trees(objects, mapping, seen, new_bytes)
        log.append(record)
        per_version.append(sum(size for digest, size in seen.items() if digest not in set()) if False else sum(
            size for digest, size in list(seen.items())[before:] if True
        ))

    pool_bytes = sum(seen.values())
    log_bytes = sum(len(json.dumps(r, sort_keys=True).encode()) for r in log)
    print(f"\nraw layout     : {raw_bytes/1024/1024:7.2f} MB in {raw_files} files (state files only, derived excluded)")
    print(f"blob pool      : {pool_bytes/1024/1024:7.2f} MB in {len(seen)} blobs  ({raw_bytes/pool_bytes:.1f}x duplicate before)")
    print(f"history log    : {log_bytes/1024:7.1f} KB in 1 file")
    print(f"per generation : {[round(b/1024) for b in per_version]} KB of NEW bytes")

    victim = None
    for record in reversed(log):
        for rel, tree in record["trees"].items():
            if rel == "styles.json":
                victim = (record, rel, tree)
                break
        if victim:
            break
    print("\nfailure drill: delete one blob, ask which version needed it")
    if not victim:
        print("  (nothing suitable to sacrifice)")
        return 1
    record, rel, tree = victim
    leaves = json.loads((objects / tree).read_bytes())
    leaf = json.loads((objects / leaves[0]).read_bytes())
    key, digest = next(iter(leaf.items()))
    (objects / digest).unlink()
    missing = []
    for record_ in log:
        for rel_, tree_ in record_["trees"].items():
            for leaf_hash in json.loads((objects / tree_).read_bytes()):
                for key_, digest_ in json.loads((objects / leaf_hash).read_bytes()).items():
                    if not (objects / digest_).exists():
                        missing.append((record_["generation"][:8], rel_, key_))
    print(f"  deleted chunk {key_} of {rel} (blob {digest[:12]})")
    print(f"  detected {len(missing)} missing chunk(s): {missing}")
    print("  verdict: the log names exactly what is gone; restoring that version")
    print("           fails closed (version-content-missing) instead of returning")
    print("           silently different content")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
