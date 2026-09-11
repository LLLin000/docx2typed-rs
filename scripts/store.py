"""Issue #50 (issue #34 contract): immutable generations, Writer lane, durable
journals, CAS pointer commit, startup recovery, and external atomic publication
for typed workdirs.

Layout of a store-backed workdir (``<root>``)::

    <root>/workdir.json                     pointer: atomically selects the current
                                            immutable Workdir generation
    <root>/.docx2typed-store/               private; excluded from manifest/asset closure
        probe.json                          filesystem qualification probe result
        lock                                Writer lane (fixed inode, OS advisory lock)
        reserve                             1 MiB genuinely allocated recovery reserve
        reserve-depleted.json               marker when the reserve was released (ENOSPC)
        generations/<gen-id>/               immutable full snapshots (authoritative)
            generation.json                 generation manifest (assets + parent + sha256)
            <workdir assets>                typed.md, format.json, ..., .review/, ...
        transactions/<operation_id>/        hash-chained phase records:
            intent.json                     (prev = pointer hash) operation started
            prepared.json                   generation/retention decision staged
            retention-marked.json           trim ledger is durable
            retention-swept.json            objects/generations swept
            external-published.json         external outputs atomically published
            generation-committed.json       pointer CAS committed
            completed.json                  operation durable; transaction finished
        staging/<operation_id>/             prepared external outputs before publish
        recovery/<run_id>.json              recovery Run evidence (immutable, one per event)
        quarantine/<name>/                  ambiguous state, never guessed into repair

The generation directory is authoritative and immutable. Root-level workdir
files are the materialized mirror of the current generation (kept for external
editors and the hash-bound ``edit.md`` draft ingress). Tool reads pin the
generation directory; mutations build a new generation snapshot, journal every
phase, and swap the pointer under the Writer lane. Retention uses the same
durable journal lane before it marks the trim ledger or sweeps content.

Guarantee boundary: every cut point (kill before/after journal write/flush/
rename, retention mark/sweep, external publish, pointer swap, materialize;
ENOSPC; short write; flush failure; corruption; CAS race; lock-holder death)
yields only the complete old generation, the complete new generation, or
explicit ``needs-recovery`` — never a mixed generation, evidence-free mutation,
duplicated Operation-ID effect, half-published external output, or unjournaled
content trim.
"""
from __future__ import annotations

import json
import os
import platform
import shutil
import tempfile
import threading
import time
import uuid
from contextlib import contextmanager
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable

try:
    import msvcrt  # type: ignore[import-not-found]  # Windows advisory lock

    _MSVCRT = True
except ImportError:  # pragma: no cover - POSIX
    import fcntl  # type: ignore[import-not-found]

    _MSVCRT = False

try:
    from .protocol import file_sha256, semantic_sha256
except ImportError:  # direct script execution has no package context.
    from protocol import file_sha256, semantic_sha256

STORE_DIR_NAME = ".docx2typed-store"
POINTER_FILE = "workdir.json"
POINTER_SCHEMA = "docx2typed-workdir-pointer-1"
GENERATION_MANIFEST_SCHEMA = "docx2typed-generation-manifest-1"
JOURNAL_SCHEMA = "docx2typed-transaction-journal-1"
PROBE_SCHEMA = "docx2typed-store-probe-1"
RECOVERY_EVIDENCE_SCHEMA = "docx2typed-recovery-evidence-1"

RESERVE_BYTES = 1024 * 1024  # 1 MiB recovery reserve, genuinely allocated
PHASE_ORDER = (
    "intent",
    "prepared",
    "retention-marked",
    "retention-swept",
    "external-published",
    "generation-committed",
    "completed",
)
RETENTION_KIND = "history-gc"
# Root files that stay mutable Draft ingress: reads take them from the root,
# mutations overlay them into the generation copy before running.
INGRESS_FILES = ("typed.md", "edit.md")

# Assets that determine the built document. Deliberately NOT the whole
# generation: evidence, review state, derived views and transaction metadata
# change constantly and must not read as a content change (ADR 0044).
CANONICAL_ASSETS = ("typed.md", "format.json", "styles.json", "_template.docx")

# Lock outcomes are stable diagnostic codes (public contract).
WRITER_BUSY = "writer-busy"
WRITER_TIMEOUT = "writer-timeout"
GENERATION_CONFLICT = "generation-conflict"
NEEDS_RECOVERY = "needs-recovery"
RESERVE_DEPLETED = "reserve-depleted"
UNSUPPORTED_BY_DESIGN = "unsupported-by-design"
STORE_INVALID = "store-invalid"
OPERATION_JOURNAL_CONFLICT = "operation-journal-conflict"

# Recovery acquires the Writer lane with a bounded wait; infinite waits are
# prohibited by the contract.
_RECOVERY_LOCK_TIMEOUT_MS = 30_000


class StoreError(Exception):
    """Recovery/journal/durability failure carrying a stable diagnostic code."""

    def __init__(self, code: str, message: str) -> None:
        super().__init__(message)
        self.code = code


class WriterBusy(StoreError):
    def __init__(self, message: str) -> None:
        super().__init__(WRITER_BUSY, message)


class WriterTimeout(StoreError):
    def __init__(self, message: str) -> None:
        super().__init__(WRITER_TIMEOUT, message)


class GenerationConflict(StoreError):
    def __init__(self, message: str) -> None:
        super().__init__(GENERATION_CONFLICT, message)


class NeedsRecovery(StoreError):
    def __init__(self, message: str) -> None:
        super().__init__(NEEDS_RECOVERY, message)


class ReserveDepleted(StoreError):
    def __init__(self, message: str) -> None:
        super().__init__(RESERVE_DEPLETED, message)


class UnsupportedFilesystem(StoreError):
    def __init__(self, message: str) -> None:
        super().__init__(UNSUPPORTED_BY_DESIGN, message)


class StoreInvalid(StoreError):
    def __init__(self, message: str) -> None:
        super().__init__(STORE_INVALID, message)


class _Kill(BaseException):
    """Simulated process death: escapes every ``except Exception`` handler so
    the journal is left exactly as the process died. Only the test harness
    raises it; real kills leave the same on-disk state."""


# --------------------------------------------------------------------------
# Fault injection (deterministic, test-only). A fault is either a BaseException
# (raised at the cut point) or a callable (mutates the staged temp path, then
# lets the flow continue — used for short writes).
# --------------------------------------------------------------------------

_FAULTS: dict[str, Any] = {}
_fault_lock = threading.Lock()


def set_fault(name: str, fault: Any) -> None:
    """Arm one named cut point: ``None`` disarms, a BaseException raises at the
    cut point, a callable receives the staged temp path (and may truncate it to
    simulate a short write)."""
    with _fault_lock:
        _FAULTS[name] = fault


def clear_faults() -> None:
    with _fault_lock:
        _FAULTS.clear()


def _fire(name: str, staged: Path | None = None) -> None:
    with _fault_lock:
        fault = _FAULTS.get(name)
    if fault is None:
        return
    if isinstance(fault, BaseException):
        raise fault
    fault(staged)


def kill_at(name: str) -> None:
    """Arm ``name`` to simulate process death (``_Kill`` escapes Exception)."""
    set_fault(name, _Kill())


_TIMINGS_ENV = "DOCX2TYPED_TIMINGS"


class _MutationProfiler:
    """Best-effort phase timings for one mutation when explicitly enabled."""

    def __init__(self, operation: str, operation_id: str) -> None:
        destination = os.environ.get(_TIMINGS_ENV)
        self.path = Path(destination) if destination else None
        self.operation = operation
        self.operation_id = operation_id
        self.started = time.perf_counter()
        self.phases: list[dict[str, Any]] = []

    @contextmanager
    def phase(self, name: str):
        if self.path is None:
            yield
            return
        started = time.perf_counter()
        status = "success"
        try:
            yield
        except BaseException:
            status = "error"
            raise
        finally:
            self.phases.append(
                {
                    "name": name,
                    "duration_ms": round((time.perf_counter() - started) * 1000, 3),
                    "status": status,
                }
            )

    def finish(self, outcome: str, error: BaseException | None = None) -> None:
        if self.path is None:
            return
        record: dict[str, Any] = {
            "schema": "docx2typed-mutation-timings-1",
            "operation": self.operation,
            "operation_id": self.operation_id,
            "outcome": outcome,
            "duration_ms": round((time.perf_counter() - self.started) * 1000, 3),
            "phases": self.phases,
        }
        if error is not None:
            record["error"] = type(error).__name__
        try:
            self.path.parent.mkdir(parents=True, exist_ok=True)
            with self.path.open("a", encoding="utf-8", newline="\n") as handle:
                handle.write(json.dumps(record, ensure_ascii=False, sort_keys=True) + "\n")
        except OSError:
            # Diagnostics must never turn a durable mutation into a failure.
            pass


# --------------------------------------------------------------------------
# Durability helpers
# --------------------------------------------------------------------------

def _now_iso() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def _canonical_bytes(value: Any) -> bytes:
    return json.dumps(
        value, ensure_ascii=False, sort_keys=True, separators=(",", ":")
    ).encode("utf-8")


def _fsync_file(path: Path) -> None:
    try:
        handle = open(path, "r+b")
    except PermissionError:
        handle = open(path, "rb")
    with handle:
        os.fsync(handle.fileno())


def _fsync_dir(path: Path) -> bool:
    """Directory durability barrier; returns True when a real barrier ran,
    False when the platform documents an equivalent (Windows: NTFS directory
    metadata is covered by the file fsync + atomic replace contract)."""
    try:
        fd = os.open(path, os.O_RDONLY)
    except OSError:
        return False  # documented platform equivalent
    try:
        os.fsync(fd)
        return True
    except OSError:
        return False
    finally:
        os.close(fd)


def _write_durable(
    path: Path,
    data: bytes,
    *,
    write_fault: str | None = None,
    flush_fault: str | None = None,
    rename_fault: str | None = None,
) -> None:
    """Atomic durable publish: temp write -> flush -> fsync -> rename ->
    parent directory barrier. Fault points carry the staged temp path so a
    short-write fault can truncate it before the rename lands."""
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temp_name = tempfile.mkstemp(prefix=f".{path.name}.", suffix=".tmp", dir=path.parent)
    os.close(fd)
    temp_path = Path(temp_name)
    try:
        _fire(write_fault, temp_path)
        with open(temp_path, "wb") as handle:
            handle.write(data)
            handle.flush()
            os.fsync(handle.fileno())
        _fire(flush_fault, temp_path)
        if temp_path.stat().st_size != len(data):
            # Short write (fault-injected or real): the staged record would
            # land torn and silently break the hash chain. Fail the write so
            # the mutation rolls back to the complete old state instead of
            # returning success over a corrupt journal.
            raise OSError(f"short write detected publishing {path.name}")
        _fire(rename_fault, temp_path)
        os.replace(temp_path, path)
        _fsync_dir(path.parent)
    except BaseException:
        try:
            temp_path.unlink(missing_ok=True)
        except OSError:
            pass
        raise


def _fsync_tree(root: Path) -> None:
    """fsync every file under ``root`` and every directory, so a pointer swap
    can never outrun its generation's bytes."""
    if not root.is_dir():
        return
    for path in sorted(root.rglob("*")):
        if path.is_file():
            _fsync_file(path)
    for path in sorted(
        (root, *[d for d in root.rglob("*") if d.is_dir()]), key=lambda p: len(p.parts), reverse=True
    ):
        _fsync_dir(path)


def _copy_tree(source: Path, target: Path) -> None:
    """Copy one generation snapshot (files + structure, byte-exact)."""
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copytree(source, target, dirs_exist_ok=False)


def _walk_files(root: Path) -> list[Path]:
    return sorted(path for path in root.rglob("*") if path.is_file())


# --------------------------------------------------------------------------
# Advisory Writer lane (fixed inode, OS lock; process death releases it)
# --------------------------------------------------------------------------

def _try_lock(fd: int) -> None:
    if _MSVCRT:
        os.lseek(fd, 0, os.SEEK_SET)
        msvcrt.locking(fd, msvcrt.LK_NBLCK, 1)
    else:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)


def _unlock(fd: int) -> None:
    if _MSVCRT:
        os.lseek(fd, 0, os.SEEK_SET)
        msvcrt.locking(fd, msvcrt.LK_UNLCK, 1)
    else:
        fcntl.flock(fd, fcntl.LOCK_UN)


@contextmanager
def _writer_lane(path: Path, timeout_ms: int = 0):
    """Acquire the exclusive Writer lane. ``timeout_ms=0`` fails immediately
    with ``writer-busy``; a bounded positive timeout fails with
    ``writer-timeout``. Infinite waits are prohibited. Lock ownership is OS-
    advisory only: nothing deletes, steals, or reclaims by PID/age."""
    path.parent.mkdir(parents=True, exist_ok=True)
    fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o600)
    try:
        if os.fstat(fd).st_size == 0:
            os.write(fd, b"0")
            os.fsync(fd)
        deadline = None if timeout_ms <= 0 else time.monotonic() + timeout_ms / 1000.0
        while True:
            try:
                _try_lock(fd)
                break
            except OSError:
                if deadline is None:
                    raise WriterBusy(f"another writer holds the workdir lane: {path}") from None
                if time.monotonic() >= deadline:
                    raise WriterTimeout(
                        f"writer lane did not become free within {timeout_ms}ms: {path}"
                    ) from None
                time.sleep(0.02)
        yield fd
    finally:
        try:
            _unlock(fd)
        except OSError:
            pass
        os.close(fd)


# Shared lane for the review writer/queue transactions (issue #51 finding 2):
# the same fixed-inode OS-advisory lock, so a crash releases the lock with its
# holder process and no stale O_EXCL file can ever wedge the review lanes.
# Callers map WriterBusy/WriterTimeout to their own busy errors.
advisory_lane = _writer_lane


# --------------------------------------------------------------------------
# Filesystem qualification probe
# --------------------------------------------------------------------------

def _volume_identity(store_dir: Path) -> str | None:
    """Identity of the host filesystem volume holding ``store_dir`` (device
    id). The probe cache is bound to this identity so a workdir moved onto a
    different volume is re-probed instead of trusting a foreign cache."""
    try:
        return str(os.stat(store_dir).st_dev)
    except OSError:
        return None


def _probe_filesystem(store_dir: Path) -> dict[str, Any]:
    checks: dict[str, Any] = {}

    def _run(name: str, fn: Callable[[], Any]) -> None:
        try:
            checks[name] = fn()
        except OSError as exc:
            checks[name] = f"{type(exc).__name__}: {exc}"

    def _atomic() -> bool:
        a = store_dir / ".probe-a"
        b = store_dir / ".probe-b"
        a.write_bytes(b"a")
        b.write_bytes(b"b")
        os.replace(a, b)
        os.replace(b, a)
        a.unlink(missing_ok=True)
        b.unlink(missing_ok=True)
        return True

    def _file_durability() -> bool:
        probe = store_dir / ".probe-fsync"
        with open(probe, "wb") as handle:
            handle.write(b"x")
            handle.flush()
            os.fsync(handle.fileno())
        probe.unlink(missing_ok=True)
        return True

    def _dir_durability() -> bool:
        return _fsync_dir(store_dir)

    def _lock_roundtrip() -> bool:
        with _writer_lane(store_dir / ".probe-lock", timeout_ms=5_000):
            pass
        return True

    def _stable_identity() -> bool:
        probe = store_dir / ".probe-id"
        probe.write_bytes(b"id")
        first = probe.stat()
        second = probe.stat()
        probe.unlink(missing_ok=True)
        return (first.st_dev, first.st_ino) == (second.st_dev, second.st_ino)

    store_dir.mkdir(parents=True, exist_ok=True)
    _run("atomic_replace", _atomic)
    _run("file_durability", _file_durability)
    _run("dir_durability", _dir_durability)  # False = documented platform equivalent
    _run("advisory_lock", _lock_roundtrip)
    _run("stable_identity", _stable_identity)
    # dir_durability False is the documented platform equivalent (Windows:
    # NTFS directory metadata is covered by file fsync + atomic replace).
    qualified = bool(
        checks.get("atomic_replace") is True
        and checks.get("file_durability") is True
        and checks.get("advisory_lock") is True
        and checks.get("stable_identity") is True
    )
    return {
        "schema": PROBE_SCHEMA,
        "qualified": qualified,
        "checked_at": _now_iso(),
        "os": platform.system(),
        "python": platform.python_version(),
        "volume_identity": _volume_identity(store_dir),
        "checks": checks,
    }


def _probe_or_reuse(store_dir: Path) -> dict[str, Any]:
    # Public qualification harness seam: DOCX2TYPED_FORCE_UNQUALIFIED=1 makes
    # every store fail closed as unsupported-by-design, so the capability
    # matrix can exercise the guard through the real CLI instead of a
    # monkeypatched unit test.
    if os.environ.get("DOCX2TYPED_FORCE_UNQUALIFIED") == "1":
        raise UnsupportedFilesystem(
            "workdir filesystem qualification forced unqualified "
            "(DOCX2TYPED_FORCE_UNQUALIFIED=1)"
        )
    probe_path = store_dir / "probe.json"
    cached: dict[str, Any] | None = None
    try:
        cached = json.loads(probe_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        cached = None
    if (
        isinstance(cached, dict)
        and cached.get("schema") == PROBE_SCHEMA
        and cached.get("qualified") is True
        and cached.get("os") == platform.system()
        and cached.get("python") == platform.python_version()
        # The cache is bound to the host volume: a workdir moved onto a
        # different filesystem must re-probe, never reuse a foreign result.
        and cached.get("volume_identity") == _volume_identity(store_dir)
    ):
        return cached
    probe = _probe_filesystem(store_dir)
    if not probe["qualified"]:
        raise UnsupportedFilesystem(
            "workdir filesystem is not qualified for atomic durability: "
            + json.dumps(probe["checks"], sort_keys=True)
        )
    _write_durable(probe_path, _canonical_bytes(probe) + b"\n")
    return probe


# --------------------------------------------------------------------------
# Pointer, generation manifest, journal records
# --------------------------------------------------------------------------

VERSION_POINTER_FIELDS = (
    "head_version",
    "head_version_generation",
    "head_tree",
    "head_tree_object",
    "head_commit",
    "baseline_epoch",
    "version_seq",
)


def _pointer_payload(
    generation: str,
    operation_id: str | None,
    manifest_sha256: str,
    *,
    previous: dict[str, Any] | None = None,
    version: dict[str, Any] | None = None,
) -> dict[str, Any]:
    """Pointer payload = HEAD: the current generation plus the version HEAD
    names. A plain mutation carries the version fields forward (canonical may
    drift ahead of HEAD — that is `version dirty`, ADR 0044); only a save
    boundary replaces them."""
    payload = {
        "schema": POINTER_SCHEMA,
        "generation": generation,
        "operation_id": operation_id,
        "manifest_sha256": manifest_sha256,
        "written_at": _now_iso(),
    }
    for field in VERSION_POINTER_FIELDS:
        carried = (previous or {}).get(field)
        if carried is not None:
            payload[field] = carried
    if version is not None:
        payload["head_version"] = version["version"]
        payload["head_version_generation"] = generation
        payload["head_tree"] = version["head_tree"]
        payload["head_tree_object"] = version.get("tree_object")
        payload["head_commit"] = version.get("commit")
        payload["baseline_epoch"] = version.get("baseline_epoch") or 1
        payload["version_seq"] = version["seq"]
    return payload


def _read_pointer(root: Path) -> dict[str, Any] | None:
    path = root / POINTER_FILE
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None
    if (
        not isinstance(data, dict)
        or data.get("schema") != POINTER_SCHEMA
        or not isinstance(data.get("generation"), str)
    ):
        return None
    return data


def _generation_manifest(
    gen_dir: Path,
    *,
    generation: str,
    parent: str | None,
    operation_id: str,
    input_sha256: str,
    version: dict[str, Any] | None = None,
) -> dict[str, Any]:
    assets = []
    for path in _walk_files(gen_dir):
        rel = path.relative_to(gen_dir).as_posix()
        if rel == "generation.json":
            continue
        assets.append({"path": rel, "bytes": path.stat().st_size, "sha256": file_sha256(path)})
    assets_sha256 = semantic_sha256(assets)
    manifest = {
        "schema": GENERATION_MANIFEST_SCHEMA,
        "generation": generation,
        "parent": parent,
        "operation_id": operation_id,
        "input_sha256": input_sha256,
        "assets": assets,
        "assets_sha256": assets_sha256,
        "created_at": _now_iso(),
    }
    if version is not None:
        manifest["version"] = version
    return manifest


def _write_generation_manifest(gen_dir: Path, manifest: dict[str, Any]) -> None:
    _write_durable(gen_dir / "generation.json", _canonical_bytes(manifest) + b"\n")


def _journal_record(phase: str, payload: dict[str, Any], prev_hash: str) -> dict[str, Any]:
    body = {
        "schema": JOURNAL_SCHEMA,
        "phase": phase,
        "prev_hash": prev_hash,
        **payload,
    }
    record_hash = semantic_sha256(body)
    return {**body, "record_sha256": record_hash}


def _journal_path(tx_dir: Path, phase: str) -> Path:
    return tx_dir / f"{phase}.json"


def _write_journal_record(tx_dir: Path, record: dict[str, Any]) -> None:
    phase = record["phase"]
    _write_durable(
        _journal_path(tx_dir, phase),
        _canonical_bytes(record) + b"\n",
        write_fault=f"journal-write-{phase}",
        flush_fault=f"journal-flush-{phase}",
        rename_fault=f"journal-rename-{phase}",
    )


def _read_phases(tx_dir: Path) -> list[dict[str, Any]]:
    """Read the present phase records in canonical order. Raises StoreInvalid
    when the chain is broken (missing predecessor, bad hash, bad link)."""
    records: list[dict[str, Any]] = []
    expected_prev: str | None = None
    for phase in PHASE_ORDER:
        path = _journal_path(tx_dir, phase)
        if not path.exists():
            continue
        try:
            record = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            raise StoreInvalid(
                f"transaction journal record is corrupt: {path}: {exc}"
            ) from exc
        if (
            not isinstance(record, dict)
            or record.get("schema") != JOURNAL_SCHEMA
            or record.get("phase") != phase
            or not isinstance(record.get("record_sha256"), str)
        ):
            raise StoreInvalid(f"transaction journal record is malformed: {path}")
        body = {key: value for key, value in record.items() if key != "record_sha256"}
        if record["record_sha256"] != semantic_sha256(body):
            raise StoreInvalid(f"transaction journal record hash mismatch: {path}")
        if expected_prev is not None and record.get("prev_hash") != expected_prev:
            raise StoreInvalid(
                f"transaction journal chain broken at {phase}: expected prev "
                f"{expected_prev}, record has {record.get('prev_hash')!r}"
            )
        expected_prev = record["record_sha256"]
        records.append(record)
    if records and _journal_path(tx_dir, "intent").exists() and records[0].get("phase") != "intent":
        raise StoreInvalid("transaction journal starts without intent")
    return records


def _read_phases_soft(tx_dir: Path) -> list[dict[str, Any]] | None:
    """Like ``_read_phases`` but returns None for a broken chain (inspection
    must not throw on corrupt journals)."""
    try:
        return _read_phases(tx_dir)
    except StoreInvalid:
        return None


# --------------------------------------------------------------------------
# Store
# --------------------------------------------------------------------------

# --------------------------------------------------------------------------
# Durable idempotency plane (independent of generation lifetime)
# --------------------------------------------------------------------------

LEDGER_LOG = "ledger.jsonl"


def _ledger_log_path(root: Path) -> Path:
    return root / STORE_DIR_NAME / LEDGER_LOG


def _append_ledger_log(root: Path, operation_id: str, record: dict[str, Any]) -> None:
    """Append one idempotency record to the store's durable log.

    The record is also written beside its artifact, but that copy travels with
    a generation — and generations are reclaimable by design (ADR 0042/0043).
    Idempotency must not depend on a directory whose purpose is to be
    collected, so every record is appended here as well, in the same
    transaction that commits the pointer. Append-only, fsynced, one line per
    operation."""
    path = _ledger_log_path(root)
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        line = json.dumps(
            {"operation_id": operation_id, **record}, ensure_ascii=False, sort_keys=True
        ) + "\n"
        with path.open("a", encoding="utf-8", newline="\n") as handle:
            handle.write(line)
            handle.flush()
            os.fsync(handle.fileno())
    except OSError:
        pass  # the artifact-side copy remains as the fallback


def _read_ledger_log(root: Path) -> dict[str, dict[str, Any]]:
    """operation_id -> the last record appended for it."""
    records: dict[str, dict[str, Any]] = {}
    path = _ledger_log_path(root)
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except OSError:
        return records
    for line in lines:
        if not line.strip():
            continue
        try:
            payload = json.loads(line)
        except json.JSONDecodeError:
            continue
        operation_id = payload.get("operation_id")
        if isinstance(operation_id, str):
            records[operation_id] = payload
    return records


class Store:
    """One store-backed workdir: pointer, generations, transactions, Writer
    lane, recovery reserve, filesystem qualification."""

    def __init__(self, root: str | Path) -> None:
        self.root = Path(root).resolve()
        self.store_dir = self.root / STORE_DIR_NAME
        self.generations_dir = self.store_dir / "generations"
        self.transactions_dir = self.store_dir / "transactions"
        self.staging_dir = self.store_dir / "staging"
        self.recovery_dir = self.store_dir / "recovery"
        self.quarantine_dir = self.store_dir / "quarantine"
        self.lock_path = self.store_dir / "lock"
        self.reserve_path = self.store_dir / "reserve"
        self.reserve_marker = self.store_dir / "reserve-depleted.json"

    # -- open / init -------------------------------------------------------

    @classmethod
    def open(cls, root: str | Path) -> "Store":
        """Open an existing store-backed workdir (probe + pointer read). Does
        not run recovery; ``recover()``/``mutate()`` do, under the Writer lane."""
        store = cls(root)
        if not store.store_dir.is_dir():
            raise StoreInvalid(f"not a store-backed workdir: {store.root}")
        _probe_or_reuse(store.store_dir)
        if _read_pointer(store.root) is None:
            raise StoreInvalid(f"store pointer is missing or corrupt: {store.root / POINTER_FILE}")
        return store

    @classmethod
    def init(
        cls,
        root: str | Path,
        *,
        operation_id: str,
        input_sha256: str,
    ) -> "Store":
        """Create a fresh store from the current root files: probe, reserve,
        snapshot generation 0, journal the birth transaction, pointer. Used by
        extract and by the lazy upgrade of pre-store workdirs."""
        store = cls(root)
        root_path = store.root
        if not root_path.is_dir():
            raise StoreInvalid(f"workdir not found: {root_path}")
        if store.store_dir.exists():
            # Already store-backed: recovery decides; never re-init over it.
            return store
        root_path.mkdir(parents=True, exist_ok=True)
        try:
            store.store_dir.mkdir(parents=True, exist_ok=True)
            _probe_or_reuse(store.store_dir)
            store._write_reserve()
            generation = uuid.uuid4().hex
            gen_dir = store.generations_dir / generation
            gen_dir.mkdir(parents=True, exist_ok=True)
            _copy_root_assets(root_path, gen_dir)
            manifest = _generation_manifest(
                gen_dir,
                generation=generation,
                parent=None,
                operation_id=operation_id,
                input_sha256=input_sha256,
            )
            _write_generation_manifest(gen_dir, manifest)
            _fsync_tree(gen_dir)
            tx_dir = store.transactions_dir / operation_id
            intent = _journal_record(
                "intent",
                {
                    "operation_id": operation_id,
                    "input_sha256": input_sha256,
                    "expected_generation": None,
                    "kind": "birth",
                },
                prev_hash="genesis",
            )
            _write_journal_record(tx_dir, intent)
            prepared = _journal_record(
                "prepared",
                {
                    "operation_id": operation_id,
                    "generation": generation,
                    "parent": None,
                    "input_sha256": input_sha256,
                    "manifest_sha256": manifest["assets_sha256"],
                    "kind": "birth",
                },
                prev_hash=intent["record_sha256"],
            )
            _write_journal_record(tx_dir, prepared)
            pointer = _pointer_payload(generation, operation_id, manifest["assets_sha256"])
            _write_durable(
                root_path / POINTER_FILE,
                _canonical_bytes(pointer) + b"\n",
                write_fault="pointer-write",
                flush_fault="pointer-flush",
                rename_fault="pointer-rename",
            )
            committed = _journal_record(
                "generation-committed",
                {
                    "operation_id": operation_id,
                    "generation": generation,
                    "parent": None,
                },
                prev_hash=prepared["record_sha256"],
            )
            _write_journal_record(tx_dir, committed)
            completed = _journal_record(
                "completed",
                {
                    "operation_id": operation_id,
                    "generation": generation,
                    "parent": None,
                    "outcome": "success",
                    "kind": "birth",
                },
                prev_hash=committed["record_sha256"],
            )
            _write_journal_record(tx_dir, completed)
            shutil.rmtree(tx_dir, ignore_errors=True)
        except BaseException:
            # A failed birth leaves no trace: the workdir stays a plain
            # pre-store workdir so a retry re-attempts init instead of
            # replaying success against a half-born store.
            shutil.rmtree(store.store_dir, ignore_errors=True)
            raise
        return store

    @classmethod
    def ensure(
        cls,
        root: str | Path,
        *,
        operation_id: str,
        input_sha256: str,
    ) -> "Store":
        """Open an existing store-backed workdir, or upgrade a pre-store
        workdir in place (birth generation 0) and open it."""
        if (Path(root) / STORE_DIR_NAME).is_dir():
            return cls.open(root)
        return cls.init(root, operation_id=operation_id, input_sha256=input_sha256)

    # -- reserve -----------------------------------------------------------

    def _write_reserve(self) -> None:
        data = os.urandom(RESERVE_BYTES)
        self.reserve_path.parent.mkdir(parents=True, exist_ok=True)
        fd, temp_name = tempfile.mkstemp(
            prefix=".reserve.", suffix=".tmp", dir=self.store_dir
        )
        os.close(fd)
        temp_path = Path(temp_name)
        try:
            with open(temp_path, "wb") as handle:
                handle.write(data)
                handle.flush()
                os.fsync(handle.fileno())
            os.replace(temp_path, self.reserve_path)
        except BaseException:
            try:
                temp_path.unlink(missing_ok=True)
            except OSError:
                pass
            raise

    def _require_reserve(self) -> None:
        if self.reserve_marker.exists():
            raise ReserveDepleted(
                "workdir recovery reserve is depleted (ENOSPC); replenish it "
                "before further mutations"
            )
        try:
            size = self.reserve_path.stat().st_size
        except OSError:
            raise ReserveDepleted(
                "workdir recovery reserve is missing; replenish it before further mutations"
            ) from None
        if size < RESERVE_BYTES:
            raise ReserveDepleted(
                "workdir recovery reserve is depleted (ENOSPC); replenish it "
                "before further mutations"
            )

    def _release_reserve(self) -> None:
        """ENOSPC emergency: release the reserve only to close the journal and
        write minimal failure evidence. The workdir becomes read-only
        ``reserve-depleted`` until replenished."""
        try:
            with open(self.reserve_path, "wb") as handle:
                handle.truncate(0)
                handle.flush()
                os.fsync(handle.fileno())
        except OSError:
            pass
        self.reserve_marker.write_text(
            json.dumps(
                {"schema": "docx2typed-reserve-depleted-1", "released_at": _now_iso()},
                ensure_ascii=False,
                indent=2,
            )
            + "\n",
            encoding="utf-8",
            newline="\n",
        )

    def replenish_reserve(self) -> None:
        """Restore the 1 MiB recovery reserve; clears ``reserve-depleted``."""
        self.reserve_marker.unlink(missing_ok=True)
        self._write_reserve()

    # -- read side ---------------------------------------------------------

    def pin(self) -> dict[str, Any]:
        """Pin the current generation for one read-only operation. Returns
        ``{"generation", "path", "manifest_sha256"}``; the generation
        directory is immutable, so the pinned path stays consistent even while
        a later writer commits. A missing generation directory or corrupt
        generation manifest is ``needs-recovery`` (never guessed into repair)."""
        pointer = _read_pointer(self.root)
        if pointer is None:
            raise StoreInvalid(f"store pointer is missing or corrupt: {self.root / POINTER_FILE}")
        generation = pointer["generation"]
        gen_dir = self.generations_dir / generation
        if not gen_dir.is_dir():
            raise NeedsRecovery(
                f"pointer selects generation {generation} but its directory is missing"
            )
        try:
            manifest = json.loads((gen_dir / "generation.json").read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            raise NeedsRecovery(
                f"generation manifest is corrupt: {gen_dir / 'generation.json'}: {exc}"
            ) from exc
        if (
            not isinstance(manifest, dict)
            or manifest.get("schema") != GENERATION_MANIFEST_SCHEMA
            or manifest.get("generation") != generation
        ):
            raise NeedsRecovery(f"generation manifest is malformed: {gen_dir / 'generation.json'}")
        return {
            "generation": generation,
            "path": gen_dir,
            "manifest_sha256": pointer.get("manifest_sha256"),
        }

    def ledger_dir(self) -> Path:
        return self.pin()["path"]

    def lookup_ledger(
        self,
        operation_id: str,
        *,
        generation: bool,
        anchor: Path | None,
        directory: bool,
    ) -> tuple[dict[str, Any] | None, Path | None]:
        """Replay lookup for one operation under this store.

        Returns ``(record, corrupt_path)``: the persisted record (or None)
        and, when a structurally corrupt row for the operation exists, the
        exact ledger file holding it (None when clean). Workdir mutations
        (``generation=True``) search every generation directory, because a
        record lives under the generation the operation committed and the
        pointer may have advanced past it — the replay must hit the
        generation the record was written under, never silently re-run.
        External-only operations search the caller's anchor (their ledger
        lives beside the external artifact). The in-memory mirror is
        consulted per ledger file only, so records belonging to another
        workdir are never replayed here (issue #50 finding 3)."""
        from .protocol import operation_ledger  # local: avoids import cycles

        if generation:
            logged = _read_ledger_log(self.root).get(operation_id)
            if logged is not None and isinstance(logged.get("envelope"), dict):
                return logged, None
            for gen_dir in sorted(self.generations_dir.iterdir(), reverse=True):
                record = operation_ledger.lookup_persisted(operation_id, gen_dir, directory=True)
                if record is not None:
                    return record, None
                corrupt_path = operation_ledger.corrupt_persisted(
                    operation_id, gen_dir, directory=True
                )
                if corrupt_path is not None:
                    return None, corrupt_path
            return None, None
        lookup_anchor = anchor if anchor is not None else self.root
        record = operation_ledger.lookup_persisted(
            operation_id, lookup_anchor, directory=directory
        )
        if record is not None:
            return record, None
        return None, operation_ledger.corrupt_persisted(
            operation_id, lookup_anchor, directory=directory
        )

    def pending_transactions(self) -> list[dict[str, Any]]:
        """Lightweight read-only transaction inspection (no lane). Returns one
        descriptor per transaction whose journal has not reached ``completed``
        or whose chain is broken."""
        pending: list[dict[str, Any]] = []
        if not self.transactions_dir.is_dir():
            return pending
        for tx_dir in sorted(self.transactions_dir.iterdir()):
            if not tx_dir.is_dir():
                continue
            records = _read_phases_soft(tx_dir)
            if records is None:
                pending.append(
                    {
                        "operation_id": tx_dir.name,
                        "state": "corrupt-journal",
                        "phases": [p.name for p in sorted(tx_dir.glob("*.json"))],
                    }
                )
                continue
            phases = [record["phase"] for record in records]
            if "completed" not in phases:
                pending.append(
                    {
                        "operation_id": tx_dir.name,
                        "state": "incomplete",
                        "phases": phases,
                    }
                )
        return pending

    def recovery_warning(self) -> list[str]:
        return [
            f"transaction {item['operation_id']} is {item['state']}"
            for item in self.pending_transactions()
        ]

    # -- writer ------------------------------------------------------------

    @contextmanager
    def writer(self, timeout_ms: int = 0):
        with _writer_lane(self.lock_path, timeout_ms=timeout_ms):
            yield self

    # -- journal helpers ---------------------------------------------------

    def _tx_dir(self, operation_id: str) -> Path:
        return self.transactions_dir / operation_id

    def _begin_journal(
        self,
        operation_id: str,
        canonical: str,
        expected_generation: str | None,
        input_sha256: str,
        kind: str,
    ) -> tuple[Path, dict[str, Any]]:
        tx_dir = self._tx_dir(operation_id)
        if tx_dir.exists():
            records = _read_phases_soft(tx_dir)
            if records is not None and any(r["phase"] == "completed" for r in records):
                raise StoreError(
                    OPERATION_JOURNAL_CONFLICT,
                    f"transaction {operation_id} already completed in its journal",
                )
            raise StoreError(
                OPERATION_JOURNAL_CONFLICT,
                f"transaction {operation_id} already exists; run recovery first",
            )
        tx_dir.mkdir(parents=True, exist_ok=True)
        pointer = _read_pointer(self.root)
        prev_hash = (
            file_sha256(self.root / POINTER_FILE)
            if pointer is not None
            else "genesis"
        )
        intent = _journal_record(
            "intent",
            {
                "operation_id": operation_id,
                "input_sha256": canonical,
                "expected_generation": expected_generation,
                "input_manifest_sha256": input_sha256,
                "kind": kind,
            },
            prev_hash=prev_hash,
        )
        _write_journal_record(tx_dir, intent)
        return tx_dir, intent

    def _copy_generation(self, parent: str | None, generation: str) -> Path:
        gen_dir = self.generations_dir / generation
        if parent is not None:
            _fire("generation-copy")
            _copy_tree(self.generations_dir / parent, gen_dir)
        else:
            gen_dir.mkdir(parents=True, exist_ok=True)
            _copy_root_assets(self.root, gen_dir)
        return gen_dir

    def _overlay_ingress(self, gen_dir: Path) -> None:
        for name in INGRESS_FILES:
            source = self.root / name
            if source.is_file():
                shutil.copyfile(source, gen_dir / name)

    def _materialize_root(self, gen_dir: Path) -> None:
        """Mirror the committed generation's files onto the root. Recovery
        re-runs this to roll forward, so every file must be replaced
        independently and idempotently."""
        _fire("materialize")
        for source in _walk_files(gen_dir):
            rel = source.relative_to(gen_dir)
            if rel.as_posix() == "generation.json":
                continue
            target = self.root / rel
            _fire(f"materialize-file-{target.name}")
            target.parent.mkdir(parents=True, exist_ok=True)
            fd, temp_name = tempfile.mkstemp(
                prefix=f".{target.name}.", suffix=".tmp", dir=target.parent
            )
            os.close(fd)
            temp_path = Path(temp_name)
            try:
                shutil.copyfile(source, temp_path)
                with open(temp_path, "r+b") as handle:
                    os.fsync(handle.fileno())
                os.replace(temp_path, target)
            except BaseException:
                try:
                    temp_path.unlink(missing_ok=True)
                except OSError:
                    pass
                raise
        _fsync_dir(self.root)

    def _commit_pointer(
        self,
        generation: str,
        operation_id: str,
        manifest_sha256: str,
        version: dict[str, Any] | None = None,
    ) -> None:
        pointer = _pointer_payload(
            generation,
            operation_id,
            manifest_sha256,
            previous=_read_pointer(self.root),
            version=version,
        )
        _write_durable(
            self.root / POINTER_FILE,
            _canonical_bytes(pointer) + b"\n",
            write_fault="pointer-write",
            flush_fault="pointer-flush",
            rename_fault="pointer-rename",
        )

    def _write_evidence(self, path: Path, evidence: dict[str, Any]) -> None:
        from .protocol import publish_run_evidence  # local: avoids import cycles

        publish_run_evidence(path, evidence)
        _fsync_file(path)

    # -- recovery ----------------------------------------------------------

    def recover(self, *, auto: bool = True) -> dict[str, Any]:
        """Startup recovery under the Writer lane. Deterministic roll forward/
        back for uniquely provable transactions; ambiguity marks
        ``needs-recovery`` (journal preserved for inspection). Publishes
        recovery Run evidence per event. Never guesses by mtime."""
        result: dict[str, Any] = {
            "recovered": [],
            "rolled_back": [],
            "needs_recovery": [],
            "cleaned": [],
        }
        with self.writer(timeout_ms=_RECOVERY_LOCK_TIMEOUT_MS):
            self._recover_all(result, auto=auto)
        return result

    def _recover_all(self, result: dict[str, Any], *, auto: bool) -> dict[str, Any]:
        """Recover every leftover transaction, mutating ``result`` in place and
        returning it (the recovery summary). ``recover()`` ignores the return;
        ``mutate()`` indexes the returned summary to gate on ``needs_recovery``."""
        if not self.transactions_dir.is_dir():
            return result
        for tx_dir in sorted(self.transactions_dir.iterdir()):
            if tx_dir.is_dir():
                self._recover_tx(tx_dir, result, auto=auto)
        self._gc_abandoned(result)
        return result

    def _recover_tx(self, tx_dir: Path, result: dict[str, Any], *, auto: bool) -> None:
        operation_id = tx_dir.name
        records = _read_phases_soft(tx_dir)
        if records is None:
            result["needs_recovery"].append(
                {"operation_id": operation_id, "reason": "corrupt-journal-chain"}
            )
            return
        if not records:
            # Empty transaction directory (crash before the intent record
            # landed): trivially rolled back.
            self._roll_back_generation(tx_dir, operation_id, None, result)
            return
        if records[0].get("kind") == RETENTION_KIND:
            self._recover_retention_tx(tx_dir, records, result, auto=auto)
            return
        last = records[-1]
        prepared = next((r for r in records if r["phase"] == "prepared"), None)
        parent = prepared.get("parent") if prepared else None
        prepared_gen = prepared.get("generation") if prepared else None
        pointer = _read_pointer(self.root)
        current = pointer.get("generation") if pointer else None

        if last["phase"] == "completed":
            # Result may already be durable; ensure ledger + evidence landed.
            self._settle_completed(tx_dir, last, result)
            return
        if prepared_gen is None:
            # External-only transaction (build / new-baseline output): the
            # pointer never moves; recovery decides on the journal + hashes.
            if last["phase"] in ("external-published", "generation-committed"):
                self._roll_forward(tx_dir, records, last, result)
                return
            if last["phase"] == "prepared":
                decision = self._external_decision(records, result)
                if decision == "forward":
                    self._roll_forward(tx_dir, records, last, result)
                elif decision == "back":
                    self._roll_back_externals(tx_dir, records, result)
                    self._roll_back_generation(tx_dir, operation_id, None, result)
                else:
                    self._ambiguous(
                        tx_dir, operation_id, result, "external output state is ambiguous"
                    )
                return
            # intent only: nothing was published.
            self._roll_back_generation(tx_dir, operation_id, None, result)
            return
        if current == prepared_gen:
            # Pointer already selects the prepared generation: complete the
            # commit (materialize, evidence, ledger, completed journal).
            self._roll_forward(tx_dir, records, last, result)
            return
        if current == parent:
            # Pointer never moved: the mutation did not commit. Restore prior
            # outputs from verified backups; remove staged generation/outputs.
            self._roll_back_externals(tx_dir, records, result)
            self._roll_back_generation(tx_dir, operation_id, prepared_gen, result)
            return
        self._ambiguous(
            tx_dir, operation_id, result, "prepared generation differs from pointer"
        )

    def _complete_retention_tx(
        self,
        tx_dir: Path,
        records: list[dict[str, Any]],
        prepared: dict[str, Any],
    ) -> dict[str, Any]:
        """Finish a journaled history trim after its decision is durable."""
        operation_id = str(prepared.get("operation_id") or tx_dir.name)
        retention = prepared.get("retention")
        if not isinstance(retention, dict):
            raise StoreInvalid("history-gc prepared record has no retention decision")
        trimmed_records = retention.get("trimmed_records") or []
        keep_trees_raw = retention.get("keep_trees") or []
        reclaimable_raw = retention.get("reclaimable") or []
        if (
            not isinstance(trimmed_records, list)
            or not all(isinstance(item, dict) for item in trimmed_records)
            or not isinstance(keep_trees_raw, list)
            or not all(isinstance(item, str) for item in keep_trees_raw)
            or not isinstance(reclaimable_raw, list)
            or not all(isinstance(item, str) for item in reclaimable_raw)
        ):
            raise StoreInvalid("history-gc retention decision has invalid lists")
        records = _read_phases(tx_dir)
        marked = next((r for r in records if r["phase"] == "retention-marked"), None)
        if marked is None:
            _fire("retention-mark")
        _append_trim_log(self.root, trimmed_records)
        if marked is None:
            records = _read_phases(tx_dir)
            marked = _journal_record(
                "retention-marked",
                {
                    "operation_id": operation_id,
                    "kind": RETENTION_KIND,
                    "trimmed": [item.get("version") for item in trimmed_records],
                },
                prev_hash=records[-1]["record_sha256"],
            )
            _fire("retention-marked")
            _write_journal_record(tx_dir, marked)
            records.append(marked)

        swept_phase = next((r for r in records if r["phase"] == "retention-swept"), None)
        if swept_phase is None:
            swept = _sweep_retention(
                self.root,
                keep_trees=set(keep_trees_raw),
                reclaimable=reclaimable_raw,
            )
            records = _read_phases(tx_dir)
            swept_phase = _journal_record(
                "retention-swept",
                {
                    "operation_id": operation_id,
                    "kind": RETENTION_KIND,
                    "swept": swept,
                    "reclaimed_generations": reclaimable_raw,
                },
                prev_hash=records[-1]["record_sha256"],
            )
            _fire("retention-swept")
            _write_journal_record(tx_dir, swept_phase)
        swept = swept_phase.get("swept") or {}
        report = dict(retention.get("report") or {})
        report["swept_objects"] = int(swept.get("removed") or 0)
        report["freed_bytes"] = int(swept.get("freed") or 0)
        report["generations_reclaimed"] = len(reclaimable_raw)

        records = _read_phases(tx_dir)
        if not any(r["phase"] == "completed" for r in records):
            _fire("retention-completed")
            completed = _journal_record(
                "completed",
                {
                    "operation_id": operation_id,
                    "kind": RETENTION_KIND,
                    "outcome": "success",
                    "retention": report,
                },
                prev_hash=records[-1]["record_sha256"],
            )
            _write_journal_record(tx_dir, completed)
        shutil.rmtree(tx_dir, ignore_errors=True)
        return report

    def _recover_retention_tx(
        self,
        tx_dir: Path,
        records: list[dict[str, Any]],
        result: dict[str, Any],
        *,
        auto: bool,
    ) -> None:
        """Recover history GC from its durable decision, never from mtimes."""
        del auto
        operation_id = records[0].get("operation_id") or tx_dir.name
        if any(record["phase"] == "completed" for record in records):
            shutil.rmtree(tx_dir, ignore_errors=True)
            result["recovered"].append(
                {"operation_id": operation_id, "action": "completed", "kind": RETENTION_KIND}
            )
            return
        prepared = next((r for r in records if r["phase"] == "prepared"), None)
        if prepared is None:
            self._roll_back_generation(tx_dir, operation_id, None, result)
            return
        try:
            report = self._complete_retention_tx(tx_dir, records, prepared)
        except Exception as exc:
            self._ambiguous(
                tx_dir,
                operation_id,
                result,
                f"retention recovery failed: {exc}",
            )
            return
        result["recovered"].append(
            {
                "operation_id": operation_id,
                "action": "retention-completed",
                "versions_trimmed": report.get("versions_trimmed", []),
            }
        )

    def _external_decision(
        self,
        records: list[dict[str, Any]],
        result: dict[str, Any],
    ) -> str:
        """Deterministic forward/back/ambiguous decision for a prepared (not
        yet external-published) external-only transaction: verify hashes, never
        mtimes. A target matching the recorded output hash landed (forward);
        a target with a verified backup and no matching hash is rolled back;
        anything else is ambiguous (quarantine)."""
        prepared = next(r for r in records if r["phase"] == "prepared")
        operation_id = records[0]["operation_id"]
        decision: str | None = None
        for ext in prepared.get("externals", []):
            target = Path(ext["target"])
            landed = target.is_file() and file_sha256(target) == ext.get("sha256")
            backup_ok = (
                ext.get("backup") and Path(ext["backup"]).is_file()
                and file_sha256(Path(ext["backup"])) == ext.get("backup_sha256")
            )
            if landed:
                decision = "forward"
            elif target.exists() and not backup_ok and ext.get("mode") == "create":
                # create-only output exists but does not match: it cannot have
                # been produced by this transaction (create never replaces).
                self._quarantine(target, operation_id, result, "unmatched external output")
                return "ambiguous"
            elif decision != "forward":
                decision = "back"
        return decision or "back"

    def _roll_forward(
        self,
        tx_dir: Path,
        records: list[dict[str, Any]],
        last: dict[str, Any],
        result: dict[str, Any],
    ) -> None:
        """The commit landed (pointer selected the prepared generation, or the
        external outputs were published): complete it — materialize (generation
        transactions), repair evidence + ledger, close the journal with a
        ``completed`` record built from the prepared envelope. Never re-publish
        externals, never guess."""
        operation_id = records[0]["operation_id"]
        prepared = next(r for r in records if r["phase"] == "prepared")
        generation = prepared.get("generation")
        envelope = prepared.get("envelope")
        if generation:
            gen_dir = self.generations_dir / generation
            if not gen_dir.is_dir():
                self._ambiguous(tx_dir, operation_id, result, "prepared generation directory missing")
                return
            try:
                self._materialize_root(gen_dir)
            except OSError as exc:
                self._ambiguous(tx_dir, operation_id, result, f"roll-forward materialize failed: {exc}")
                return
        self._finish_commit(tx_dir, prepared, envelope, generation, result)

    def _repair_semantic_result(
        self,
        tx_dir: Path,
        prepared: dict[str, Any] | None,
        completed: dict[str, Any] | None,
        result: dict[str, Any],
    ) -> bool:
        """Ensure ledger + evidence are durable for one transaction. Returns
        False (and records ambiguity) when a required repair fails."""
        operation_id = (completed or prepared or {}).get("operation_id") or tx_dir.name
        envelope = (completed or prepared or {}).get("envelope")
        canonical = (prepared or completed or {}).get("input_sha256") or ""
        generation = (completed or prepared or {}).get("generation")
        records = _read_phases_soft(tx_dir) or []
        prepared_rec = next((r for r in records if r["phase"] == "prepared"), None)
        ledger_anchor = (
            Path(prepared_rec["ledger_anchor"])
            if prepared_rec and prepared_rec.get("ledger_anchor")
            else (
                self.generations_dir / generation
                if generation and (self.generations_dir / generation).is_dir()
                else self.pin_path_or_none()
            )
        )
        ledger_directory = bool(prepared_rec.get("ledger_directory", True)) if prepared_rec else True
        evidence_path = (
            Path(prepared_rec["evidence_path"])
            if prepared_rec and prepared_rec.get("evidence_path")
            else None
        )
        if ledger_anchor is not None and isinstance(envelope, dict) and envelope.get("schema") == "docx2typed-result-1":
            from .protocol import operation_ledger

            record = operation_ledger.lookup_persisted(
                operation_id, ledger_anchor, directory=ledger_directory
            )
            if record is None and envelope.get("outcome") in ("success", "partial"):
                try:
                    self._write_ledger_at(envelope, canonical, ledger_anchor, ledger_directory)
                    result["recovered"].append(
                        {"operation_id": operation_id, "action": "ledger-repaired"}
                    )
                except OSError as exc:
                    self._ambiguous(tx_dir, operation_id, result, f"ledger repair failed: {exc}")
                    return False
        if evidence_path is not None and isinstance(envelope, dict):
            stored_evidence = envelope.get("evidence") or []
            if len(stored_evidence) == 1:
                try:
                    if (
                        not evidence_path.is_file()
                        or evidence_path.read_text(encoding="utf-8").strip()
                        != json.dumps(stored_evidence[0], ensure_ascii=False, indent=2, sort_keys=True)
                    ):
                        self._write_evidence(evidence_path, stored_evidence[0])
                        result["recovered"].append(
                            {"operation_id": operation_id, "action": "evidence-repaired"}
                        )
                except OSError as exc:
                    self._ambiguous(tx_dir, operation_id, result, f"evidence repair failed: {exc}")
                    return False
        return True

    def _finish_commit(
        self,
        tx_dir: Path,
        prepared: dict[str, Any],
        envelope: dict[str, Any] | None,
        generation: str | None,
        result: dict[str, Any],
    ) -> None:
        """Close an interrupted commit: repair ledger/evidence, write the
        ``completed`` journal record chained from the last phase, then remove
        the transaction directory."""
        operation_id = prepared["operation_id"]
        if not self._repair_semantic_result(tx_dir, prepared, None, result):
            return
        records = _read_phases_soft(tx_dir) or []
        prev_hash = records[-1]["record_sha256"] if records else "genesis"
        completed = _journal_record(
            "completed",
            {
                "operation_id": operation_id,
                "generation": generation,
                "parent": prepared.get("parent"),
                "outcome": (envelope or {}).get("outcome", "success"),
                "input_sha256": prepared.get("input_sha256", ""),
                "envelope": envelope,
            },
            prev_hash=prev_hash,
        )
        try:
            _write_journal_record(tx_dir, completed)
        except OSError as exc:
            self._ambiguous(tx_dir, operation_id, result, f"completed journal close failed: {exc}")
            return
        shutil.rmtree(tx_dir, ignore_errors=True)
        result["recovered"].append(
            {"operation_id": operation_id, "action": "completed", "generation": generation}
        )

    def _settle_completed(
        self,
        tx_dir: Path,
        completed: dict[str, Any],
        result: dict[str, Any],
    ) -> None:
        """A completed record exists: ensure its semantic result (ledger + run
        evidence) is durable, then remove the transaction directory."""
        operation_id = completed.get("operation_id") or tx_dir.name
        generation = completed.get("generation")
        if not self._repair_semantic_result(tx_dir, None, completed, result):
            return
        shutil.rmtree(tx_dir, ignore_errors=True)
        result["recovered"].append(
            {"operation_id": operation_id, "action": "completed", "generation": generation}
        )

    def pin_path_or_none(self) -> Path | None:
        pointer = _read_pointer(self.root)
        if pointer is None:
            return None
        gen_dir = self.generations_dir / pointer["generation"]
        return gen_dir if gen_dir.is_dir() else None

    def _roll_back_externals(
        self,
        tx_dir: Path,
        records: list[dict[str, Any]],
        result: dict[str, Any],
    ) -> None:
        external = next(
            (r for r in records if r["phase"] == "external-published"), None
        )
        operation_id = records[0]["operation_id"]
        for ext in (external or {}).get("externals", []):
            target = Path(ext["target"])
            backup = ext.get("backup")
            if backup and Path(backup).is_file():
                if file_sha256(Path(backup)) != ext.get("backup_sha256"):
                    self._quarantine(target, operation_id, result, "backup hash mismatch")
                    continue
                try:
                    os.replace(Path(backup), target)
                    _fsync_dir(target.parent)
                except OSError as exc:
                    self._quarantine(target, operation_id, result, f"backup restore failed: {exc}")
                    continue
            elif ext.get("mode") == "create" and target.exists():
                # Create-only output: it did not exist before the transaction.
                try:
                    target.unlink()
                    _fsync_dir(target.parent)
                except OSError as exc:
                    self._quarantine(target, operation_id, result, f"output removal failed: {exc}")
                    continue
            result["rolled_back"].append(
                {"operation_id": operation_id, "target": str(target)}
            )
        # Remove prepared external staging.
        staging = self.staging_dir / operation_id
        shutil.rmtree(staging, ignore_errors=True)

    def _roll_back_generation(
        self,
        tx_dir: Path,
        operation_id: str,
        prepared_gen: str | None,
        result: dict[str, Any],
    ) -> None:
        if prepared_gen:
            shutil.rmtree(self.generations_dir / prepared_gen, ignore_errors=True)
        staging = self.staging_dir / operation_id
        shutil.rmtree(staging, ignore_errors=True)
        records = _read_phases_soft(tx_dir)
        prev_hash = records[-1]["record_sha256"] if records else "genesis"
        completed = _journal_record(
            "completed",
            {
                "operation_id": operation_id,
                "outcome": "rolled-back",
                "generation": prepared_gen,
            },
            prev_hash=prev_hash,
        )
        try:
            _write_journal_record(tx_dir, completed)
        except OSError as exc:
            # Reserve may be depleted; keep the journal inspectable.
            try:
                _write_journal_record(tx_dir, completed)
            except OSError:
                pass
        result["rolled_back"].append({"operation_id": operation_id})
        shutil.rmtree(tx_dir, ignore_errors=True)

    def _ambiguous(
        self,
        tx_dir: Path,
        operation_id: str,
        result: dict[str, Any],
        reason: str,
    ) -> None:
        result["needs_recovery"].append(
            {"operation_id": operation_id, "reason": reason}
        )

    def _quarantine(
        self,
        target: Path,
        operation_id: str,
        result: dict[str, Any],
        reason: str,
    ) -> None:
        destination = self.quarantine_dir / f"{operation_id}-{uuid.uuid4().hex[:8]}-{target.name}"
        destination.parent.mkdir(parents=True, exist_ok=True)
        try:
            os.replace(target, destination)
        except OSError as exc:
            result["needs_recovery"].append(
                {"operation_id": operation_id, "reason": f"quarantine move failed: {exc}"}
            )
            return
        result["needs_recovery"].append(
            {"operation_id": operation_id, "reason": reason, "quarantined": str(destination)}
        )

    def _gc_abandoned(self, result: dict[str, Any]) -> None:
        """Delete abandoned temp generations and staging not referenced by the
        pointer, the version chain, or any transaction journal. No speculative
        GC beyond that."""
        referenced: set[str] = set()
        pointer = _read_pointer(self.root)
        if pointer and pointer.get("generation"):
            referenced.add(pointer["generation"])
        # Every generation the version chain names is user history: dropping one
        # would turn a listed version into an unrestorable one (ADR 0042/0043).
        for record in _version_chain(self.root):
            generation = record.get("generation")
            if isinstance(generation, str):
                referenced.add(generation)
            else:
                break
        if self.transactions_dir.is_dir():
            for tx_dir in self.transactions_dir.iterdir():
                if not tx_dir.is_dir():
                    continue
                for record in _read_phases_soft(tx_dir) or []:
                    generation = record.get("generation")
                    if isinstance(generation, str):
                        referenced.add(generation)
        if self.generations_dir.is_dir():
            for gen_dir in self.generations_dir.iterdir():
                if gen_dir.is_dir() and gen_dir.name not in referenced:
                    shutil.rmtree(gen_dir, ignore_errors=True)
                    result["cleaned"].append(f"generation/{gen_dir.name}")
        for name in ("staging",):
            root = self.store_dir / name
            if root.is_dir():
                for child in root.iterdir():
                    if child.name not in {
                        tx.name for tx in (self.transactions_dir.iterdir() if self.transactions_dir.is_dir() else [])
                    }:
                        shutil.rmtree(child, ignore_errors=True)
                        result["cleaned"].append(f"{name}/{child.name}")

    # -- mutation ----------------------------------------------------------

    def mutate(
        self,
        *,
        operation: str,
        operation_id: str,
        canonical: str,
        input_sha256: str,
        expected_generation: str | None,
        run: Callable[[Path, "Transaction"], tuple[Any, ...]],
        generation: bool = True,
        ledger_anchor: Path | None = None,
        ledger_directory: bool = True,
        evidence_path: Path | None = None,
        kind: str = "mutation",
        lock_timeout_ms: int = 0,
    ) -> dict[str, Any]:
        """Run one mutation under the Writer lane with journaled phases.

        ``run(target, tx)`` returns ``(outcome, data, kind, payload,
        diagnostics)``; external outputs are staged by ``tx.stage_external``
        and published by this method after the ``prepared`` journal record.
        Returns the committed Result envelope (ledger + journal durable).

        ``generation=True`` mutates a fresh immutable generation snapshot and
        commits the pointer (workdir mutations); ``target`` is the snapshot.
        ``generation=False`` only publishes external outputs (build, decide
        new-baseline): ``target`` is the workdir root (reads pin the current
        generation), the pointer never moves, and the ledger/evidence live at
        the caller-provided external anchors.

        Commit ordering (deterministic recovery): intent -> prepared ->
        external-published -> pointer CAS -> ledger -> materialize ->
        completed journal. Every cut point yields only old/new/needs-recovery.
        """
        profiler = _MutationProfiler(operation, operation_id)
        with self.writer(timeout_ms=lock_timeout_ms):
            self._require_reserve()
            if self.transactions_dir.exists() and any(self.transactions_dir.iterdir()):
                # Startup recovery: settle/roll-back any journal left by a
                # crashed or completed run, and GC abandoned generations.
                recovery = self._recover_all(
                    {"recovered": [], "rolled_back": [], "needs_recovery": [], "cleaned": []},
                    auto=True,
                )
                if recovery["needs_recovery"]:
                    raise NeedsRecovery(
                        "workdir needs recovery: "
                        + "; ".join(
                            f"{item['operation_id']} ({item.get('reason', 'ambiguous')})"
                            for item in recovery["needs_recovery"]
                        )
                    )
            pointer = _read_pointer(self.root)
            if pointer is None:
                raise NeedsRecovery(
                    f"workdir pointer is missing or corrupt: {self.root / POINTER_FILE}"
                )
            current = pointer["generation"]
            if current != expected_generation:
                raise GenerationConflict(
                    f"expected parent generation {expected_generation}, current is {current}"
                )
            manifest_sha256 = pointer.get("manifest_sha256")
            if manifest_sha256 and input_sha256 and manifest_sha256 != input_sha256:
                raise GenerationConflict(
                    "generation content changed since planning "
                    f"(expected manifest {input_sha256}, current {manifest_sha256})"
                )
            # Operation-ID semantics are part of the store contract: identical
            # op-id + canonical input replays the original envelope without a
            # second effect; changed input is rejected before any journaling.
            # The lookup hits the generation the record was written under
            # (records live in the generation the operation committed, and the
            # pointer may have advanced past it since).
            with profiler.phase("ledger-lookup"):
                prior, _corrupt_path = self.lookup_ledger(
                    operation_id,
                    generation=generation,
                    anchor=ledger_anchor,
                    directory=ledger_directory,
                )
            if prior is not None:
                prior_envelope = prior.get("envelope")
                if prior["input_sha256"] == canonical and isinstance(prior_envelope, dict):
                    profiler.finish("replay")
                    return prior_envelope
                raise StoreError(
                    "operation-id-reused",
                    f"operation_id {operation_id!r} was already used with different canonical input",
                )
            generation_id = uuid.uuid4().hex
            with profiler.phase("journal-intent"):
                tx_dir, intent = self._begin_journal(
                    operation_id,
                    canonical,
                    expected_generation,
                    input_sha256,
                    kind,
                )
            pointer_committed = False
            try:
                with profiler.phase("generation-copy"):
                    if generation:
                        gen_dir = self._copy_generation(current, generation_id)
                        self._overlay_ingress(gen_dir)
                        target: Path = gen_dir
                    else:
                        gen_dir = None
                        target = self.root
                transaction = Transaction(self, tx_dir, operation_id, generation_id)
                if evidence_path is not None:
                    transaction.set_evidence_path(evidence_path)
                with profiler.phase("operation-run"):
                    result = run(target, transaction)
                outcome, data, kind_name, payload, diagnostics = result
                from .protocol import result_envelope, run_evidence
                with profiler.phase("result-envelope"):
                    evidence = run_evidence(
                        operation,
                        outcome,
                        kind=kind_name,
                        operation_id=operation_id,
                        payload=payload,
                    )
                    envelope = result_envelope(
                        operation,
                        outcome,
                        data={"operation_id": operation_id, **data},
                        diagnostics=diagnostics,
                        evidence=[evidence],
                    )
                externals = transaction.externals()
                version_record: dict[str, Any] | None = None
                if generation and transaction.save_boundary is not None:
                    # The save boundary turns this mutation into a user Version
                    # (ADR 0044). The tree digest covers the canonical assets only,
                    # so evidence or derived views changing never invents a version.
                    boundary = transaction.save_boundary
                    predecessor = _read_pointer(self.root) or {}
                    sequence = int(predecessor.get("version_seq") or 0) + 1
                    digest = _canonical_tree_digest(gen_dir)
                    # history lives in the object pool, not in the generation
                    # directory: the content is deduplicated, verifiable, and
                    # survives the generation being reclaimed (ADR 0043)
                    from .objectstore import build_tree, write_commit

                    with profiler.phase("version-tree"):
                        _fire("version-objects")
                        tree_result = build_tree(self.root, gen_dir, digest=digest)
                    version_record = {
                        "version": f"V{sequence}",
                        "seq": sequence,
                        "parent_version": predecessor.get("head_version"),
                        "parent_generation": predecessor.get("head_version_generation"),
                        "parent_commit": predecessor.get("head_commit"),
                        "head_tree": digest,
                        "tree_object": tree_result["tree"],
                        "baseline_epoch": int(
                            boundary.get("baseline_epoch") or predecessor.get("baseline_epoch") or 1
                        ),
                        "origin": boundary.get("origin") or "commit_sync",
                        "label": boundary.get("label"),
                        "pin": bool(boundary.get("pin")),
                        "restored_from": boundary.get("restored_from"),
                        "created_at": _now_iso(),
                    }
                    with profiler.phase("version-commit"):
                        _fire("version-commit")
                        version_record["commit"] = write_commit(
                            self.root,
                            {
                                key: value
                                for key, value in version_record.items()
                                if key != "commit"
                            }
                            | {"generation": generation_id},
                        )
                if generation:
                    with profiler.phase("generation-manifest"):
                        manifest = _generation_manifest(
                            gen_dir,
                            generation=generation_id,
                            parent=current,
                            operation_id=operation_id,
                            input_sha256=canonical,
                            version=version_record,
                        )
                        _write_generation_manifest(gen_dir, manifest)
                        _fsync_tree(gen_dir)
                        manifest_sha = manifest["assets_sha256"]
                else:
                    manifest_sha = manifest_sha256 or canonical
                with profiler.phase("evidence-write"):
                    transaction.write_evidence(evidence)
                evidence_target = str(
                    transaction.evidence_path
                    if transaction.evidence_path is not None
                    else (gen_dir / "run.evidence.json" if gen_dir else self.root / "run.evidence.json")
                )
                ledger_anchor_path = ledger_anchor or (gen_dir if gen_dir else self.root)
                with profiler.phase("journal-prepared"):
                    prepared = _journal_record(
                        "prepared",
                        {
                            "operation_id": operation_id,
                            "generation": generation_id if generation else None,
                            "parent": current,
                            "input_sha256": canonical,
                            "manifest_sha256": manifest_sha,
                            "evidence_path": evidence_target,
                            "evidence_sha256": semantic_sha256(evidence),
                            "envelope": envelope,
                            "envelope_sha256": semantic_sha256(envelope),
                            "ledger_anchor": str(ledger_anchor_path),
                            "ledger_directory": bool(ledger_directory),
                            "externals": [
                                {
                                    "target": str(ext["target"]),
                                    "staged": str(ext["staged"]),
                                    "mode": ext["mode"],
                                    "sha256": ext.get("sha256"),
                                    "backup": ext.get("backup"),
                                    "backup_sha256": ext.get("backup_sha256"),
                                }
                                for ext in externals
                            ],
                        },
                        prev_hash=intent["record_sha256"],
                    )
                    _write_journal_record(tx_dir, prepared)
                with profiler.phase("external-publish"):
                    self._publish_externals(tx_dir, prepared, externals, operation_id)
                with profiler.phase("pointer-commit"):
                    committed = _journal_record(
                        "generation-committed",
                        {
                            "operation_id": operation_id,
                            "generation": generation_id if generation else None,
                            "parent": current,
                        },
                        prev_hash=prepared["record_sha256"],
                    )
                    if generation:
                        self._commit_pointer(generation_id, operation_id, manifest_sha, version_record)
                    pointer_committed = generation
                    _write_journal_record(tx_dir, committed)
                with profiler.phase("ledger-write"):
                    self._write_ledger_at(envelope, canonical, ledger_anchor_path, ledger_directory)
                if generation:
                    with profiler.phase("materialize"):
                        self._materialize_root(gen_dir)
                with profiler.phase("completion"):
                    completed = _journal_record(
                        "completed",
                        {
                            "operation_id": operation_id,
                            "generation": generation_id if generation else None,
                            "parent": current,
                            "outcome": outcome,
                            "input_sha256": canonical,
                            "envelope": envelope,
                        },
                        prev_hash=committed["record_sha256"],
                    )
                    _write_journal_record(tx_dir, completed)
                    shutil.rmtree(tx_dir, ignore_errors=True)
                    shutil.rmtree(self.staging_dir / operation_id, ignore_errors=True)
                    try:
                        self.staging_dir.rmdir()
                    except OSError:
                        pass
                profiler.finish(outcome)
                return envelope
            except _Kill:
                profiler.finish("killed")
                raise
            except BaseException as exc:
                profiler.finish("failure", exc)
                self._abort(tx_dir, operation_id, generation_id, exc, current, pointer_committed)
                if isinstance(exc, OSError) and exc.errno == 28:
                    raise ReserveDepleted(
                        "workdir recovery reserve was depleted by ENOSPC; "
                        "replenish it before further mutations"
                    ) from exc
                raise

    def _write_ledger_at(
        self,
        envelope: dict[str, Any],
        canonical: str,
        anchor: Path,
        directory: bool,
    ) -> None:
        _fire("ledger-write")
        from .protocol import operation_ledger  # local: avoids import cycles

        operation_id = (
            (envelope.get("data") or {}).get("operation_id")
            or envelope.get("operation", "mutation")
        )
        operation_ledger.record(
            operation_id,
            canonical,
            envelope,
            anchor,
            directory=directory,
        )
        # the same record goes into the store's own log, so idempotency does
        # not depend on a generation that retention may reclaim
        _append_ledger_log(self.root, operation_id, {"input_sha256": canonical, "envelope": envelope})

    def _publish_externals(
        self,
        tx_dir: Path,
        prepared: dict[str, Any],
        externals: list[dict[str, Any]],
        operation_id: str,
    ) -> None:
        if not externals:
            return
        for ext in externals:
            target = Path(ext["target"])
            staged = Path(ext["staged"])
            _fire(f"external-publish-{target.name}", staged)
            if ext.get("mode") == "replace" and target.exists():
                _fire("external-backup")
                backup = self.staging_dir / operation_id / f"backup-{target.name}"
                backup.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(target, backup)
                with open(backup, "r+b") as handle:
                    os.fsync(handle.fileno())
                ext["backup"] = str(backup)
                ext["backup_sha256"] = file_sha256(backup)
            os.replace(staged, target)
            _fsync_dir(target.parent)
        published = _journal_record(
            "external-published",
            {
                "operation_id": operation_id,
                "externals": [
                    {
                        "target": str(ext["target"]),
                        "staged": str(ext["staged"]),
                        "mode": ext["mode"],
                        "sha256": ext.get("sha256"),
                        "backup": ext.get("backup"),
                        "backup_sha256": ext.get("backup_sha256"),
                    }
                    for ext in externals
                ],
            },
            prev_hash=prepared["record_sha256"],
        )
        _write_journal_record(tx_dir, published)

    def _abort(
        self,
        tx_dir: Path,
        operation_id: str,
        generation: str,
        exc: BaseException,
        parent: str | None,
        pointer_committed: bool,
    ) -> None:
        """Roll back an uncommitted mutation: remove the prepared generation
        and external staging, restore the pointer when it was already swapped,
        journal the failure. ENOSPC releases the reserve and closes the
        journal with minimal failure evidence."""
        enospc = isinstance(exc, OSError) and exc.errno == 28
        if pointer_committed and parent:
            # The pointer moved before the failure: restore the parent
            # generation so the workdir is the complete old state (never a
            # mixed generation). We hold the Writer lane, so this CAS is safe.
            try:
                parent_manifest = ""
                parent_dir = self.generations_dir / parent
                if parent_dir.is_dir():
                    try:
                        parent_manifest = json.loads(
                            (parent_dir / "generation.json").read_text(encoding="utf-8")
                        ).get("assets_sha256", "")
                    except (OSError, json.JSONDecodeError):
                        parent_manifest = ""
                self._commit_pointer(parent, None, parent_manifest)
            except OSError:
                pass
        shutil.rmtree(self.generations_dir / generation, ignore_errors=True)
        shutil.rmtree(self.staging_dir / operation_id, ignore_errors=True)
        try:
            self.staging_dir.rmdir()
        except OSError:
            pass
        records = _read_phases_soft(tx_dir) or []
        prev_hash = records[-1]["record_sha256"] if records else "genesis"
        completed = _journal_record(
            "completed",
            {
                "operation_id": operation_id,
                "generation": generation,
                "parent": parent,
                "outcome": "failed",
                "error_code": getattr(exc, "code", None) or type(exc).__name__,
                "error_message": str(exc)[:400],
                "reserve_released": enospc,
            },
            prev_hash=prev_hash,
        )
        try:
            _write_journal_record(tx_dir, completed)
        except OSError:
            if enospc:
                self._release_reserve()
            try:
                _write_journal_record(tx_dir, completed)
            except OSError:
                pass
        if enospc:
            self._release_reserve()
        shutil.rmtree(tx_dir, ignore_errors=True)


class Transaction:
    """Per-mutation journal context handed to ``run(gen_dir, tx)``: external
    output staging and evidence path registration."""

    def __init__(self, store: Store, tx_dir: Path, operation_id: str, generation: str) -> None:
        self.store = store
        self.tx_dir = tx_dir
        self.operation_id = operation_id
        self.generation = generation
        self._externals: list[dict[str, Any]] = []
        self._evidence_path: Path | None = None
        self._save_boundary: dict[str, Any] | None = None

    def staging(self, name: str) -> Path:
        """A prepared staging path for an external output (parent created)."""
        path = self.store.staging_dir / self.operation_id / name
        path.parent.mkdir(parents=True, exist_ok=True)
        return path

    def stage_external(self, target: Path, staged: Path, *, mode: str = "replace") -> None:
        """Register one external output for journaled publication. ``mode`` is
        ``"create"`` (target must not exist) or ``"replace"`` (prior output is
        backed up). ``staged`` must already be flushed."""
        sha256 = file_sha256(staged)
        self._externals.append(
            {
                "target": str(Path(target).resolve()),
                "staged": str(Path(staged).resolve()),
                "mode": mode,
                "sha256": sha256,
            }
        )

    def externals(self) -> list[dict[str, Any]]:
        return list(self._externals)

    def set_evidence_path(self, path: Path) -> None:
        self._evidence_path = Path(path)

    def mark_save_boundary(
        self,
        *,
        origin: str,
        label: str | None = None,
        pin: bool | None = None,
        restored_from: str | None = None,
        baseline_epoch: int | None = None,
    ) -> None:
        """Declare that THIS mutation creates a user Version (ADR 0044).

        Version intent belongs to the Store transaction, not to the collaboration
        layer: publishing a snapshot happens many times per task, saving a version
        happens once. Only the save boundary sets it; the default is no version."""
        self._save_boundary = {
            "origin": origin,
            "label": label,
            # only an intentional name pins a version past retention: system
            # labels ("restore V1", "accept all revisions") merely describe
            "pin": pin,
            "restored_from": restored_from,
            "baseline_epoch": baseline_epoch,
        }

    @property
    def save_boundary(self) -> dict[str, Any] | None:
        return self._save_boundary

    @property
    def evidence_path(self) -> Path | None:
        return self._evidence_path

    def write_evidence(self, evidence: dict[str, Any]) -> None:
        # Default evidence location for workdir mutations is the fresh
        # generation directory (authoritative, fsynced before the pointer
        # moves); callers pin an external sidecar for build outputs.
        target = self._evidence_path or (self.store.generations_dir / self.generation)
        if target.is_dir():
            target = target / "run.evidence.json"
        from .protocol import publish_run_evidence

        publish_run_evidence(target, evidence)
        _fsync_file(target)


# --------------------------------------------------------------------------
# Module-level seams (read pinning + store discovery)
# --------------------------------------------------------------------------

def has_store(root: str | Path) -> bool:
    return (Path(root) / STORE_DIR_NAME).is_dir()


def store_dir_path(root: str | Path) -> Path:
    return Path(root) / STORE_DIR_NAME


def _canonical_tree_digest(gen_dir: Path) -> str:
    """Digest of the authoritative inputs of one generation.

    This is the pre-Merkle stand-in for a version's tree root (ADR 0043): the
    same semantic content, so `version dirty` keeps its meaning when the object
    graph arrives."""
    assets = {
        name: (file_sha256(gen_dir / name) if (gen_dir / name).is_file() else None)
        for name in CANONICAL_ASSETS
    }
    return semantic_sha256(assets)


def canonical_tree_digest(root: str | Path) -> str:
    """Tree digest of the CURRENT generation of ``root`` (read-only)."""
    root_path = Path(root).resolve()
    if not has_store(root_path):
        return _canonical_tree_digest(root_path)
    pointer = _read_pointer(root_path) or {}
    gen_dir = root_path / STORE_DIR_NAME / "generations" / str(pointer.get("generation") or "")
    return _canonical_tree_digest(gen_dir if gen_dir.is_dir() else root_path)


def head_version(root: str | Path) -> dict[str, Any]:
    """The version HEAD names, plus whether canonical still matches it."""
    root_path = Path(root).resolve()
    pointer = _read_pointer(root_path) or {}
    current = canonical_tree_digest(root_path)
    recorded = pointer.get("head_tree")
    return {
        "version": pointer.get("head_version"),
        "generation": pointer.get("head_version_generation"),
        "seq": pointer.get("version_seq"),
        "commit": pointer.get("head_commit"),
        "tree": recorded,
        "tree_object": pointer.get("head_tree_object"),
        "baseline_epoch": pointer.get("baseline_epoch"),
        "canonical_tree": current,
        # no recorded tree yet == the document has never been saved: it counts
        # as dirty so the first commit creates V1
        "dirty": current != recorded,
    }


def _legacy_chain(root: Path, generation: str | None) -> list[dict[str, Any]]:
    """Version records that live in generation manifests (pre-object-pool
    workdirs). Newer saves write commit objects instead (ADR 0043)."""
    generations_dir = root / STORE_DIR_NAME / "generations"
    chain: list[dict[str, Any]] = []
    seen: set[str] = set()
    while isinstance(generation, str) and generation and generation not in seen:
        seen.add(generation)
        manifest_path = generations_dir / generation / "generation.json"
        if not manifest_path.is_file():
            break
        try:
            manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            break
        record = dict(manifest.get("version") or {})
        if not record:
            break
        record["generation"] = generation
        record["content"] = (
            "trimmed" if record.get("version") in trimmed_versions(root) else "retained"
        )
        chain.append(record)
        generation = record.get("parent_generation")
    return chain


def _version_chain(root: Path) -> list[dict[str, Any]]:
    """Version records from HEAD backwards, newest first.

    The commit chain in the object pool is the history; a workdir saved before
    the pool existed falls back to its generation manifests, and the two are
    joined at the point where the newest commit names a legacy parent."""
    pointer = _read_pointer(root) or {}
    try:
        from .objectstore import has as _has_object, read_commit as _read_commit
    except ImportError:  # pragma: no cover - direct script execution
        from objectstore import has as _has_object, read_commit as _read_commit

    chain: list[dict[str, Any]] = []
    seen: set[str] = set()
    trimmed = trimmed_versions(root)
    commit = pointer.get("head_commit")
    while isinstance(commit, str) and commit and commit not in seen:
        seen.add(commit)
        record = _read_commit(root, commit)
        if record is None:
            break
        record["commit"] = commit
        tree_object = record.get("tree_object")
        if isinstance(tree_object, str):
            record["content"] = (
                "trimmed"
                if record.get("version") in trimmed
                else ("retained" if _has_object(root, "tree", tree_object) else "trimmed")
            )
        else:
            record["content"] = "retained"  # content still rides in its generation
        chain.append(record)
        commit = record.get("parent_commit")
    tail = chain[-1] if chain else {}
    legacy_generation = tail.get("parent_generation") if chain else pointer.get("head_version_generation")
    chain.extend(_legacy_chain(root, legacy_generation))
    return chain


def history_list(root: str | Path, *, limit: int = 20, offset: int = 0) -> dict[str, Any]:
    """Walk the version chain from HEAD — the commit graph is the history."""
    root_path = Path(root).resolve()
    chain = _version_chain(root_path)
    head = head_version(root_path)
    window = chain[max(0, offset): max(0, offset) + max(1, limit)]
    return {
        "schema": "docx2typed-history-1",
        "current": head["version"],
        "current_tree": head["tree"],
        "version_dirty": head["dirty"],
        "total": len(chain),
        "retained": sum(1 for item in chain if item.get("content") == "retained"),
        "trimmed": sum(1 for item in chain if item.get("content") == "trimmed"),
        "offset": max(0, offset),
        "versions": window,
    }


def find_version(root: str | Path, version: str) -> dict[str, Any] | None:
    """One version record by name (``V12``), or None."""
    for record in _version_chain(Path(root).resolve()):
        if record.get("version") == version:
            return record
    return None


def read_root(root: str | Path) -> Path:
    """Read root of the current generation for one read-only operation: the
    pinned immutable generation directory for store-backed workdirs, the
    workdir itself otherwise (schema-1 compatibility). Never takes the Writer
    lane and never mutates anything — this is the lightweight entry-point
    transaction inspection."""
    root_path = Path(root).resolve()
    if not has_store(root_path):
        return root_path
    pointer = _read_pointer(root_path)
    if pointer is None:
        return root_path  # degenerate; recovery repairs on the next mutation
    gen_dir = root_path / STORE_DIR_NAME / "generations" / pointer["generation"]
    return gen_dir if gen_dir.is_dir() else root_path


def pending_recovery(root: str | Path) -> list[str]:
    """Read-only recovery warning for read-only operations (they may pin the
    last committed generation but must report the warning)."""
    if not has_store(root):
        return []
    try:
        return Store(root).recovery_warning()
    except (StoreError, OSError):
        return []


def state(root: str | Path) -> dict[str, Any]:
    """Stable diagnostics descriptor for ``inspect``/``edit status``."""
    root_path = Path(root).resolve()
    if not has_store(root_path):
        return {"schema": "docx2typed-store-state-1", "backed": False}
    pointer = _read_pointer(root_path)
    store = Store(root_path)
    return {
        "schema": "docx2typed-store-state-1",
        "backed": True,
        "generation": (pointer or {}).get("generation"),
        "pending_recovery": store.recovery_warning(),
        "reserve_depleted": (root_path / STORE_DIR_NAME / "reserve-depleted.json").exists(),
        "filesystem_qualified": True,
    }


TRIM_LOG = "history-trim.jsonl"


def _trim_log_path(root: Path) -> Path:
    return root / STORE_DIR_NAME / TRIM_LOG


def _read_trim_records(root: Path) -> list[dict[str, Any]]:
    path = _trim_log_path(root)
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except FileNotFoundError:
        return []
    except (OSError, UnicodeError) as exc:
        raise StoreInvalid(f"history trim ledger cannot be read: {path}") from exc
    records: list[dict[str, Any]] = []
    seen: set[str] = set()
    for number, line in enumerate(lines, start=1):
        if not line.strip():
            continue
        try:
            payload = json.loads(line)
        except json.JSONDecodeError as exc:
            raise StoreInvalid(f"history trim ledger line {number} is invalid JSON") from exc
        if not isinstance(payload, dict) or not isinstance(payload.get("version"), str):
            raise StoreInvalid(f"history trim ledger line {number} has no version")
        version = payload["version"]
        if version in seen:
            continue
        seen.add(version)
        records.append(payload)
    return records


def _append_trim_log(root: Path, versions: list[dict[str, Any]]) -> None:
    """Durably mark content trims before sweeping their objects.

    The ledger is append-only logically but rewritten atomically to make
    retries idempotent and to prevent a torn JSONL tail from becoming state."""
    path = _trim_log_path(root)
    existing = _read_trim_records(root)
    known = {record["version"] for record in existing}
    existing_by_version = {record["version"]: record for record in existing}
    if not versions:
        return
    merged = list(existing)
    for record in versions:
        version = record.get("version")
        if not isinstance(version, str) or not version:
            raise StoreInvalid("history trim record has no version")
        if version in known:
            recorded_tree = existing_by_version[version].get("tree_object")
            requested_tree = record.get("tree_object")
            if recorded_tree and requested_tree and recorded_tree != requested_tree:
                raise StoreInvalid(f"history trim tree mismatch for {version}")
            continue
        merged.append(
            {
                "version": version,
                "tree_object": record.get("tree_object"),
                "trimmed_at": _now_iso(),
            }
        )
        known.add(version)
    if len(merged) == len(existing):
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = b"".join(_canonical_bytes(record) + b"\n" for record in merged)
    _write_durable(
        path,
        payload,
        write_fault="retention-mark-write",
        flush_fault="retention-mark-flush",
        rename_fault="retention-mark-rename",
    )


def trimmed_versions(root: str | Path) -> set[str]:
    """Version names retention has trimmed (empty when nothing was trimmed)."""
    return {record["version"] for record in _read_trim_records(Path(root).resolve())}


def history_verify(root: str | Path) -> dict[str, Any]:
    """Check retained content and report deliberate trims separately.

    A missing object is detected and named; content intentionally released by
    retention is reported as ``trimmed`` rather than corruption."""
    root_path = Path(root).resolve()
    try:
        from .objectstore import has as _has_object, verify as _verify_tree
    except ImportError:  # pragma: no cover - direct script execution
        from objectstore import has as _has_object, verify as _verify_tree

    results: list[dict[str, Any]] = []
    ok = True
    trimmed = trimmed_versions(root_path)
    for record in _version_chain(root_path):
        name = str(record.get("version"))
        if name in trimmed:
            # content dropped on purpose: reported, never counted as damage
            results.append({"version": name, "content": "trimmed", "missing": []})
            continue
        tree_object = record.get("tree_object")
        if isinstance(tree_object, str) and tree_object:
            report = (
                _verify_tree(root_path, tree_object)
                if _has_object(root_path, "tree", tree_object)
                else {"ok": False, "missing": ["tree"], "checked": 0}
            )
            results.append({
                "version": name,
                "content": "retained" if report["ok"] else "missing",
                "missing": report["missing"][:5],
            })
            ok = ok and report["ok"]
            continue
        generation = root_path / STORE_DIR_NAME / "generations" / str(record.get("generation") or "")
        present = generation.is_dir()
        results.append({
            "version": name,
            "content": "retained" if present else "missing",
            "missing": [] if present else ["generation"],
        })
        ok = ok and present
    return {"schema": "docx2typed-history-verify-1", "ok": ok, "versions": results}


def _retention_plan(
    root_path: Path,
    store: Store,
    *,
    keep_last: int,
    reclaim_generations: bool,
    dry_run: bool,
) -> dict[str, Any]:
    chain = _version_chain(root_path)
    trimmed_names = trimmed_versions(root_path)
    retained = [
        record
        for index, record in enumerate(chain)
        if record.get("version") not in trimmed_names
        and (index < keep_last or record.get("pin"))
    ]
    retained_names = {record.get("version") for record in retained}
    keep_trees = {r["tree_object"] for r in retained if r.get("tree_object")}
    trimmed: list[dict[str, Any]] = []
    for record in chain:
        version = record.get("version")
        if version in trimmed_names:
            trimmed.append(record)
        elif version not in retained_names and record.get("tree_object") not in keep_trees:
            # A version sharing a retained tree still has restorable content.
            trimmed.append(record)
    trimmed_version_names = {r.get("version") for r in trimmed}
    keep_generations = {
        r.get("generation")
        for r in chain
        if not r.get("tree_object") and r.get("version") not in trimmed_version_names
    }
    pointer = _read_pointer(root_path) or {}
    if pointer.get("generation"):
        keep_generations.add(pointer["generation"])
    if store.transactions_dir.is_dir():
        for tx_dir in store.transactions_dir.iterdir():
            for record in _read_phases_soft(tx_dir) or []:
                if isinstance(record.get("generation"), str):
                    keep_generations.add(record["generation"])

    reclaimable: list[str] = []
    if reclaim_generations and store.generations_dir.is_dir():
        for gen_dir in sorted(store.generations_dir.iterdir()):
            if not gen_dir.is_dir() or gen_dir.name in keep_generations:
                continue
            try:
                manifest = json.loads((gen_dir / "generation.json").read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError):
                continue
            version = manifest.get("version") or {}
            tree_object = version.get("tree_object")
            version_name = version.get("version")
            if isinstance(tree_object, str) and tree_object in keep_trees:
                reclaimable.append(gen_dir.name)
            elif version_name in trimmed_version_names:
                reclaimable.append(gen_dir.name)

    report: dict[str, Any] = {
        "schema": "docx2typed-history-gc-1",
        "versions_total": len(chain),
        "versions_retained": len(chain) - len(trimmed),
        "trees_kept": len(keep_trees),
        "generations_reclaimable": len(reclaimable),
        "dry_run": dry_run,
        "swept_objects": 0,
        "freed_bytes": 0,
        "versions_trimmed": [r.get("version") for r in trimmed],
    }
    newly_trimmed = [
        record for record in trimmed if record.get("version") not in trimmed_names
    ]
    return {
        "report": report,
        "trimmed_records": trimmed,
        "keep_trees": sorted(keep_trees),
        "reclaimable": reclaimable,
        "newly_trimmed": newly_trimmed,
    }


def _sweep_retention(
    root_path: Path,
    *,
    keep_trees: set[str],
    reclaimable: list[str],
) -> dict[str, Any]:
    _fire("retention-sweep")
    try:
        from .objectstore import sweep as _sweep
    except ImportError:  # pragma: no cover - direct script execution
        from objectstore import sweep as _sweep

    swept = _sweep(root_path, keep_trees=keep_trees)
    store_root = store_dir_path(root_path)
    for generation in reclaimable:
        _fire("retention-generation")
        shutil.rmtree(store_root / "generations" / generation, ignore_errors=True)
    return swept


def history_gc(
    root: str | Path,
    *,
    keep_last: int = 50,
    reclaim_generations: bool = True,
    dry_run: bool = True,
) -> dict[str, Any]:
    """Journal retention decisions before marking, sweeping, and reclaiming.

    The trim ledger is a durable marker, not a history authority. Applied
    decisions recover through ``retention-marked``, ``retention-swept``, and
    ``completed`` phases; ``dry_run`` never creates those records."""
    root_path = Path(root).resolve()
    store = Store(root_path)
    with store.writer(timeout_ms=0):
        if dry_run:
            return _retention_plan(
                root_path,
                store,
                keep_last=keep_last,
                reclaim_generations=reclaim_generations,
                dry_run=True,
            )["report"]

        store._require_reserve()
        recovery = store._recover_all(
            {"recovered": [], "rolled_back": [], "needs_recovery": [], "cleaned": []},
            auto=True,
        )
        if recovery["needs_recovery"]:
            raise NeedsRecovery(
                "workdir needs recovery: "
                + "; ".join(
                    f"{item['operation_id']} ({item.get('reason', 'ambiguous')})"
                    for item in recovery["needs_recovery"]
                )
            )
        plan = _retention_plan(
            root_path,
            store,
            keep_last=keep_last,
            reclaim_generations=reclaim_generations,
            dry_run=False,
        )
        if not plan["newly_trimmed"] and not plan["reclaimable"]:
            return plan["report"]

        pointer = _read_pointer(root_path) or {}
        operation_id = f"history-gc-{uuid.uuid4().hex}"
        canonical = semantic_sha256(
            {
                "operation": "history_gc",
                "keep_last": keep_last,
                "reclaim_generations": reclaim_generations,
                "dry_run": False,
                "versions_trimmed": plan["report"]["versions_trimmed"],
                "keep_trees": plan["keep_trees"],
                "reclaimable": plan["reclaimable"],
            }
        )
        tx_dir, intent = store._begin_journal(
            operation_id,
            canonical,
            pointer.get("generation"),
            canonical,
            RETENTION_KIND,
        )
        _fire("retention-decision")
        prepared = _journal_record(
            "prepared",
            {
                "operation_id": operation_id,
                "kind": RETENTION_KIND,
                "parent": pointer.get("generation"),
                "retention": plan,
            },
            prev_hash=intent["record_sha256"],
        )
        _write_journal_record(tx_dir, prepared)
        _fire("retention-prepared")
        return store._complete_retention_tx(tx_dir, [intent, prepared], prepared)


def _copy_root_assets(root: Path, gen_dir: Path) -> None:
    """Copy every root workdir file into a generation snapshot. The private
    store directory and pointer are never part of a generation."""
    for path in _walk_files(root):
        rel = path.relative_to(root).as_posix()
        if rel.startswith(STORE_DIR_NAME + "/") or rel == POINTER_FILE:
            continue
        target = gen_dir / rel
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(path, target)
