"""Default-model selection — ``models.get_default`` / ``models.set_default``.

Covers the persisted preference layer (``model_state.{get,set}_default_model``)
and the two pipe handlers that wrap it. The default is distinct from the
*active* model: it's the user's starred "start new chats with this"
choice, persisted to its own JSON and falling back to the
``WYLDE_DEFAULT_MODEL`` env when unset.

Each test isolates persistence by pointing the module's ``_DEFAULT_PATH``
at a tmp file and resetting the in-process cache, so the suite never
touches the real ``data/default_model.json``.
"""

from __future__ import annotations

from pathlib import Path
from typing import Iterator

import pytest

from Core.harness.backend import model_state
from Core.harness.pipe._models import (
    _models_get_default_action,
    _models_set_default_action,
)


@pytest.fixture(autouse=True)
def _pin_python_impl(monkeypatch: pytest.MonkeyPatch) -> None:
    """Pin the impl flag to ``python`` so these tests exercise the retained
    in-process Python bodies (the persisted-default storage layer). The
    forward/gate path is covered by ``test_models_strangler.py``."""
    monkeypatch.setenv("WYLDE_HARNESS_MODELS_IMPL", "python")


@pytest.fixture
def isolated_default(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Iterator[Path]:
    """Repoint the default-model store at a tmp file and clear the cache.

    Restores the module globals on teardown so other harness tests that
    import ``model_state`` see a clean slate."""
    path = tmp_path / "default_model.json"
    monkeypatch.setattr(model_state, "_DEFAULT_PATH", path, raising=True)
    monkeypatch.setattr(model_state, "_default_cached", None, raising=True)
    monkeypatch.setattr(model_state, "_default_loaded", False, raising=True)
    monkeypatch.delenv("WYLDE_DEFAULT_MODEL", raising=False)
    yield path


def test_get_default_is_none_when_unset(isolated_default: Path) -> None:
    assert model_state.get_default_model() is None
    assert _models_get_default_action(None) == {"model": None}


def test_set_default_persists_and_reads_back(isolated_default: Path) -> None:
    out = _models_set_default_action({"model": "qwen2.5:1.5b"})
    assert out == {"ok": True, "model": "qwen2.5:1.5b"}
    # Survives a cache reset (i.e. it really hit disk).
    model_state._default_cached = None
    model_state._default_loaded = False
    assert _models_get_default_action(None) == {"model": "qwen2.5:1.5b"}
    assert isolated_default.exists()


def test_set_default_clears_on_null(isolated_default: Path) -> None:
    _models_set_default_action({"model": "llama3"})
    cleared = _models_set_default_action({"model": None})
    assert cleared == {"ok": True, "model": None}
    assert _models_get_default_action(None) == {"model": None}


def test_set_default_clears_on_empty_string(isolated_default: Path) -> None:
    _models_set_default_action({"model": "llama3"})
    cleared = _models_set_default_action({"model": "   "})
    assert cleared == {"ok": True, "model": None}
    assert _models_get_default_action(None) == {"model": None}


def test_get_default_falls_back_to_env(
    isolated_default: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # No persisted choice → the WYLDE_DEFAULT_MODEL env is honoured.
    monkeypatch.setenv("WYLDE_DEFAULT_MODEL", "phi4:latest")
    assert model_state.get_default_model() == "phi4:latest"
    assert _models_get_default_action(None) == {"model": "phi4:latest"}


def test_persisted_choice_overrides_env(
    isolated_default: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("WYLDE_DEFAULT_MODEL", "phi4:latest")
    _models_set_default_action({"model": "qwen2.5:1.5b"})
    # The user's explicit star wins over the deployment env default.
    assert _models_get_default_action(None) == {"model": "qwen2.5:1.5b"}


def test_set_default_rejects_non_string(isolated_default: Path) -> None:
    from Core.harness.pipe._common import _ActionError

    with pytest.raises(_ActionError) as exc:
        _models_set_default_action({"model": 123})
    assert exc.value.code == "bad_request"
