r"""Pipe-backed shim that mirrors `ollama_client`'s public surface.

When ``WYLDE_HARNESS_OLLAMA_TRANSPORT=pipe`` (the default once the Rust
``wylde-ollama`` service is in production), every Ollama call the
harness makes routes through the pipe at ``\\.\pipe\wylde-ollama``
instead of direct HTTP to ``127.0.0.1:11434``. Public function
signatures and return shapes match ``ollama_client`` exactly so callers
don't change.

Streaming functions (:func:`stream_chat`, :func:`pull_model`) are NOT
ported in this shim — they still call the underlying HTTP implementation
because the Python ``Core.shared.ipc`` package does not yet expose a
streaming-IPC client primitive (master plan Q-S6). The day the Python
streaming client lands, those two functions move here too. Until then
the gate covers the eight unary functions only.

Error shape:

* ``IpcError`` codes from the service (``ollama_unreachable``,
  ``ollama_http``, ``model_not_found``, ``invalid_request``,
  ``vram_admission_denied``) are mapped back to the Python types the
  callers already catch (``OllamaUnreachable``, ``OllamaHTTPError``,
  generic empty-result defaults for the swallow-on-error functions).
* Pipe-transport failures (``pipe_unavailable``, ``pipe_connect``,
  etc.) are surfaced as ``OllamaUnreachable`` so the existing
  swallow-on-error functions behave the same way they do today.
"""

from __future__ import annotations

import logging
from typing import Any, Dict, List, Optional, cast

from Core.shared import ipc

logger = logging.getLogger(__name__)

# The pipe service name. The IPC layer prepends `\\.\pipe\wylde-` —
# both `wylde-ollama` and `ollama` resolve to the same pipe path.
_SERVICE = "wylde-ollama"

# Pipe-transport error codes that mean "service unreachable, treat as if
# Ollama itself is down". Distinct from a successful pipe round-trip
# that returned `ollama_unreachable`.
_TRANSPORT_ERROR_CODES = frozenset(
    {
        "pipe_unavailable",
        "pipe_connect",
        "pipe_timeout",
        "pipe_io",
        "handshake_timeout",
        "handshake_io",
        "handshake_rejected",
        "encode",
        "decode",
        "ipc_disabled",
        "no_http_backend",
    }
)


class PipeTransportError(RuntimeError):
    """Pipe couldn't be reached at all — caller's wrapper should fall
    through to direct HTTP. Distinct from a service-level error (which
    means the pipe round-tripped but the service returned not-ok)."""


def _is_transport_failure(code: str) -> bool:
    return code in _TRANSPORT_ERROR_CODES


def _raise_if_transport(reply: "ipc.Reply") -> None:
    """If the reply error is a pipe-transport failure, raise
    :class:`PipeTransportError` so the wrapping caller's try/except can
    fall through to the legacy HTTP path. Service-level errors (ok=False
    from the running service) are NOT raised — the caller handles them
    via the existing swallow-on-error semantics."""
    if reply.ok:
        return
    code = (reply.error or {}).get("code", "")
    if _is_transport_failure(code):
        raise PipeTransportError(
            f"wylde-ollama pipe unreachable ({code}): "
            f"{(reply.error or {}).get('message', '')}"
        )


# ─── Liveness ───────────────────────────────────────────────────────────


def check_health(timeout: float = 3.0) -> bool:
    """Return True iff the Rust wylde-ollama service answered ``ollama.health``
    with ok and the upstream daemon is reachable.

    Raises :class:`PipeTransportError` if the pipe itself is unreachable
    so the caller's HTTP fallback engages."""
    reply = ipc.send_action(_SERVICE, "ollama.health", {}, timeout=timeout)
    _raise_if_transport(reply)
    if reply.ok:
        return bool(reply.data.get("ok"))
    return False


# ─── Model-management endpoints ─────────────────────────────────────────


def list_models() -> List[str]:
    reply = ipc.send_action(_SERVICE, "ollama.list_models", {})
    _raise_if_transport(reply)
    if not reply.ok:
        return []
    models = reply.data.get("models") or []
    return [m.get("name", "") for m in models if m.get("name")]


def list_models_detailed() -> List[Dict[str, Any]]:
    reply = ipc.send_action(_SERVICE, "ollama.list_models", {})
    _raise_if_transport(reply)
    if not reply.ok:
        return []
    return list(reply.data.get("models") or [])


def list_loaded_models() -> List[str]:
    reply = ipc.send_action(_SERVICE, "ollama.list_loaded", {})
    _raise_if_transport(reply)
    if not reply.ok:
        return []
    models = reply.data.get("models") or []
    return [m.get("name", "") for m in models if m.get("name")]


def list_loaded_models_detailed() -> List[Dict[str, Any]]:
    reply = ipc.send_action(_SERVICE, "ollama.list_loaded", {})
    _raise_if_transport(reply)
    if not reply.ok:
        return []
    return list(reply.data.get("models") or [])


def show_model(name: str) -> Optional[Dict[str, Any]]:
    if not name:
        return None
    reply = ipc.send_action(_SERVICE, "ollama.show", {"model": name})
    _raise_if_transport(reply)
    if not reply.ok:
        return None
    return cast(Dict[str, Any], reply.data)


def delete_model(name: str) -> bool:
    if not name:
        return False
    reply = ipc.send_action(_SERVICE, "ollama.delete", {"name": name})
    _raise_if_transport(reply)
    return bool(reply.ok)


def unload_model(name: str) -> bool:
    if not name:
        return False
    reply = ipc.send_action(_SERVICE, "ollama.eject", {"model": name})
    _raise_if_transport(reply)
    return bool(reply.ok)


# ─── Embed ──────────────────────────────────────────────────────────────


def embed_via_pipe(
    model: str,
    input_texts: List[str],
    *,
    timeout: float = 30.0,
) -> Dict[str, Any]:
    """Call ``ollama.embed`` and return the raw envelope (or raise).

    Raises:
        OllamaUnreachable  — pipe transport failure, or service-reported
                             ollama_unreachable / broker_unreachable.
        OllamaHTTPError    — service-reported ollama_http (with status).
        RuntimeError       — model_not_found or anything else.
    """
    # Local import to dodge the circular dep (embeddings.py also imports
    # OLLAMA_URL from ollama_client).
    from .ollama_client import OllamaHTTPError, OllamaUnreachable

    reply = ipc.send_action(
        _SERVICE,
        "ollama.embed",
        {"model": model, "input": input_texts},
        timeout=timeout,
    )
    if reply.ok:
        return cast(Dict[str, Any], reply.data)
    err = reply.error or {}
    code = err.get("code", "unknown")
    msg = err.get("message", "")
    if _is_transport_failure(code):
        raise OllamaUnreachable(f"wylde-ollama pipe: {msg}")
    if code == "ollama_unreachable" or code == "broker_unreachable":
        raise OllamaUnreachable(msg)
    if code == "ollama_http":
        details = err.get("details") or {}
        status = int(details.get("status", 0))
        raise OllamaHTTPError(msg, status=status)
    if code == "model_not_found":
        raise RuntimeError(f"404 {msg}")
    raise RuntimeError(f"{code}: {msg}")


# ─── Chat (non-streaming) ───────────────────────────────────────────────


def chat_via_pipe(
    body: Dict[str, Any],
    *,
    timeout: float = 120.0,
) -> Dict[str, Any]:
    """Call ``ollama.chat`` (non-streaming) and return the raw upstream envelope.

    Maps service errors to the same exception shapes the existing
    ``backend_routing._call_ollama`` raises so callers (``InferenceRouter``)
    can catch them uniformly.
    """
    from .ollama_client import OllamaHTTPError, OllamaUnreachable

    reply = ipc.send_action(_SERVICE, "ollama.chat", body, timeout=timeout)
    if reply.ok:
        return cast(Dict[str, Any], reply.data)
    err = reply.error or {}
    code = err.get("code", "unknown")
    msg = err.get("message", "")
    if _is_transport_failure(code):
        raise OllamaUnreachable(f"wylde-ollama pipe: {msg}")
    if code in ("ollama_unreachable", "broker_unreachable"):
        raise OllamaUnreachable(msg)
    if code == "ollama_http":
        details = err.get("details") or {}
        status = int(details.get("status", 0))
        raise OllamaHTTPError(msg, status=status)
    if code == "vram_admission_denied":
        raise OllamaHTTPError(f"vram broker denied admission: {msg}", status=503)
    raise RuntimeError(f"{code}: {msg}")


__all__ = [
    "check_health",
    "list_models",
    "list_models_detailed",
    "list_loaded_models",
    "list_loaded_models_detailed",
    "show_model",
    "delete_model",
    "unload_model",
    "embed_via_pipe",
    "chat_via_pipe",
]
