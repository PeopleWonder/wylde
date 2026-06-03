"""chat.* Rust forwarder — `_chat.py` thin-forwarder behaviour.

Phase 5.D retired the in-process Python chat-turn driver
(``Core/harness/turn/``). The three unary chat.* verbs this Python pipe
still exposes are now pure forwarders to the Rust ``wylde-harness`` pipe:
on a successful reply they surface the Rust ``data`` verbatim; on a
transport-class fault they raise ``harness_unavailable`` (there is no
Python loop to fall back to); on a service-level error they re-raise the
Rust code/message.

This file is the descendant of ``test_turn/test_strangler_fig.py`` — the
env-var-gating and Python-fallback tests it carried died with the
strangler knob and the driver. What remains is the forwarder contract,
which is independent of the (now sole) Rust implementation and is worth
pinning so the wire shape can't drift.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Dict, Optional
from unittest import mock

import pytest

from Core.harness.pipe import _chat


@dataclass
class _StubReply:
    ok: bool
    data: Any = None
    error: Optional[Dict[str, Any]] = None


# ── chat.run_turn forwarding ───────────────────────────────────────────


def test_forward_returns_data_on_ok_reply() -> None:
    reply = _StubReply(
        ok=True,
        data={
            "turn_id": "t1",
            "conversation_id": "c1",
            "final_message": "hi from rust",
            "tool_calls_summary": [],
            "aborted": False,
            "abort_reason": None,
        },
    )

    def fake_send_action(
        service: str, action: str, payload: Any, timeout: float
    ) -> _StubReply:
        assert service == "wylde-harness"
        assert action == "chat.run_turn"
        return reply

    with mock.patch("Core.shared.ipc.send_action", side_effect=fake_send_action):
        out = _chat._try_forward_run_turn_to_rust(
            {"user_message": "hi", "conversation_id": "c1"}, timeout=10.0
        )
    assert out is not None
    assert out["final_message"] == "hi from rust"


@pytest.mark.parametrize(
    "transport_code",
    [
        "not_found",
        "pipe_unavailable",
        "pipe_connect",
        "pipe_timeout",
        "no_action",
        "not_implemented",
    ],
)
def test_forward_returns_none_on_transport_failure(transport_code: str) -> None:
    reply = _StubReply(ok=False, error={"code": transport_code, "message": "x"})
    with mock.patch("Core.shared.ipc.send_action", return_value=reply):
        out = _chat._try_forward_run_turn_to_rust(
            {"user_message": "hi", "conversation_id": "c1"}, timeout=10.0
        )
    assert out is None, f"transport failure {transport_code!r} should return None"


def test_forward_surfaces_service_level_error() -> None:
    reply = _StubReply(
        ok=False, error={"code": "bad_request", "message": "model is required"}
    )
    with mock.patch("Core.shared.ipc.send_action", return_value=reply):
        with pytest.raises(_chat._ActionError) as ei:
            _chat._try_forward_run_turn_to_rust(
                {"user_message": "hi", "conversation_id": "c1"}, timeout=10.0
            )
    assert ei.value.code == "bad_request"


def test_forward_returns_none_on_transport_exception() -> None:
    def boom(*_a: Any, **_kw: Any) -> Any:
        raise RuntimeError("pipe gone")

    with mock.patch("Core.shared.ipc.send_action", side_effect=boom):
        out = _chat._try_forward_run_turn_to_rust(
            {"user_message": "hi", "conversation_id": "c1"}, timeout=10.0
        )
    assert out is None


def test_run_turn_action_surfaces_rust_reply() -> None:
    rust_reply = {
        "turn_id": "t-rust",
        "conversation_id": "c1",
        "final_message": "from rust",
        "tool_calls_summary": [],
        "aborted": False,
        "abort_reason": None,
    }
    with mock.patch.object(
        _chat, "_try_forward_run_turn_to_rust", return_value=rust_reply
    ):
        out = _chat._run_turn_action({"user_message": "hi", "conversation_id": "c1"})
    assert out == rust_reply


def test_run_turn_action_raises_harness_unavailable_when_rust_down() -> None:
    """No Python fall-back since Phase 5.D — an unreachable Rust pipe
    surfaces ``harness_unavailable`` instead of silently driving a
    (deleted) in-process loop."""
    with mock.patch.object(_chat, "_try_forward_run_turn_to_rust", return_value=None):
        with pytest.raises(_chat._ActionError) as ei:
            _chat._run_turn_action({"user_message": "hi", "conversation_id": "c1"})
    assert ei.value.code == "harness_unavailable"


# ── chat.start_turn forwarding ─────────────────────────────────────────


def test_forward_start_turn_returns_data_on_ok_reply() -> None:
    reply = _StubReply(ok=True, data={"turn_id": "t1", "conversation_id": "c1"})

    def fake_send_action(
        service: str, action: str, payload: Any, timeout: float
    ) -> _StubReply:
        assert service == "wylde-harness"
        assert action == "chat.start_turn"
        return reply

    with mock.patch("Core.shared.ipc.send_action", side_effect=fake_send_action):
        out = _chat._try_forward_start_turn_to_rust(
            {"user_message": "hi", "conversation_id": "c1"}
        )
    assert out == {"turn_id": "t1", "conversation_id": "c1"}


@pytest.mark.parametrize(
    "transport_code",
    ["not_found", "pipe_unavailable", "pipe_timeout", "no_action", "not_implemented"],
)
def test_forward_start_turn_returns_none_on_transport_failure(
    transport_code: str,
) -> None:
    reply = _StubReply(ok=False, error={"code": transport_code, "message": "x"})
    with mock.patch("Core.shared.ipc.send_action", return_value=reply):
        out = _chat._try_forward_start_turn_to_rust(
            {"user_message": "hi", "conversation_id": "c1"}
        )
    assert out is None


def test_forward_start_turn_surfaces_service_level_error() -> None:
    reply = _StubReply(
        ok=False, error={"code": "bad_request", "message": "model is required"}
    )
    with mock.patch("Core.shared.ipc.send_action", return_value=reply):
        with pytest.raises(_chat._ActionError) as ei:
            _chat._try_forward_start_turn_to_rust(
                {"user_message": "hi", "conversation_id": "c1"}
            )
    assert ei.value.code == "bad_request"


def test_start_turn_action_surfaces_rust_reply() -> None:
    rust_reply = {"turn_id": "t-rust", "conversation_id": "c1"}
    with mock.patch.object(
        _chat, "_try_forward_start_turn_to_rust", return_value=rust_reply
    ):
        out = _chat._start_turn_action({"user_message": "hi", "conversation_id": "c1"})
    assert out == rust_reply


def test_start_turn_action_raises_harness_unavailable_when_rust_down() -> None:
    with mock.patch.object(_chat, "_try_forward_start_turn_to_rust", return_value=None):
        with pytest.raises(_chat._ActionError) as ei:
            _chat._start_turn_action({"user_message": "hi", "conversation_id": "c1"})
    assert ei.value.code == "harness_unavailable"


# ── chat.cancel forwarding ─────────────────────────────────────────────


def test_forward_cancel_maps_rust_shape_to_python_contract() -> None:
    """Rust replies ``{turn_id, cancelled}``; the Python pipe contract is
    ``{ok, turn_id}``. The forwarder maps so the wire shape callers see
    is unchanged."""
    reply = _StubReply(ok=True, data={"turn_id": "t1", "cancelled": True})

    def fake_send_action(
        service: str, action: str, payload: Any, timeout: float
    ) -> _StubReply:
        assert service == "wylde-harness"
        assert action == "chat.cancel"
        assert payload == {"turn_id": "t1"}
        return reply

    with mock.patch("Core.shared.ipc.send_action", side_effect=fake_send_action):
        out = _chat._try_forward_cancel_to_rust({"turn_id": "t1"})
    assert out == {"ok": True, "turn_id": "t1"}


def test_forward_cancel_maps_cancelled_false() -> None:
    reply = _StubReply(ok=True, data={"turn_id": "t1", "cancelled": False})
    with mock.patch("Core.shared.ipc.send_action", return_value=reply):
        out = _chat._try_forward_cancel_to_rust({"turn_id": "t1"})
    assert out == {"ok": False, "turn_id": "t1"}


@pytest.mark.parametrize(
    "transport_code",
    ["not_found", "pipe_unavailable", "pipe_timeout", "no_action", "not_implemented"],
)
def test_forward_cancel_returns_none_on_transport_failure(transport_code: str) -> None:
    reply = _StubReply(ok=False, error={"code": transport_code, "message": "x"})
    with mock.patch("Core.shared.ipc.send_action", return_value=reply):
        out = _chat._try_forward_cancel_to_rust({"turn_id": "t1"})
    assert out is None


def test_cancel_action_surfaces_rust_reply() -> None:
    rust_reply = {"ok": True, "turn_id": "t1"}
    with mock.patch.object(
        _chat, "_try_forward_cancel_to_rust", return_value=rust_reply
    ):
        out = _chat._cancel_action({"turn_id": "t1"})
    assert out == rust_reply


def test_cancel_action_raises_harness_unavailable_when_rust_down() -> None:
    with mock.patch.object(_chat, "_try_forward_cancel_to_rust", return_value=None):
        with pytest.raises(_chat._ActionError) as ei:
            _chat._cancel_action({"turn_id": "t1"})
    assert ei.value.code == "harness_unavailable"


# ── driver-package retirement guard ────────────────────────────────────


def test_chat_module_does_not_reference_deleted_turn_driver() -> None:
    """``Core/harness/turn/`` was deleted in Phase 5.D. ``_chat`` must
    not hold any reference to the retired driver (module attr or the
    former lazy accessor / strangler gate)."""
    assert not hasattr(_chat, "_turn"), "stale module-level turn reference"
    assert not hasattr(_chat, "_turn_module"), "stale lazy turn accessor"
    assert not hasattr(_chat, "_harness_turn_impl"), "stale strangler env gate"


def test_turn_driver_package_is_gone() -> None:
    """The Python chat-turn driver package must no longer be importable."""
    with pytest.raises(ImportError):
        import Core.harness.turn  # noqa: F401
