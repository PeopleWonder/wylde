"""Action-based dispatch — pipe-only handlers bypassing Flask routing.

Some surfaces (like the orchestrator harness) deliberately have no
Flask/HTTP routes — they are pipe-only. Callers send envelopes whose
``method`` is the literal string ``/__action__`` with
``data = {"action": "harness.health", "payload": ...}``. The pipe server
resolves the action name through this registry and invokes the handler
directly, bypassing the Flask test client entirely. Used to keep policy
code (egress gateway, model-state cache) unreachable over HTTP even in
mixed mode.

The module also tracks each registered action's docstring + handler
module path in ``_REGISTERED_ACTIONS`` so the pipe server can emit a
JSON contract artifact (``data/contracts/actions/<service>.json``) at
startup. That contract is the cross-language source of truth for "what
actions does <service> expose" — wylde_check rules read it instead of
grepping Python source so Rust services (when they land) participate
in the same checks without special-casing.
"""

from __future__ import annotations

import logging
import threading
from typing import Any, Callable, Dict, List

from ._wire import IpcError

logger = logging.getLogger(__name__)

_ACTION_DISPATCH_PATH = "/__action__"
_action_handlers: Dict[str, Callable[[Any], Any]] = {}
_action_handlers_lock = threading.Lock()

# Parallel registry capturing per-action metadata for the contract writer.
# Keyed by action name; values are ``{"doc": <first-line>, "handler_module": <__module__>}``.
# Kept in sync with ``_action_handlers`` under the same lock.
_REGISTERED_ACTIONS: Dict[str, Dict[str, str]] = {}


def _handler_doc_first_line(handler: Callable[[Any], Any]) -> str:
    """Return the first non-empty line of ``handler.__doc__`` or ''."""
    doc = getattr(handler, "__doc__", None) or ""
    for line in doc.splitlines():
        stripped = line.strip()
        if stripped:
            return stripped
    return ""


def register_action(name: str, handler: Callable[[Any], Any]) -> None:
    """Bind `handler` to action `name` for pipe dispatch.

    Handlers receive the request payload (the value of `data.payload` in
    the envelope) and return any msgpack-serialisable value, which is sent
    back as `data` in the success reply. Raise to send a structured error.
    Re-registering an action replaces the previous handler.
    """
    if not isinstance(name, str) or not name:
        raise ValueError("action name must be a non-empty string")
    if not callable(handler):
        raise TypeError("action handler must be callable")
    with _action_handlers_lock:
        _action_handlers[name] = handler
        _REGISTERED_ACTIONS[name] = {
            "doc": _handler_doc_first_line(handler),
            "handler_module": getattr(handler, "__module__", "") or "",
        }


def unregister_action(name: str) -> None:
    """Remove an action binding. Idempotent."""
    with _action_handlers_lock:
        _action_handlers.pop(name, None)
        _REGISTERED_ACTIONS.pop(name, None)


def list_actions() -> List[str]:
    """Snapshot of registered action names — for diagnostics / introspection."""
    with _action_handlers_lock:
        return sorted(_action_handlers.keys())


def _dispatch_action(payload: Any) -> Dict[str, Any]:
    """Resolve `payload['action']` and invoke its handler.

    Return value is the full reply envelope so the pipe server can send it
    verbatim. Errors are converted to the standard {ok:False, error:...}
    shape — handlers don't need to care about wire format.
    """
    if not isinstance(payload, dict):
        return {
            "ok": False,
            "error": {"code": "bad_request", "message": "action payload must be a map"},
        }
    name = payload.get("action")
    if not isinstance(name, str) or not name:
        return {
            "ok": False,
            "error": {"code": "bad_request", "message": "missing 'action' field"},
        }
    with _action_handlers_lock:
        handler = _action_handlers.get(name)
    if handler is None:
        return {
            "ok": False,
            "error": {"code": "no_action", "message": f"unknown action {name!r}"},
        }
    try:
        result = handler(payload.get("payload"))
    except IpcError as e:
        # Structured handler-side errors propagate code/message/details
        # verbatim. Matches the Rust dispatcher's `Reply::err(IpcError)`
        # path so cross-impl callers see identical error shapes.
        err: Dict[str, Any] = {"code": e.code, "message": e.message}
        if e.details:
            err["details"] = e.details
        return {"ok": False, "error": err}
    except Exception as e:  # noqa: BLE001
        logger.exception("ipc: action %s raised", name)
        return {
            "ok": False,
            "error": {
                "code": "handler",
                "message": f"{type(e).__name__}: {e}",
                "details": {"action": name},
            },
        }
    return {"ok": True, "data": result}
