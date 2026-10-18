"""Freeze + verify the real-docx corpus (real-docx-corpus-v1).

The corpus is frozen from EXACTLY the approved snapshot inventory
(217 SHA-deduplicated inputs). This tool never rescans, never substitutes
inputs, and never deduplicates similar-but-different versions: only exact
SHA-256-of-original-bytes duplicates collapse, and the approved snapshot
already holds 217 unique hashes.

Public manifest (committable)::

    schema = 'real-docx-corpus-manifest-1'
    corpus_id = 'real-docx-corpus-v1'
    document_count = 217
    dedup_rule = exact SHA-256 of original bytes
    documents[] = {document_id, source_path, sha256, size_bytes, features}

``document_id`` is ``'doc-' + sha256``: stable under inventory reordering.
``source_path`` is always ``originals/<sha256>.docx`` relative to the
manifest directory. Original filesystem paths (private) live only in
``provenance-private.json`` in the external archive root, never in the
public manifest.

Feature counts are deterministic ZIP/XML inspections of the source bytes.
``opaque_candidates`` counts structural candidates (field runs, math,
embedded objects, ...) and is explicitly NOT a claim about engine
supportedness.

CLI::

    python qualification/real_docx_corpus.py freeze --inventory INV --out MANIFEST
    python qualification/real_docx_corpus.py verify --manifest MANIFEST [--root DIR]

Library interface (imported by the Scanner owner)::

    load_manifest(path) -> dict
    validate_manifest(path) -> dict
    resolve_source(manifest_path, entry) -> Path
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import sys
import zipfile
import xml.etree.ElementTree as ET
from pathlib import Path, PurePosixPath

SCHEMA = "real-docx-corpus-manifest-1"
CORPUS_ID = "real-docx-corpus-v1"
EXPECTED_DOCUMENTS = 217
DEDUP_RULE = "exact-sha256-of-original-bytes; similar-versions-retained"
ARCHIVE_SUBDIR = "originals"
PRIVATE_PROVENANCE_NAME = "provenance-private.json"
PRIVATE_PROVENANCE_SCHEMA = "real-docx-corpus-provenance-private-1"

W_NS = "http://schemas.openxmlformats.org/wordprocessingml/2006/main"
M_NS = "http://schemas.openxmlformats.org/officeDocument/2006/math"
_REVISION_LOCALS = frozenset(
    {
        "ins",
        "del",
        "moveFrom",
        "moveTo",
        "moveFromRangeStart",
        "moveFromRangeEnd",
        "moveToRangeStart",
        "moveToRangeEnd",
    }
)
# Structural candidates that the engine may treat as opaque. This is a
# deterministic candidate count, NOT a claim of engine supportedness.
_OPAQUE_CANDIDATES = frozenset(
    {
        (W_NS, "fldSimple"),
        (W_NS, "object"),
        (W_NS, "pict"),
        (W_NS, "altChunk"),
        (M_NS, "oMath"),
        (M_NS, "oMathPara"),
    }
)
_FEATURE_KEYS = ("tables", "revisions", "sdt", "opaque_candidates", "media", "parse_error")


class ManifestError(ValueError):
    """Raised when a manifest or inventory fails structural validation."""


class FreezeError(ValueError):
    """Raised when freezing cannot proceed without breaking immutability."""


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def extract_features(docx_path: Path) -> dict:
    """Deterministic ZIP/XML feature counts for one DOCX file.

    Never raises: unreadable or malformed inputs yield zero counts plus an
    explicit ``parse_error`` code.
    """
    features: dict = {
        "tables": 0,
        "revisions": 0,
        "sdt": 0,
        "opaque_candidates": 0,
        "media": 0,
        "parse_error": None,
    }
    try:
        archive = zipfile.ZipFile(docx_path)
    except zipfile.BadZipFile:
        features["parse_error"] = "not-zip"
        return features
    except OSError:
        features["parse_error"] = "unreadable"
        return features
    with archive:
        names = archive.namelist()
        features["media"] = sum(
            1 for n in names if n.startswith("word/media/") and not n.endswith("/")
        )
        try:
            raw = archive.read("word/document.xml")
        except KeyError:
            features["parse_error"] = "missing-document-xml"
            return features
        try:
            root = ET.fromstring(raw)
        except ET.ParseError:
            features["parse_error"] = "xml-parse-error"
            return features
        tables = revisions = sdt = opaque = 0
        for element in root.iter():
            tag = element.tag
            if not isinstance(tag, str) or not tag.startswith("{"):
                continue
            ns, _, local = tag[1:].partition("}")
            if ns == W_NS and local == "tbl":
                tables += 1
            elif ns == W_NS and local == "sdt":
                sdt += 1
            elif ns == W_NS and local in _REVISION_LOCALS:
                revisions += 1
            if (ns, local) in _OPAQUE_CANDIDATES:
                opaque += 1
        features.update(
            {
                "tables": tables,
                "revisions": revisions,
                "sdt": sdt,
                "opaque_candidates": opaque,
            }
        )
    return features


def _canonical_bytes(payload: dict) -> bytes:
    return (json.dumps(payload, sort_keys=True, ensure_ascii=False, indent=2) + "\n").encode(
        "utf-8"
    )


def _atomic_write_bytes(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.parent / f"{path.name}.tmp-{os.getpid()}"
    tmp.write_bytes(data)
    os.replace(tmp, path)


def _publish_immutable(path: Path, data: bytes, what: str) -> None:
    """Write ``path`` atomically; refuse to overwrite differing bytes."""
    if path.exists():
        if path.read_bytes() != data:
            raise FreezeError(f"refusing to overwrite a different frozen {what}: {path}")
        return
    _atomic_write_bytes(path, data)


def load_inventory(inventory_path: Path) -> tuple[list[dict], str]:
    raw = inventory_path.read_bytes()
    snapshot_sha256 = hashlib.sha256(raw).hexdigest()
    try:
        data = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ManifestError(f"inventory is not valid JSON: {exc}") from exc
    if not isinstance(data, dict) or not isinstance(data.get("documents"), list):
        raise ManifestError("inventory must be an object with a 'documents' list")
    return data["documents"], snapshot_sha256


def freeze_inventory(
    inventory_path: Path,
    out_manifest: Path,
    expected_count: int = EXPECTED_DOCUMENTS,
) -> dict:
    """Freeze the corpus described by ``inventory_path`` into ``out_manifest``.

    ``expected_count`` is 217 in production; tests override it to freeze
    tiny synthetic corpora.
    """
    inventory_path = Path(inventory_path)
    out_manifest = Path(out_manifest)
    entries, snapshot_sha256 = load_inventory(inventory_path)
    if len(entries) != expected_count:
        raise FreezeError(
            f"inventory holds {len(entries)} entries, expected exactly {expected_count}; "
            "no rescan or substitution allowed"
        )
    # Exact-hash dedup only: same bytes collapse, similar versions never do.
    # The approved snapshot already holds unique hashes, so this is a no-op
    # there and a loud error anywhere else.
    by_sha: dict[str, dict] = {}
    for entry in entries:
        sha = entry.get("sha256")
        if not isinstance(sha, str) or sha in by_sha:
            if sha in by_sha:
                raise FreezeError(f"duplicate exact-sha256 inventory entry: {sha}")
            raise ManifestError(f"inventory entry missing sha256: {entry!r:.120}")
        by_sha[sha] = entry
    if len(by_sha) != expected_count:
        raise FreezeError("exact-hash dedup changed the entry count; refusing")

    archive_dir = out_manifest.parent
    originals_dir = archive_dir / ARCHIVE_SUBDIR
    originals_dir.mkdir(parents=True, exist_ok=True)

    documents: list[dict] = []
    aliases: dict[str, dict] = {}
    for sha in sorted(by_sha):
        entry = by_sha[sha]
        declared_size = entry.get("bytes")
        src_text = entry.get("path")
        if not isinstance(declared_size, int) or not isinstance(src_text, str):
            raise ManifestError(f"inventory entry missing bytes/path: {sha}")
        src = Path(src_text)
        if not src.is_file():
            raise FreezeError(f"original missing, refusing to freeze: {src_text}")
        actual_size = src.stat().st_size
        if actual_size != declared_size:
            raise FreezeError(
                f"original size drift for {sha}: snapshot {declared_size}, disk {actual_size}"
            )
        actual_sha = sha256_file(src)
        if actual_sha != sha:
            raise FreezeError(f"original hash drift for {src_text}: snapshot {sha}")
        dest = originals_dir / f"{sha}.docx"
        if dest.exists():
            if sha256_file(dest) != sha:
                raise FreezeError(
                    f"refusing to overwrite differing archived bytes: {dest}"
                )
        else:
            tmp = originals_dir / f"{sha}.docx.tmp-{os.getpid()}"
            shutil.copyfile(src, tmp)
            os.replace(tmp, dest)
        document_id = f"doc-{sha}"
        documents.append(
            {
                "document_id": document_id,
                "source_path": f"{ARCHIVE_SUBDIR}/{sha}.docx",
                "sha256": sha,
                "size_bytes": declared_size,
                "features": extract_features(dest),
            }
        )
        aliases[document_id] = {
            "snapshot_id": entry.get("id"),
            "original_path": src_text,
            "sha256": sha,
            "size_bytes": declared_size,
        }

    manifest = {
        "schema": SCHEMA,
        "corpus_id": CORPUS_ID,
        "document_count": expected_count,
        "dedup_rule": DEDUP_RULE,
        "snapshot_sha256": snapshot_sha256,
        "provenance": {
            "selection": (
                "frozen from the approved snapshot inventory only; "
                "no rescan, no substitution, no reselection"
            ),
            "snapshot_entries": expected_count,
            "note": (
                "upstream sweep exclusions (including the 10KiB-threshold "
                "rule from the previous inventory) are recorded here as "
                "provenance only; freeze does not reselect inputs"
            ),
            "private_aliases": (
                "original filesystem paths are kept out of this manifest; see "
                f"{PRIVATE_PROVENANCE_NAME} in the external archive root"
            ),
        },
        "features_note": (
            "tables/revisions/sdt/media are deterministic ZIP/XML counts. "
            "opaque_candidates counts structural candidates (field runs, math, "
            "embedded objects) and is explicitly NOT a claim about engine "
            "supportedness. parse_error is null on success, else a short code."
            ),
        "documents": documents,
    }
    _publish_immutable(out_manifest, _canonical_bytes(manifest), "manifest")
    provenance = {
        "schema": PRIVATE_PROVENANCE_SCHEMA,
        "corpus_id": CORPUS_ID,
        "snapshot_sha256": snapshot_sha256,
        "aliases": aliases,
    }
    _publish_immutable(
        archive_dir / PRIVATE_PROVENANCE_NAME,
        _canonical_bytes(provenance),
        "private provenance",
    )
    return manifest


def load_manifest(path: Path) -> dict:
    """Parse a manifest and validate its schema (no file hashing)."""
    path = Path(path)
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ManifestError(f"cannot parse manifest {path}: {exc}") from exc
    if not isinstance(data, dict):
        raise ManifestError("manifest must be a JSON object")
    if data.get("schema") != SCHEMA:
        raise ManifestError(f"manifest schema drift: {data.get('schema')!r}")
    if data.get("corpus_id") != CORPUS_ID:
        raise ManifestError(f"manifest corpus_id drift: {data.get('corpus_id')!r}")
    if not isinstance(data.get("document_count"), int):
        raise ManifestError("manifest document_count must be an int")
    if not isinstance(data.get("documents"), list):
        raise ManifestError("manifest documents must be a list")
    for entry in data["documents"]:
        if not isinstance(entry, dict):
            raise ManifestError("manifest document entry must be an object")
        for key in ("document_id", "source_path", "sha256", "size_bytes", "features"):
            if key not in entry:
                raise ManifestError(f"manifest entry missing {key}")
        if not isinstance(entry["features"], dict):
            raise ManifestError("manifest entry features must be an object")
    return data


def resolve_source(manifest_path: Path, entry: dict) -> Path:
    """Resolve an entry's archived file under the manifest directory.

    Rejects absolute paths and ``..`` traversal; raises ValueError.
    """
    rel = entry.get("source_path", "")
    if not isinstance(rel, str) or not rel:
        raise ValueError("entry source_path must be a non-empty string")
    posix = PurePosixPath(rel)
    if posix.is_absolute() or ".." in posix.parts:
        raise ValueError(f"refusing traversing source_path: {rel!r}")
    return Path(manifest_path).parent.joinpath(*posix.parts)


def _check_documents(manifest: dict) -> list[str]:
    problems: list[str] = []
    documents = manifest["documents"]
    if manifest["document_count"] != len(documents):
        problems.append(
            f"count drift: declared {manifest['document_count']}, "
            f"listed {len(documents)}"
        )
    seen_ids: set[str] = set()
    seen_shas: set[str] = set()
    for index, entry in enumerate(documents):
        sha = entry.get("sha256", "")
        if entry.get("document_id") != f"doc-{sha}":
            problems.append(f"entry {index}: unstable document_id {entry.get('document_id')!r}")
        if sha in seen_shas:
            problems.append(f"entry {index}: duplicate sha256 {sha}")
        seen_shas.add(sha)
        if entry.get("document_id") in seen_ids:
            problems.append(f"entry {index}: duplicate document_id")
        seen_ids.add(entry.get("document_id"))
        want_source = f"{ARCHIVE_SUBDIR}/{sha}.docx"
        if entry.get("source_path") != want_source:
            problems.append(
                f"entry {index}: source_path drift {entry.get('source_path')!r}"
            )
        features = entry.get("features", {})
        for key in _FEATURE_KEYS:
            if key not in features:
                problems.append(f"entry {index}: features missing {key}")
    return problems


def validate_manifest(path: Path) -> dict:
    """Validate a frozen manifest plus its archived bytes.

    Returns ``{ok, problems, document_count, manifest_sha256}``. Never
    raises on content drift; raises only when the manifest file itself is
    unreadable (missing file).
    """
    return verify_manifest(Path(path), None, None)


def verify_manifest(
    manifest_path: Path,
    root: Path | None = None,
    expected_count: int | None = None,
) -> dict:
    """Like :func:`validate_manifest` but resolves archives under ``root``.

    ``root`` supports a moved archive (no re-enumeration of sources).
    ``expected_count`` additionally pins the declared count (217 default at
    the CLI; ``None`` checks declared-vs-listed consistency only).
    """
    manifest_path = Path(manifest_path)
    base = Path(root) if root is not None else manifest_path.parent
    manifest_sha256 = sha256_file(manifest_path)
    try:
        manifest = load_manifest(manifest_path)
    except ManifestError as exc:
        return {
            "ok": False,
            "problems": [str(exc)],
            "document_count": 0,
            "manifest_sha256": manifest_sha256,
        }
    problems = _check_documents(manifest)
    if expected_count is not None and manifest["document_count"] != expected_count:
        problems.append(
            f"count drift: declared {manifest['document_count']}, "
            f"expected {expected_count}"
        )
    for index, entry in enumerate(manifest["documents"]):
        try:
            rel = PurePosixPath(entry.get("source_path", ""))
            if rel.is_absolute() or ".." in rel.parts:
                raise ValueError(f"refusing traversing source_path: {rel!r}")
            source = base.joinpath(*rel.parts)
        except ValueError as exc:
            problems.append(f"entry {index}: {exc}")
            continue
        if not source.is_file():
            problems.append(f"entry {index}: archived file missing: {entry['source_path']}")
            continue
        if source.stat().st_size != entry.get("size_bytes"):
            problems.append(f"entry {index}: archived size drift")
        elif sha256_file(source) != entry.get("sha256"):
            problems.append(f"entry {index}: archived hash drift")
    return {
        "ok": not problems,
        "problems": problems,
        "document_count": manifest["document_count"],
        "manifest_sha256": manifest_sha256,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    freeze = sub.add_parser("freeze", help="freeze the approved snapshot into a manifest")
    freeze.add_argument("--inventory", required=True, help="approved snapshot JSON path")
    freeze.add_argument("--out", required=True, help="manifest output path")
    freeze.add_argument(
        "--expected-count",
        type=int,
        default=EXPECTED_DOCUMENTS,
        help="exact entry count (default 217; tests only may override)",
    )

    verify = sub.add_parser("verify", help="verify a frozen manifest and its archives")
    verify.add_argument("--manifest", required=True, help="frozen manifest path")
    verify.add_argument(
        "--root",
        default=None,
        help="archive root holding originals/ (default: manifest directory)",
    )
    verify.add_argument(
        "--expected-count",
        type=int,
        default=EXPECTED_DOCUMENTS,
        help="pinned declared count (default 217; tests only may override)",
    )

    args = parser.parse_args(argv)
    try:
        if args.command == "freeze":
            manifest = freeze_inventory(
                Path(args.inventory), Path(args.out), args.expected_count
            )
            print(
                json.dumps(
                    {
                        "ok": True,
                        "document_count": manifest["document_count"],
                        "snapshot_sha256": manifest["snapshot_sha256"],
                        "manifest": str(args.out),
                    }
                )
            )
            return 0
        result = verify_manifest(
            Path(args.manifest),
            Path(args.root) if args.root else None,
            args.expected_count,
        )
        print(json.dumps(result, ensure_ascii=False, indent=2))
        return 0 if result["ok"] else 1
    except (ManifestError, FreezeError, FileNotFoundError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
