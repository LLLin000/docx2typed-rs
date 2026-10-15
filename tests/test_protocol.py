"""Protocol-major-1 engine handshake and first read-only Result path."""
from __future__ import annotations

import json
import sys
from pathlib import Path

import anyio
from docx import Document
from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client

from scripts import main
from scripts.extract import extract
from scripts.mcp_server import engine_info, session
from scripts.protocol import capability_manifest, engine_descriptor, schema_bundle, semantic_sha256

ROOT = Path(__file__).resolve().parents[1]


def _workdir(tmp_path: Path) -> Path:
    source = tmp_path / "source.docx"
    workdir = tmp_path / "workdir"
    document = Document()
    document.add_paragraph("Protocol one")
    document.save(source)
    assert extract([str(source), "-o", str(workdir)]) == 0
    return workdir


def test_cli_version_and_validate_share_protocol_descriptor(tmp_path, capsys):
    workdir = _workdir(tmp_path)
    capsys.readouterr()

    assert main(["--version", "--json"]) == 0
    cli_descriptor = json.loads(capsys.readouterr().out)
    assert cli_descriptor == engine_info() == engine_descriptor()

    assert main(["--json", "validate", str(workdir)]) == 0
    result = json.loads(capsys.readouterr().out)
    assert result["schema"] == "docx2typed-result-1"
    assert result["operation"] == "validate"
    assert result["outcome"] == "success"
    assert result["data"]["valid"] is True
    assert result["data"]["workdir"] == {
        "kind": "absolute",
        "value": str(workdir.resolve()),
    }
    assert result["diagnostics"] == []
    assert result["evidence"] == []
    assert result["engine"] == cli_descriptor


def test_validate_json_reports_invocation_and_domain_failures(capsys, tmp_path):
    assert main(["validate", "--json"]) == 2
    invocation = json.loads(capsys.readouterr().out)
    assert invocation["outcome"] == "failure"
    assert invocation["diagnostics"][0]["code"] == "invalid-arguments"

    assert main(["validate", str(tmp_path / "missing"), "--json"]) == 1
    missing = json.loads(capsys.readouterr().out)
    assert missing["outcome"] == "failure"
    assert missing["diagnostics"][0]["code"] == "workdir-not-found"


def test_shipped_protocol_assets_bind_engine_descriptor():
    bundle = schema_bundle()
    manifest = capability_manifest()
    descriptor = engine_descriptor()
    assert descriptor["schema_bundle"] == {
        "schema": "docx2typed-tool-schema-bundle-1",
        "sha256": semantic_sha256(bundle),
    }
    assert descriptor["capability_manifest"] == {
        "schema": "docx2typed-capability-manifest-1",
        "sha256": semantic_sha256(manifest),
    }
    assert manifest == json.loads(
        (ROOT / "capabilities" / "manifest.json").read_text(encoding="utf-8")
    )


def test_mcp_stdio_negotiates_before_open_and_returns_structured_result(tmp_path):
    workdir = _workdir(tmp_path)
    session.workdir = None

    async def probe() -> None:
        parameters = StdioServerParameters(
            command=sys.executable,
            args=["-m", "scripts", "mcp"],
            cwd=str(ROOT),
        )
        async with stdio_client(parameters) as (reader, writer):
            async with ClientSession(reader, writer) as client:
                await client.initialize()
                tools = {tool.name: tool for tool in (await client.list_tools()).tools}
                assert tools["engine_info"].outputSchema is not None
                assert tools["workdir_open"].outputSchema is not None

                info = await client.call_tool("engine_info")
                assert info.isError is False
                assert info.structuredContent == engine_descriptor()

                incompatible = await client.call_tool(
                    "workdir_open",
                    {
                        "workdir": str(tmp_path / "does-not-exist"),
                        "contract_ranges": {
                            "result": {"major": 2, "min_minor": 0, "max_minor": 0}
                        },
                    },
                )
                assert incompatible.isError is True
                assert incompatible.structuredContent["diagnostics"][0]["code"] == "contract-incompatible"

                missing_feature = await client.call_tool(
                    "workdir_open",
                    {
                        "workdir": str(tmp_path / "still-does-not-exist"),
                        "required_features": ["future-feature"],
                    },
                )
                assert missing_feature.isError is True
                assert missing_feature.structuredContent["diagnostics"][0]["code"] == "required-feature-unsupported"

                opened = await client.call_tool("workdir_open", {"workdir": str(workdir)})
                assert opened.isError is False
                result = opened.structuredContent
                assert result["schema"] == "docx2typed-result-1"
                assert result["outcome"] == "success"
                opened_session = result["data"]["session"]
                assert opened_session["workdir"]["value"] == str(workdir.resolve())
                assert len(opened_session["workdir_manifest_sha256"]) == 64
                # The agent surface, not the engine's full list: this field is read by
                # an agent asking what it may call. It pinned the old two-name
                # value, which was wrong twice over -- see AGENT_TOOLS.
                assert opened_session["supported_tools"] == [
                    "document_open",
                    "document_find",
                    "document_edit",
                    "document_save",
                    "document_status",
                ]

                second = await client.call_tool("workdir_open", {"workdir": str(workdir)})
                assert second.isError is True
                assert second.structuredContent["diagnostics"][0]["code"] == "workdir-already-open"

    anyio.run(probe)
