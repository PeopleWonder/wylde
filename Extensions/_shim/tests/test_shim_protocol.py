"""Unit tests for the wylde_mcp_py_shim MCP-over-stdio bridge.

Tests cover:
  * initialize / protocolVersion handshake
  * tools/list returns the legacy manifest's tool catalog
  * tools/call dispatches to the legacy handler's endpoint function
  * tools/call surfaces handler exceptions as JSON-RPC errors
  * ping returns an empty result
  * unknown methods return -32601
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any, Dict

import pytest

from Extensions._shim.server import MCP_SPEC_VERSION, Shim


def _build_synthetic_extension(root: Path, name: str, *, raises: bool = False) -> None:
    """Plant a tiny extension on disk with one tool that echoes its args."""
    ext = root / name
    ext.mkdir(parents=True, exist_ok=True)
    (ext / "manifest.json").write_text(
        json.dumps(
            {
                "name": name,
                "description": "synthetic test extension",
                "version": "1.0",
                "enabled": True,
                "transport": "http",
                "handler": "handler",
                "capabilities": [],
                "tools": [
                    {
                        "tool_id": "echo",
                        "description": "Echoes back the args.",
                        "endpoint": "do_echo",
                        "parameters": [
                            {"name": "msg", "type": "string", "required": True},
                            {
                                "name": "n",
                                "type": "number",
                                "required": False,
                                "default": 1,
                            },
                        ],
                        "tags": ["test"],
                    }
                ],
            }
        ),
        encoding="utf-8",
    )
    handler_body = (
        "def do_echo(params):\n"
        "    if " + ("True" if raises else "False") + ":\n"
        "        raise RuntimeError('boom')\n"
        "    return {'echo': params}\n"
    )
    (ext / "handler.py").write_text(handler_body, encoding="utf-8")


@pytest.fixture()
def shim_factory(tmp_path: Path):
    def _make(*, raises: bool = False, name: str = "synth") -> Shim:
        _build_synthetic_extension(tmp_path, name, raises=raises)
        return Shim(name, tmp_path)

    return _make


def _parse(line: str) -> Dict[str, Any]:
    return json.loads(line)


def test_initialize_returns_current_spec_version(shim_factory) -> None:
    shim = shim_factory()
    resp = _parse(shim.handle_initialize(1, {}))
    assert resp["jsonrpc"] == "2.0"
    assert resp["id"] == 1
    assert resp["result"]["protocolVersion"] == MCP_SPEC_VERSION
    assert "tools" in resp["result"]["capabilities"]
    assert "synth" in resp["result"]["serverInfo"]["name"]


def test_tools_list_reflects_manifest(shim_factory) -> None:
    shim = shim_factory()
    resp = _parse(shim.handle_tools_list(2, {}))
    tools = resp["result"]["tools"]
    assert len(tools) == 1
    t = tools[0]
    assert t["name"] == "echo"
    # No leaked internals.
    assert "_endpoint" not in t
    schema = t["inputSchema"]
    assert schema["type"] == "object"
    assert "msg" in schema["properties"]
    assert "msg" in schema["required"]
    assert "n" not in schema["required"]


def test_tools_call_dispatches_to_handler(shim_factory) -> None:
    shim = shim_factory()
    resp = _parse(
        shim.handle_tools_call(3, {"name": "echo", "arguments": {"msg": "hi", "n": 2}})
    )
    assert resp["result"]["isError"] is False
    body = resp["result"]["structuredContent"]
    assert body["echo"] == {"msg": "hi", "n": 2}


def test_tools_call_unknown_tool_returns_method_not_found(shim_factory) -> None:
    shim = shim_factory()
    resp = _parse(shim.handle_tools_call(4, {"name": "missing", "arguments": {}}))
    assert resp["error"]["code"] == -32601
    assert "missing" in resp["error"]["message"]


def test_tools_call_handler_exception_surfaces(shim_factory) -> None:
    shim = shim_factory(raises=True)
    resp = _parse(
        shim.handle_tools_call(5, {"name": "echo", "arguments": {"msg": "x"}})
    )
    assert resp["error"]["code"] == -32000
    assert "boom" in resp["error"]["message"]
    assert "traceback" in resp["error"]["data"]


def test_ping_returns_empty_result(shim_factory) -> None:
    shim = shim_factory()
    resp = _parse(shim.handle_ping(6, {}))
    assert resp["result"] == {}
