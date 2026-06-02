"""chat.* action handlers backed by :mod:`Core.harness.turn`.

Three unary chat.* actions: ``chat.start_turn``, ``chat.run_turn``,
``chat.cancel``.

The two streaming verbs (``chat.stream_turn`` / ``chat.stream_tools``)
are **Rust-only** — they are served by the Rust ``wylde-harness``
binary's true ``ChunkFrame`` streaming handlers (registered in
``rust/crates/wylde-harness/src/pipe.rs``), which is how the gpui GUI
already consumes them (``wylde_gui_pipe::stream_call``). The old Python
long-poll cursor bridge (``{turn_id, cursor, max_wait_ms}`` →
``{events, next_cursor, done}``) was dropped once the consumer audit
confirmed the GUI is the sole streaming consumer and it streams via
Rust — see ``docs/plans/harness-phase-5b-decision.md`` (Path A). The
Python IPC server has no ``ChunkFrame`` path, so these verbs were never
servable as true streams from here anyway.

Phase 5 strangler-fig
---------------------

``WYLDE_HARNESS_IMPL=rust`` forwards ``chat.run_turn`` to the Rust
``wylde-harness`` pipe at ``\\\\.\\pipe\\wylde-harness``. Slice
5.D (2026-05-25) flipped the default from ``python`` to ``rust``
after byte-level parity coverage landed on the salvage parser,
``_call_hash``, and ``_find_balanced_braces`` — the pure functions
whose port fidelity is load-bearing for the dispatch loop
(``rust/tests/parity/tests/harness_turn.rs``, 25 cases, all green).
The other four chat.* actions stay on Python until 5.B streaming
parity coverage is broad enough to forward them too.
Misconfigured / missing-binary deployments degrade silently to
Python so a typo can't take the chat brain offline. Set
``WYLDE_HARNESS_IMPL=python`` to revert.

The env var was renamed from ``WYLDE_HARNESS_TURN_IMPL`` to
``WYLDE_HARNESS_IMPL`` in 2026-05-24's consolidation rename (the
Rust crate ``wylde-harness-turn`` was folded into ``wylde-harness``).
The old name is honoured as a one-release fallback so a partial
rollout can't mis-flip.
"""

from __future__ import annotations

import os
from typing import Any, Dict

from .. import turn as _turn
from ._common import (
    _ActionError,
    _payload_dict,
)


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


def _start_turn_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    user_message = p.get("user_message")
    conversation_id = p.get("conversation_id")
    if not isinstance(user_message, str) or not user_message:
        raise _ActionError("bad_request", "user_message is required")
    if not isinstance(conversation_id, str) or not conversation_id:
        raise _ActionError("bad_request", "conversation_id is required")
    state = _turn.start_turn(
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

    result = _turn.run_turn(
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


def _try_forward_run_turn_to_rust(
    p: Dict[str, Any], timeout: float
) -> Dict[str, Any] | None:
    """Forward ``chat.run_turn`` to ``\\\\.\\pipe\\wylde-harness``.

    Returns the Rust reply on success, ``None`` on transport failure
    (so the caller can fall back to the in-process Python driver).
    Service-level errors (Rust returned ``ok=false``) are re-raised as
    ``_ActionError`` so the harness pipe surfaces them with the same
    envelope shape as a Python failure.
    """
    try:
        from Core.shared.ipc import send_action as _ipc_send_action
    except ImportError:  # pragma: no cover — IPC shim always present in prod
        return None

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
    try:
        reply = _ipc_send_action(
            "wylde-harness",
            "chat.run_turn",
            forward_payload,
            timeout=timeout + 5.0,
        )
    except Exception:  # noqa: BLE001 — transport failures are caught for fallback
        return None

    # Transport-layer failure (no Rust pipe up, daemon not running,
    # binary not implemented yet) → silent fallback to Python. The
    # strangler is "use Rust when it answers"; a daemon mis-spawn or
    # missing binary must not break chat.
    if not getattr(reply, "ok", False):
        err = getattr(reply, "error", None) or {}
        code = err.get("code") if isinstance(err, dict) else None
        transport_codes = {
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
        if code in transport_codes:
            return None
        # Genuine service-level failure — surface it as if Python had
        # raised. Caller's envelope shape stays consistent.
        message = ""
        if isinstance(err, dict):
            message = str(
                err.get("message") or err.get("code") or "rust turn driver error"
            )
        raise _ActionError(str(code or "rust_turn_error"), message)

    data = getattr(reply, "data", None)
    if not isinstance(data, dict):
        return None
    # Trust the Rust reply shape — chat.run_turn returns the same
    # {turn_id, conversation_id, final_message, tool_calls_summary,
    # aborted, abort_reason} envelope this Python handler builds.
    return data


def _cancel_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    turn_id = p.get("turn_id")
    if not isinstance(turn_id, str) or not turn_id:
        raise _ActionError("bad_request", "turn_id is required")
    cancelled = _turn.cancel_turn(turn_id)
    return {"ok": cancelled, "turn_id": turn_id}
