"""Smoke for the ``models.show / delete / unload / set_active`` pipe actions.

Each action is a thin wrapper around an existing backend helper. We
monkey-patch the helpers so the tests don't need a live Ollama daemon —
the contract under test is "the pipe action calls into the right
backend with the right shape and surfaces the right envelope".
"""

from __future__ import annotations

import importlib

from typing import Any

import sys
from pathlib import Path

import pytest

_HERE = Path(__file__).resolve()
_VAULT_ROOT = _HERE.parents[4]
if str(_VAULT_ROOT) not in sys.path:
    sys.path.insert(0, str(_VAULT_ROOT))


@pytest.fixture
def harness_pipe() -> Any:
    try:
        harness_pipe = importlib.import_module("Core.harness.pipe")
    except ImportError:  # pragma: no cover
        harness_pipe = importlib.import_module("Wylde.Core.harness.pipe")
    return harness_pipe


def test_models_actions_registered(harness_pipe: Any) -> None:
    for name in (
        "models.show",
        "models.delete",
        "models.unload",
        "models.set_active",
    ):
        assert name in harness_pipe._ACTIONS, f"{name} missing from _ACTIONS"


def test_models_show_passes_through(
    harness_pipe: Any, monkeypatch: pytest.MonkeyPatch
) -> Any:
    captured = {}

    def _fake_show(name: Any) -> Any:
        captured["name"] = name
        return {"details": {"family": "qwen"}, "model_info": {}}

    monkeypatch.setattr(harness_pipe._ollama_client_module(), "show_model", _fake_show)
    resp = harness_pipe._models_show_action({"name": "qwen3:0.6b"})
    assert captured["name"] == "qwen3:0.6b"
    assert resp == {"details": {"family": "qwen"}, "model_info": {}}


def test_models_show_missing_raises_not_found(
    harness_pipe: Any, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(
        harness_pipe._ollama_client_module(), "show_model", lambda _name: None
    )
    with pytest.raises(Exception) as exc_info:
        harness_pipe._models_show_action({"name": "ghost:1b"})
    assert getattr(exc_info.value, "code", None) == "not_found"


def test_models_show_requires_name(harness_pipe: Any) -> None:
    with pytest.raises(Exception) as exc_info:
        harness_pipe._models_show_action({})
    assert getattr(exc_info.value, "code", None) == "bad_request"


def test_models_delete_clears_capability_cache(
    harness_pipe: Any, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(
        harness_pipe._ollama_client_module(), "delete_model", lambda _name: True
    )
    forget_called: list[str] = []
    monkeypatch.setattr(
        harness_pipe._model_state_module(),
        "forget_model",
        lambda name: forget_called.append(name),
    )
    resp = harness_pipe._models_delete_action({"name": "qwen3:0.6b"})
    assert resp == {"ok": True, "name": "qwen3:0.6b"}
    assert forget_called == ["qwen3:0.6b"]


def test_models_delete_failure_does_not_clear_cache(
    harness_pipe: Any, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(
        harness_pipe._ollama_client_module(), "delete_model", lambda _name: False
    )
    forget_called: list[str] = []
    monkeypatch.setattr(
        harness_pipe._model_state_module(),
        "forget_model",
        lambda name: forget_called.append(name),
    )
    resp = harness_pipe._models_delete_action({"name": "qwen3:0.6b"})
    assert resp == {"ok": False, "name": "qwen3:0.6b"}
    assert forget_called == []


def test_models_unload_passes_through(
    harness_pipe: Any, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(
        harness_pipe._ollama_client_module(), "unload_model", lambda _name: True
    )
    resp = harness_pipe._models_unload_action({"name": "qwen3:0.6b"})
    assert resp == {"ok": True, "name": "qwen3:0.6b"}


def test_models_set_active_persists(
    harness_pipe: Any, monkeypatch: pytest.MonkeyPatch
) -> Any:
    seen = {}

    def _fake_set(name: Any) -> Any:
        seen["name"] = name
        return name or None

    monkeypatch.setattr(
        harness_pipe._model_state_module(), "set_active_model", _fake_set
    )
    resp = harness_pipe._models_set_active_action({"model": "qwen3:0.6b"})
    assert resp == {"model": "qwen3:0.6b"}
    assert seen["name"] == "qwen3:0.6b"


def test_models_set_active_empty_string_clears(
    harness_pipe: Any, monkeypatch: pytest.MonkeyPatch
) -> Any:
    seen = {}

    def _fake_set(name: Any) -> Any:
        seen["name"] = name
        return None

    monkeypatch.setattr(
        harness_pipe._model_state_module(), "set_active_model", _fake_set
    )
    resp = harness_pipe._models_set_active_action({"model": ""})
    assert resp == {"model": ""}
    assert seen["name"] == ""


def test_models_set_active_rejects_non_string(harness_pipe: Any) -> None:
    with pytest.raises(Exception) as exc_info:
        harness_pipe._models_set_active_action({"model": 42})
    assert getattr(exc_info.value, "code", None) == "bad_request"
