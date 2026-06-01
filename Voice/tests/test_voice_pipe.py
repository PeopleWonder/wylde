"""Smokes for the voice pipe action handlers.

Goes after the registered ``voice.*`` actions directly — we don't spin
up a real pipe server. The action handlers are pure functions over a
:class:`VoiceState`, an :class:`AudioCaptureProtocol`, an
:class:`AudioPlaybackProtocol`, and a :class:`HarnessClientProtocol`,
so test doubles work cleanly.

Tests asserted:

* ``voice.toggle`` runs the orchestrator end-to-end against a fake
  harness and returns the session's transcript + response.
* ``voice.set_mode`` / ``voice.get_mode`` round-trip through the
  persistent :class:`VoiceConfig` (uses a tmpdir).
* ``voice.set_active_conversation`` mirrors the GUI's id.
* ``voice.get_status`` reflects state transitions.
* ``voice.subscribe_status`` returns events emitted during a session.
* ``voice.toggle`` errors with ``no_conversation`` when nothing's
  active and the conversation store is empty.
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
    """Load the Voice package modules with a tmpdir-backed config so
    ``set_mode`` doesn't write to the user's real ~/.wylde dir."""
    monkeypatch.setenv("WYLDE_VOICE_CONFIG_DIR", str(tmp_path))
    from Voice import audio_io, orchestrator, pipe, state, wake_word

    # Force-reset module-level singletons between tests; the pipe module
    # caches them lazily on first use.
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
    """Test double for HarnessClientProtocol."""

    def __init__(
        self, *, transcript: str = "hello", response: str = "hi there"
    ) -> None:
        self.transcript = transcript
        self.response = response
        self.calls: list[tuple[str, dict]] = []
        self.tts_audio = b"\x00\x00\x00\x00" * 240  # 240 samples float32

    def transcribe(self, audio: bytes, *, sample_rate: int = 16000) -> str:
        self.calls.append(
            ("transcribe", {"bytes": len(audio), "sample_rate": sample_rate})
        )
        return self.transcript

    def run_chat_turn(
        self, *, user_message: str, conversation_id: str, model: Optional[str] = None
    ) -> Dict[str, Any]:
        self.calls.append(
            (
                "run_chat_turn",
                {
                    "user_message": user_message,
                    "conversation_id": conversation_id,
                    "model": model,
                },
            )
        )
        return {
            "turn_id": "turn_x",
            "conversation_id": conversation_id,
            "final_message": self.response,
            "tool_calls_summary": [],
            "aborted": False,
            "abort_reason": None,
        }

    def synthesize(self, text: str) -> Dict[str, Any]:
        import base64

        self.calls.append(("synthesize", {"chars": len(text)}))
        return {
            "audio_b64": base64.b64encode(self.tts_audio).decode("ascii"),
            "sample_rate": 24000,
            "format": "float32_pcm",
            "voice": "",
        }


def _install(
    voice_modules: dict[str, Any], *, harness: _FakeHarness, conv_id: str = "conv_1"
) -> tuple[Any, Any, Any]:
    pipe = voice_modules["pipe"]
    audio_io = voice_modules["audio_io"]
    state_mod = voice_modules["state"]

    voice_state = state_mod.VoiceState()
    voice_state.set_active_conversation(conv_id)
    capture = audio_io.FakeCapture(payload=b"\x00\x00" * 1600)
    playback = audio_io.FakePlayback()

    pipe.install_test_doubles(
        state=voice_state,
        capture=capture,
        playback=playback,
        harness=harness,
    )
    return voice_state, capture, playback


def test_toggle_runs_full_round_trip(voice_modules: dict[str, Any]) -> None:
    pipe = voice_modules["pipe"]
    harness = _FakeHarness(
        transcript="please remind me at 5pm", response="reminder set"
    )
    voice_state, capture, playback = _install(voice_modules, harness=harness)

    result = pipe._voice_toggle_action({})

    assert result["transcript"] == "please remind me at 5pm"
    assert result["response"] == "reminder set"
    assert result["error"] is None
    assert result["conversation_id"] == "conv_1"
    assert capture.called_with_max_seconds == 30.0
    # Harness saw all three calls.
    assert [name for name, _ in harness.calls] == [
        "transcribe",
        "run_chat_turn",
        "synthesize",
    ]
    # Playback received the float32 PCM.
    assert len(playback.calls) == 1
    assert playback.calls[0]["sample_rate"] == 24000


def test_toggle_with_empty_transcript_returns_error(
    voice_modules: dict[str, Any],
) -> None:
    pipe = voice_modules["pipe"]
    harness = _FakeHarness(transcript="   ")
    _install(voice_modules, harness=harness)

    result = pipe._voice_toggle_action({})

    assert result["transcript"] == "   "
    assert result["error"] == "empty_transcript"
    # Chat / TTS not invoked.
    assert [name for name, _ in harness.calls] == ["transcribe"]


def test_toggle_no_conversation_errors_cleanly(
    voice_modules: dict[str, Any], monkeypatch: pytest.MonkeyPatch
) -> None:
    pipe = voice_modules["pipe"]
    state_mod = voice_modules["state"]
    audio_io = voice_modules["audio_io"]

    # No GUI conversation pushed; conversation store empty.
    voice_state = state_mod.VoiceState()
    capture = audio_io.FakeCapture()
    playback = audio_io.FakePlayback()
    harness = _FakeHarness()

    pipe.install_test_doubles(
        state=voice_state,
        capture=capture,
        playback=playback,
        harness=harness,
    )

    # Stub the orchestrator's resolver to simulate empty store.
    from Voice import orchestrator

    monkeypatch.setattr(orchestrator, "_resolve_conversation", lambda _id: None)

    result = pipe._voice_toggle_action({})
    assert result["error"] == "no_conversation"
    assert result["transcript"] == ""
    assert harness.calls == []  # Capture never even ran.


def test_set_mode_persists(voice_modules: dict[str, Any]) -> None:
    pipe = voice_modules["pipe"]
    state_mod = voice_modules["state"]

    pipe.install_test_doubles(state=state_mod.VoiceState())

    result = pipe._voice_set_mode_action({"mode": state_mod.MODE_ALWAYS_ON})
    assert result == {"mode": state_mod.MODE_ALWAYS_ON}

    assert pipe._voice_get_mode_action(None) == {
        "mode": state_mod.MODE_ALWAYS_ON,
    }


def test_set_mode_rejects_unknown(voice_modules: dict[str, Any]) -> None:
    pipe = voice_modules["pipe"]
    state_mod = voice_modules["state"]
    pipe.install_test_doubles(state=state_mod.VoiceState())

    with pytest.raises(pipe._ActionError) as exc:
        pipe._voice_set_mode_action({"mode": "nonsense"})
    assert exc.value.code == "bad_request"


def test_status_reflects_active_session(voice_modules: dict[str, Any]) -> None:
    pipe = voice_modules["pipe"]
    state_mod = voice_modules["state"]
    pipe.install_test_doubles(state=state_mod.VoiceState())

    snap = pipe._voice_get_status_action(None)
    assert snap["state"] == state_mod.STATE_IDLE
    assert snap["active_session"] is None


def test_subscribe_status_returns_emitted_events(voice_modules: dict[str, Any]) -> None:
    pipe = voice_modules["pipe"]
    state_mod = voice_modules["state"]
    voice_state = state_mod.VoiceState()
    pipe.install_test_doubles(state=voice_state)

    voice_state.set_state(state_mod.STATE_LISTENING)
    voice_state.set_state(state_mod.STATE_IDLE)

    poll = pipe._voice_subscribe_status_action({"cursor": 0, "max_wait_ms": 1})
    types = [e["type"] for e in poll["events"]]
    assert "state" in types
    assert poll["next_cursor"] == len(poll["events"])


def test_set_active_conversation_mirror(voice_modules: dict[str, Any]) -> None:
    pipe = voice_modules["pipe"]
    state_mod = voice_modules["state"]
    pipe.install_test_doubles(state=state_mod.VoiceState())

    result = pipe._voice_set_active_conversation_action(
        {
            "conversation_id": "conv_42",
        }
    )
    assert result == {"conversation_id": "conv_42"}

    snap = pipe._voice_get_status_action(None)
    assert snap["active_conversation_id"] == "conv_42"
