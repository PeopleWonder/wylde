"""Extension-ingress dispatch contract — shared in-process helper.

Relocated from ``Gateway/extension_routes.py`` on 2026-05-30 when the
Python FastAPI Gateway server was deleted (the Rust ``wylde-gateway``
crate is the live server). The HTTP route group these helpers describe
is served by that Rust crate now; what survives here is the
language-agnostic *contract* — a pure route-table enumerator and a
synchronous dispatch shim — usable by any in-process Python caller and
exercised by the extension-bridge smoke test.

What the Gateway exposes
========================
A route group at::

    POST /extensions/<extension_name>/<endpoint>
    Content-Type: application/json
    Body: <handler params>

with the following behaviour:

1. **Existence check.** ``<extension_name>`` must be a folder under
   ``Wylde/Extensions/`` containing a ``manifest.json``. If not,
   return 404.
2. **Enabled check.** The extension's manifest ``enabled`` flag must
   be true. If false, return 409 Conflict with body
   ``{"error": "extension disabled"}`` so the browser side surfaces
   a "click to enable" affordance.
3. **Capability check.** The route group is only mounted when the
   extension declares ``ingress.http`` in its ``capabilities`` list
   (or, for browser extensions, ``ingress.browser``). Other extensions
   are pure egress and don't accept inbound requests.
4. **Dispatch.** Forward to
   :func:`Wylde.Extensions.extension_bridge.dispatch_external`,
   which loads the handler module and calls the appropriate function.
5. **Response.** JSON-encode the dict the handler returns and return
   200 (or 4xx if the handler set ``status="error"`` and the error
   code maps to a known HTTP status — keep that mapping
   conservative).
6. **Auth.** Localhost / WyldeLink-local only, per Wylde Design
   Principle #16 (single auth boundary at the VPN tunnel).

What lives here
---------------
* :func:`build_route_table` — pure function that returns the routes
  the Gateway *should* mount, with no web-framework dependency. Easy to
  test in isolation; used by the smoke test to confirm the bridge ↔
  Gateway contract is consistent.
* :func:`handle_extension_request` — synchronous dispatch shim callers
  invoke as ``handle_extension_request(ext_name, endpoint, params)``.

Dispatch transport
------------------
:func:`handle_extension_request` does **not** load the bridge
in-process. It dispatches through ``\\\\.\\pipe\\wylde-extension-bridge``
— the pipe service that wraps the in-process dispatcher (see
:mod:`Extensions.extension_bridge.pipe`). Routing every call through
the pipe means the Python and Rust Gateway paths produce byte-identical
envelopes from the same upstream. The bridge service's typed error codes
(``extension_not_found`` / ``extension_disabled`` / ``extension_error``)
map back onto the HTTP status codes below; a pipe-transport failure
maps to 503 so the caller sees ``extension_bridge_unavailable``.

:func:`build_route_table` still reads the bridge in-process — it is a
diagnostic that enumerates on-disk extensions, not a request path.
"""

from __future__ import annotations

import logging
from typing import Any, Dict, List, Optional, Tuple

logger = logging.getLogger("wylde.gateway.extension_routes")

# Pipe service hosting the extension dispatcher externally. See
# Extensions/extension_bridge/pipe.py.
BRIDGE_SERVICE = "wylde-extension-bridge"


def _bridge() -> Any:
    """Late import — the bridge isn't always present (e.g. during
    initial Gateway boot before extensions are scanned)."""
    try:
        from Wylde.Extensions import extension_bridge

        return extension_bridge
    except Exception as exc:
        logger.warning("extension bridge not importable: %s", exc)
        return None


def build_route_table() -> List[Dict[str, Any]]:
    """Return the list of routes the Gateway should mount.

    Each entry is a dict::

        {
          "path":      "/extensions/<ext>/<endpoint>",
          "method":    "POST",
          "extension": "<ext>",
          "endpoint":  "<endpoint>",
          "tool_id":   "<tool_id>",
          "enabled":   true / false,
          "capabilities": ["ingress.http", ...],
        }

    Includes routes for *all* extensions, enabled or not; the
    ``enabled`` field tells the Gateway which to actually mount and
    which to render as 409 stubs. Iterating both keeps the route
    table observable from a single call.
    """
    bridge = _bridge()
    if bridge is None:
        return []
    routes: List[Dict[str, Any]] = []
    for ext in bridge.list_extensions().values():
        for tool in ext.tools:
            endpoint = tool.endpoint or tool.tool_id
            routes.append(
                {
                    "path": f"/extensions/{ext.name}/{endpoint}",
                    "method": "POST",
                    "extension": ext.name,
                    "endpoint": endpoint,
                    "tool_id": tool.tool_id,
                    "enabled": ext.enabled,
                    "capabilities": [c.value for c in ext.capabilities],
                }
            )
    return routes


def _status_for_bridge_error(code: str) -> int:
    """Map a bridge / transport error code onto an HTTP status.

    The three structured codes the bridge pipe raises get a specific
    status; everything else (pipe down, service not registered, decode
    failure) is a transport fault → 503, which the Gateway's extension
    dispatch then surfaces as ``extension_bridge_unavailable``."""
    if code == "extension_not_found":
        return 404
    if code == "extension_disabled":
        return 409
    if code == "extension_error":
        return 500
    return 503


def handle_extension_request(
    extension_name: str,
    endpoint: str,
    params: Optional[Dict[str, Any]] = None,
) -> Tuple[int, Dict[str, Any]]:
    """Dispatch an extension call through the extension-bridge pipe.

    Sends ``extensions.dispatch`` to ``\\.\\pipe\\wylde-extension-bridge``
    rather than loading the bridge in-process, so the Python and Rust
    Gateway paths produce identical envelopes from the same upstream.

    Returns ``(status_code, json_body)``. Status codes:

    * 200 — handler returned successfully
    * 404 — extension not found / no such endpoint
    * 409 — extension disabled
    * 500 — handler raised inside the extension
    * 503 — the bridge pipe was unreachable
    """
    try:
        from Core.shared import ipc
    except ImportError as exc:
        logger.warning("extension bridge pipe unreachable: %s", exc)
        return 503, {"error": f"ipc transport unavailable: {exc}"}

    reply = ipc.send_action(
        BRIDGE_SERVICE,
        "extensions.dispatch",
        {
            "extension": extension_name,
            "endpoint": endpoint,
            "params": params or {},
        },
    )
    if reply.ok:
        body: Dict[str, Any] = (
            reply.data if isinstance(reply.data, dict) else {"result": reply.data}
        )
        return 200, body
    err = reply.error or {}
    code = str(err.get("code") or "unknown")
    message = str(err.get("message") or "extension call failed")
    return _status_for_bridge_error(code), {"error": message}


__all__ = ["build_route_table", "handle_extension_request"]
