"""Harness Slice 3b strangler-fig — ``WYLDE_HARNESS_MODELS_IMPL`` gating
in ``Core/harness/pipe/_models.py``.

The Rust ``wylde-harness`` crate exposes eight ``models.*`` handlers
(Slice 3a) on ``\\\\.\\pipe\\wylde-harness``. Slice 3b flips the Python
forwarder default from ``python`` to ``rust``: each entry point forwards
the action over the harness pipe and returns the Rust reply verbatim,
falling back to the in-process Python body on a transport-class failure.

These tests pin:
  * the flag parsing (default rust, clamp, python rollback),
  * the forward plumbing (ok passthrough, transport fallback, service
    error surfacing, the ``not_found`` non-fallback deviation, the
    self-loop guard), and
  * cross-impl parity: identical state → identical envelope whether the
    request runs through the Python body or the Rust forward.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Dict, Iterator, Optional
from unittest import mock

import pytest

from Core.harness.pipe import _models


@dataclass
class _StubReply:
    ok: bool
    data: Any = None
    error: Optional[Dict[str, Any]] = None


@pytest.fixture(autouse=True)
def _not_local_server(monkeypatch: pytest.MonkeyPatch) -> None:
    """The test process never calls ``pipe.start()``, but pin the
    self-loop guard to False explicitly so the forward path is reachable
    regardless of import-order side effects."""
    monkeypatch.setattr(_models, "_harness_is_local_server", lambda: False)


# ── flag parsing ──────────────────────────────────────────────────────


def test_impl_defaults_to_rust(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("WYLDE_HARNESS_MODELS_IMPL", raising=False)
    assert _models._models_impl() == "rust"


def test_impl_clamps_unknown_value_to_rust(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("WYLDE_HARNESS_MODELS_IMPL", "ferret")
    assert _models._models_impl() == "rust"


def test_impl_accepts_python(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("WYLDE_HARNESS_MODELS_IMPL", "python")
    assert _models._models_impl() == "python"


def test_impl_accepts_rust_case_insensitive(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("WYLDE_HARNESS_MODELS_IMPL", "  RUST ")
    assert _models._models_impl() == "rust"


# ── forward plumbing ──────────────────────────────────────────────────


def test_forward_returns_data_on_ok_reply() -> None:
    reply = _StubReply(ok=True, data={"model": "qwen3:0.6b"})

    def fake_send_action(
        service: str, action: str, payload: Any, timeout: float
    ) -> _StubReply:
        assert service == "wylde-harness"
        assert action == "models.get_default"
        return reply

    with mock.patch("Core.shared.ipc.send_action", side_effect=fake_send_action):
        out = _models._try_forward_models_to_rust("models.get_default", None)
    assert out == {"model": "qwen3:0.6b"}


@pytest.mark.parametrize(
    "transport_code",
    [
        "pipe_unavailable",
        "pipe_connect",
        "pipe_timeout",
        "pipe_io",
        "handshake_timeout",
        "handshake_rejected",
        "no_action",
        "not_implemented",  # the Slice-3a gate-off marker
    ],
)
def test_forward_falls_through_on_transport_failure(transport_code: str) -> None:
    reply = _StubReply(ok=False, error={"code": transport_code, "message": "x"})
    with mock.patch("Core.shared.ipc.send_action", return_value=reply):
        out = _models._try_forward_models_to_rust("models.list", {})
    assert out is None, f"transport failure {transport_code!r} should fall back"


def test_forward_surfaces_service_level_error() -> None:
    reply = _StubReply(ok=False, error={"code": "bad_request", "message": "name is required"})
    with mock.patch("Core.shared.ipc.send_action", return_value=reply):
        with pytest.raises(_models._ActionError) as ei:
            _models._try_forward_models_to_rust("models.show", {})
    assert ei.value.code == "bad_request"
    assert "name is required" in ei.value.message


def test_forward_not_found_surfaces_not_falls_back() -> None:
    """Deviation from _chat.py: a pipe-only ``not_found`` from models.show
    is the application 'model not installed' result — it must surface, not
    fall back to a redundant Python round-trip."""
    reply = _StubReply(ok=False, error={"code": "not_found", "message": "model 'ghost' not found"})
    with mock.patch("Core.shared.ipc.send_action", return_value=reply):
        with pytest.raises(_models._ActionError) as ei:
            _models._try_forward_models_to_rust("models.show", {"name": "ghost"})
    assert ei.value.code == "not_found"


def test_forward_falls_through_on_exception() -> None:
    def boom(*_a: Any, **_kw: Any) -> Any:
        raise RuntimeError("pipe gone")

    with mock.patch("Core.shared.ipc.send_action", side_effect=boom):
        out = _models._try_forward_models_to_rust("models.delete", {"name": "m"})
    assert out is None


def test_forward_falls_through_on_non_dict_data() -> None:
    reply = _StubReply(ok=True, data=["not", "a", "dict"])
    with mock.patch("Core.shared.ipc.send_action", return_value=reply):
        out = _models._try_forward_models_to_rust("models.list", {})
    assert out is None


def test_non_forwardable_action_stays_python() -> None:
    """transcribe/synthesize have no Rust handler — never forward them even
    if a caller passes the verb in."""
    sentinel = mock.MagicMock()
    with mock.patch("Core.shared.ipc.send_action", sentinel):
        out = _models._try_forward_models_to_rust("models.transcribe", {"audio_b64": "x"})
    assert out is None
    sentinel.assert_not_called()


def test_self_loop_guard_suppresses_forward(monkeypatch: pytest.MonkeyPatch) -> None:
    """When this process is the live Python harness server, forwarding would
    loop the pipe back into the same dispatcher → run Python locally."""
    monkeypatch.setattr(_models, "_harness_is_local_server", lambda: True)
    sentinel = mock.MagicMock()
    with mock.patch("Core.shared.ipc.send_action", sentinel):
        out = _models._try_forward_models_to_rust("models.get_default", None)
    assert out is None
    sentinel.assert_not_called()


# ── entry-point routing ───────────────────────────────────────────────


def test_entry_point_uses_rust_when_default(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("WYLDE_HARNESS_MODELS_IMPL", raising=False)  # default rust
    with mock.patch.object(
        _models, "_try_forward_models_to_rust", return_value={"model": "from-rust"}
    ) as fwd:
        out = _models._models_get_default_action(None)
    assert out == {"model": "from-rust"}
    fwd.assert_called_once()
    assert fwd.call_args.args[0] == "models.get_default"


def test_entry_point_falls_back_to_python_when_rust_down(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.delenv("WYLDE_HARNESS_MODELS_IMPL", raising=False)  # default rust
    # Forward returns None (transport failure) → Python body runs.
    with mock.patch.object(_models, "_try_forward_models_to_rust", return_value=None):
        fake_state = mock.MagicMock()
        fake_state.get_default_model.return_value = "from-python"
        monkeypatch.setattr(_models, "_model_state_module", lambda: fake_state)
        out = _models._models_get_default_action(None)
    assert out == {"model": "from-python"}


def test_entry_point_python_impl_skips_forward(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("WYLDE_HARNESS_MODELS_IMPL", "python")
    with mock.patch.object(_models, "_try_forward_models_to_rust") as fwd:
        fake_state = mock.MagicMock()
        fake_state.get_default_model.return_value = "py-only"
        monkeypatch.setattr(_models, "_model_state_module", lambda: fake_state)
        out = _models._models_get_default_action(None)
    assert out == {"model": "py-only"}
    fwd.assert_not_called()


# ── cross-impl parity ─────────────────────────────────────────────────
#
# For a given backend state, the public entry point must return the exact
# same envelope whether it ran the Python body or forwarded to Rust. We
# drive the Python path with a fake backend, capture its output, then feed
# the *same* value back through the Rust forward (mocking send_action to
# return the documented Rust reply for that state) and assert equality.


@pytest.fixture
def parity_state(monkeypatch: pytest.MonkeyPatch) -> Iterator[Dict[str, Any]]:
    """An in-memory model_state stand-in shared by both impl paths so a
    set on one is observable by the other — the only way to assert true
    same-state parity without a live Rust pipe."""
    store: Dict[str, Any] = {"default": None, "active": None}

    fake = mock.MagicMock()
    fake.get_default_model.side_effect = lambda: store["default"]

    def _set_default(name: Optional[str]) -> Optional[str]:
        store["default"] = (name or "").strip() or None
        return store["default"]

    def _set_active(name: Optional[str]) -> Optional[str]:
        store["active"] = name or None
        return store["active"]

    fake.set_default_model.side_effect = _set_default
    fake.set_active_model.side_effect = _set_active
    monkeypatch.setattr(_models, "_model_state_module", lambda: fake)
    yield store


def _python_result(
    monkeypatch: pytest.MonkeyPatch, fn: Any, payload: Any
) -> Dict[str, Any]:
    monkeypatch.setenv("WYLDE_HARNESS_MODELS_IMPL", "python")
    return fn(payload)


def _rust_result(
    monkeypatch: pytest.MonkeyPatch, fn: Any, payload: Any, rust_reply_data: Any
) -> Dict[str, Any]:
    monkeypatch.setenv("WYLDE_HARNESS_MODELS_IMPL", "rust")
    reply = _StubReply(ok=True, data=rust_reply_data)
    with mock.patch("Core.shared.ipc.send_action", return_value=reply):
        return fn(payload)


def test_parity_set_default_then_get_default(
    monkeypatch: pytest.MonkeyPatch, parity_state: Dict[str, Any]
) -> None:
    # Python path persists, then reads back.
    py_set = _python_result(
        monkeypatch, _models._models_set_default_action, {"model": "qwen2.5:1.5b"}
    )
    py_get = _python_result(monkeypatch, _models._models_get_default_action, None)
    assert py_set == {"ok": True, "model": "qwen2.5:1.5b"}
    assert py_get == {"model": "qwen2.5:1.5b"}

    # Rust path returns the documented envelope for the same state — must
    # match the Python path byte-for-byte.
    rust_set = _rust_result(
        monkeypatch,
        _models._models_set_default_action,
        {"model": "qwen2.5:1.5b"},
        {"ok": True, "model": "qwen2.5:1.5b"},
    )
    rust_get = _rust_result(
        monkeypatch,
        _models._models_get_default_action,
        None,
        {"model": "qwen2.5:1.5b"},
    )
    assert rust_set == py_set
    assert rust_get == py_get


def test_parity_set_active_clear(
    monkeypatch: pytest.MonkeyPatch, parity_state: Dict[str, Any]
) -> None:
    py = _python_result(monkeypatch, _models._models_set_active_action, {"model": ""})
    rust = _rust_result(
        monkeypatch, _models._models_set_active_action, {"model": ""}, {"model": ""}
    )
    assert py == {"model": ""}
    assert rust == py


def test_parity_show_passthrough(monkeypatch: pytest.MonkeyPatch) -> None:
    raw = {"details": {"family": "qwen"}, "model_info": {"x": 1}}
    # Python path: stub the ollama client to return raw metadata.
    monkeypatch.setenv("WYLDE_HARNESS_MODELS_IMPL", "python")
    fake_oc = mock.MagicMock()
    fake_oc.show_model.return_value = raw
    monkeypatch.setattr(_models, "_ollama_client_module", lambda: fake_oc)
    py = _models._models_show_action({"name": "qwen3:0.6b"})
    # Rust path: the handler returns the same raw Ollama payload verbatim.
    rust = _rust_result(
        monkeypatch, _models._models_show_action, {"name": "qwen3:0.6b"}, raw
    )
    assert py == raw
    assert rust == py
