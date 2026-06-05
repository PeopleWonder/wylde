r"""extension_bridge pipe — ``\\.\pipe\wylde-extension-bridge``.

The bridge proper (loader / registry / dispatcher) is plain in-process
code that Wylde-Core and the Python Gateway import directly. This pipe
is the *external* surface: it lets callers with no in-process Python —
chiefly the Rust Gateway port — reach the bridge's dispatch entry point
over the named-pipe transport every other Wylde service already speaks.

Action surface (mirrors the ``actions`` list in ``manifest.json``):

* ``extensions.dispatch`` — route an inbound HTTP-style call to an
  extension handler. Payload ``{extension, endpoint, params?}``;
  delegates straight to :func:`extension_bridge.dispatch_external`.

The handler is a thin wrapper around the unchanged in-process
dispatcher, so HTTP ingress (via ``Core/shared/extension_routes.py``) and
pipe ingress produce identical envelopes. The typed bridge errors map
onto structured :class:`~Core.shared.ipc.IpcError` codes the dispatcher
serialises into the wire envelope:

* :class:`~extension_bridge.dispatcher.ExtensionNotFound`
  → ``extension_not_found``
* :class:`~extension_bridge.dispatcher.ExtensionNotEnabled`
  → ``extension_disabled``
* :class:`~extension_bridge.dispatcher.DispatchError`
  → ``extension_error``
"""

from __future__ import annotations

import logging
import threading
from types import ModuleType
from typing import Any, Callable, Dict, Optional

from Core.shared.ipc import IpcError

from .dispatcher import (
    DispatchError,
    ExtensionNotEnabled,
    ExtensionNotFound,
    dispatch_external,
)

ActionHandler = Callable[[Any], Dict[str, Any]]

logger = logging.getLogger("wylde.extensions.bridge.pipe")

SERVICE_NAME = "wylde-extension-bridge"

_started = False
_started_lock = threading.Lock()


# ── Helpers ────────────────────────────────────────────────────────────


class _ActionError(Exception):
    """Structured error surfaced through the pipe envelope."""

    def __init__(self, code: str, message: str) -> None:
        super().__init__(message)
        self.code = code
        self.message = message


def _payload_dict(payload: Any) -> Dict[str, Any]:
    if payload is None:
        return {}
    if not isinstance(payload, dict):
        raise _ActionError("bad_request", "payload must be a map")
    return payload


# ── Action handlers ────────────────────────────────────────────────────


def _extensions_dispatch_action(payload: Any) -> Dict[str, Any]:
    """Route an external call to an extension handler.

    Payload: ``{extension, endpoint, params?}``. Returns whatever the
    extension handler returned (a JSON-serialisable dict). Typed bridge
    failures raise :class:`_ActionError` with the structured code the
    HTTP side maps to a status."""
    p = _payload_dict(payload)
    extension = p.get("extension")
    endpoint = p.get("endpoint")
    if not isinstance(extension, str) or not extension:
        raise _ActionError("bad_request", "extension is required")
    if not isinstance(endpoint, str) or not endpoint:
        raise _ActionError("bad_request", "endpoint is required")
    params = p.get("params") or {}
    if not isinstance(params, dict):
        raise _ActionError("bad_request", "params must be a map")
    try:
        return dispatch_external(extension, endpoint, params)
    except ExtensionNotFound as exc:
        raise _ActionError("extension_not_found", str(exc))
    except ExtensionNotEnabled as exc:
        raise _ActionError("extension_disabled", str(exc))
    except DispatchError as exc:
        raise _ActionError("extension_error", str(exc))


# ── Wiring ─────────────────────────────────────────────────────────────


_ACTIONS: Dict[str, ActionHandler] = {
    "extensions.dispatch": _extensions_dispatch_action,
}


def _wrap_handler(handler: ActionHandler) -> ActionHandler:
    """Convert ``_ActionError`` into the structured :class:`IpcError`
    the dispatcher serialises — the same code/message shape the Rust
    port emits, so cross-impl callers see identical error envelopes."""

    def _wrapped(payload: Any) -> Dict[str, Any]:
        try:
            return handler(payload)
        except _ActionError as exc:
            raise IpcError(exc.code, exc.message)

    _wrapped.__name__ = getattr(handler, "__name__", "wrapped")
    return _wrapped


def _ipc_module() -> Optional[ModuleType]:
    try:
        from Core.shared import ipc

        return ipc
    except ImportError as exc:
        logger.warning(
            "extension_bridge pipe: ipc not importable (%s) — pipe disabled", exc
        )
        return None


def register_actions() -> Optional[ModuleType]:
    """Bind every ``extensions.*`` action onto the ipc registry.

    Returns the ``ipc`` module on success so the caller can reach
    :func:`serve_forever_background` without a second import, or
    ``None`` when the transport is unavailable."""
    ipc = _ipc_module()
    if ipc is None:
        return None
    for name, handler in _ACTIONS.items():
        ipc.register_action(name, _wrap_handler(handler))
    logger.info(
        "extension_bridge pipe: registered %d extensions.* action(s)", len(_ACTIONS)
    )
    return ipc


def start() -> bool:
    """Start the extension-bridge pipe in a daemon thread.

    Pipe-only: every request the bridge serves is either the in-band
    ``__ping__`` health probe or the ``extensions.dispatch`` action,
    both of which the shared PipeServer answers from the registered
    action table — neither path touches a Flask app. So we hand
    ``serve_forever_background`` no app at all; it stands up the pipe on
    the strength of the registered actions alone. (The previous code
    built a throwaway Flask app purely to satisfy a now-relaxed
    ``app is None`` guard; when Flask happened to be absent that app was
    ``None`` and the guard silently skipped binding the pipe, leaving a
    live process with no pipe — the "extension-bridge offline" zombie.)

    Idempotent. Returns True if the pipe is now serving (or was
    already), False if dependencies are missing (msgpack/pywin32
    absent, non-Windows host)."""
    global _started
    with _started_lock:
        if _started:
            return True
        ipc = register_actions()
        if ipc is None:
            return False
        try:
            ipc.serve_forever_background(SERVICE_NAME)
        except Exception as exc:  # noqa: BLE001
            logger.warning(
                "extension_bridge pipe: serve_forever_background failed (%s)", exc
            )
            return False
        _started = True
        logger.info("extension_bridge pipe: serving \\\\.\\pipe\\%s", SERVICE_NAME)
        return True


def stop() -> None:
    """Reserved for future graceful shutdown — the shared PipeServer
    doesn't expose a stop hook today; the pipe drains on process exit."""
    return None


__all__ = ["SERVICE_NAME", "register_actions", "start", "stop"]
