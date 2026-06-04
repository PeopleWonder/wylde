"""conversations.* Rust forwarder — `_conversations.py` thin-forwarder.

Memory Slice B ported the conversation-lifecycle verbs to Rust
(``rust/crates/wylde-harness/src/memory/conversations/``). The
``conversations.*`` verbs this Python pipe still exposes are now pure
forwarders to the Rust ``wylde-harness`` pipe: on a successful reply they
surface the Rust ``data`` verbatim; on a transport-class fault they raise
``harness_unavailable`` (there is no in-process Python path to fall back
to); on a service-level error they re-raise the Rust code/message.

This descends from the in-process smoke that previously drove
``conversation.py`` directly — that coverage now lives on the Rust side
(``memory::conversations::{store,actions}::tests``). What remains here is
the forwarder contract, which is independent of the Rust implementation
and worth pinning so the wire shape can't drift. Mirrors
``test_chat_forwarder.py``.

The one wrinkle versus the chat forwarder: ``not_found`` is a GENUINE
service-level reply for ``conversations.get`` (the conversation doesn't
exist), NOT a transport failure — so it must propagate, not collapse to
``harness_unavailable``.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Dict, Optional
from unittest import mock

import pytest

from Core.harness.pipe import _conversations


@dataclass
class _StubReply:
    ok: bool
    data: Any = None
    error: Optional[Dict[str, Any]] = None


# ── Low-level forwarder contract ───────────────────────────────────────


def test_forward_returns_data_on_ok_reply() -> None:
    reply = _StubReply(ok=True, data={"conversations": [], "count": 0})

    def fake_send_action(
        service: str, action: str, payload: Any, timeout: float
    ) -> _StubReply:
        assert service == "wylde-harness"
        assert action == "conversations.list"
        return reply

    with mock.patch("Core.shared.ipc.send_action", side_effect=fake_send_action):
        out = _conversations._forward_conversations_to_rust("conversations.list", {})
    assert out == {"conversations": [], "count": 0}


@pytest.mark.parametrize(
    "transport_code",
    [
        "pipe_unavailable",
        "pipe_connect",
        "pipe_timeout",
        "pipe_io",
        "handshake_timeout",
        "no_action",
        "not_implemented",
    ],
)
def test_forward_returns_none_on_transport_failure(transport_code: str) -> None:
    reply = _StubReply(ok=False, error={"code": transport_code, "message": "x"})
    with mock.patch("Core.shared.ipc.send_action", return_value=reply):
        out = _conversations._forward_conversations_to_rust("conversations.list", {})
    assert out is None, f"transport failure {transport_code!r} should return None"


def test_forward_not_found_is_service_level_not_transport() -> None:
    """``not_found`` is a real ``conversations.get`` answer (the chat is
    gone), so it must re-raise — collapsing it to ``harness_unavailable``
    would mask a legitimate 404 as an outage."""
    reply = _StubReply(
        ok=False, error={"code": "not_found", "message": "conversation 'x' not found"}
    )
    with mock.patch("Core.shared.ipc.send_action", return_value=reply):
        with pytest.raises(_conversations._ActionError) as ei:
            _conversations._forward_conversations_to_rust(
                "conversations.get", {"id": "x"}
            )
    assert ei.value.code == "not_found"


def test_forward_surfaces_service_level_bad_request() -> None:
    reply = _StubReply(
        ok=False, error={"code": "bad_request", "message": "id is required"}
    )
    with mock.patch("Core.shared.ipc.send_action", return_value=reply):
        with pytest.raises(_conversations._ActionError) as ei:
            _conversations._forward_conversations_to_rust(
                "conversations.get", {"id": "bad/slash"}
            )
    assert ei.value.code == "bad_request"


def test_forward_returns_none_on_transport_exception() -> None:
    def boom(*_a: Any, **_kw: Any) -> Any:
        raise RuntimeError("pipe gone")

    with mock.patch("Core.shared.ipc.send_action", side_effect=boom):
        out = _conversations._forward_conversations_to_rust("conversations.new", {})
    assert out is None


# ── Action wrappers ────────────────────────────────────────────────────


def test_new_action_surfaces_rust_id() -> None:
    with mock.patch.object(
        _conversations, "_forward_conversations_to_rust", return_value={"id": "c-rust"}
    ):
        out = _conversations._conversations_new_action(None)
    assert out == {"id": "c-rust"}


def test_list_action_surfaces_rust_reply() -> None:
    rust = {"conversations": [{"id": "a"}], "count": 1}
    with mock.patch.object(
        _conversations, "_forward_conversations_to_rust", return_value=rust
    ):
        out = _conversations._conversations_list_action(None)
    assert out == rust


def test_get_action_requires_id_before_forwarding() -> None:
    # Validation happens Python-side, so a missing id never touches the pipe.
    with mock.patch.object(_conversations, "_forward_conversations_to_rust") as fwd:
        with pytest.raises(_conversations._ActionError) as ei:
            _conversations._conversations_get_action({})
    assert ei.value.code == "bad_request"
    fwd.assert_not_called()


def test_get_action_surfaces_rust_document() -> None:
    doc = {"id": "abc", "messages": [{"role": "user", "content": "the body"}]}
    with mock.patch.object(
        _conversations, "_forward_conversations_to_rust", return_value=doc
    ):
        out = _conversations._conversations_get_action({"id": "abc"})
    assert out == doc


def test_delete_action_requires_id() -> None:
    with mock.patch.object(_conversations, "_forward_conversations_to_rust") as fwd:
        with pytest.raises(_conversations._ActionError) as ei:
            _conversations._conversations_delete_action({})
    assert ei.value.code == "bad_request"
    fwd.assert_not_called()


def test_delete_action_surfaces_rust_reply() -> None:
    with mock.patch.object(
        _conversations,
        "_forward_conversations_to_rust",
        return_value={"ok": True, "id": "d"},
    ):
        out = _conversations._conversations_delete_action({"id": "d"})
    assert out == {"ok": True, "id": "d"}


def test_set_active_action_passes_empty_id_through_to_clear() -> None:
    captured: Dict[str, Any] = {}

    def fake_forward(action: str, payload: Dict[str, Any]) -> Dict[str, Any]:
        captured["action"] = action
        captured["payload"] = payload
        return {"id": ""}

    with mock.patch.object(
        _conversations, "_forward_conversations_to_rust", side_effect=fake_forward
    ):
        out = _conversations._conversations_set_active_action({})
    assert out == {"id": ""}
    assert captured["action"] == "conversations.set_active"
    # A missing id forwards as "" — the Rust side reads that as "clear".
    assert captured["payload"] == {"id": ""}


def test_get_active_action_surfaces_rust_reply() -> None:
    with mock.patch.object(
        _conversations,
        "_forward_conversations_to_rust",
        return_value={"id": "c-active"},
    ):
        out = _conversations._conversations_get_active_action(None)
    assert out == {"id": "c-active"}


@pytest.mark.parametrize(
    "call",
    [
        lambda: _conversations._conversations_new_action(None),
        lambda: _conversations._conversations_list_action(None),
        lambda: _conversations._conversations_get_active_action(None),
    ],
)
def test_action_raises_harness_unavailable_when_rust_down(call: Any) -> None:
    with mock.patch.object(
        _conversations, "_forward_conversations_to_rust", return_value=None
    ):
        with pytest.raises(_conversations._ActionError) as ei:
            call()
    assert ei.value.code == "harness_unavailable"


# ── Dispatch-table registration ────────────────────────────────────────


def test_conversations_actions_registered_on_pipe() -> None:
    """Each handler — including the two net-new active-selection verbs —
    must be reachable through the ``_ACTIONS`` dispatch table; this guards
    against a silent ``action_not_found`` on the wire."""
    import importlib

    try:
        harness_pipe = importlib.import_module("Core.harness.pipe")
    except ImportError:
        harness_pipe = importlib.import_module("Wylde.Core.harness.pipe")
    for name in (
        "conversations.new",
        "conversations.list",
        "conversations.get",
        "conversations.delete",
        "conversations.get_active",
        "conversations.set_active",
    ):
        assert name in harness_pipe._ACTIONS, f"{name} missing from _ACTIONS"
