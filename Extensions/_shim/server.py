"""MCP-over-stdio shim for legacy importlib-style Wylde extensions.

This module is invoked by the Rust ``wylde-extension-bridge`` host
when an extension's ``mcp-server.json`` points at it. It speaks the
minimum subset of the Model Context Protocol (spec version
``2025-11-25``) required by the host: ``initialize``,
``notifications/initialized``, ``tools/list``, ``tools/call``,
``ping``. Stdout is JSON-RPC; stderr is reserved for logs.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import logging
import os
import sys
import traceback
from pathlib import Path
from typing import Any, Callable, Dict, List, Optional

MCP_SPEC_VERSION = "2025-11-25"
SHIM_NAME = "wylde_mcp_py_shim"
SHIM_VERSION = "1.0.0"

logger = logging.getLogger(SHIM_NAME)


# ── tool catalog ──────────────────────────────────────────────────────


def _load_legacy_manifest(extension_root: Path) -> Dict[str, Any]:
    """Read the legacy ``manifest.json`` written by the Python bridge.

    Falls back to an empty stub if the file is missing — the shim can
    still serve ``initialize`` + ``ping`` in that case, so the host
    can diagnose the empty extension cleanly.
    """
    legacy = extension_root / "manifest.json"
    if not legacy.is_file():
        return {"name": extension_root.name, "tools": [], "handler": "handler"}
    with legacy.open("r", encoding="utf-8") as fh:
        return json.load(fh)


def _load_handler_module(extension_root: Path, handler_module_name: str) -> Any:
    """Import ``<extension_root>/<handler_module_name>.py`` via importlib."""
    handler_path = extension_root / f"{handler_module_name}.py"
    if not handler_path.is_file():
        raise FileNotFoundError(f"handler file missing: {handler_path}")
    spec = importlib.util.spec_from_file_location(
        f"wylde_shim.{extension_root.name}.{handler_module_name}",
        str(handler_path),
    )
    if spec is None or spec.loader is None:
        raise ImportError(f"failed to build module spec for {handler_path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _parameters_to_input_schema(params: List[Dict[str, Any]]) -> Dict[str, Any]:
    """Translate legacy parameter list to a minimal JSON Schema object.

    The legacy shape is::

        [{"name": "...", "type": "string"|"number"|"array"|"object"|"boolean",
          "required": true|false, "description": "...", "default": ...}]

    MCP wants a single JSON Schema object describing ``arguments``. We
    map types literally and add ``required`` for required fields.
    """
    type_map = {
        "string": "string",
        "number": "number",
        "integer": "integer",
        "array": "array",
        "object": "object",
        "boolean": "boolean",
    }
    properties: Dict[str, Any] = {}
    required: List[str] = []
    for p in params or []:
        name = p.get("name")
        if not isinstance(name, str) or not name:
            continue
        prop: Dict[str, Any] = {}
        ptype = type_map.get(str(p.get("type", "string")).lower(), "string")
        prop["type"] = ptype
        if "description" in p:
            prop["description"] = p["description"]
        if "default" in p:
            prop["default"] = p["default"]
        properties[name] = prop
        if p.get("required") is True:
            required.append(name)
    schema: Dict[str, Any] = {"type": "object", "properties": properties}
    if required:
        schema["required"] = required
    return schema


def _build_tool_catalog(manifest: Dict[str, Any]) -> List[Dict[str, Any]]:
    catalog: List[Dict[str, Any]] = []
    for t in manifest.get("tools") or []:
        tool_id = t.get("tool_id") or t.get("name")
        if not isinstance(tool_id, str) or not tool_id:
            continue
        catalog.append(
            {
                "name": tool_id,
                "description": t.get("description", ""),
                "inputSchema": _parameters_to_input_schema(t.get("parameters") or []),
                # Preserved out-of-band for endpoint dispatch.
                "_endpoint": t.get("endpoint") or tool_id,
            }
        )
    return catalog


# ── JSON-RPC dispatch ────────────────────────────────────────────────


def _ok(rid: Any, result: Dict[str, Any]) -> str:
    return json.dumps({"jsonrpc": "2.0", "id": rid, "result": result})


def _err(rid: Any, code: int, message: str, data: Any = None) -> str:
    err: Dict[str, Any] = {"code": code, "message": message}
    if data is not None:
        err["data"] = data
    return json.dumps({"jsonrpc": "2.0", "id": rid, "error": err})


class Shim:
    def __init__(self, extension_name: str, extensions_root: Path) -> None:
        self.extension_name = extension_name
        self.extension_root = extensions_root / extension_name
        if not self.extension_root.is_dir():
            raise FileNotFoundError(f"extension folder missing: {self.extension_root}")
        self.manifest = _load_legacy_manifest(self.extension_root)
        self.handler_module_name = self.manifest.get("handler") or "handler"
        self.handler = _load_handler_module(
            self.extension_root, self.handler_module_name
        )
        self.tools = _build_tool_catalog(self.manifest)
        # tool name -> endpoint (callable name on the handler module).
        self.tool_to_endpoint: Dict[str, str] = {
            t["name"]: t["_endpoint"] for t in self.tools
        }

    def handle_initialize(self, rid: Any, _params: Dict[str, Any]) -> str:
        return _ok(
            rid,
            {
                "protocolVersion": MCP_SPEC_VERSION,
                "capabilities": {"tools": {"listChanged": False}},
                "serverInfo": {
                    "name": f"{SHIM_NAME}:{self.extension_name}",
                    "version": SHIM_VERSION,
                },
            },
        )

    def handle_tools_list(self, rid: Any, _params: Dict[str, Any]) -> str:
        # Strip the internal `_endpoint` field — host shouldn't see it.
        public = [{k: v for k, v in t.items() if k != "_endpoint"} for t in self.tools]
        return _ok(rid, {"tools": public})

    def handle_tools_call(self, rid: Any, params: Dict[str, Any]) -> str:
        name = params.get("name")
        if not isinstance(name, str):
            return _err(rid, -32602, "missing string `name`")
        endpoint = self.tool_to_endpoint.get(name)
        if endpoint is None:
            return _err(rid, -32601, f"unknown tool `{name}`")
        fn: Optional[Callable[..., Any]] = getattr(self.handler, endpoint, None)
        if fn is None or not callable(fn):
            return _err(rid, -32601, f"handler missing function `{endpoint}`")
        arguments = params.get("arguments") or {}
        if not isinstance(arguments, dict):
            return _err(rid, -32602, "`arguments` must be an object")
        try:
            result = fn(arguments)
        except Exception as exc:  # noqa: BLE001 — shim translates ALL handler errors
            logger.exception("handler raised: %s", exc)
            return _err(
                rid,
                -32000,
                f"handler `{endpoint}` raised: {exc}",
                {"traceback": traceback.format_exc(limit=10)},
            )
        # MCP's tools/call envelope: {content: [{type:"text", text:"..."}], isError:bool}
        # plus a structuredContent field for machine-readable results.
        return _ok(
            rid,
            {
                "content": [{"type": "text", "text": json.dumps(result)}],
                "structuredContent": result
                if isinstance(result, dict)
                else {"value": result},
                "isError": False,
            },
        )

    def handle_ping(self, rid: Any, _params: Dict[str, Any]) -> str:
        return _ok(rid, {})


# ── stdio loop ───────────────────────────────────────────────────────


def _main_loop(shim: Shim) -> int:
    stdout = sys.stdout
    for raw in sys.stdin:
        line = raw.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError as exc:
            logger.warning("ignoring non-JSON line: %s — %s", line[:120], exc)
            continue
        method = msg.get("method")
        rid = msg.get("id")
        params = msg.get("params") or {}
        if method is None:
            # Response from us shouldn't come back to us; ignore.
            continue
        if rid is None:
            # Notification — ack by silence.
            if method == "notifications/initialized":
                pass
            continue
        try:
            if method == "initialize":
                resp = shim.handle_initialize(rid, params)
            elif method == "tools/list":
                resp = shim.handle_tools_list(rid, params)
            elif method == "tools/call":
                resp = shim.handle_tools_call(rid, params)
            elif method == "ping":
                resp = shim.handle_ping(rid, params)
            else:
                resp = _err(rid, -32601, f"method `{method}` not implemented")
        except Exception as exc:  # noqa: BLE001
            logger.exception("dispatch raised: %s", exc)
            resp = _err(rid, -32000, f"dispatch raised: {exc}")
        stdout.write(resp + "\n")
        stdout.flush()
    return 0


def _build_arg_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(prog=SHIM_NAME, description=__doc__)
    p.add_argument(
        "--extension",
        required=True,
        help="name of an extension folder under Extensions/ (sibling to _shim/)",
    )
    p.add_argument(
        "--extensions-root",
        default=None,
        help="override the extensions root directory; defaults to this file's parent",
    )
    p.add_argument(
        "--log-level",
        default=os.environ.get("WYLDE_EXT_LOG_LEVEL", "WARNING"),
        help="logging level for the shim's own log surface (stderr)",
    )
    return p


def main(argv: Optional[List[str]] = None) -> int:
    args = _build_arg_parser().parse_args(argv)
    # Canonical Wylde logging setup — `service` tag distinguishes
    # shim log lines from the wrapped extension's own logs. The host's
    # tracing surface captures the shim's stderr.
    from Core.shared.logging_setup import configure_logging

    configure_logging(
        level=getattr(logging, args.log_level.upper(), logging.WARNING),
        service=f"{SHIM_NAME}:{args.extension}",
        force=True,
    )
    extensions_root = (
        Path(args.extensions_root).resolve()
        if args.extensions_root
        else Path(__file__).resolve().parent.parent
    )
    shim = Shim(args.extension, extensions_root)
    return _main_loop(shim)


if __name__ == "__main__":
    raise SystemExit(main())
