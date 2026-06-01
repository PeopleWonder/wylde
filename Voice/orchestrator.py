r"""Per-session orchestration: capture → STT → chat → TTS → play.

The Voice service's central flow. One ``run_session`` call drives a
complete round-trip: pull audio off the mic, send it to the harness
for transcription, post the transcript as a chat turn, wait for the
final assistant text, send that to the harness for synthesis, play
the resulting audio.

The harness is reached via :class:`HarnessClient` — a thin wrapper
over ``\\.\pipe\wylde-harness`` action calls. The harness owns STT,
TTS, and the chat-turn loop; Voice never imports those engines
directly.

Conversation handling per the Wylde user's spec:

* If the GUI has called ``voice.set_active_conversation(<id>)``,
  ``run_session`` uses that id.
* If no conversation has been pushed (cold start), we fall back to
  the most recently updated conversation from the conversations
  store. Picked because it's the simpler choice — the user's most
  likely target is whatever they had open last.
* Voice never creates a conversation. If the store is empty the
  session ends with ``no_conversation`` error and the GUI is
  expected to create one before retrying.
"""

from __future__ import annotations

import base64
import logging
import time
from dataclasses import dataclass, field
from typing import Any, Dict, Optional, Protocol

from . import audio_io as _audio
from . import state as _state

logger = logging.getLogger("wylde.voice.orchestrator")


# ── Harness client surface ─────────────────────────────────────────────


class HarnessClientProtocol(Protocol):
    """Whatever object the orchestrator uses to talk to the harness.
    Both the production pipe-backed client and test fakes implement
    this."""

    def transcribe(self, audio: bytes, *, sample_rate: int = 16000) -> str: ...
    def run_chat_turn(
        self,
        *,
        user_message: str,
        conversation_id: str,
        model: Optional[str] = None,
    ) -> Dict[str, Any]: ...
    def synthesize(self, text: str) -> Dict[str, Any]: ...


class HarnessPipeClient:
    """Production client. Calls the harness pipe via ``Core.shared.ipc``
    — same shared transport every other Wylde IPC consumer uses. Lazy-
    imports so a test environment without msgpack/pywin32 doesn't
    break module load."""

    SERVICE = "wylde-harness"

    def transcribe(self, audio: bytes, *, sample_rate: int = 16000) -> str:
        reply = self._call(
            "models.transcribe",
            {
                "audio_b64": base64.b64encode(audio).decode("ascii"),
                "sample_rate": sample_rate,
                "sample_dtype": "int16",
            },
        )
        return str(reply.get("text") or "")

    def run_chat_turn(
        self,
        *,
        user_message: str,
        conversation_id: str,
        model: Optional[str] = None,
    ) -> Dict[str, Any]:
        return self._call(
            "chat.run_turn",
            {
                "user_message": user_message,
                "conversation_id": conversation_id,
                "model": model,
                "modality": "voice",
            },
        )

    def synthesize(self, text: str) -> Dict[str, Any]:
        return self._call("models.synthesize", {"text": text})

    def _call(self, action: str, payload: Dict[str, Any]) -> Dict[str, Any]:
        from Core.shared import ipc

        reply = ipc.send(
            self.SERVICE,
            "/__action__",
            data={"action": action, "payload": payload},
            http_verb="POST",
            timeout=120.0,
        )
        if not getattr(reply, "ok", False):
            err = getattr(reply, "error", None) or {}
            raise RuntimeError(f"harness {action} failed: {err.get('message', err)}")
        return getattr(reply, "data", None) or {}


# ── Result shape ───────────────────────────────────────────────────────


@dataclass
class SessionResult:
    session_id: str
    conversation_id: str
    transcript: str = ""
    response: str = ""
    aborted: bool = False
    error: Optional[str] = None
    timings_ms: Dict[str, int] = field(default_factory=dict)

    def to_dict(self) -> Dict[str, Any]:
        return {
            "session_id": self.session_id,
            "conversation_id": self.conversation_id,
            "transcript": self.transcript,
            "response": self.response,
            "aborted": self.aborted,
            "error": self.error,
            "timings_ms": dict(self.timings_ms),
        }


# ── Conversation fallback ──────────────────────────────────────────────


def _resolve_conversation(active_id: str) -> Optional[str]:
    """Pick the conversation to bind this session to.

    Priority:
      1. Whatever ``voice.set_active_conversation`` last said.
      2. Most recently updated conversation from the store.
      3. None — caller surfaces a ``no_conversation`` error.

    the Wylde user's design said either fall back to most-recent OR create
    a "voice" labeled conversation; we pick most-recent because
    Voice "never creates conversations" elsewhere in the spec.
    """
    if isinstance(active_id, str) and active_id:
        return active_id
    try:
        from Core.harness.memory import conversation as _conv
    except ImportError:
        return None
    try:
        metas = _conv.list_conversations()
    except Exception:  # noqa: BLE001
        return None
    if not metas:
        return None
    return metas[0].get("id") or None


# ── Main flow ──────────────────────────────────────────────────────────


def run_session(
    state: _state.VoiceState,
    *,
    capture: _audio.AudioCaptureProtocol,
    playback: _audio.AudioPlaybackProtocol,
    harness: HarnessClientProtocol,
    max_capture_seconds: float = 30.0,
    model: Optional[str] = None,
) -> SessionResult:
    """Drive one capture → STT → chat → TTS → play round-trip.

    Mutates ``state`` to reflect each phase (LISTENING → PROCESSING
    → PLAYING → IDLE) so subscribers on ``voice.subscribe_status``
    see a live feed. On any error the session ends cleanly with the
    error recorded; the audio cap chain is best-effort and never
    raises out of this function.
    """
    conv_id = _resolve_conversation(state.active_conversation_id)
    if not conv_id:
        sess = state.begin_session(conversation_id="")
        state.end_session(error="no_conversation")
        return SessionResult(
            session_id=sess.id,
            conversation_id="",
            error="no_conversation",
        )

    sess = state.begin_session(conversation_id=conv_id)
    timings: Dict[str, int] = {}
    transcript = ""
    response = ""
    error: Optional[str] = None

    try:
        # Capture.
        t0 = time.monotonic()
        try:
            audio_bytes = capture.capture(max_seconds=max_capture_seconds)
        except _audio.AudioUnavailable as exc:
            error = f"audio_unavailable: {exc}"
            return _finalize(state, sess, transcript, response, error, timings)
        timings["capture_ms"] = int((time.monotonic() - t0) * 1000)

        if not audio_bytes:
            error = "no_audio_captured"
            return _finalize(state, sess, transcript, response, error, timings)

        # STT.
        state.set_state(_state.STATE_PROCESSING)
        t0 = time.monotonic()
        try:
            transcript = harness.transcribe(
                audio_bytes,
                sample_rate=capture.sample_rate,
            )
        except Exception as exc:  # noqa: BLE001
            error = f"transcribe_failed: {exc}"
            return _finalize(state, sess, transcript, response, error, timings)
        timings["transcribe_ms"] = int((time.monotonic() - t0) * 1000)
        if not transcript.strip():
            error = "empty_transcript"
            return _finalize(state, sess, transcript, response, error, timings)

        # Chat turn through the harness — always with modality="voice"
        # so the slot-ordering builder folds in the voice prelude.
        t0 = time.monotonic()
        try:
            chat_result = harness.run_chat_turn(
                user_message=transcript,
                conversation_id=conv_id,
                model=model,
            )
        except Exception as exc:  # noqa: BLE001
            error = f"chat_failed: {exc}"
            return _finalize(state, sess, transcript, response, error, timings)
        timings["chat_ms"] = int((time.monotonic() - t0) * 1000)
        response = str(chat_result.get("final_message") or "")
        if chat_result.get("aborted"):
            error = f"chat_aborted: {chat_result.get('abort_reason') or 'unknown'}"
            return _finalize(state, sess, transcript, response, error, timings)
        if not response.strip():
            error = "empty_response"
            return _finalize(state, sess, transcript, response, error, timings)

        # TTS.
        t0 = time.monotonic()
        try:
            tts = harness.synthesize(response)
        except Exception as exc:  # noqa: BLE001
            error = f"synthesize_failed: {exc}"
            return _finalize(state, sess, transcript, response, error, timings)
        timings["synthesize_ms"] = int((time.monotonic() - t0) * 1000)

        audio_b64 = tts.get("audio_b64") or ""
        sample_rate = int(tts.get("sample_rate") or 24000)
        if not audio_b64:
            error = "tts_returned_no_audio"
            return _finalize(state, sess, transcript, response, error, timings)

        # Playback.
        state.set_state(_state.STATE_PLAYING)
        t0 = time.monotonic()
        try:
            audio_pcm = base64.b64decode(audio_b64)
            playback.play(audio_pcm, sample_rate=sample_rate)
        except _audio.AudioUnavailable as exc:
            # Don't error out the session — the chat turn LANDED, the
            # transcript and response are real; we just couldn't speak
            # the response. Caller can still see it in the conversation.
            error = f"playback_unavailable: {exc}"
            return _finalize(state, sess, transcript, response, error, timings)
        except Exception as exc:  # noqa: BLE001
            error = f"playback_failed: {exc}"
            return _finalize(state, sess, transcript, response, error, timings)
        timings["playback_ms"] = int((time.monotonic() - t0) * 1000)

        return _finalize(state, sess, transcript, response, None, timings)

    finally:
        # Belt and braces: if anything raised past the explicit returns
        # above, make sure the state lands back at IDLE rather than a
        # stuck intermediate.
        if state.state in (
            _state.STATE_LISTENING,
            _state.STATE_PROCESSING,
            _state.STATE_PLAYING,
        ):
            state.set_state(_state.STATE_IDLE)


def _finalize(
    state: _state.VoiceState,
    sess: _state.Session,
    transcript: str,
    response: str,
    error: Optional[str],
    timings: Dict[str, int],
) -> SessionResult:
    state.end_session(transcript=transcript, response=response, error=error)
    return SessionResult(
        session_id=sess.id,
        conversation_id=sess.conversation_id,
        transcript=transcript,
        response=response,
        error=error,
        timings_ms=timings,
    )


__all__ = [
    "HarnessClientProtocol",
    "HarnessPipeClient",
    "SessionResult",
    "run_session",
]
