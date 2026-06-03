"""Phase 5 strangler-fig — `WYLDE_HARNESS_IMPL` gating in `_chat.py`.

The consolidated Rust ``wylde-harness`` crate exposes ``chat.run_turn``
(slice 5.A) plus the streaming surface (slice 5.B) on
``\\\\.\\pipe\\wylde-harness``. The Python pipe handler in
``Core/harness/pipe/_chat.py`` forwards ``chat.run_turn`` to it when
the env var is set and falls back to the in-process Python driver
when the Rust pipe is unreachable. The slice-5.A-era env var
``WYLDE_HARNESS_TURN_IMPL`` is still honoured as a one-release
fallback so partial rollouts can't mis-flip. These tests pin both
halves of that behaviour.
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


def test_impl_defaults_to_rust(monkeypatch: pytest.MonkeyPatch) -> None:
    # Slice 5.D (2026-05-25) flipped the default from python to rust.
    monkeypatch.delenv("WYLDE_HARNESS_IMPL", raising=False)
    monkeypatch.delenv("WYLDE_HARNESS_TURN_IMPL", raising=False)
    assert _chat._harness_turn_impl() == "rust"


def test_impl_clamps_unknown_value_to_default(monkeypatch: pytest.MonkeyPatch) -> None:
    # Clamps to the current default (rust, post-5.D) so a typo can't
    # silently disable the chat brain — it stays on whatever the
    # default is at the time of the typo.
    monkeypatch.setenv("WYLDE_HARNESS_IMPL", "ferret")
    assert _chat._harness_turn_impl() == "rust"


def test_impl_accepts_python(monkeypatch: pytest.MonkeyPatch) -> None:
    # Rollback path: setting the env var to python reverts to the
    # in-process Python driver inside Core/harness/turn/.
    monkeypatch.setenv("WYLDE_HARNESS_IMPL", "python")
    assert _chat._harness_turn_impl() == "python"


def test_impl_accepts_rust(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("WYLDE_HARNESS_IMPL", "rust")
    assert _chat._harness_turn_impl() == "rust"


def test_legacy_env_var_still_accepted_as_fallback(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """The slice-5.A-era ``WYLDE_HARNESS_TURN_IMPL`` is honoured for one
    release so partial rollouts can't mis-flip. New var, when set,
    wins."""
    monkeypatch.delenv("WYLDE_HARNESS_IMPL", raising=False)
    monkeypatch.setenv("WYLDE_HARNESS_TURN_IMPL", "rust")
    assert _chat._harness_turn_impl() == "rust"

    # New var present overrides legacy.
    monkeypatch.setenv("WYLDE_HARNESS_IMPL", "python")
    monkeypatch.setenv("WYLDE_HARNESS_TURN_IMPL", "rust")
    assert _chat._harness_turn_impl() == "python"


def test_forward_returns_data_on_ok_reply(monkeypatch: pytest.MonkeyPatch) -> None:
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
def test_forward_falls_through_on_transport_failure(
    monkeypatch: pytest.MonkeyPatch, transport_code: str
) -> None:
    reply = _StubReply(ok=False, error={"code": transport_code, "message": "x"})
    with mock.patch("Core.shared.ipc.send_action", return_value=reply):
        out = _chat._try_forward_run_turn_to_rust(
            {"user_message": "hi", "conversation_id": "c1"}, timeout=10.0
        )
    assert out is None, f"transport failure {transport_code!r} should fall back"


def test_forward_surfaces_service_level_error(monkeypatch: pytest.MonkeyPatch) -> None:
    reply = _StubReply(
        ok=False, error={"code": "bad_request", "message": "model is required"}
    )
    with mock.patch("Core.shared.ipc.send_action", return_value=reply):
        with pytest.raises(_chat._ActionError) as ei:
            _chat._try_forward_run_turn_to_rust(
                {"user_message": "hi", "conversation_id": "c1"}, timeout=10.0
            )
    assert ei.value.code == "bad_request"


def test_forward_falls_through_on_exception(monkeypatch: pytest.MonkeyPatch) -> None:
    def boom(*_a: Any, **_kw: Any) -> Any:
        raise RuntimeError("pipe gone")

    with mock.patch("Core.shared.ipc.send_action", side_effect=boom):
        out = _chat._try_forward_run_turn_to_rust(
            {"user_message": "hi", "conversation_id": "c1"}, timeout=10.0
        )
    assert out is None


def test_run_turn_action_uses_rust_when_env_set(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("WYLDE_HARNESS_IMPL", "rust")
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


def test_run_turn_action_falls_back_to_python_when_rust_down(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("WYLDE_HARNESS_IMPL", "rust")
    # Rust forward returns None → driver runs Python turn. We stub the
    # (lazily-imported) turn module so this stays a pure unit test.
    fake_turn = mock.Mock()
    fake_turn.run_turn.return_value = _StubTurn(
        turn_id="t-py",
        conversation_id="c1",
        final_message="from python",
        tool_calls_summary=[],
        aborted=False,
        abort_reason=None,
    )
    with mock.patch.object(_chat, "_try_forward_run_turn_to_rust", return_value=None):
        with mock.patch.object(_chat, "_turn_module", return_value=fake_turn):
            out = _chat._run_turn_action(
                {"user_message": "hi", "conversation_id": "c1"}
            )
    assert out["turn_id"] == "t-py"
    assert out["final_message"] == "from python"


# ── chat.start_turn forwarding (Phase 5.D prereq) ──────────────────────


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
def test_forward_start_turn_falls_through_on_transport_failure(
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


def test_start_turn_action_uses_rust_when_env_set(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("WYLDE_HARNESS_IMPL", "rust")
    rust_reply = {"turn_id": "t-rust", "conversation_id": "c1"}
    with mock.patch.object(
        _chat, "_try_forward_start_turn_to_rust", return_value=rust_reply
    ):
        out = _chat._start_turn_action(
            {"user_message": "hi", "conversation_id": "c1"}
        )
    assert out == rust_reply


def test_start_turn_python_and_rust_paths_agree(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Cross-impl parity: the verbatim Rust reply and the Python driver's
    reply carry the same ``{turn_id, conversation_id}`` shape."""
    monkeypatch.setenv("WYLDE_HARNESS_IMPL", "rust")
    payload = {"user_message": "hi", "conversation_id": "c1", "turn_id": "t-shared"}

    rust_reply = {"turn_id": "t-shared", "conversation_id": "c1"}
    with mock.patch.object(
        _chat, "_try_forward_start_turn_to_rust", return_value=rust_reply
    ):
        rust_out = _chat._start_turn_action(payload)

    fake_turn = mock.Mock()
    fake_turn.start_turn.return_value = _StubStartState(
        turn_id="t-shared", conversation_id="c1"
    )
    with mock.patch.object(_chat, "_try_forward_start_turn_to_rust", return_value=None):
        with mock.patch.object(_chat, "_turn_module", return_value=fake_turn):
            py_out = _chat._start_turn_action(payload)

    assert rust_out == py_out == {"turn_id": "t-shared", "conversation_id": "c1"}


# ── chat.cancel forwarding (Phase 5.D prereq) ──────────────────────────


def test_forward_cancel_maps_rust_shape_to_python_contract() -> None:
    """Rust replies ``{turn_id, cancelled}``; the Python pipe contract is
    ``{ok, turn_id}``. The forwarder maps so the wire shape is identical
    whichever impl served the cancel."""
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
def test_forward_cancel_falls_through_on_transport_failure(
    transport_code: str,
) -> None:
    reply = _StubReply(ok=False, error={"code": transport_code, "message": "x"})
    with mock.patch("Core.shared.ipc.send_action", return_value=reply):
        out = _chat._try_forward_cancel_to_rust({"turn_id": "t1"})
    assert out is None


def test_cancel_action_uses_rust_when_env_set(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("WYLDE_HARNESS_IMPL", "rust")
    rust_reply = {"ok": True, "turn_id": "t1"}
    with mock.patch.object(
        _chat, "_try_forward_cancel_to_rust", return_value=rust_reply
    ):
        out = _chat._cancel_action({"turn_id": "t1"})
    assert out == rust_reply


def test_cancel_python_and_rust_paths_agree(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Cross-impl parity: forwarded-Rust cancel and Python-driver cancel
    both produce ``{ok, turn_id}`` for the same input."""
    monkeypatch.setenv("WYLDE_HARNESS_IMPL", "rust")
    payload = {"turn_id": "t1"}

    # Rust path: handler replies {turn_id, cancelled}; mapped to {ok, turn_id}.
    rust_pipe_reply = _StubReply(ok=True, data={"turn_id": "t1", "cancelled": True})
    with mock.patch("Core.shared.ipc.send_action", return_value=rust_pipe_reply):
        rust_out = _chat._cancel_action(payload)

    # Python path: driver's cancel_turn returns the bool.
    fake_turn = mock.Mock()
    fake_turn.cancel_turn.return_value = True
    with mock.patch.object(_chat, "_try_forward_cancel_to_rust", return_value=None):
        with mock.patch.object(_chat, "_turn_module", return_value=fake_turn):
            py_out = _chat._cancel_action(payload)

    assert rust_out == py_out == {"ok": True, "turn_id": "t1"}


def test_chat_module_import_does_not_load_turn_driver() -> None:
    """The default Rust path must not import ``Core.harness.turn`` at
    module load — that is what lets the follow-up slice delete the
    Python driver package. The driver is reached only via the lazy
    :func:`_turn_module` accessor."""
    assert not hasattr(_chat, "_turn"), (
        "_chat must not hold a module-level reference to the turn driver"
    )


@dataclass
class _StubTurn:
    turn_id: str
    conversation_id: str
    final_message: str
    tool_calls_summary: list
    aborted: bool
    abort_reason: Optional[str]


@dataclass
class _StubStartState:
    turn_id: str
    conversation_id: str
