"""chat.* action handlers — thin forwarders to the Rust harness pipe.

Phase 5.D retirement (2026-06-03)
---------------------------------

The in-process Python chat-turn driver (``Core/harness/turn/``) is gone.
Rust is the sole chat-turn implementation. Every unary chat.* verb this
Python pipe still exposes — ``chat.run_turn``, ``chat.start_turn``,
``chat.cancel`` — forwards to the Rust ``wylde-harness`` pipe at
``\\\\.\\pipe\\wylde-harness`` and surfaces the reply verbatim. There is
no longer a Python fall-back: if the Rust pipe is unreachable the verb
raises ``harness_unavailable`` rather than silently driving a (deleted)
in-process loop.

The streaming surface (``chat.stream_turn`` / ``chat.stream_tools``) is
NOT served by this Python pipe — those are long-poll cursor actions the
Rust ``wylde-harness`` binary serves natively; there is no Python
handler here to forward. See ``rust/crates/wylde-harness/src/turn/
actions.rs``.

History: slice 5.D (2026-05-25) flipped the default from the Python
driver to the Rust forward after byte-level parity coverage landed on
the salvage parser, ``_call_hash``, and ``_find_balanced_braces``
(``rust/tests/parity/tests/harness_turn.rs``, 25 cases). The 5.D
prerequisite slice extended the forward from ``chat.run_turn`` alone to
all three unary verbs. This slice deletes the Python driver and the
``WYLDE_HARNESS_IMPL`` strangler knob it gated — the rollback path it
guarded no longer exists.
"""

from __future__ import annotations

from typing import Any, Dict, Optional

from ._common import (
    _ActionError,
    _payload_dict,
)

# Quick-verb forward timeout (start_turn / cancel return immediately;
# run_turn carries its own longer per-call timeout).
_FORWARD_TIMEOUT = 30.0

# Reply error codes that mean "the Rust pipe didn't actually serve this"
# (binary down, daemon mis-spawn, verb not registered yet). With the
# Python driver retired there is no fall-back to run, so these surface as
# ``harness_unavailable`` to the caller. Any OTHER ``ok=false`` code is a
# genuine service-level error and is re-raised verbatim so the envelope
# shape matches whatever Rust reported.
_TRANSPORT_FALLBACK_CODES = {
    "not_found",  # _resolve() couldn't find the service instance
    "pipe_unavailable",
    "pipe_connect",
    "pipe_timeout",
    "pipe_io",
    "handshake_timeout",
    "handshake_io",
    "handshake_rejected",
    "no_action",
    "not_implemented",
}


# ── chat.* action handlers ─────────────────────────────────────────────


def _start_turn_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    user_message = p.get("user_message")
    conversation_id = p.get("conversation_id")
    if not isinstance(user_message, str) or not user_message:
        raise _ActionError("bad_request", "user_message is required")
    if not isinstance(conversation_id, str) or not conversation_id:
        raise _ActionError("bad_request", "conversation_id is required")

    forwarded = _try_forward_start_turn_to_rust(p)
    if forwarded is None:
        raise _ActionError(
            "harness_unavailable",
            "wylde-harness pipe is unreachable (no Python chat-turn fallback)",
        )
    return forwarded


def _run_turn_action(payload: Any) -> Dict[str, Any]:
    """Blocking variant — drives the turn to completion server-side and
    returns the final result. For non-streaming consumers (tests, future
    MCP, mobile single-shot).

    Forwards to the Rust ``wylde-harness`` pipe and surfaces its reply
    verbatim. If the pipe is unreachable the verb raises
    ``harness_unavailable`` — the Python driver that used to serve this
    in-process was retired in Phase 5.D.
    """
    p = _payload_dict(payload)
    user_message = p.get("user_message")
    conversation_id = p.get("conversation_id")
    if not isinstance(user_message, str) or not user_message:
        raise _ActionError("bad_request", "user_message is required")
    if not isinstance(conversation_id, str) or not conversation_id:
        raise _ActionError("bad_request", "conversation_id is required")
    timeout = float(p.get("timeout") or 300.0)

    forwarded = _try_forward_run_turn_to_rust(p, timeout)
    if forwarded is None:
        raise _ActionError(
            "harness_unavailable",
            "wylde-harness pipe is unreachable (no Python chat-turn fallback)",
        )
    return forwarded


def _cancel_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    turn_id = p.get("turn_id")
    if not isinstance(turn_id, str) or not turn_id:
        raise _ActionError("bad_request", "turn_id is required")

    forwarded = _try_forward_cancel_to_rust(p)
    if forwarded is None:
        raise _ActionError(
            "harness_unavailable",
            "wylde-harness pipe is unreachable (no Python chat-turn fallback)",
        )
    return forwarded


# ── Rust-pipe forwarding ───────────────────────────────────────────────


def _forward_chat_action_to_rust(
    action: str,
    payload: Dict[str, Any],
    timeout: float,
    default_code: str,
) -> Optional[Dict[str, Any]]:
    """Forward one chat.* action to ``\\\\.\\pipe\\wylde-harness``.

    Returns the Rust reply ``data`` (a dict) on success, ``None`` on a
    transport-class failure so the caller can raise ``harness_unavailable``
    (there is no Python driver to fall back to since Phase 5.D). A genuine
    service-level error (Rust returned ``ok=false`` with a non-transport
    code) is re-raised as :class:`_ActionError` so the harness pipe
    surfaces it with the Rust code/message intact.

    Shared by every chat.* verb's per-verb forwarder so the transport
    fault / error-surfacing semantics stay identical across them.
    """
    try:
        from Core.shared.ipc import send_action as _ipc_send_action
    except ImportError:  # pragma: no cover — IPC shim always present in prod
        return None
    try:
        reply = _ipc_send_action("wylde-harness", action, payload, timeout=timeout)
    except Exception:  # noqa: BLE001 — transport failures become harness_unavailable
        return None

    if not getattr(reply, "ok", False):
        err = getattr(reply, "error", None) or {}
        code = err.get("code") if isinstance(err, dict) else None
        if code in _TRANSPORT_FALLBACK_CODES:
            # Rust unreachable / gated off / verb not registered → caller
            # raises harness_unavailable.
            return None
        # Genuine service-level failure — surface it verbatim.
        message = ""
        if isinstance(err, dict):
            message = str(err.get("message") or err.get("code") or default_code)
        raise _ActionError(str(code or default_code), message)

    data = getattr(reply, "data", None)
    if not isinstance(data, dict):
        return None
    return data


def _try_forward_run_turn_to_rust(
    p: Dict[str, Any], timeout: float
) -> Optional[Dict[str, Any]]:
    """Forward ``chat.run_turn``. The Rust reply uses the same
    ``{turn_id, conversation_id, final_message, tool_calls_summary,
    aborted, abort_reason}`` envelope, so it is surfaced verbatim."""
    forward_payload = {
        "user_message": p["user_message"],
        "conversation_id": p["conversation_id"],
        "model": p.get("model") or None,
        "turn_id": p.get("turn_id") or None,
        "workspace_id": p.get("workspace_id") or None,
        "modality": str(p.get("modality") or "text"),
        "device_tier": p.get("device_tier") or None,
        "timeout": timeout,
    }
    return _forward_chat_action_to_rust(
        "chat.run_turn", forward_payload, timeout + 5.0, "rust_turn_error"
    )


def _try_forward_start_turn_to_rust(
    p: Dict[str, Any]
) -> Optional[Dict[str, Any]]:
    """Forward ``chat.start_turn``. The Rust reply
    (``{turn_id, conversation_id}``) is surfaced verbatim."""
    forward_payload = {
        "user_message": p["user_message"],
        "conversation_id": p["conversation_id"],
        "model": p.get("model") or None,
        "turn_id": p.get("turn_id") or None,
        "workspace_id": p.get("workspace_id") or None,
        "modality": str(p.get("modality") or "text"),
        "device_tier": p.get("device_tier") or None,
    }
    return _forward_chat_action_to_rust(
        "chat.start_turn", forward_payload, _FORWARD_TIMEOUT, "rust_start_turn_error"
    )


def _try_forward_cancel_to_rust(
    p: Dict[str, Any]
) -> Optional[Dict[str, Any]]:
    """Forward ``chat.cancel``.

    The Rust handler replies ``{turn_id, cancelled}``; the Python pipe's
    long-standing contract is ``{ok, turn_id}`` (see WYLDE_ENDPOINTS.md).
    Map the reply so the wire shape callers see is unchanged.
    """
    turn_id = p["turn_id"]
    data = _forward_chat_action_to_rust(
        "chat.cancel", {"turn_id": turn_id}, _FORWARD_TIMEOUT, "rust_cancel_error"
    )
    if data is None:
        return None
    return {"ok": bool(data.get("cancelled")), "turn_id": data.get("turn_id", turn_id)}
