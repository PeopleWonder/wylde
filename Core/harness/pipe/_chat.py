"""chat.* action handlers.

Phase 5 strangler-fig
---------------------

``WYLDE_HARNESS_IMPL=rust`` (the default since slice 5.D) forwards the
unary chat-turn verbs — ``chat.run_turn``, ``chat.start_turn``,
``chat.cancel`` — to the Rust ``wylde-harness`` pipe at
``\\\\.\\pipe\\wylde-harness`` and surfaces the reply verbatim. Slice
5.D (2026-05-25) flipped the default from ``python`` to ``rust`` after
byte-level parity coverage landed on the salvage parser, ``_call_hash``,
and ``_find_balanced_braces`` — the pure functions whose port fidelity
is load-bearing for the dispatch loop (``rust/tests/parity/tests/
harness_turn.rs``, 25 cases, all green). Phase 5.D's prerequisite slice
extended the forward from ``chat.run_turn`` alone to all three unary
verbs so nothing on the default path imports the Python driver package.

The streaming surface (``chat.stream_turn`` / ``chat.stream_tools``) is
NOT served by this Python pipe — those are long-poll cursor actions the
Rust ``wylde-harness`` binary serves natively; there is no Python
handler here to forward. See ``rust/crates/wylde-harness/src/turn/
actions.rs``.

Misconfigured / missing-binary deployments degrade silently to Python
so a typo or a daemon mis-spawn can't take the chat brain offline. Set
``WYLDE_HARNESS_IMPL=python`` to revert to the in-process Python driver
inside ``Core/harness/turn/`` (kept for one release as the rollback
path; the Python driver is reached only via :func:`_turn_module`, a
lazy import, so the default Rust path never loads it).

The env var was renamed from ``WYLDE_HARNESS_TURN_IMPL`` to
``WYLDE_HARNESS_IMPL`` in 2026-05-24's consolidation rename (the
Rust crate ``wylde-harness-turn`` was folded into ``wylde-harness``).
The old name is honoured as a one-release fallback so a partial
rollout can't mis-flip.
"""

from __future__ import annotations

import os
from typing import Any, Dict, Optional

from ._common import (
    _ActionError,
    _payload_dict,
)

# Quick-verb forward timeout (start_turn / cancel return immediately;
# run_turn carries its own longer per-call timeout). Kept modest so a
# wedged Rust pipe falls back to Python fast instead of blocking the GUI.
_FORWARD_TIMEOUT = 30.0

# Reply error codes that mean "the Rust pipe didn't actually serve this"
# (binary down, daemon mis-spawn, verb not registered yet). The strangler
# treats these as transport failures and falls back to the in-process
# Python driver — a partial rollout or missing binary must never brick
# chat. Any OTHER ``ok=false`` code is a genuine service-level error and
# is re-raised so the envelope shape matches a Python failure.
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


def _harness_turn_impl() -> str:
    """Read ``WYLDE_HARNESS_IMPL`` (with ``WYLDE_HARNESS_TURN_IMPL`` as
    a one-release fallback) once per call.

    Default ``rust`` since slice 5.D (2026-05-25) — see module
    docstring. Anything other than ``python`` / ``rust`` is logged
    and clamped to the default — same fail-safe semantics the
    Lifecycle daemon's ``_impl_for`` helper uses.
    """
    raw = os.environ.get("WYLDE_HARNESS_IMPL")
    if raw is None:
        # Legacy name from the slice-5.A standalone-crate era. Honoured
        # for one release so a partial rollout can't mis-flip.
        raw = os.environ.get("WYLDE_HARNESS_TURN_IMPL", "rust")
    val = raw.strip().lower()
    if val in ("python", "rust"):
        return val
    return "rust"


def _turn_module() -> Any:
    """Lazily import the in-process Python chat-turn driver.

    Only the ``WYLDE_HARNESS_IMPL=python`` rollback path reaches this —
    the default Rust path forwards every chat.* verb to the Rust
    ``wylde-harness`` pipe and never touches ``Core.harness.turn``. The
    import is deliberately lazy so importing this module (and thus the
    harness pipe) does NOT pull in the Python driver package; that lets
    the Phase 5.D follow-up slice delete ``Core/harness/turn/`` without
    breaking the default deployment. That deletion task removes this
    fallback (and its three call sites below) entirely.
    """
    from .. import turn as _turn

    return _turn


# ── chat.* action handlers ─────────────────────────────────────────────


def _start_turn_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    user_message = p.get("user_message")
    conversation_id = p.get("conversation_id")
    if not isinstance(user_message, str) or not user_message:
        raise _ActionError("bad_request", "user_message is required")
    if not isinstance(conversation_id, str) or not conversation_id:
        raise _ActionError("bad_request", "conversation_id is required")

    if _harness_turn_impl() == "rust":
        forwarded = _try_forward_start_turn_to_rust(p)
        if forwarded is not None:
            return forwarded
        # Fall-through: Rust unreachable / not implemented → Python.

    state = _turn_module().start_turn(
        user_message=user_message,
        conversation_id=conversation_id,
        model=p.get("model") or None,
        turn_id=p.get("turn_id") or None,
        workspace_id=p.get("workspace_id") or None,
        modality=str(p.get("modality") or "text"),
        device_tier=p.get("device_tier") or None,
    )
    return {"turn_id": state.turn_id, "conversation_id": state.conversation_id}


def _run_turn_action(payload: Any) -> Dict[str, Any]:
    """Blocking variant — drives the turn to completion server-side and
    returns the final result. For non-streaming consumers (tests, future
    MCP, mobile single-shot).

    Phase 5 strangler-fig: when ``WYLDE_HARNESS_IMPL=rust`` is set,
    this forwards to the Rust ``wylde-harness`` pipe and surfaces its
    reply verbatim. The default (``python``) drives the in-process
    Python driver as before. Transport-level failures fall through to
    Python so a daemon mis-spawn never bricks chat. (The legacy
    ``WYLDE_HARNESS_TURN_IMPL`` env var is honoured as a one-release
    fallback for the 2026-05-24 consolidation rename.)
    """
    p = _payload_dict(payload)
    user_message = p.get("user_message")
    conversation_id = p.get("conversation_id")
    if not isinstance(user_message, str) or not user_message:
        raise _ActionError("bad_request", "user_message is required")
    if not isinstance(conversation_id, str) or not conversation_id:
        raise _ActionError("bad_request", "conversation_id is required")
    timeout = float(p.get("timeout") or 300.0)

    if _harness_turn_impl() == "rust":
        forwarded = _try_forward_run_turn_to_rust(p, timeout)
        if forwarded is not None:
            return forwarded
        # Fall-through: Rust unreachable / not implemented yet → Python.

    result = _turn_module().run_turn(
        user_message=user_message,
        conversation_id=conversation_id,
        model=p.get("model") or None,
        turn_id=p.get("turn_id") or None,
        workspace_id=p.get("workspace_id") or None,
        modality=str(p.get("modality") or "text"),
        device_tier=p.get("device_tier") or None,
        timeout=timeout,
    )
    return {
        "turn_id": result.turn_id,
        "conversation_id": result.conversation_id,
        "final_message": result.final_message,
        "tool_calls_summary": list(result.tool_calls_summary),
        "aborted": bool(result.aborted),
        "abort_reason": result.abort_reason,
    }


def _cancel_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    turn_id = p.get("turn_id")
    if not isinstance(turn_id, str) or not turn_id:
        raise _ActionError("bad_request", "turn_id is required")

    if _harness_turn_impl() == "rust":
        forwarded = _try_forward_cancel_to_rust(p)
        if forwarded is not None:
            return forwarded
        # Fall-through: Rust unreachable / not implemented → Python.

    cancelled = _turn_module().cancel_turn(turn_id)
    return {"ok": cancelled, "turn_id": turn_id}


# ── Rust-pipe forwarding ───────────────────────────────────────────────


def _forward_chat_action_to_rust(
    action: str,
    payload: Dict[str, Any],
    timeout: float,
    default_code: str,
) -> Optional[Dict[str, Any]]:
    """Forward one chat.* action to ``\\\\.\\pipe\\wylde-harness``.

    Returns the Rust reply ``data`` (a dict) on success, ``None`` on a
    transport-class failure so the caller can fall back to the
    in-process Python driver. A genuine service-level error (Rust
    returned ``ok=false`` with a non-transport code) is re-raised as
    :class:`_ActionError` so the harness pipe surfaces it with the same
    envelope shape a Python failure would produce.

    Shared by every chat.* verb's per-verb forwarder so the transport
    fall-back / error-surfacing semantics stay identical across them.
    """
    try:
        from Core.shared.ipc import send_action as _ipc_send_action
    except ImportError:  # pragma: no cover — IPC shim always present in prod
        return None
    try:
        reply = _ipc_send_action("wylde-harness", action, payload, timeout=timeout)
    except Exception:  # noqa: BLE001 — transport failures are caught for fallback
        return None

    if not getattr(reply, "ok", False):
        err = getattr(reply, "error", None) or {}
        code = err.get("code") if isinstance(err, dict) else None
        if code in _TRANSPORT_FALLBACK_CODES:
            # Rust unreachable / gated off / verb not registered → Python.
            return None
        # Genuine service-level failure — surface it as if Python had
        # raised. Caller's envelope shape stays consistent.
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
    aborted, abort_reason}`` envelope this Python handler builds, so it
    is surfaced verbatim."""
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
    (``{turn_id, conversation_id}``) matches the Python handler's, so it
    is surfaced verbatim."""
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
    Map the reply so the wire shape is identical whichever impl served
    the cancel — the strangler must be invisible to callers.
    """
    turn_id = p["turn_id"]
    data = _forward_chat_action_to_rust(
        "chat.cancel", {"turn_id": turn_id}, _FORWARD_TIMEOUT, "rust_cancel_error"
    )
    if data is None:
        return None
    return {"ok": bool(data.get("cancelled")), "turn_id": data.get("turn_id", turn_id)}
