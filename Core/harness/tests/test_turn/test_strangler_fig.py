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
    # turn module so this stays a pure unit test (no Ollama).
    with mock.patch.object(_chat, "_try_forward_run_turn_to_rust", return_value=None):
        with mock.patch.object(_chat._turn, "run_turn") as fake_run:
            fake_run.return_value = _StubTurn(
                turn_id="t-py",
                conversation_id="c1",
                final_message="from python",
                tool_calls_summary=[],
                aborted=False,
                abort_reason=None,
            )
            out = _chat._run_turn_action(
                {"user_message": "hi", "conversation_id": "c1"}
            )
    assert out["turn_id"] == "t-py"
    assert out["final_message"] == "from python"


@dataclass
class _StubTurn:
    turn_id: str
    conversation_id: str
    final_message: str
    tool_calls_summary: list
    aborted: bool
    abort_reason: Optional[str]
