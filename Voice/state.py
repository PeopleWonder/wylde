"""Voice service in-process state + persistent mode config.

Two stores:

* In-memory ``VoiceState`` — current session, listening flag, last
  error, active-conversation mirror. Held by the running service;
  evaporates on shutdown.
* On-disk ``voice_config.json`` — push-to-talk vs always-on toggle,
  default wake-word model name. Persists across restarts so the user
  doesn't re-confirm always-on every boot.

Both surfaces are thread-safe via a single RLock — pipe handlers can
land on multiple worker threads inside the shared ipc.PipeServer.
"""

from __future__ import annotations

import json
import logging
import threading
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Dict, List, Optional

logger = logging.getLogger("wylde.voice.state")


# ── Constants ──────────────────────────────────────────────────────────


MODE_PUSH_TO_TALK = "push_to_talk"
MODE_ALWAYS_ON = "always_on"
ALL_MODES = (MODE_PUSH_TO_TALK, MODE_ALWAYS_ON)

# Default wake-word model — small, open, runs on CPU. Pulled through
# Gateway's /api/models/pull on user opt-in. the Wylde user can swap this later.
DEFAULT_WAKE_WORD_MODEL = "openWakeWord/hey-jarvis"

# Service states reported via voice.get_status.
STATE_IDLE = "idle"
STATE_LISTENING = "listening"
STATE_PROCESSING = "processing"
STATE_PLAYING = "playing"
STATE_ERROR = "error"


# ── Persistent config ─────────────────────────────────────────────────


def _config_path() -> Path:
    """Voice's own config file. Lives next to other Wylde data so the
    Lifecycle daemon's data dir cleanup picks it up."""
    import os

    base = os.getenv("WYLDE_VOICE_CONFIG_DIR")
    if base:
        return Path(base) / "voice_config.json"
    # Default: alongside the conversations / scheduler state.
    home = Path(__file__).resolve().parents[1]
    data_dir = Path(os.getenv("WYLDE_DATA_DIR") or (home / ".wylde" / "data"))
    data_dir.mkdir(parents=True, exist_ok=True)
    return data_dir / "voice_config.json"


@dataclass
class VoiceConfig:
    mode: str = MODE_PUSH_TO_TALK
    wake_word_model: str = DEFAULT_WAKE_WORD_MODEL

    def to_dict(self) -> Dict[str, Any]:
        return {"mode": self.mode, "wake_word_model": self.wake_word_model}

    @classmethod
    def from_dict(cls, d: Dict[str, Any]) -> "VoiceConfig":
        mode = d.get("mode")
        if mode not in ALL_MODES:
            mode = MODE_PUSH_TO_TALK
        return cls(
            mode=mode,
            wake_word_model=str(d.get("wake_word_model") or DEFAULT_WAKE_WORD_MODEL),
        )


def load_config(path: Optional[Path] = None) -> VoiceConfig:
    p = path or _config_path()
    if not p.exists():
        return VoiceConfig()
    try:
        return VoiceConfig.from_dict(json.loads(p.read_text(encoding="utf-8")))
    except Exception as exc:  # noqa: BLE001
        logger.warning("voice config unreadable, using defaults: %s", exc)
        return VoiceConfig()


def save_config(cfg: VoiceConfig, path: Optional[Path] = None) -> None:
    p = path or _config_path()
    p.parent.mkdir(parents=True, exist_ok=True)
    tmp = p.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(cfg.to_dict(), indent=2), encoding="utf-8")
    tmp.replace(p)


# ── In-memory session + state ──────────────────────────────────────────


@dataclass
class Session:
    """One push-to-talk / wake-word activation cycle."""

    id: str
    started_at: float
    conversation_id: str
    transcript: str = ""
    response: str = ""
    completed_at: Optional[float] = None
    error: Optional[str] = None

    def to_dict(self) -> Dict[str, Any]:
        return {
            "session_id": self.id,
            "started_at": self.started_at,
            "completed_at": self.completed_at,
            "conversation_id": self.conversation_id,
            "transcript": self.transcript,
            "response": self.response,
            "error": self.error,
        }


@dataclass
class StatusEvent:
    """One emission for the voice.subscribe_status long-poll stream."""

    type: str  # "state" | "session_started" | "session_ended" | "error"
    at: float
    data: Dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> Dict[str, Any]:
        return {"type": self.type, "at": self.at, **self.data}


class VoiceState:
    """Thread-safe handle on the service's runtime state."""

    def __init__(self, *, config: Optional[VoiceConfig] = None) -> None:
        self._lock = threading.RLock()
        self._cv = threading.Condition(self._lock)
        self.config: VoiceConfig = config or load_config()
        self.state: str = STATE_IDLE
        self.last_error: Optional[str] = None
        # Mirrors whatever the GUI says is active — never authoritative.
        self.active_conversation_id: str = ""
        self.active_session: Optional[Session] = None
        # Bounded ring of events so subscribers can long-poll with cursors.
        # Older events fall off the front when the ring is full.
        self._events: List[StatusEvent] = []
        self._max_events = 256
        # Wake-word model state. Voice doesn't actually load the model
        # (the harness owns model files); we only track installed/active.
        self.wake_word_installed: bool = False
        self.wake_word_pull_job: Optional[str] = None

    # ── Mutators (thread-safe) ────────────────────────────────────────

    def set_mode(self, mode: str) -> str:
        if mode not in ALL_MODES:
            raise ValueError(f"unknown mode {mode!r}")
        with self._lock:
            self.config.mode = mode
            save_config(self.config)
            self._emit("state", {"state": self.state, "mode": mode})
            return mode

    def get_mode(self) -> str:
        with self._lock:
            return self.config.mode

    def set_active_conversation(self, conversation_id: str) -> str:
        with self._lock:
            self.active_conversation_id = str(conversation_id or "")
            return self.active_conversation_id

    def begin_session(self, conversation_id: str) -> Session:
        sess = Session(
            id=uuid.uuid4().hex[:12],
            started_at=time.time(),
            conversation_id=conversation_id,
        )
        with self._lock:
            self.active_session = sess
            self.state = STATE_LISTENING
            self._emit("session_started", sess.to_dict())
            return sess

    def end_session(
        self,
        *,
        transcript: str = "",
        response: str = "",
        error: Optional[str] = None,
    ) -> Optional[Session]:
        with self._lock:
            sess = self.active_session
            if sess is None:
                return None
            sess.transcript = transcript
            sess.response = response
            sess.error = error
            sess.completed_at = time.time()
            self.active_session = None
            self.state = STATE_IDLE if not error else STATE_ERROR
            self.last_error = error
            self._emit("session_ended", sess.to_dict())
            return sess

    def set_state(self, new_state: str) -> None:
        with self._lock:
            self.state = new_state
            self._emit("state", {"state": new_state})

    def set_wake_word_installed(self, installed: bool) -> None:
        with self._lock:
            self.wake_word_installed = bool(installed)
            self._emit(
                "wake_word_status",
                {
                    "installed": self.wake_word_installed,
                    "model": self.config.wake_word_model,
                },
            )

    # ── Status snapshot ───────────────────────────────────────────────

    def snapshot(self) -> Dict[str, Any]:
        with self._lock:
            return {
                "state": self.state,
                "mode": self.config.mode,
                "listening": self.state == STATE_LISTENING,
                "last_error": self.last_error,
                "active_session": (
                    self.active_session.to_dict() if self.active_session else None
                ),
                "active_conversation_id": self.active_conversation_id,
                "wake_word_installed": self.wake_word_installed,
                "wake_word_model": self.config.wake_word_model,
            }

    # ── Long-poll event stream ────────────────────────────────────────

    def _emit(self, type_: str, data: Dict[str, Any]) -> None:
        ev = StatusEvent(type=type_, at=time.time(), data=dict(data))
        # Caller already holds the lock from the public mutator.
        self._events.append(ev)
        # Bound the ring so a long-running daemon doesn't grow unbounded.
        if len(self._events) > self._max_events:
            del self._events[: len(self._events) - self._max_events]
        self._cv.notify_all()

    def poll_events(
        self, *, cursor: int = 0, max_wait_ms: int = 5000
    ) -> Dict[str, Any]:
        """Block until at least one event past ``cursor`` is available
        or ``max_wait_ms`` elapses, then return the new events + the
        next cursor. Cursor wraps to the current length on overflow
        (so a stale subscriber re-syncs gracefully)."""
        deadline = time.monotonic() + max(0, max_wait_ms) / 1000.0
        with self._cv:
            while True:
                # Cursor outside the ring → reset to current head.
                if cursor < 0 or cursor > len(self._events):
                    cursor = len(self._events)
                if cursor < len(self._events):
                    break
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    break
                self._cv.wait(timeout=remaining)
            new_events = [e.to_dict() for e in self._events[cursor:]]
            return {
                "events": new_events,
                "next_cursor": len(self._events),
            }


__all__ = [
    "MODE_PUSH_TO_TALK",
    "MODE_ALWAYS_ON",
    "ALL_MODES",
    "DEFAULT_WAKE_WORD_MODEL",
    "STATE_IDLE",
    "STATE_LISTENING",
    "STATE_PROCESSING",
    "STATE_PLAYING",
    "STATE_ERROR",
    "Session",
    "StatusEvent",
    "VoiceConfig",
    "VoiceState",
    "load_config",
    "save_config",
]
