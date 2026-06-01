"""Active-conversation mirroring + cold-start fallback.

Voice is not authoritative for which conversation a session belongs
to. The GUI pushes the active id; if Voice doesn't have one (cold
start), the orchestrator falls back to the most recent conversation
in the store. If the store is empty, the session ends with a
``no_conversation`` error.

Tests:

* Pushing an id via ``set_active_conversation`` is reflected in the
  next session's ``conversation_id``.
* No GUI push + non-empty store → most recent is used.
* No GUI push + empty store → ``no_conversation`` error.
* ``set_active_conversation`` with empty string clears the mirror
  (so cold-start fallback fires again).
"""

from __future__ import annotations

import sys
from pathlib import Path
from typing import Any, Dict, Generator, Optional

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
    from Voice import audio_io, orchestrator, pipe, state, wake_word

    pipe.reset_singletons()
    yield {
        "audio_io": audio_io,
        "orchestrator": orchestrator,
        "pipe": pipe,
        "state": state,
        "wake_word": wake_word,
    }
    pipe.reset_singletons()


class _FakeHarness:
    def __init__(self) -> None:
        self.last_conversation_id: Optional[str] = None

    def transcribe(self, audio: bytes, *, sample_rate: int = 16000) -> str:
        return "test message"

    def run_chat_turn(
        self, *, user_message: str, conversation_id: str, model: Optional[str] = None
    ) -> Dict[str, Any]:
        self.last_conversation_id = conversation_id
        return {
            "turn_id": "turn_x",
            "conversation_id": conversation_id,
            "final_message": "ack",
            "tool_calls_summary": [],
            "aborted": False,
            "abort_reason": None,
        }

    def synthesize(self, text: str) -> Dict[str, Any]:
        import base64

        return {
            "audio_b64": base64.b64encode(b"\x00" * 16).decode("ascii"),
            "sample_rate": 24000,
            "format": "float32_pcm",
            "voice": "",
        }


def _install(voice_modules: dict[str, Any], *, conv_id: str = "") -> _FakeHarness:
    pipe = voice_modules["pipe"]
    state_mod = voice_modules["state"]
    audio_io = voice_modules["audio_io"]

    voice_state = state_mod.VoiceState()
    if conv_id:
        voice_state.set_active_conversation(conv_id)
    capture = audio_io.FakeCapture()
    playback = audio_io.FakePlayback()
    harness = _FakeHarness()

    pipe.install_test_doubles(
        state=voice_state,
        capture=capture,
        playback=playback,
        harness=harness,
    )
    return harness


def test_explicit_active_id_is_used(voice_modules: dict[str, Any]) -> None:
    pipe = voice_modules["pipe"]
    harness = _install(voice_modules, conv_id="conv_explicit")

    result = pipe._voice_toggle_action({})

    assert result["error"] is None
    assert result["conversation_id"] == "conv_explicit"
    assert harness.last_conversation_id == "conv_explicit"


def test_cold_start_falls_back_to_most_recent(
    voice_modules: dict[str, Any], monkeypatch: pytest.MonkeyPatch
) -> None:
    pipe = voice_modules["pipe"]
    orchestrator = voice_modules["orchestrator"]
    harness = _install(voice_modules)  # No active id pushed.

    # Patch the orchestrator's resolver directly — it's the cleanest
    # seam (the real implementation imports conversation lazily).
    monkeypatch.setattr(
        orchestrator,
        "_resolve_conversation",
        lambda active_id: active_id or "conv_most_recent",
    )

    result = pipe._voice_toggle_action({})
    assert result["error"] is None
    assert result["conversation_id"] == "conv_most_recent"
    assert harness.last_conversation_id == "conv_most_recent"


def test_cold_start_empty_store_errors(
    voice_modules: dict[str, Any], monkeypatch: pytest.MonkeyPatch
) -> None:
    pipe = voice_modules["pipe"]
    orchestrator = voice_modules["orchestrator"]
    harness = _install(voice_modules)

    monkeypatch.setattr(orchestrator, "_resolve_conversation", lambda _id: None)

    result = pipe._voice_toggle_action({})
    assert result["error"] == "no_conversation"
    assert harness.last_conversation_id is None


def test_clear_active_id_falls_back_to_resolver(
    voice_modules: dict[str, Any], monkeypatch: pytest.MonkeyPatch
) -> None:
    pipe = voice_modules["pipe"]
    orchestrator = voice_modules["orchestrator"]

    harness = _install(voice_modules, conv_id="conv_initial")

    # Clear the mirror via the pipe action — empty string means "no
    # active conversation pushed."
    pipe._voice_set_active_conversation_action({"conversation_id": ""})

    monkeypatch.setattr(
        orchestrator, "_resolve_conversation", lambda active: active or "conv_recent"
    )

    result = pipe._voice_toggle_action({})
    assert result["conversation_id"] == "conv_recent"
    assert harness.last_conversation_id == "conv_recent"


def test_resolve_conversation_prefers_explicit_id(
    voice_modules: dict[str, Any],
) -> None:
    """Whitebox: ``_resolve_conversation`` returns the GUI id verbatim
    even if a most-recent record would otherwise apply."""
    orch = voice_modules["orchestrator"]
    assert orch._resolve_conversation("conv_x") == "conv_x"
