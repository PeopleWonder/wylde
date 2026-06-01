"""Voice service pipe — ``\\\\.\\pipe\\wylde-voice``.

Ten ``voice.*`` actions backed by the in-process :class:`VoiceState` and
the :func:`run_session` orchestrator. The pipe is the only way callers
(GUI, tray, hotkey daemon, tests) reach the voice service — same
contract every other Wylde service follows.

Action surface (mirrors the ``actions`` list in ``manifest.json``):

* ``voice.toggle`` — push-to-talk trigger; runs one full session.
* ``voice.start_session`` — like toggle but with explicit start hook.
* ``voice.end_session`` — early-stop the current capture.
* ``voice.set_mode`` / ``voice.get_mode`` — push-to-talk vs always-on.
* ``voice.set_active_conversation`` — GUI mirror push.
* ``voice.get_status`` — snapshot of state, mode, last error, session.
* ``voice.check_wake_word_model`` — does the harness have it?
* ``voice.pull_wake_word_model`` — kick a Gateway model pull.
* ``voice.subscribe_status`` — long-poll cursor for status events.

Most actions are thin wrappers around :class:`VoiceState`; ``toggle`` /
``start_session`` are where the orchestrator drives. They run on the
calling pipe-worker thread because the harness pipe action handler
chain already does the same — one session at a time per service is
enough for v1 (push-to-talk model). Future concurrency would need a
worker pool; a TODO marker is fine for now.
"""

from __future__ import annotations

import logging
import threading
from typing import Any, Dict, Optional

from . import audio_io as _audio
from . import orchestrator as _orch
from . import state as _state
from . import wake_word as _ww

logger = logging.getLogger("wylde.voice.pipe")

SERVICE_NAME = "wylde-voice"

_started = False
_started_lock = threading.Lock()

# A single session-running guard so two ``voice.toggle`` calls in flight
# don't fight over the mic. The ``ipc.PipeServer`` spawns a worker per
# connection; without this two GUI clicks could race.
_session_lock = threading.Lock()


# ── Singletons ─────────────────────────────────────────────────────────


_voice_state: Optional[_state.VoiceState] = None
_capture: Optional[_audio.AudioCaptureProtocol] = None
_playback: Optional[_audio.AudioPlaybackProtocol] = None
_harness_client: Optional[_orch.HarnessClientProtocol] = None
_singletons_lock = threading.Lock()


def _state_singleton() -> _state.VoiceState:
    global _voice_state
    with _singletons_lock:
        if _voice_state is None:
            _voice_state = _state.VoiceState()
        return _voice_state


def _capture_singleton() -> _audio.AudioCaptureProtocol:
    global _capture
    with _singletons_lock:
        if _capture is None:
            _capture = _audio.SounddeviceCapture()
        return _capture


def _playback_singleton() -> _audio.AudioPlaybackProtocol:
    global _playback
    with _singletons_lock:
        if _playback is None:
            _playback = _audio.SounddevicePlayback()
        return _playback


def _harness_singleton() -> _orch.HarnessClientProtocol:
    global _harness_client
    with _singletons_lock:
        if _harness_client is None:
            _harness_client = _orch.HarnessPipeClient()
        return _harness_client


def install_test_doubles(
    *,
    state: Optional[_state.VoiceState] = None,
    capture: Optional[_audio.AudioCaptureProtocol] = None,
    playback: Optional[_audio.AudioPlaybackProtocol] = None,
    harness: Optional[_orch.HarnessClientProtocol] = None,
) -> None:
    """Test seam — replace the module-level singletons.

    Tests call this before invoking actions so ``voice.toggle`` runs
    against a :class:`FakeCapture` / fake harness instead of touching
    real audio devices. Resetting in tearDown is the caller's job; pass
    ``None`` for any slot you want to leave alone.
    """
    global _voice_state, _capture, _playback, _harness_client
    with _singletons_lock:
        if state is not None:
            _voice_state = state
        if capture is not None:
            _capture = capture
        if playback is not None:
            _playback = playback
        if harness is not None:
            _harness_client = harness


def reset_singletons() -> None:
    """Test seam — clear the singletons so the next access lazy-builds
    fresh defaults. Pairs with :func:`install_test_doubles`."""
    global _voice_state, _capture, _playback, _harness_client
    with _singletons_lock:
        _voice_state = None
        _capture = None
        _playback = None
        _harness_client = None


# ── Helpers ────────────────────────────────────────────────────────────


class _ActionError(Exception):
    """Structured error surfaced through the pipe envelope."""

    def __init__(self, code: str, message: str) -> None:
        super().__init__(message)
        self.code = code
        self.message = message


def _payload_dict(payload: Any) -> Dict[str, Any]:
    if payload is None:
        return {}
    if not isinstance(payload, dict):
        raise _ActionError("bad_request", "payload must be a map")
    return payload


# ── Action handlers ────────────────────────────────────────────────────


def _voice_toggle_action(payload: Any) -> Dict[str, Any]:
    """Run a full capture → STT → chat → TTS → play session synchronously.

    Push-to-talk semantics: hold (or click) toggles a single round-trip.
    Returns the ``SessionResult`` shape so the GUI can show transcript
    + response inline. ``voice.start_session`` is an alias kept for
    callers that want a more explicit name.
    """
    p = _payload_dict(payload)
    max_seconds = float(p.get("max_seconds") or 30.0)
    model = p.get("model") if isinstance(p.get("model"), str) else None
    if not _session_lock.acquire(blocking=False):
        raise _ActionError("busy", "a session is already in flight")
    try:
        result = _orch.run_session(
            _state_singleton(),
            capture=_capture_singleton(),
            playback=_playback_singleton(),
            harness=_harness_singleton(),
            max_capture_seconds=max_seconds,
            model=model,
        )
    finally:
        _session_lock.release()
    return result.to_dict()


def _voice_start_session_action(payload: Any) -> Dict[str, Any]:
    """Alias for ``voice.toggle``. Reserved for a future async start —
    today the orchestrator runs synchronously on the calling worker."""
    return _voice_toggle_action(payload)


def _voice_end_session_action(_payload: Any) -> Dict[str, Any]:
    """Stop the in-flight capture early.

    Wakes the capture's stop event so the orchestrator's ``capture()``
    loop returns whatever it has so far and the rest of the flow runs
    on that audio. If there's no active session, returns a no-op.
    """
    state = _state_singleton()
    cap = _capture_singleton()
    try:
        cap.stop()
    except Exception as exc:  # noqa: BLE001
        logger.warning("voice.end_session: capture stop raised: %s", exc)
    snap = state.snapshot()
    return {
        "ok": True,
        "had_active_session": bool(snap.get("active_session")),
        "state": snap.get("state"),
    }


def _voice_set_mode_action(payload: Any) -> Dict[str, Any]:
    """Switch the active capture mode.  ``payload.mode`` must be one of
    the values in ``_state.ALL_MODES`` (e.g. ``"voice"``, ``"text"``).
    Returns ``{mode: <new_mode>}``."""
    p = _payload_dict(payload)
    mode = p.get("mode")
    if not isinstance(mode, str) or mode not in _state.ALL_MODES:
        raise _ActionError(
            "bad_request",
            f"mode must be one of {list(_state.ALL_MODES)!r}",
        )
    state = _state_singleton()
    try:
        new_mode = state.set_mode(mode)
    except ValueError as exc:
        raise _ActionError("bad_request", str(exc))
    return {"mode": new_mode}


def _voice_get_mode_action(_payload: Any) -> Dict[str, Any]:
    """Return the current capture mode.  Envelope: ``{mode: <str>}``."""
    return {"mode": _state_singleton().get_mode()}


def _voice_set_active_conversation_action(payload: Any) -> Dict[str, Any]:
    """Bind the voice service to a conversation id so transcribed utterances
    are routed there.  Required: ``payload.conversation_id`` (string).
    Returns ``{conversation_id: <id>}``."""
    p = _payload_dict(payload)
    cid = p.get("conversation_id")
    if not isinstance(cid, str):
        raise _ActionError("bad_request", "conversation_id is required")
    state = _state_singleton()
    new_id = state.set_active_conversation(cid)
    return {"conversation_id": new_id}


def _voice_get_status_action(_payload: Any) -> Dict[str, Any]:
    """Return a full state snapshot — active session, mode, wake-word
    status, etc.  The dashboard polls this for the voice indicator."""
    return _state_singleton().snapshot()


def _voice_check_wake_word_model_action(payload: Any) -> Dict[str, Any]:
    """Ask the harness model registry whether the wake-word model is
    installed. Returns ``{installed, model}``; the GUI uses this to
    decide whether to show the trust-and-pull dialog."""
    p = _payload_dict(payload)
    state = _state_singleton()
    model_name = p.get("model")
    if not isinstance(model_name, str) or not model_name:
        model_name = state.config.wake_word_model
    installed = _ww.is_model_installed(model_name)
    state.set_wake_word_installed(installed)
    return {"installed": bool(installed), "model": model_name}


def _voice_pull_wake_word_model_action(payload: Any) -> Dict[str, Any]:
    """Kick off a Gateway-mediated pull of the wake-word model.

    This is a stub today (returns a synthetic ``job_id``); the real
    flow streams progress from Gateway's ``/api/models/pull``. The GUI
    flow is: user clicks "install", we call this, GUI polls
    ``voice.check_wake_word_model`` until installed=true.
    """
    p = _payload_dict(payload)
    state = _state_singleton()
    model_name = p.get("model")
    if not isinstance(model_name, str) or not model_name:
        model_name = state.config.wake_word_model
    job_id = _ww.initiate_pull(model_name)
    with state._lock:
        state.wake_word_pull_job = job_id
    return {"job_id": job_id, "model": model_name}


def _voice_subscribe_status_action(payload: Any) -> Dict[str, Any]:
    """Long-poll the status event stream.

    ``cursor`` starts at 0. Each response carries ``next_cursor``;
    callers feed it back to chain polls. ``max_wait_ms`` caps the
    server-side wait (capped further by the harness pipe pattern at
    25 s so a connection isn't held indefinitely).
    """
    p = _payload_dict(payload)
    cursor = int(p.get("cursor") or 0)
    max_wait_ms = int(p.get("max_wait_ms") or 5000)
    max_wait_ms = max(0, min(max_wait_ms, 25000))
    return _state_singleton().poll_events(
        cursor=cursor,
        max_wait_ms=max_wait_ms,
    )


# ── Wiring ────────────────────────────────────────────────────────────


_ACTIONS = {
    "voice.toggle": _voice_toggle_action,
    "voice.start_session": _voice_start_session_action,
    "voice.end_session": _voice_end_session_action,
    "voice.set_mode": _voice_set_mode_action,
    "voice.get_mode": _voice_get_mode_action,
    "voice.set_active_conversation": _voice_set_active_conversation_action,
    "voice.get_status": _voice_get_status_action,
    "voice.check_wake_word_model": _voice_check_wake_word_model_action,
    "voice.pull_wake_word_model": _voice_pull_wake_word_model_action,
    "voice.subscribe_status": _voice_subscribe_status_action,
}


def _wrap_handler(handler: Any) -> Any:
    """Translate ``_ActionError`` into the wire envelope the dispatcher
    expects. Other exceptions bubble — the shared ipc layer wraps them."""

    def _wrapped(payload: Any) -> Any:
        try:
            return handler(payload)
        except _ActionError as exc:
            raise RuntimeError(f"[{exc.code}] {exc.message}")

    _wrapped.__name__ = getattr(handler, "__name__", "wrapped")
    return _wrapped


def _ipc_module() -> Any:
    try:
        from Core.shared import ipc

        return ipc
    except ImportError as exc:
        logger.warning("voice pipe: ipc not importable (%s) — pipe disabled", exc)
        return None


def _register_actions() -> Any:
    ipc = _ipc_module()
    if ipc is None:
        return None
    for name, handler in _ACTIONS.items():
        ipc.register_action(name, _wrap_handler(handler))
    logger.info("voice pipe: registered %d voice.* actions", len(_ACTIONS))
    return ipc


def _build_stub_app() -> Any:
    """Minimal Flask app for the ipc fallback. Action dispatch never
    falls through to this in practice — actions live in the dispatch
    table the shared ipc module owns."""
    try:
        from flask import Flask
    except ImportError:
        return None
    app = Flask("wylde-voice")

    @app.route("/health", methods=["GET"])
    def _health() -> dict[str, Any]:  # pragma: no cover
        return {"ok": True, "service": SERVICE_NAME}

    return app


def start() -> bool:
    """Start the voice pipe in a daemon thread.

    Returns True if the pipe is now serving (or was already), False if
    dependencies are missing (msgpack/pywin32 absent, non-Windows host).
    Safe to call multiple times.
    """
    global _started
    with _started_lock:
        if _started:
            return True
        ipc = _register_actions()
        if ipc is None:
            return False
        try:
            ipc.serve_forever_background(SERVICE_NAME, _build_stub_app())
        except Exception as exc:  # noqa: BLE001
            logger.warning("voice pipe: serve_forever_background failed (%s)", exc)
            return False
        _started = True
        logger.info("voice pipe: serving \\\\.\\pipe\\%s", SERVICE_NAME)
        return True


def stop() -> None:
    """Reserved for future graceful shutdown. The shared PipeServer
    doesn't expose a shutdown hook today; the pipe drains when the
    daemon process exits."""
    return None


__all__ = [
    "SERVICE_NAME",
    "start",
    "stop",
    "install_test_doubles",
    "reset_singletons",
]
