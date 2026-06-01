"""Smokes for the wake-word check / pull plumbing.

We don't load a real wake-word model — those live behind Gateway's
``/api/models/pull`` and the harness model registry. Tests assert:

* ``is_model_installed`` returns True / False based on the harness's
  ``get_model`` lookup.
* ``initiate_pull`` returns a non-empty job id.
* The pipe's ``check_wake_word_model`` action records the result on
  the in-memory state (so the GUI can read it via ``get_status``).
* The pipe's ``pull_wake_word_model`` action stashes the job id on
  state so subsequent polls can match it back.
"""

from __future__ import annotations

import sys
from pathlib import Path
from typing import Any, Generator

import pytest

_HERE = Path(__file__).resolve()
_VAULT_ROOT = _HERE.parents[3]
if str(_VAULT_ROOT) not in sys.path:
    sys.path.insert(0, str(_VAULT_ROOT))


@pytest.fixture
def voice_modules(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> Generator[dict[str, Any], None, None]:
    monkeypatch.setenv("WYLDE_VOICE_CONFIG_DIR", str(tmp_path))
    from Voice import pipe, state, wake_word

    pipe.reset_singletons()
    yield {"pipe": pipe, "state": state, "wake_word": wake_word}
    pipe.reset_singletons()


def test_is_model_installed_true(
    voice_modules: dict[str, Any], monkeypatch: pytest.MonkeyPatch
) -> None:
    ww = voice_modules["wake_word"]

    class _StubEntry:
        id = "test/model"

    # Patch the import inside is_model_installed so it returns our stub.
    import Core.harness.model_registry as registry

    monkeypatch.setattr(registry, "get_model", lambda name: _StubEntry())

    assert ww.is_model_installed("test/model") is True


def test_is_model_installed_false_when_missing(
    voice_modules: dict[str, Any], monkeypatch: pytest.MonkeyPatch
) -> None:
    ww = voice_modules["wake_word"]

    import Core.harness.model_registry as registry

    monkeypatch.setattr(registry, "get_model", lambda name: None)

    assert ww.is_model_installed("missing/model") is False


def test_is_model_installed_swallows_exceptions(
    voice_modules: dict[str, Any], monkeypatch: pytest.MonkeyPatch
) -> None:
    ww = voice_modules["wake_word"]

    def _boom(_name: str) -> None:
        raise RuntimeError("registry blew up")

    import Core.harness.model_registry as registry

    monkeypatch.setattr(registry, "get_model", _boom)

    assert ww.is_model_installed("any") is False


def test_initiate_pull_returns_job_id(voice_modules: dict[str, Any]) -> None:
    ww = voice_modules["wake_word"]
    job = ww.initiate_pull("test/model")
    assert isinstance(job, str) and job


def test_pipe_check_action_records_status(
    voice_modules: dict[str, Any], monkeypatch: pytest.MonkeyPatch
) -> None:
    pipe = voice_modules["pipe"]
    state_mod = voice_modules["state"]
    ww = voice_modules["wake_word"]

    voice_state = state_mod.VoiceState()
    pipe.install_test_doubles(state=voice_state)
    monkeypatch.setattr(ww, "is_model_installed", lambda _m: True)

    result = pipe._voice_check_wake_word_model_action({})
    assert result["installed"] is True
    assert result["model"] == state_mod.DEFAULT_WAKE_WORD_MODEL

    # State now reflects installed=True.
    snap = pipe._voice_get_status_action(None)
    assert snap["wake_word_installed"] is True


def test_pipe_check_action_uses_payload_model_when_provided(
    voice_modules: dict[str, Any],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    pipe = voice_modules["pipe"]
    state_mod = voice_modules["state"]
    ww = voice_modules["wake_word"]

    pipe.install_test_doubles(state=state_mod.VoiceState())
    seen: list[str] = []

    def _fake_is_installed(m: str) -> bool:
        seen.append(m)
        return True

    monkeypatch.setattr(ww, "is_model_installed", _fake_is_installed)

    result = pipe._voice_check_wake_word_model_action(
        {"model": "custom/wake-model"},
    )
    assert result["model"] == "custom/wake-model"
    assert seen == ["custom/wake-model"]


def test_pipe_pull_action_stashes_job_id(
    voice_modules: dict[str, Any], monkeypatch: pytest.MonkeyPatch
) -> None:
    pipe = voice_modules["pipe"]
    state_mod = voice_modules["state"]
    ww = voice_modules["wake_word"]

    voice_state = state_mod.VoiceState()
    pipe.install_test_doubles(state=voice_state)
    monkeypatch.setattr(ww, "initiate_pull", lambda _m: "job_xyz123")

    result = pipe._voice_pull_wake_word_model_action({})
    assert result["job_id"] == "job_xyz123"
    assert result["model"] == state_mod.DEFAULT_WAKE_WORD_MODEL
    assert voice_state.wake_word_pull_job == "job_xyz123"
