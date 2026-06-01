"""Egress client — outbound-to-internet network calls via the Gateway.

This is the in-process client that talks to the unified Wylde Gateway. It
replaces the legacy ``harness.security_gateway`` shim that pointed at the
older ``core/security-api`` egress service; the policy still lives in the  # wylde-check: dead-ref-ok
Gateway, this module is purely the client-side wire layer.

Relocated from ``Wylde/Gateway/client.py`` into ``Core/shared/`` on
2026-05-30 when the Python FastAPI Gateway server was deleted (the Rust
``wylde-gateway`` crate is the live server). This module is a *client* of
that server's egress pipe — it was the egress shim that kept ``Gateway/``
on disk after the server flip — so it lives with the other shared
in-process libraries now, not under the deleted service folder.

Used **only** for backends that actually leave the machine: remote vLLM
clusters, OpenAI-compatible APIs over the public internet, and any other
non-localhost endpoint. Calls go through the Gateway so allowlist,
auth-injection, kill-switch, and audit log are enforced uniformly.

The local Ollama daemon and local Memgraph service do **not** route through
here — they're called directly from
:mod:`Core.harness.backend.ollama_client` and
:mod:`Core.harness.memory.memgraph`. Loopback traffic isn't in scope
for the Gateway because there's no internet boundary to police on it.

Transport: ``ipc.call_action("wylde-gateway", "egress.forward", ...)`` over
the Gateway pipe. Used for unary calls (chat completion, model metadata).
The legacy streaming HTTP escape hatch (``stream`` / ``stream_lines``) was
audited out in Phase 4c and is not provided here.

Errors map to three exception classes so callers can react meaningfully:

  * :class:`GatewayBlocked` — kill switch is on; calling code should surface
    the reason rather than retry.
  * :class:`GatewayDenied` — destination or path not allowed; usually a code
    bug, log loudly.
  * :class:`GatewayError` — transport / upstream failure.
"""

from __future__ import annotations

import base64
import logging
import os
from dataclasses import dataclass
from typing import Any, Dict, Optional

logger = logging.getLogger(__name__)

_PIPE_SERVICE = "wylde-gateway"
_CALLER = os.getenv("WYLDE_SERVICE_NAME", "wylde-harness")


class GatewayError(RuntimeError):
    """Generic gateway failure — transport, upstream HTTP error, decode."""


class GatewayBlocked(GatewayError):
    """Kill switch is engaged. Outbound traffic is intentionally blocked."""


class GatewayDenied(GatewayError):
    """Destination or path is not on the allowlist. Code bug — investigate."""


@dataclass
class GatewayResponse:
    status: int
    headers: Dict[str, str]
    body: Any  # JSON-decoded when upstream sent JSON; str for text; bytes for binary
    duration_ms: float = 0.0

    @property
    def ok(self) -> bool:
        return 200 <= self.status < 300


def _classify(reply: Dict[str, Any]) -> None:
    """Translate the gateway reply envelope into the right exception."""
    if reply.get("blocked"):
        raise GatewayBlocked(reply.get("error") or "egress kill switch is engaged")
    if reply.get("denied"):
        raise GatewayDenied(reply.get("error") or "egress denied")


def forward(
    *,
    dest: str,
    method: str,
    path: str,
    body: Any = None,
    headers: Optional[Dict[str, str]] = None,
    timeout: float = 30.0,
) -> GatewayResponse:
    """Single round-trip outbound call via the Gateway over the pipe.

    Returns a :class:`GatewayResponse` even for upstream HTTP errors
    (status >= 400). Raises only on policy rejections or transport failure
    between the harness and the Gateway itself.
    """
    import ipc  # imported lazily so test imports don't pull in win32

    payload = {
        "caller": _CALLER,
        "dest": dest,
        "method": method,
        "path": path,
        "body": body,
        "headers": headers or {},
        "timeout": timeout,
    }
    try:
        # The pipe RPC has its own deadline; give the inner upstream a tighter
        # budget so we don't lose the difference to pipe transit.
        reply = ipc.call_action(
            _PIPE_SERVICE,
            "egress.forward",
            payload,
            timeout=max(timeout + 5, 10),
        )
    except ipc.IpcError as e:
        raise GatewayError(f"gateway pipe error: {e.code}: {e.message}") from e

    if not isinstance(reply, dict):
        raise GatewayError(f"gateway returned non-map reply: {type(reply).__name__}")
    _classify(reply)

    body_field = reply.get("body")
    if isinstance(body_field, dict) and "_b64" in body_field:
        try:
            body_field = base64.b64decode(body_field["_b64"])
        except Exception:
            raise GatewayError("gateway returned malformed base64 body")
    return GatewayResponse(
        status=int(reply.get("status") or 0),
        headers=dict(reply.get("headers") or {}),
        body=body_field,
        duration_ms=float(reply.get("duration_ms") or 0.0),
    )


def kill_switch_state() -> bool:
    """Return True if the Gateway's kill switch is currently engaged."""
    import ipc

    try:
        reply = ipc.call_action(_PIPE_SERVICE, "egress.kill_switch", {}, timeout=5)
    except ipc.IpcError as e:
        # Treat gateway unreachability as "blocked" so callers fail closed.
        logger.warning(
            "kill_switch_state: gateway unreachable (%s); treating as blocked", e
        )
        return True
    return bool((reply or {}).get("engaged"))


def set_kill_switch(enabled: bool) -> bool:
    """Toggle the Gateway kill switch. Returns the new state."""
    import ipc

    reply = ipc.call_action(
        _PIPE_SERVICE, "egress.kill_switch", {"enabled": bool(enabled)}, timeout=5
    )
    return bool((reply or {}).get("engaged"))


__all__ = [
    "GatewayBlocked",
    "GatewayDenied",
    "GatewayError",
    "GatewayResponse",
    "forward",
    "kill_switch_state",
    "set_kill_switch",
]
