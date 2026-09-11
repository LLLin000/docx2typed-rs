"""Serve the document review console and its agent handoff API.

The server binds to localhost by default. ``--tailscale`` binds only to the
machine's Tailscale IPv4 address so a phone on the same tailnet can use the
review console without exposing it on every network interface. The browser
writes review drafts to the workdir inbox, and an explicit dispatch moves them
to the MCP-visible ``queued`` state.

Usage: python -m docx2typed review WORKDIR --tailscale --port 8876
"""
from __future__ import annotations

import argparse
import ipaddress
import json
import re
import shutil
import subprocess
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlparse

try:
    from .review_collab import CollaborationError, document_state, document_state_readonly, external_write_guard, publish_current, settle_decisions, stage_patch
    from .review_console import render_document_fragment, render_html, review_history
    from .review_queue import dispatch, snapshot, snapshot_readonly, upsert_event
    from .protocol import canonical_operation_input, result_envelope
    from .review_security import (
        CONTENT_SECURITY_POLICY,
        NOT_FOUND_BODY,
        ReviewSecurity,
        UnauthorizedThrottle,
        build_allowlist,
        content_type_allowed,
        generate_capability,
        session_fingerprint,
    )
    from .store import (
        GENERATION_CONFLICT,
        NEEDS_RECOVERY,
        OPERATION_JOURNAL_CONFLICT,
        RESERVE_DEPLETED,
        STORE_INVALID,
        UNSUPPORTED_BY_DESIGN,
        WRITER_BUSY,
        WRITER_TIMEOUT,
        Store,
        StoreError,
        read_root,
    )
except ImportError:  # pragma: no cover - direct script invocation fallback
    from review_collab import CollaborationError, document_state, document_state_readonly, external_write_guard, publish_current, settle_decisions, stage_patch  # type: ignore[no-redef]
    from review_console import render_document_fragment, render_html, review_history  # type: ignore[no-redef]
    from review_queue import dispatch, snapshot, snapshot_readonly, upsert_event  # type: ignore[no-redef]
    from protocol import canonical_operation_input, result_envelope  # type: ignore[no-redef]
    from review_security import (  # type: ignore[no-redef]
        CONTENT_SECURITY_POLICY,
        NOT_FOUND_BODY,
        ReviewSecurity,
        UnauthorizedThrottle,
        build_allowlist,
        content_type_allowed,
        generate_capability,
        session_fingerprint,
    )
    from store import (  # type: ignore[no-redef]
        GENERATION_CONFLICT,
        NEEDS_RECOVERY,
        OPERATION_JOURNAL_CONFLICT,
        RESERVE_DEPLETED,
        STORE_INVALID,
        UNSUPPORTED_BY_DESIGN,
        WRITER_BUSY,
        WRITER_TIMEOUT,
        Store,
        StoreError,
        read_root,
    )


_MAX_BODY = 256 * 1024
_IDEMPOTENCY_RE = re.compile(r"^[A-Za-z0-9_-]{8,128}$")
# Every handler socket gets a bounded read timeout (issue #51 finding 1): a
# stalled client — mid-request-line, mid-body, or mid-drain — releases its
# thread when the bound expires instead of pinning it forever.
_SOCKET_TIMEOUT = 30.0
# Unsupported methods that pass the Host+capability gate receive this uniform
# 501 (issue #51 finding 4); unauthorized ones keep the byte-identical 404.
_UNSUPPORTED_BODY = {"error": "method-not-supported"}


def _review_mutation(
    target: Path,
    run: Callable[[Path], dict[str, object]],
    *,
    extra: Callable[[Path], dict[str, object]],
) -> tuple[str, dict[str, object], str, dict[str, object], list[dict[str, object]]]:
    """Adapt one review mutation to the store's run contract. ``run(target)``
    performs the review state change against the generation snapshot; the
    response data merges the mutation result with extras computed AFTER the
    mutation lands, so success payloads carry fresh session/count state and
    the next review POST needs no extra GET (issue #50 finding 1)."""
    result = run(target)
    data: dict[str, object] = {}
    if isinstance(result, dict):
        data.update(result)
    data.update(extra(target))
    return (
        "success",
        data,
        "mutation",
        {"checks": [{"name": "review-mutation", "status": "pass"}]},
        [],
    )


def _json_bytes(value: object) -> bytes:
    # Stable bytes are part of idempotent HTTP replay: the first response and
    # the persisted envelope must serialize to the same body.
    return json.dumps(value, ensure_ascii=False, sort_keys=True).encode("utf-8")

# Browser surfaces never receive store-failure internals: absolute paths,
# Windows drive letters, or transient temp filenames embedded in StoreError
# messages. The detail is the stable per-code message — identical on every
# machine and run — so the review console shows a deterministic diagnostic.
_STORE_ERROR_DETAIL: dict[str, str] = {
    STORE_INVALID: "store state is invalid; inspect the workdir for recovery status",
    WRITER_BUSY: "another writer is active; retry after it finishes",
    WRITER_TIMEOUT: "writer lane timed out; retry after the current writer finishes",
    GENERATION_CONFLICT: "workdir changed since planning; re-read the workdir and retry",
    NEEDS_RECOVERY: "workdir needs recovery; run the recovery pass before retrying",
    RESERVE_DEPLETED: "workdir recovery reserve is depleted; the workdir is read-only",
    UNSUPPORTED_BY_DESIGN: "filesystem does not meet the store durability requirements",
    OPERATION_JOURNAL_CONFLICT: "operation journal conflict; retry with a fresh operation id",
    "workdir-unreadable": "workdir cannot be opened as a store-backed workdir",
}

def _error_payload(exc: Exception) -> dict[str, str]:
    code = str(getattr(exc, "code", "") or "")
    detail = str(getattr(exc, "detail", "") or str(exc))
    if not code:
        match = re.match(r"^([a-z][a-z0-9-]*):\s*(.*)$", detail)
        if match:
            code, detail = match.groups()
    elif code in _STORE_ERROR_DETAIL:
        detail = _STORE_ERROR_DETAIL[code]
    return {"error": detail, "code": code or "server-error"}

def _tailscale_ipv4() -> str:
    command = shutil.which("tailscale")
    if not command:
        raise RuntimeError("tailscale-not-installed: install Tailscale and add it to PATH")
    try:
        result = subprocess.run(
            [command, "ip", "-4"],
            check=True,
            capture_output=True,
            text=True,
            timeout=5,
        )
    except (OSError, subprocess.SubprocessError) as exc:
        raise RuntimeError(f"tailscale-unavailable: could not query Tailscale ({exc})") from exc
    for token in result.stdout.split():
        try:
            address = ipaddress.ip_address(token)
        except ValueError:
            continue
        if address.version == 4:
            return str(address)
    raise RuntimeError("tailscale-no-ipv4: Tailscale returned no IPv4 address")


def _handler_for(workdir: Path, security: ReviewSecurity, *, socket_timeout: float = _SOCKET_TIMEOUT) -> type[BaseHTTPRequestHandler]:
    """Build the handler class for one review session. ``security`` pins the
    single process-scoped capability and the advertised origin allowlist; the
    static bootstrap shell is rendered once at startup (it carries no
    document/review/state/workdir data) and served publicly at ``/``.
    ``socket_timeout`` bounds every connection's socket reads (tests use a
    short value to prove stalled drains release their threads)."""
    throttle = UnauthorizedThrottle()
    shell_payload = render_html(workdir, server_mode=True).encode("utf-8")
    mutation_routes = frozenset({
        "/api/reviews",
        "/api/reviews/patch",
        "/api/reviews/dispatch",
        "/api/reviews/settle",
        "/api/reviews/publish",
        "/api/reviews/external-preflight",
    })

    class ReviewHandler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.0"
        timeout = socket_timeout  # bounded socket reads: stalled clients release their thread

        def _send(self, status: int, content_type: str, payload: bytes, *, head: bool = False, extra_headers: dict[str, str] | None = None) -> None:
            try:
                self.send_response(status)
                self.send_header("Content-Type", content_type)
                self.send_header("Content-Length", str(len(payload)))
                self.send_header("Cache-Control", "no-store")
                self.send_header("Referrer-Policy", "no-referrer")
                self.send_header("X-Content-Type-Options", "nosniff")
                self.send_header("X-Frame-Options", "DENY")
                self.send_header("Content-Security-Policy", CONTENT_SECURITY_POLICY)
                for name, value in (extra_headers or {}).items():
                    self.send_header(name, value)
                self.end_headers()
                if not head:
                    self.wfile.write(payload)
            except OSError:
                # The peer is gone or stalled past the socket timeout; the
                # response is undeliverable, so close without a traceback.
                self.close_connection = True

        def _send_json(self, status: int, value: object, *, head: bool = False, extra_headers: dict[str, str] | None = None) -> None:
            self._send(status, "application/json; charset=utf-8", _json_bytes(value), head=head, extra_headers=extra_headers)

        def _client(self) -> str:
            return str(self.client_address[0])

        def _content_length(self) -> int:
            """Content-Length parsed defensively (issue #51 finding 3):
            missing, empty, non-numeric, and negative values all read as 0 so
            a malformed header can never raise past the request gate."""
            raw = self.headers.get("Content-Length", "")
            try:
                length = int(raw)
            except (TypeError, ValueError):
                return 0
            return length if length > 0 else 0

        def _drain_body(self) -> None:
            """Consume a rejected request's body so the connection can close
            cleanly (bounded to the size cap; the cap is generous for a
            legitimate client and a lying Content-Length is not followed).
            Bounded by the socket timeout: a stalled drain releases its
            thread and closes the connection instead of pinning it."""
            length = self._content_length()
            if length <= 0:
                return
            remaining = min(length, _MAX_BODY + 1)
            if remaining:
                try:
                    self.rfile.read(remaining)
                except OSError:
                    self.close_connection = True

        def _deny(self, *, head: bool = False) -> None:
            """Uniform detail-free refusal for missing/invalid authority, with
            bounded per-client throttling of unauthorized attempts."""
            if throttle.allow(self._client()):
                self._send_json(404, NOT_FOUND_BODY, head=head)
            else:
                self._send_json(429, NOT_FOUND_BODY, head=head, extra_headers={"Retry-After": "1"})

        def _gate(self, *, head: bool = False) -> bool:
            """Host allowlist + capability gate for every protected route.
            ``X-Forwarded-*`` is never consulted; only the raw Host header
            decides, so proxy-forwarding and DNS-rebinding hostnames fail."""
            header = self.headers.get("Host", "")
            scheme, _, token = self.headers.get("Authorization", "").partition(" ")
            if security.host_allowed(header) and scheme.lower() == "bearer" and security.verify(token):
                return True
            self._deny(head=head)
            return False

        def _gate_mutation(self) -> bool:
            """Browser-origin gates for mutating POSTs: Origin must equal an
            advertised origin and Sec-Fetch-Site must be same-origin. A
            browser write without Origin fails. Applied only after the
            capability gate, so only authorized protocol errors observe it."""
            origin = self.headers.get("Origin", "")
            if not origin or not security.origin_allowed(origin):
                self._send_json(403, {"error": "forbidden", "code": "origin-mismatch"})
                return False
            if self.headers.get("Sec-Fetch-Site", "") != "same-origin":
                self._send_json(403, {"error": "forbidden", "code": "fetch-site-mismatch"})
                return False
            return True

        def _method_not_supported(self) -> None:
            """OPTIONS/PUT/DELETE/TRACE/PATCH pass the same Host+capability
            gate and unauthorized throttle as every other route (issue #51
            finding 4): a gated request gets the uniform 501, an unauthorized
            one the byte-identical 404, both with the full security header
            set and never any CORS headers. The body is drained bounded."""
            if not self._gate():
                self._drain_body()
                return
            self._drain_body()
            self._send_json(501, _UNSUPPORTED_BODY)

        def do_OPTIONS(self) -> None:  # noqa: N802 - stdlib handler API
            self._method_not_supported()

        def do_PUT(self) -> None:  # noqa: N802 - stdlib handler API
            self._method_not_supported()

        def do_DELETE(self) -> None:  # noqa: N802 - stdlib handler API
            self._method_not_supported()

        def do_TRACE(self) -> None:  # noqa: N802 - stdlib handler API
            self._method_not_supported()

        def do_PATCH(self) -> None:  # noqa: N802 - stdlib handler API
            self._method_not_supported()

        def __getattr__(self, name: str):
            """Catch-all for any verb without an explicit ``do_*`` handler
            (CONNECT, BREW, …). BaseHTTPRequestHandler dispatches via
            ``hasattr(self, "do_"+command)``, so returning a bound
            ``_method_not_supported`` routes every unknown verb through the
            same Host+capability gate, throttle, and security header set
            instead of the stdlib 501 fallback."""
            if name.startswith("do_"):
                return self._method_not_supported
            raise AttributeError(name)

        def _route_read(self, *, head: bool) -> None:
            path = urlparse(self.path).path
            if path == "/":
                # The bootstrap shell is public by design and carries no
                # document/review/state/workdir data, but it is still bound to
                # the advertised Host so a rebinding probe learns nothing.
                if not security.host_allowed(self.headers.get("Host", "")):
                    self._deny(head=head)
                    return
                self._send(200, "text/html; charset=utf-8", shell_payload, head=head)
                return
            if not self._gate(head=head):
                return
            request = urlparse(self.path)
            query = parse_qs(request.query)
            # Read endpoints pin the current immutable generation so a GET or
            # HEAD never creates a queue, session, snapshot, or history record.
            root = read_root(workdir)
            try:
                if path == "/api/reviews":
                    self._send_json(200, {**snapshot_readonly(root), "session": document_state_readonly(root)}, head=head)
                elif path == "/api/document-state":
                    self._send_json(200, document_state_readonly(root), head=head)
                elif path == "/api/document-fragment":
                    fragment = render_document_fragment(root)
                    self._send_json(
                        200,
                        {
                            **fragment,
                            "history": review_history(root, include_fragments=False),
                            "review": snapshot_readonly(root),
                            "session": document_state_readonly(root),
                        },
                        head=head,
                    )
                elif path == "/api/review-history":
                    selected = query.get("snapshot", [None])[0]
                    history = review_history(root, snapshot_id=selected, include_fragments=True)
                    self._send_json(200, {"history": history}, head=head)
                elif path == "/health":
                    self._send_json(200, {"ok": True, "service": "docx2typed-review"}, head=head)
                else:
                    self._send_json(404, NOT_FOUND_BODY, head=head)
            except Exception as exc:  # noqa: BLE001 - turn local errors into API diagnostics
                self._send_json(500, _error_payload(exc), head=head)

        def _read_json(self) -> dict[str, object]:
            content_type = self.headers.get("Content-Type", "")
            length = self._content_length()
            if length <= 0:
                raise ValueError("request-body-required: this endpoint requires a JSON object body")
            if not content_type_allowed(content_type):
                self._drain_body()
                raise ValueError(
                    "unsupported-content-type: only application/json bodies are accepted"
                )
            if length > _MAX_BODY:
                self._drain_body()
                raise ValueError("request-body-too-large: request body exceeds 256 KiB")
            raw = self.rfile.read(length)
            try:
                value = json.loads(raw.decode("utf-8"))
            except (UnicodeDecodeError, json.JSONDecodeError) as exc:
                raise ValueError("invalid-json-body: request body is not valid JSON") from exc
            if not isinstance(value, dict):
                raise ValueError("request-body-not-object: request body must be an object")
            return value

        def _idempotency_key(self) -> str:
            """Every mutating review POST requires an Idempotency-Key with the
            same syntax and ledger behavior as CLI/MCP operation IDs (issue
            #34): identical key + canonical payload replays the original
            response; a changed payload is rejected as operation-id-reused."""
            key = self.headers.get("Idempotency-Key", "")
            if not key or not _IDEMPOTENCY_RE.match(key):
                raise ValueError(
                    "idempotency-key-required: Idempotency-Key header required for "
                    "mutating POSTs (8-128 chars of [A-Za-z0-9_-])"
                )
            return key

        def _post_mutation(self, path: str, payload: dict[str, object], run) -> dict[str, object]:
            """Run one review mutation through the immutable-generation store
            (Writer lane, CAS, durable journal, recovery) and return the
            response data. Replay returns the original committed data."""
            key = self._idempotency_key()
            canonical = canonical_operation_input("review_post", {"path": path, "payload": payload})
            try:
                store = Store.ensure(workdir, operation_id=key, input_sha256=canonical)
            except (StoreError, OSError) as exc:
                raise CollaborationError(
                    getattr(exc, "code", None) or "workdir-unreadable", str(exc)
                ) from exc
            pin = store.pin()

            def adapter(target, tx):
                outcome, data, kind, evidence_payload, diagnostics = run(target)
                return outcome, data, kind, evidence_payload, diagnostics

            envelope = store.mutate(
                operation="review_post",
                operation_id=key,
                canonical=canonical,
                input_sha256=pin["manifest_sha256"],
                expected_generation=pin["generation"],
                run=adapter,
            )
            return dict(envelope["data"])

        def do_GET(self) -> None:  # noqa: N802 - stdlib handler API
            self._route_read(head=False)

        def do_HEAD(self) -> None:  # noqa: N802 - stdlib handler API
            self._route_read(head=True)

        def do_POST(self) -> None:  # noqa: N802 - stdlib handler API
            path = urlparse(self.path).path
            if not self._gate():
                self._drain_body()
                return
            if path not in mutation_routes:
                self._drain_body()
                self._send_json(404, NOT_FOUND_BODY)
                return
            if not self._gate_mutation():
                self._drain_body()
                return
            try:
                if path == "/api/reviews":
                    payload = self._read_json()
                    data = self._post_mutation(
                        path,
                        payload,
                        lambda target: _review_mutation(
                            target,
                            lambda wd: stage_patch(wd, payload)
                            if payload.get("type") == "patch"
                            else upsert_event(wd, payload),
                            extra=lambda target: {
                                "counts": snapshot(target)["counts"],
                                "session": document_state(target),
                            },
                        ),
                    )
                    self._send_json(200, data)
                elif path == "/api/reviews/patch":
                    payload = self._read_json()
                    data = self._post_mutation(
                        path,
                        payload,
                        lambda target: _review_mutation(
                            target,
                            lambda wd: stage_patch(wd, payload),
                            extra=lambda target: {
                                "counts": snapshot(target)["counts"],
                                "session": document_state(target),
                            },
                        ),
                    )
                    self._send_json(200, data)
                elif path == "/api/reviews/dispatch":
                    data = self._post_mutation(
                        path,
                        {},
                        lambda target: _review_mutation(
                            target,
                            lambda wd: dispatch(wd),
                            extra=lambda target: {
                                "counts": snapshot(target)["counts"],
                                "session": document_state(target),
                            },
                        ),
                    )
                    self._send_json(200, data)
                elif path == "/api/reviews/external-preflight":
                    # A CAS guard for an external import/rollback writer,
                    # issued through the store Writer lane as an idempotent
                    # POST (issue #51 finding 5): the session bootstrap runs
                    # under the lane, so concurrent preflights can never
                    # duplicate the session or its history; the same
                    # Idempotency-Key replays the original guard. The MCP/CLI
                    # seam keeps calling external_write_guard directly.
                    payload = self._read_json()
                    data = self._post_mutation(
                        path,
                        payload,
                        lambda target: _review_mutation(
                            target,
                            lambda wd: external_write_guard(
                                wd,
                                expected_parent_snapshot=str(payload.get("expected_parent_snapshot", "")),
                                operation=str(payload.get("operation", "import")),
                            ),
                            extra=lambda target: {},
                        ),
                    )
                    self._send_json(200, data)
                elif path == "/api/reviews/settle":
                    payload = self._read_json()
                    event_ids = payload.get("event_ids")
                    if event_ids is not None and (
                        not isinstance(event_ids, list) or not all(isinstance(item, str) for item in event_ids)
                    ):
                        raise ValueError("event_ids must be a string array")
                    data = self._post_mutation(
                        path,
                        payload,
                        lambda target: _review_mutation(
                            target,
                            lambda wd: settle_decisions(wd, event_ids),
                            extra=lambda target: {
                                "counts": snapshot(target)["counts"],
                                "session": document_state(target),
                            },
                        ),
                    )
                    self._send_json(200, data)
                elif path == "/api/reviews/publish":
                    payload = self._read_json()
                    changed = payload.get("changed_paragraph_ids", [])
                    if not isinstance(changed, list) or not all(isinstance(item, str) for item in changed):
                        raise ValueError("changed_paragraph_ids must be a string array")
                    data = self._post_mutation(
                        path,
                        payload,
                        lambda target: _review_mutation(
                            target,
                            lambda wd: publish_current(
                                wd,
                                expected_parent_snapshot=str(payload.get("expected_parent_snapshot", "")),
                                origin=str(payload.get("origin", "human_ui")),
                                changed_paragraph_ids=changed,
                                batch_id=str(payload["batch_id"]) if payload.get("batch_id") else None,
                            ),
                            extra=lambda target: {
                                "counts": snapshot(target)["counts"],
                                "session": document_state(target),
                            },
                        ),
                    )
                    self._send_json(200, data)
                else:
                    self._send_json(404, NOT_FOUND_BODY)
            except CollaborationError as exc:
                self._send_json(409, _error_payload(exc))
            except StoreError as exc:
                self._send_json(409, _error_payload(exc))
            except (ValueError, json.JSONDecodeError) as exc:
                self._send_json(400, _error_payload(exc))
            except TimeoutError:
                # Stalled body past the socket bound: drop the connection
                # without a response or traceback; the thread is released.
                self.close_connection = True
            except Exception as exc:  # noqa: BLE001 - turn local errors into API diagnostics
                self._send_json(500, _error_payload(exc))

        def log_request(self, code: int | str = "-", size: int | str = "-") -> None:
            """Sanitized access log: timestamp, request category, result
            class, source class, and an irreversible truncated session hash.
            Never logs Authorization, fragment, query, body, selected text,
            comments/notes, or workdir paths."""
            command = self.command
            path = urlparse(self.path).path
            if command == "POST":
                category = "write" if path in mutation_routes else "unknown"
            elif command in ("GET", "HEAD"):
                category = "bootstrap" if path == "/" else "read" if path == "/health" or path.startswith("/api/") else "unknown"
            else:
                category = "unknown"
            code_class = f"{int(code) // 100}xx" if str(code).isdigit() else "unknown"
            source = "loopback" if self.client_address[0] in ("127.0.0.1", "::1") else "remote"
            presented = self.headers.get("Authorization", "").partition(" ")[2]
            sid = session_fingerprint(presented) if presented else "-"
            print(
                f"[review-server] {datetime.now(timezone.utc).isoformat(timespec='seconds')} "
                f"cat={category} result={code_class} src={source} sid={sid}"
            )

        def log_message(self, format: str, *args: object) -> None:
            pass  # superseded by the sanitized log_request above

    return ReviewHandler


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("workdir", type=Path)
    network = parser.add_mutually_exclusive_group()
    network.add_argument("--host", help="bind address (default: 127.0.0.1)")
    network.add_argument(
        "--tailscale",
        action="store_true",
        help="bind only to the local Tailscale IPv4 address",
    )
    parser.add_argument("--port", type=int, default=8876)
    args = parser.parse_args(argv)
    workdir = args.workdir.resolve()
    if not (workdir / "typed.md").exists():
        parser.error(f"not a typed workdir: {workdir}")
    if args.tailscale:
        try:
            host = _tailscale_ipv4()
        except RuntimeError as exc:
            parser.error(str(exc))
    else:
        host = args.host or "127.0.0.1"
    allowed_hosts, allowed_origins = build_allowlist(host, args.port)
    security = ReviewSecurity(
        capability=generate_capability(),
        allowed_hosts=allowed_hosts,
        allowed_origins=allowed_origins,
        port=args.port,
        source_label="tailnet" if args.tailscale else "loopback",
    )
    server = ThreadingHTTPServer((host, args.port), _handler_for(workdir, security))
    # The launch fragment is consumed by the browser and retained only in
    # tab-scoped sessionStorage for refresh recovery.
    print(f"review session: http://{host}:{args.port}/#token={security.capability}")
    print(f"advertised origin: http://{host}:{args.port}/")
    print("capability is single-session and tab-scoped; restarting the server revokes it")
    if args.tailscale:
        print("access: Tailscale tailnet only; open the printed URL on a phone in the same tailnet")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\nreview server stopped")
    finally:
        server.server_close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
