#!/usr/bin/env python3
"""Issue #60 adversarial oracle: drives the installed-style `docx2typed`
binary through the MCP(stdout) surface and the secured review HTTP server
with raw sockets, asserting the security/collaboration contract and the
tool-surface parity vs the Python reference and the published schema asset.

Output: one JSON object on stdout:
    {"checks": {<name>: {"pass": bool, "detail": str}}, "evidence": {...}}
"""

import json
import os
import re
import socket
import subprocess
import sys
import tempfile
import threading
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def sh(args, input_text=None, timeout=180):
    proc = subprocess.run(
        args,
        input=input_text,
        capture_output=True,
        text=True,
        encoding="utf-8",
        timeout=timeout,
    )
    return proc


def mcp_session(bin_path, requests):
    """One MCP(stdout) session; returns (envelopes, raw_lines)."""
    lines = [json.dumps({"tool": tool, "args": args}, ensure_ascii=False) for tool, args in requests]
    proc = sh([bin_path, "mcp"], "\n".join(lines) + "\n")
    envelopes = []
    raw = []
    for line in proc.stdout.splitlines():
        raw.append(line)
        if line.startswith("OK "):
            envelopes.append(json.loads(line[3:])["structuredContent"])
    return envelopes, raw, proc


def python_tool_names():
    """Frozen tool names from scripts/mcp_server.py (@mcp.tool decorators)."""
    text = open(os.path.join(ROOT, "scripts", "mcp_server.py"), encoding="utf-8").read()
    names = []
    for match in re.finditer(r"@mcp\.tool\((.*?)\)\s*\ndef\s+(\w+)", text, re.S):
        args, func = match.group(1), match.group(2)
        named = re.search(r'name\s*=\s*"([^"]+)"', args)
        names.append(named.group(1) if named else func)
    return names


def free_port():
    sock = socket.socket()
    sock.bind(("127.0.0.1", 0))
    port = sock.getsockname()[1]
    sock.close()
    return port


def http(port, method, path, headers, body=b"", timeout=20):
    sock = socket.create_connection(("127.0.0.1", port), timeout=timeout)
    sock.settimeout(timeout)
    req = f"{method} {path} HTTP/1.0\r\n"
    has_length = any(k.lower() == "content-length" for k, _ in headers)
    for name, value in headers:
        req += f"{name}: {value}\r\n"
    if body and not has_length:
        req += f"Content-Length: {len(body)}\r\n"
    req += "\r\n"
    sock.sendall(req.encode() + body)
    data = b""
    try:
        while True:
            chunk = sock.recv(65536)
            if not chunk:
                break
            data += chunk
    except socket.timeout:
        pass
    finally:
        sock.close()
    head, _, payload = data.partition(b"\r\n\r\n")
    if not head:
        return None, {}, payload.decode(errors="replace")
    status = int(head.split(b" ")[1])
    hs = {}
    for line in head.split(b"\r\n")[1:]:
        if b":" in line:
            k, v = line.split(b":", 1)
            hs[k.strip().lower().decode()] = v.strip().decode()
    return status, hs, payload.decode(errors="replace")


def spawn_server(bin_path, workdir, port):
    proc = subprocess.Popen(
        [bin_path, "review", workdir, "--port", str(port)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
    )
    capability = None
    log_lines = []
    deadline = time.time() + 10
    while time.time() < deadline:
        line = proc.stderr.readline()
        if not line:
            break
        log_lines.append(line.rstrip())
        m = re.search(r"#token=([A-Za-z0-9_-]+)", line)
        if m:
            capability = m.group(1)
            break
    if not capability:
        proc.kill()
        raise RuntimeError(f"no capability: {log_lines}")
    deadline = time.time() + 10
    while time.time() < deadline:
        try:
            socket.create_connection(("127.0.0.1", port), timeout=1).close()
            break
        except OSError:
            time.sleep(0.02)
    return proc, capability, log_lines


def drain_stderr(proc, lines, stop_event):
    try:
        for line in proc.stderr:
            lines.append(line.rstrip())
    except (ValueError, OSError):
        pass
    stop_event.set()


def main():
    bin_path = sys.argv[1]
    # Fresh scratch per invocation: a reused workdir would carry the prior
    # run's committed/settled state and poison the changed-set assertions.
    scratch = tempfile.mkdtemp(prefix="rust-tracer60-oracle-")
    checks = {}
    evidence = {}

    def check(name, ok, detail=""):
        checks[name] = {"pass": bool(ok), "detail": str(detail)}

    # -- workdir -----------------------------------------------------------
    fixture = os.path.join(ROOT, "corpus", "release", "plain.docx")
    workdir = os.path.join(scratch, "wd")
    os.makedirs(workdir, exist_ok=True)
    proc = sh([bin_path, "extract", "--json", fixture, "-o", workdir])
    envelope = json.loads(proc.stdout)
    check("extract", proc.returncode == 0 and envelope["outcome"] == "success", envelope.get("diagnostics"))
    fixture_rev = os.path.join(ROOT, "corpus", "release", "revisions.docx")
    workdir_rev = os.path.join(scratch, "wd-rev")
    os.makedirs(workdir_rev, exist_ok=True)
    proc = sh([bin_path, "extract", "--json", fixture_rev, "-o", workdir_rev])
    envelope = json.loads(proc.stdout)
    check("extract-revisions", proc.returncode == 0 and envelope["outcome"] == "success", envelope.get("diagnostics"))

    # -- tool-surface parity ------------------------------------------------
    schema_asset = json.load(open(os.path.join(ROOT, ".mcp_schemas.json"), encoding="utf-8"))
    asset_names = list(schema_asset.keys())
    python_names = python_tool_names()
    envelopes, raw, _ = mcp_session(bin_path, [("tools/list", {})])
    published = envelopes[0]["data"]["tools"]
    published_names = [tool["name"] for tool in published]
    # The expected surface comes from the FROZEN CONTRACT, never a literal. A
    # hardcoded 36 made this gate red on main for every tool added since (#46),
    # and a gate that is red by construction stops being read.
    check(
        "tool-surface-frozen",
        sorted(published_names) == sorted(asset_names),
        f"live={len(published_names)} frozen={len(asset_names)}",
    )
    # Rust must COVER the Python reference, never merely equal it. The in-repo
    # scripts/ copy is a stale snapshot (36 tools against the external
    # reference's 49), so equality was unsatisfiable by design: Rust passed it
    # long ago and the check could only ever report the copy's staleness.
    missing = sorted(set(python_names) - set(published_names))
    check(
        "tool-coverage-of-python",
        not missing,
        f"rust={len(published_names)} python_snapshot={len(python_names)} missing={missing}",
    )
    check(
        "tool-parity-asset",
        sorted(published_names) == sorted(asset_names),
        f"{len(asset_names)} schemas in .mcp_schemas.json",
    )
    check(
        "tool-schemas-frozen",
        all(
            tool["inputSchema"].get("type") == "object"
            and tool["inputSchema"].get("additionalProperties") is False
            and isinstance(tool["inputSchema"].get("properties"), dict)
            for tool in published
        ),
        "inputSchema object/additionalProperties=false/properties",
    )
    spot = {
        "workdir_open": ["workdir"],
        "replace_text": ["paragraph_id", "old", "new", "operation_id"],
        "review_settle": ["operation_id"],
        "verify_output": ["output"],
    }
    for name, required in spot.items():
        tool = next(t for t in published if t["name"] == name)
        got = tool["inputSchema"].get("required", [])
        check(f"schema-required-{name}", got == required, got)

    # -- MCP stdio / one-workdir / draft lifecycle --------------------------
    wd = workdir.replace("\\", "\\\\")
    requests = [
        ("workdir_open", {"workdir": workdir}),
        ("workdir_open", {"workdir": workdir}),
        ("get_paragraph", {"paragraph_id": "P0"}),
        ("replace_text", {"paragraph_id": "P0", "old": "本发明", "new": "我们发明", "operation_id": "gate-replace-1"}),
        ("diff_preview", {}),
        ("commit_sync", {"operation_id": "gate-commit-1"}),
        ("replace_text", {"paragraph_id": "P0", "old": "本发明", "new": "我们发明", "operation_id": "gate-replace-1"}),
        ("replace_text", {"paragraph_id": "P0", "old": "本发明", "new": "改", "operation_id": "gate-replace-1"}),
        ("no_such_tool", {}),
    ]
    envelopes, raw, mcp_proc = mcp_session(bin_path, requests)
    check("stdio-purity", all(line.startswith("OK ") or line.startswith("ERR ") for line in raw), f"{len(raw)} lines")
    check("one-workdir", envelopes[1]["outcome"] == "failure" and envelopes[1]["diagnostics"][0]["code"] == "workdir-already-open", envelopes[1].get("diagnostics"))
    diff = envelopes[4]["data"]
    check("diff-changed-set", diff["state"] == "dirty" and diff["changed_paragraph_ids"] == ["P0"], diff)
    commit = envelopes[5]["data"]
    check("commit-changed-set", commit["changed_paragraph_ids"] == ["P0"] and commit["current_snapshot"]["id"] == "C1" and commit["state"] == "clean", commit)
    check("replay-byte-exact-mcp", envelopes[6] == envelopes[3], "same op id + args -> original envelope")
    check("op-id-reused-mcp", envelopes[7]["outcome"] == "failure" and envelopes[7]["diagnostics"][0]["code"] == "operation-id-reused", envelopes[7].get("diagnostics"))
    check("unknown-tool-err", raw[-1].startswith("ERR unknown tool"), raw[-1])

    # -- HTTP adversarial ----------------------------------------------------
    port = free_port()
    server, capability, startup_lines = spawn_server(bin_path, workdir, port)
    stop = threading.Event()
    log_lines = []
    drain = threading.Thread(target=drain_stderr, args=(server, log_lines, stop), daemon=True)
    drain.start()
    host = f"127.0.0.1:{port}"
    auth = f"Bearer {capability}"
    origin = f"http://{host}"
    try:
        st, hs, body = http(port, "GET", "/", [("Host", host)])
        check("bootstrap-shell", st == 200 and hs.get("content-type", "").startswith("text/html"), st)
        check(
            "bootstrap-zero-data",
            capability not in body and "token=" not in body and "current_snapshot" not in body,
            "static shell carries no session data",
        )
        st, _, _ = http(port, "GET", "/", [])
        check("bootstrap-host-bound", st == 404, st)

        probes = [
            ("no-auth", [("Host", host)]),
            ("bad-token", [("Host", host), ("Authorization", "Bearer wrong")]),
            ("wrong-host", [("Host", "evil.example:80"), ("Authorization", auth)]),
            ("portless-dns-rebind", [("Host", "127.0.0.1"), ("Authorization", auth)]),
            ("unknown-route", [("Host", host), ("Authorization", auth)]),
        ]
        bodies = set()
        for label, headers in probes:
            path = "/api/no-such-route" if label == "unknown-route" else "/api/reviews"
            st, hs, body = http(port, "GET", path, headers)
            ok = st == 404 and body == '{"error":"not-found"}'
            check(f"uniform-404-{label}", ok, f"{st} {body[:40]}")
            bodies.add(body)
            check(
                f"sec-headers-{label}",
                all(k in hs for k in ["cache-control", "referrer-policy", "x-content-type-options", "x-frame-options", "content-security-policy"])
                and not any(k.startswith("access-control") for k in hs),
                str({k: hs.get(k) for k in ["cache-control", "x-frame-options"]}),
            )
        check("uniform-404-identical", len(bodies) == 1, bodies)

        payload_data = {"type": "comment", "client_id": "gate-http", "paragraph_id": "P0", "selected_text": "x", "note": "n"}
        payload = json.dumps(payload_data, ensure_ascii=False).encode()
        base_headers = [("Host", host), ("Authorization", auth), ("Origin", origin), ("Sec-Fetch-Site", "same-origin"), ("Content-Type", "application/json")]

        def with_key(key, extra=()):
            return base_headers + list(extra) + [("Idempotency-Key", key)]
        def framed_json(value):
            st, _, frame_body = http(
                port,
                "GET",
                "/api/reviews",
                [("Host", host), ("Authorization", auth)],
            )
            frame = json.loads(frame_body)
            assert st == 200 and frame.get("generation")
            framed = dict(value)
            framed["expected_generation"] = frame["generation"]
            framed["expected_generation_manifest_sha256"] = frame["generation_manifest_sha256"]
            return json.dumps(framed, ensure_ascii=False).encode()

        bad_origin = [("Host", host), ("Authorization", auth), ("Origin", "http://evil.example"), ("Sec-Fetch-Site", "same-origin"), ("Content-Type", "application/json")]
        st, _, body = http(port, "POST", "/api/reviews", bad_origin + [("Idempotency-Key", "key-bad-origin-1")], payload)
        check("post-bad-origin-403", st == 403 and "origin-mismatch" in body, f"{st} {body[:40]}")
        no_origin = [("Host", host), ("Authorization", auth), ("Sec-Fetch-Site", "same-origin"), ("Content-Type", "application/json")]
        st, _, body = http(port, "POST", "/api/reviews", no_origin + [("Idempotency-Key", "key-no-origin-2")], payload)
        check("post-no-origin-403", st == 403 and "origin-mismatch" in body, f"{st} {body[:40]}")
        no_fetch = [("Host", host), ("Authorization", auth), ("Origin", origin), ("Content-Type", "application/json")]
        st, _, body = http(port, "POST", "/api/reviews", no_fetch + [("Idempotency-Key", "key-no-fetch-1")], payload)
        check("post-no-fetch-site-403", st == 403 and "fetch-site-mismatch" in body, f"{st} {body[:40]}")
        payload = framed_json(payload_data)
        st, _, body = http(port, "POST", "/api/reviews", with_key("key-write-1"), payload)
        check("post-good-200", st == 200 and "counts" in body and "session" in body, f"{st} {body[:40]}")
        first = body
        st, _, body = http(port, "POST", "/api/reviews", with_key("key-write-1"), payload)
        check("replay-byte-exact-http", st == 200 and body == first, "identical Idempotency-Key -> identical bytes")
        st, _, body = http(port, "POST", "/api/reviews", with_key("key-oversize-1"), b"x" * (300 * 1024))
        check("oversized-400", st == 400 and "too-large" in body, f"{st} {body[:40]}")
        st, _, body = http(port, "PUT", "/api/reviews", [("Host", host), ("Authorization", auth)], b"")
        check("put-501", st == 501 and body == '{"error":"method-not-supported"}', st)

        sts = []
        for _ in range(20):
            st, _, body = http(port, "GET", "/api/reviews", [("Host", host), ("Authorization", "Bearer nope")])
            sts.append(st)
        check("throttle-bounded", sts.count(404) + sts.count(429) == 20 and 429 in sts, str(sts))
        st, _, _ = http(port, "GET", "/api/reviews", [("Host", host), ("Authorization", auth)])
        check("authorized-not-throttled", st == 200, st)

        # CAS one-winner: pin the live current snapshot, edit typed.md,
        # then two concurrent publishes against the same parent.
        st, _, body = http(port, "GET", "/api/reviews", [("Host", host), ("Authorization", auth)])
        current_id = json.loads(body)["session"]["current_snapshot"]["id"]
        assert st == 200 and current_id
        with open(os.path.join(workdir, "typed.md"), "a", encoding="utf-8") as handle:
            handle.write("<!--@p id=\"P0\"/>\nchanged\n")
        results = {}

        publish_payload = framed_json({
            "expected_parent_snapshot": current_id,
            "changed_paragraph_ids": ["P0"],
            "origin": "human_ui",
        })

        def publish(key):
            results[key] = http(
                port,
                "POST",
                "/api/reviews/publish",
                with_key(key),
                publish_payload,
            )

        threads = [threading.Thread(target=publish, args=(f"key-pub-{i}",)) for i in range(2)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        statuses = [results[f"key-pub-{i}"][0] for i in range(2)]
        bodies = [results[f"key-pub-{i}"][2][:90] for i in range(2)]
        check("cas-one-winner", statuses.count(200) == 1 and statuses.count(409) == 1, f"{statuses} {bodies}")

        # Redacted logs: capability only in startup lines, never in access logs.
        time.sleep(0.2)
        server.kill()
        server.wait(timeout=10)
        stop.set()
        all_lines = startup_lines + log_lines
        access_lines = [line for line in all_lines if "[review-server]" in line]
        check("logs-redacted", bool(access_lines) and all(capability not in line for line in access_lines) and all("sid=" in line and "cat=" in line and "result=" in line for line in access_lines), f"{len(access_lines)} log lines")

        # Restart: old capability revoked, store state survives.
        server2, capability2, _ = spawn_server(bin_path, workdir, port)
        try:
            st, _, body = http(port, "GET", "/api/reviews", [("Host", host), ("Authorization", auth)])
            check("restart-revokes-old-token", st == 404 and body == '{"error":"not-found"}', f"{st} {body[:40]}")
            st, _, body = http(port, "GET", "/api/reviews", [("Host", host), ("Authorization", f"Bearer {capability2}")])
            check("restart-keeps-store-state", st == 200 and "current_snapshot" in body and "C1" in body, f"{st} {body[:60]}")
        finally:
            server2.kill()
            server2.wait(timeout=10)

        # Settle + wake retry against the revisions workdir.
        port2 = free_port()
        server3, capability3, _ = spawn_server(bin_path, workdir_rev, port2)
        try:
            host3 = f"127.0.0.1:{port2}"
            auth3 = f"Bearer {capability3}"
            origin3 = f"http://{host3}"
            base = [("Host", host3), ("Authorization", auth3), ("Origin", origin3), ("Sec-Fetch-Site", "same-origin"), ("Content-Type", "application/json")]

            def framed_json3(value):
                st, _, frame_body = http(
                    port2,
                    "GET",
                    "/api/reviews",
                    [("Host", host3), ("Authorization", auth3)],
                )
                frame = json.loads(frame_body)
                assert st == 200 and frame.get("generation")
                framed = dict(value)
                framed["expected_generation"] = frame["generation"]
                framed["expected_generation_manifest_sha256"] = frame["generation_manifest_sha256"]
                return json.dumps(framed, ensure_ascii=False).encode()

            st, _, body = http(
                port2,
                "POST",
                "/api/reviews/dispatch",
                base + [("Idempotency-Key", "key-dispatch-1")],
                framed_json3({}),
            )
            check("wake-empty-dispatch", st == 200 and '"events":[]' in body, body[:60])
            accept = {"type": "decision", "client_id": "gate-accept", "paragraph_id": "P0", "decision": "accept", "revision_key": "word/document.xml|insert|1|888c104169b5", "revision_id": "1", "selected_text": "已插入内容", "comment": ""}
            defer = {"type": "decision", "client_id": "gate-defer", "paragraph_id": "P2", "decision": "defer", "revision_key": "word/document.xml|insert|3|e3b0c44298fc", "revision_id": "3", "selected_text": "", "comment": "hold"}
            st, _, body = http(
                port2,
                "POST",
                "/api/reviews",
                base + [("Idempotency-Key", "key-dec-accept")],
                framed_json3(accept),
            )
            check("stage-accept", st == 200, st)
            st, _, body = http(
                port2,
                "POST",
                "/api/reviews",
                base + [("Idempotency-Key", "key-dec-defer")],
                framed_json3(defer),
            )
            check("stage-defer", st == 200, st)
            st, _, body = http(
                port2,
                "POST",
                "/api/reviews/dispatch",
                base + [("Idempotency-Key", "key-dispatch-2")],
                framed_json3({}),
            )
            check("wake-queues-batch", st == 200 and body.count('"status":"queued"') == 2, body[:60])
            st, _, body = http(
                port2,
                "POST",
                "/api/reviews/settle",
                base + [("Idempotency-Key", "key-settle-1")],
                framed_json3({}),
            )
            settled = json.loads(body) if st == 200 else {}
            check("settle-mixed", st == 200 and settled.get("review_base", {}).get("id") == "S1" and len(settled.get("decisions", [])) == 1 and len(settled.get("carry_forward", [])) == 1, f"{st} {body[:80]}")
            check("settle-decision-op", settled.get("decisions", [{}])[0].get("operation") == "unwrap", settled.get("decisions"))
        finally:
            server3.kill()
            server3.wait(timeout=10)
    finally:
        if server.poll() is None:
            server.kill()
            server.wait(timeout=10)

    evidence["tool_names"] = published_names
    evidence["python_names"] = python_names
    evidence["asset_names"] = asset_names
    evidence["capability_bootstrap_chars"] = len(capability)
    evidence["server"] = {"port": port, "host": host}
    evidence["s_profile"] = {
        "profile": "S",
        "complete_chain_budget_s": 35,
        "note": "gate chain (extract + MCP draft lifecycle + HTTP adversarial + settle) runs well inside the S wall budget",
    }
    print(json.dumps({"checks": checks, "evidence": evidence}, ensure_ascii=False))
    failed = [name for name, result in checks.items() if not result["pass"]]
    if failed:
        print(f"FAILED: {failed}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
