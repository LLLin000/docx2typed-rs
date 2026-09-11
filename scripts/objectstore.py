"""Content-addressed object pool for version history (ADR 0043).

The Store's ``generations/`` lane exists for transaction durability; history
lives here instead. Objects are addressed by
``sha256(object_type + "\\0" + canonical_bytes)``, so the physical layout can
change later (zstd, packs) without any version id changing.

What a tree stores is the CANONICAL state only — ``typed.md`` (per paragraph),
``format.json`` (per paragraph record), ``revisions.json``, ``styles.json``,
``_template.docx``. Derived views (``edit.md``, ``regions.md``,
``revisions.md``, ``.review/snapshots/*``) are regenerated on materialisation
and never stored: that is where most of the old per-generation bytes went.

Objects:
    blob:<sha>      raw bytes of one asset or one chunk
    map:<sha>       bucketed {key -> blob id} lookup
    tree:<sha>      the state manifest: parts -> blobs/maps + the canonical digest
    version:<sha>   a commit: parent, tree, labels, provenance
"""
from __future__ import annotations

import hashlib
import json
import re
import shutil
from pathlib import Path
from typing import Any

OBJECT_DIR = "objects"
BUCKET_ENTRIES = 64  # per-version cost knob: bigger buckets rewrite more bytes
TREE_SCHEMA = "docx2typed-tree-1"
MAP_SCHEMA = "docx2typed-map-1"
BUCKET_SCHEMA = "docx2typed-bucket-1"
COMMIT_SCHEMA = "docx2typed-version-1"

# The assets a tree carries. Keep in sync with store.CANONICAL_ASSETS plus the
# revision inventory (cheap, deduplicated, and read by the review surface).
TREE_ASSETS = ("typed.md", "format.json", "revisions.json", "styles.json", "_template.docx")
TEXT_ASSETS = ("typed.md", "format.json", "revisions.json", "styles.json")


def objects_dir(root: Path) -> Path:
    return Path(root) / ".docx2typed-store" / OBJECT_DIR


# --------------------------------------------------------------------------
# Object identity and storage
# --------------------------------------------------------------------------

def object_id(kind: str, data: bytes) -> str:
    digest = hashlib.sha256()
    digest.update(kind.encode("utf-8"))
    digest.update(b"\0")
    digest.update(data)
    return digest.hexdigest()


def object_path(root: Path, kind: str, digest: str) -> Path:
    return objects_dir(root) / kind / digest[:2] / digest[2:]


def put(root: Path, kind: str, data: bytes) -> tuple[str, bool]:
    """Store one object; returns (id, was_new). Identical content is stored once."""
    digest = object_id(kind, data)
    path = object_path(root, kind, digest)
    if path.exists():
        return digest, False
    path.parent.mkdir(parents=True, exist_ok=True)
    staged = path.with_name(path.name + ".tmp")
    staged.write_bytes(data)
    staged.replace(path)
    return digest, True


def get(root: Path, kind: str, digest: str) -> bytes | None:
    path = object_path(root, kind, digest)
    try:
        return path.read_bytes()
    except OSError:
        return None


def has(root: Path, kind: str, digest: str) -> bool:
    return object_path(root, kind, digest).exists()


def _json_bytes(value: Any) -> bytes:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode("utf-8")


# --------------------------------------------------------------------------
# Bucketed maps: only the bucket an edit touches is rewritten
# --------------------------------------------------------------------------

def put_map(root: Path, items: dict[str, str]) -> str:
    """{key -> blob id}, fanned out into buckets so an edit rewrites one bucket."""
    keys = sorted(items)
    buckets: list[str] = []
    for start in range(0, len(keys), BUCKET_ENTRIES):
        bucket = {key: items[key] for key in keys[start:start + BUCKET_ENTRIES]}
        digest, _ = put(root, "map", _json_bytes({"schema": BUCKET_SCHEMA, "entries": bucket}))
        buckets.append(digest)
    digest, _ = put(root, "map", _json_bytes({"schema": MAP_SCHEMA, "buckets": buckets}))
    return digest


def read_map(root: Path, digest: str) -> dict[str, str]:
    raw = get(root, "map", digest)
    if raw is None:
        raise KeyError(f"map object {digest[:12]} is missing")
    data = json.loads(raw.decode("utf-8"))
    if data.get("schema") == MAP_SCHEMA:
        merged: dict[str, str] = {}
        for bucket in data.get("buckets") or []:
            merged.update(read_map(root, bucket))
        return merged
    if data.get("schema") == BUCKET_SCHEMA:
        return {str(k): str(v) for k, v in (data.get("entries") or {}).items()}
    raise KeyError(f"object {digest[:12]} is not a map")


# --------------------------------------------------------------------------
# Splitting canonical assets into stable, paragraph-keyed chunks
# --------------------------------------------------------------------------

_PARAGRAPH_MARKER = re.compile(r'<!--@(?:p|new|delete) (?:id|temp)="([^"]+)"')


def split_typed(text: str) -> tuple[str, list[str], dict[str, str]] | None:
    """(header, order, {key -> block}) for the canonical typed.md.

    Blocks are keyed by paragraph identity, so inserting a paragraph does not
    churn the objects of the paragraphs after it."""
    blocks = text.split("\n\n")
    if not blocks or not blocks[0].startswith("<!--@typed"):
        return None
    header, body = blocks[0], blocks[1:]
    order: list[str] = []
    chunks: dict[str, str] = {}
    for index, block in enumerate(body):
        if not block.strip() and not chunks:
            continue
        marker = _PARAGRAPH_MARKER.match(block)
        key = marker.group(1) if marker else f"~{index}"
        chunks[key] = block
        order.append(key)
    return header, order, chunks


def join_typed(header: str, order: list[str], chunks: dict[str, str]) -> str:
    return "\n\n".join([header] + [chunks[key] for key in order]) + "\n"


def split_format(text: str) -> tuple[dict[str, Any], list[str], dict[str, str]] | None:
    """(global, order, {paragraph id -> record json}) for format.json."""
    try:
        data = json.loads(text)
    except json.JSONDecodeError:
        return None
    if not isinstance(data, dict) or "paragraphs" not in data:
        return None
    global_part = {k: v for k, v in data.items() if k != "paragraphs"}
    records: dict[str, str] = {}
    order: list[str] = []
    for record in data["paragraphs"] or []:
        identifier = str(record.get("id"))
        records[identifier] = json.dumps(record, ensure_ascii=False, sort_keys=True)
        order.append(identifier)
    return global_part, order, records


def join_format(global_part: dict[str, Any], order: list[str], records: dict[str, str]) -> str:
    data = dict(global_part)
    data["paragraphs"] = [json.loads(records[key]) for key in order]
    return json.dumps(data, ensure_ascii=False, indent=2, sort_keys=True) + "\n"


# --------------------------------------------------------------------------
# Trees
# --------------------------------------------------------------------------

def _parts(root: Path, state_dir: Path) -> dict[str, Any] | None:
    """Split every canonical asset; None when a split would not round-trip."""
    parts: dict[str, Any] = {}
    for name in TEXT_ASSETS:
        path = state_dir / name
        if not path.is_file():
            continue
        text = path.read_text(encoding="utf-8")
        if name == "typed.md":
            split = split_typed(text)
            if split is None:
                return None
            header, order, chunks = split
            if join_typed(header, order, chunks) != text:
                return None
            parts[name] = {
                "header": put(root, "blob", header.encode("utf-8"))[0],
                "order": put(root, "blob", _json_bytes(order))[0],
                "chunks": put_map(root, {key: put(root, "blob", value.encode("utf-8"))[0] for key, value in chunks.items()}),
            }
            continue
        if name == "format.json":
            split = split_format(text)
            if split is None:
                return None
            global_part, order, records = split
            if join_format(global_part, order, records) != text:
                return None
            parts[name] = {
                "global": put(root, "blob", _json_bytes(global_part))[0],
                "order": put(root, "blob", _json_bytes(order))[0],
                "chunks": put_map(root, {key: put(root, "blob", value.encode("utf-8"))[0] for key, value in records.items()}),
            }
            continue
        parts[name] = {"whole": put(root, "blob", text.encode("utf-8"))[0]}
    for name in TREE_ASSETS:
        path = state_dir / name
        if path.is_file() and name not in parts:
            parts[name] = {"whole": put(root, "blob", path.read_bytes())[0]}
    return parts


def build_tree(root: Path, state_dir: Path, *, digest: str | None = None) -> dict[str, Any]:
    """Store one canonical state as objects; returns {tree, digest, parts}."""
    parts = _parts(root, state_dir)
    if parts is None:  # a state this splitter cannot round-trip stays whole
        parts = {
            name: {"whole": put(root, "blob", (state_dir / name).read_bytes())[0]}
            for name in TREE_ASSETS
            if (state_dir / name).is_file()
        }
    tree = {"schema": TREE_SCHEMA, "parts": parts}
    if digest is not None:
        tree["digest"] = digest
    tree_id, _ = put(root, "tree", _json_bytes(tree))
    return {"tree": tree_id, "digest": digest, "parts": parts}


def read_tree(root: Path, tree_id: str) -> dict[str, Any]:
    raw = get(root, "tree", tree_id)
    if raw is None:
        raise KeyError(f"tree object {tree_id[:12]} is missing")
    return json.loads(raw.decode("utf-8"))


def materialize(root: Path, tree_id: str, destination: Path) -> list[str]:
    """Write a tree's canonical assets into ``destination``; returns their names."""
    tree = read_tree(root, tree_id)
    written: list[str] = []
    destination.mkdir(parents=True, exist_ok=True)
    for name, part in sorted((tree.get("parts") or {}).items()):
        target = destination / name
        target.parent.mkdir(parents=True, exist_ok=True)
        if "whole" in part:
            payload = get(root, "blob", part["whole"])
            if payload is None:
                raise KeyError(f"{name} blob is missing")
            target.write_bytes(payload)
            written.append(name)
            continue
        order_raw = get(root, "blob", part["order"])
        if order_raw is None:
            raise KeyError(f"{name} chunk order is missing")
        order = json.loads(order_raw.decode("utf-8"))
        chunks = {key: get(root, "blob", digest) for key, digest in read_map(root, part["chunks"]).items()}
        missing = [key for key, payload in chunks.items() if payload is None]
        if missing:
            raise KeyError(f"{name} chunks are missing: {missing[:3]}")
        decoded = {key: payload.decode("utf-8") for key, payload in chunks.items() if payload is not None}
        if name == "typed.md":
            header_raw = get(root, "blob", part["header"])
            if header_raw is None:
                raise KeyError("typed.md header is missing")
            text = join_typed(header_raw.decode("utf-8"), order, decoded)
        elif name == "format.json":
            global_raw = get(root, "blob", part["global"])
            if global_raw is None:
                raise KeyError("format.json global part is missing")
            text = join_format(json.loads(global_raw.decode("utf-8")), order, decoded)
        else:
            text = "\n".join(decoded[key] for key in order)
        target.write_bytes(text.encode("utf-8"))
        written.append(name)
    return written


def verify(root: Path, tree_id: str) -> dict[str, Any]:
    """Every object a tree needs, present? (a missing blob is detected, not silent)"""
    tree = read_tree(root, tree_id)
    missing: list[str] = []
    checked = 0
    for name, part in (tree.get("parts") or {}).items():
        if "whole" in part:
            checked += 1
            if not has(root, "blob", part["whole"]):
                missing.append(f"{name}:blob")
            continue
        order_raw = get(root, "blob", part["order"])
        checked += 1
        if order_raw is None:
            missing.append(f"{name}:order")
        try:
            entries = read_map(root, part["chunks"])
        except KeyError:
            missing.append(f"{name}:map")
            continue
        for key, digest in entries.items():
            checked += 1
            if not has(root, "blob", digest):
                missing.append(f"{name}:{key}")
    return {"schema": "docx2typed-history-verify-1", "tree": tree_id, "checked": checked, "missing": missing, "ok": not missing}


# --------------------------------------------------------------------------
# Commits
# --------------------------------------------------------------------------

def write_commit(root: Path, commit: dict[str, Any]) -> str:
    payload = {"schema": COMMIT_SCHEMA, **commit}
    digest, _ = put(root, "version", _json_bytes(payload))
    return digest


def read_commit(root: Path, commit_id: str) -> dict[str, Any] | None:
    raw = get(root, "version", commit_id)
    return json.loads(raw.decode("utf-8")) if raw is not None else None


def sweep(root: Path, *, keep_trees: set[str]) -> dict[str, Any]:
    """Drop blobs/trees no retained version references; commits are never swept."""
    reachable: set[tuple[str, str]] = set()
    for tree_id in keep_trees:
        try:
            tree = read_tree(root, tree_id)
        except KeyError:
            continue
        reachable.add(("tree", tree_id))
        for part in (tree.get("parts") or {}).values():
            if "whole" in part:
                reachable.add(("blob", part["whole"]))
                continue
            reachable.add(("blob", part["order"]))
            if "header" in part:
                reachable.add(("blob", part["header"]))
            if "global" in part:
                reachable.add(("blob", part["global"]))
            _map_objects(root, part["chunks"], reachable)
    removed = 0
    freed = 0
    base = objects_dir(root)
    for kind in ("blob", "tree", "map"):
        kind_dir = base / kind
        if not kind_dir.is_dir():
            continue
        for path in sorted(kind_dir.rglob("*")):
            if not path.is_file():
                continue
            digest = path.parent.name + path.name
            if (kind, digest) in reachable:
                continue
            freed += path.stat().st_size
            path.unlink()
            removed += 1
    return {"removed": removed, "freed": freed}


def _map_objects(root: Path, map_id: str, reachable: set[tuple[str, str]]) -> list[Any]:
    """Walk a bucketed map, marking every bucket and blob it references."""
    reachable.add(("map", map_id))
    raw = get(root, "map", map_id)
    if raw is None:
        return []
    data = json.loads(raw.decode("utf-8"))
    if data.get("schema") == MAP_SCHEMA:
        for bucket in data.get("buckets") or []:
            _map_objects(root, bucket, reachable)
        return []
    for digest in (data.get("entries") or {}).values():
        reachable.add(("blob", digest))
    return []


def copy_objects(source_root: Path, target_root: Path, objects: set[tuple[str, str]]) -> int:
    """Copy specific objects between pools (used when adopting a baseline)."""
    copied = 0
    for kind, digest in objects:
        destination = object_path(target_root, kind, digest)
        if destination.exists():
            continue
        origin = object_path(source_root, kind, digest)
        if not origin.is_file():
            continue
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(origin, destination)
        copied += 1
    return copied
