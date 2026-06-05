"""Daemon-side reflection + curation scheduler.

A single background thread that polls every ``poll_interval`` seconds
and fires :func:`Core.harness.memory.reflection.reflect` and
:func:`Core.harness.memory.workspace_memory.curate` at separate
cadences — most chat-turn latency stays out of the way because the
scheduler runs them on idle.

Cadence defaults (override via env or constructor kwargs)::

    conversation reflection : every 10 min of conversation idle
    workspace reflection    : every 6 hours per workspace
    long-term reflection    : every 24 hours
    workspace curation      : every 24 hours per workspace

State is persisted to ``$DATA_DIR/scheduler_state.json`` so a daemon
restart doesn't replay the entire backlog. The state file tracks
``last_reflected_at`` per (conversation_id, workspace_id, "long_term")
and ``last_curated_at`` per workspace.

The scheduler needs an LLM-callable. The Lifecycle daemon constructs a
default chat_fn from :mod:`Core.harness.backend.backend_routing` and
hands it in via :meth:`MemoryScheduler.start`. Tests inject a synthetic
chat_fn + an injectable clock so cadence assertions are deterministic.
"""

from __future__ import annotations

import json
import logging
import os
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Dict, Optional

from . import reflection as _reflection
from . import workspace_memory as _ws_mem

# workspaces: removed in config-file-backed redesign (2026-06-05) —
# the `workspaces` MRU index now lives in Rust; the periodic per-workspace
# reflect/curate ticks below are now no-ops.
from . import conversation as _conv
from ._common import DATA_DIR, ensure_dir

logger = logging.getLogger("wylde.harness.memory.scheduler")


STATE_PATH: Path = DATA_DIR / "scheduler_state.json"


# Cadence defaults — env-overridable so a deployment can dial them
# without touching code.
DEFAULT_POLL_INTERVAL_S = float(os.getenv("WYLDE_SCHED_POLL_S", "60"))
DEFAULT_CONVERSATION_IDLE_S = float(
    os.getenv("WYLDE_SCHED_CONV_IDLE_S", "600")
)  # 10 min
DEFAULT_WORKSPACE_REFLECT_S = float(
    os.getenv("WYLDE_SCHED_WS_REFLECT_S", "21600")
)  # 6 h
DEFAULT_LONG_TERM_REFLECT_S = float(
    os.getenv("WYLDE_SCHED_LT_REFLECT_S", "86400")
)  # 24 h
DEFAULT_WORKSPACE_CURATE_S = float(
    os.getenv("WYLDE_SCHED_WS_CURATE_S", "86400")
)  # 24 h


# ── State persistence ──────────────────────────────────────────────────


@dataclass
class SchedulerState:
    """Last-fired timestamps per scope. Mutated by the scheduler thread
    only; loaded once at start, persisted after every fire."""

    long_term_reflected_at: float = 0.0
    workspace_reflected_at: Dict[str, float] = field(default_factory=dict)
    workspace_curated_at: Dict[str, float] = field(default_factory=dict)
    conversation_reflected_at: Dict[str, float] = field(default_factory=dict)

    def to_dict(self) -> Dict[str, Any]:
        return {
            "long_term_reflected_at": self.long_term_reflected_at,
            "workspace_reflected_at": dict(self.workspace_reflected_at),
            "workspace_curated_at": dict(self.workspace_curated_at),
            "conversation_reflected_at": dict(self.conversation_reflected_at),
        }

    @classmethod
    def from_dict(cls, d: Dict[str, Any]) -> "SchedulerState":
        return cls(
            long_term_reflected_at=float(d.get("long_term_reflected_at") or 0.0),
            workspace_reflected_at=dict(d.get("workspace_reflected_at") or {}),
            workspace_curated_at=dict(d.get("workspace_curated_at") or {}),
            conversation_reflected_at=dict(d.get("conversation_reflected_at") or {}),
        )


def _load_state(path: Path = STATE_PATH) -> SchedulerState:
    if not path.exists():
        return SchedulerState()
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
    except Exception as exc:  # noqa: BLE001
        logger.warning("scheduler: state unreadable, treating as empty: %s", exc)
        return SchedulerState()
    return SchedulerState.from_dict(raw if isinstance(raw, dict) else {})


def _save_state(state: SchedulerState, path: Path = STATE_PATH) -> None:
    ensure_dir(path.parent)
    tmp = path.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(state.to_dict(), indent=2), encoding="utf-8")
    tmp.replace(path)


# ── Scheduler ──────────────────────────────────────────────────────────


@dataclass
class CadenceConfig:
    """Per-scope cadence floors. ``poll_interval`` is the loop tick;
    the others are minimum gaps between fires."""

    poll_interval_s: float = DEFAULT_POLL_INTERVAL_S
    conversation_idle_s: float = DEFAULT_CONVERSATION_IDLE_S
    workspace_reflect_s: float = DEFAULT_WORKSPACE_REFLECT_S
    long_term_reflect_s: float = DEFAULT_LONG_TERM_REFLECT_S
    workspace_curate_s: float = DEFAULT_WORKSPACE_CURATE_S


class MemoryScheduler:
    """Thread-backed scheduler. One instance is constructed by the
    Lifecycle daemon at boot.

    Public entry points:
        :meth:`start`  — kick the loop in a daemon thread.
        :meth:`stop`   — request a clean exit.
        :meth:`tick`   — run one iteration synchronously. Tests use
                         this with an injected clock.
        :meth:`state`  — read-only snapshot of the state dict.
    """

    def __init__(
        self,
        chat_fn: Optional[Callable[..., Any]] = None,
        *,
        cadence: Optional[CadenceConfig] = None,
        clock: Optional[Callable[[], float]] = None,
        state_path: Path = STATE_PATH,
        logger_: Optional[logging.Logger] = None,
    ) -> None:
        self._chat_fn = chat_fn
        self._cadence = cadence or CadenceConfig()
        self._clock = clock or time.time
        self._state_path = state_path
        self._state = _load_state(state_path)
        self._stop_event = threading.Event()
        self._thread: Optional[threading.Thread] = None
        self._logger = logger_ or logger

    # ── Public API ────────────────────────────────────────────────────

    def start(self) -> bool:
        """Spawn the daemon thread. Returns True if started, False if
        already running. Failure to construct the thread (no chat_fn,
        no permissions, …) is non-fatal — the Lifecycle daemon logs
        and continues without a scheduler."""
        if self._thread is not None and self._thread.is_alive():
            return True
        if self._chat_fn is None:
            self._logger.info(
                "scheduler: not started — no chat_fn supplied (LLM not "
                "wired); reflection / curation will run only via direct "
                "Python calls."
            )
            return False
        self._thread = threading.Thread(
            target=self._loop,
            name="wylde-memory-scheduler",
            daemon=True,
        )
        self._thread.start()
        self._logger.info(
            "scheduler: started (poll=%.0fs, ws_reflect=%.0fs, "
            "ws_curate=%.0fs, lt_reflect=%.0fs)",
            self._cadence.poll_interval_s,
            self._cadence.workspace_reflect_s,
            self._cadence.workspace_curate_s,
            self._cadence.long_term_reflect_s,
        )
        return True

    def stop(self) -> None:
        self._stop_event.set()

    def state(self) -> Dict[str, Any]:
        return self._state.to_dict()

    # ── Loop ──────────────────────────────────────────────────────────

    def _loop(self) -> None:
        while not self._stop_event.is_set():
            try:
                self.tick()
            except Exception:  # noqa: BLE001
                self._logger.exception("scheduler: tick raised; continuing")
            # Wait up to poll_interval_s, checking the stop flag every
            # second so a shutdown signal lands quickly.
            deadline = self._clock() + self._cadence.poll_interval_s
            while self._clock() < deadline and not self._stop_event.is_set():
                self._stop_event.wait(timeout=1.0)

    def tick(self) -> Dict[str, int]:
        """Run one scheduler iteration. Returns a count dict so tests
        can assert which scopes fired this tick.

        Always runs in the calling thread — the loop calls this in the
        scheduler thread, but tests call it directly so they don't need
        to start a thread + sleep through poll intervals.
        """
        counts = {
            "conversation": 0,
            "workspace_reflect": 0,
            "workspace_curate": 0,
            "long_term": 0,
        }
        if self._chat_fn is None:
            return counts

        now = self._clock()

        # Conversation reflection — idle window driven.
        try:
            counts["conversation"] = self._tick_conversations(now)
        except Exception:  # noqa: BLE001
            self._logger.exception("scheduler: conversation tick raised")

        # Workspace reflection — periodic per workspace.
        try:
            counts["workspace_reflect"] = self._tick_workspaces_reflect(now)
        except Exception:  # noqa: BLE001
            self._logger.exception("scheduler: workspace reflect tick raised")

        # Workspace curation — longer cadence per workspace.
        try:
            counts["workspace_curate"] = self._tick_workspaces_curate(now)
        except Exception:  # noqa: BLE001
            self._logger.exception("scheduler: workspace curate tick raised")

        # Long-term reflection — global, daily.
        try:
            counts["long_term"] = self._tick_long_term(now)
        except Exception:  # noqa: BLE001
            self._logger.exception("scheduler: long-term tick raised")

        # Persist state once per tick (cheap; small file).
        try:
            _save_state(self._state, self._state_path)
        except Exception:  # noqa: BLE001
            self._logger.exception("scheduler: state save failed")
        return counts

    # ── Per-scope tickers ─────────────────────────────────────────────

    def _tick_conversations(self, now: float) -> int:
        """Fire conversation-scoped reflection on idle windows."""
        fired = 0
        for meta in _conv.list_conversations():
            cid = meta.get("id")
            if not isinstance(cid, str) or not cid:
                continue
            updated_at = float(meta.get("updated_at") or 0.0)
            last = float(self._state.conversation_reflected_at.get(cid) or 0.0)
            # Idle window: convo hasn't been touched in conversation_idle_s,
            # AND we haven't already reflected since the last activity.
            idle_for = now - updated_at
            if idle_for < self._cadence.conversation_idle_s:
                continue
            if last >= updated_at:
                continue
            self._fire_reflect(f"conversation:{cid}")
            self._state.conversation_reflected_at[cid] = now
            fired += 1
        return fired

    def _tick_workspaces_reflect(self, now: float) -> int:
        # workspaces: removed in config-file-backed redesign (2026-06-05) —
        # no Python workspace MRU index to iterate; no-op.
        return 0

    def _tick_workspaces_curate(self, now: float) -> int:
        # workspaces: removed in config-file-backed redesign (2026-06-05) —
        # no Python workspace MRU index to iterate; no-op.
        return 0

    def _tick_long_term(self, now: float) -> int:
        if now - self._state.long_term_reflected_at < self._cadence.long_term_reflect_s:
            return 0
        self._fire_reflect("long_term")
        self._state.long_term_reflected_at = now
        return 1

    # ── Fire helpers (each catches exceptions so one bad scope doesn't
    # blow up the whole tick) ─────────────────────────────────────────

    def _fire_reflect(self, scope: str) -> None:
        try:
            result = _reflection.reflect(scope, chat_fn=self._chat_fn)
            self._logger.info(
                "scheduler: reflected %s (skipped=%s, inputs=%d)",
                scope,
                result.skipped,
                result.inputs_considered,
            )
        except Exception:  # noqa: BLE001
            self._logger.exception("scheduler: reflect(%s) failed", scope)

    def _fire_curate(self, workspace_id: str) -> None:
        try:
            result = _ws_mem.curate(workspace_id, chat_fn=self._chat_fn)
            self._logger.info(
                "scheduler: curated workspace %s (skipped=%s, kept=%d, "
                "superseded=%d, merged=%d)",
                workspace_id,
                result.skipped,
                len(result.kept),
                len(result.superseded),
                len(result.merged),
            )
        except Exception:  # noqa: BLE001
            self._logger.exception("scheduler: curate(%s) failed", workspace_id)


# ── Production chat_fn factory ─────────────────────────────────────────


def default_chat_fn() -> Optional[Callable[..., Any]]:
    """Build a chat_fn from the harness backend router.

    Returns None if the router can't be constructed (no Ollama, no
    backend module). The Lifecycle daemon calls this at boot to wire
    the scheduler; failure here means the scheduler runs in
    "skipped-only" mode.
    """
    try:
        from ..backend.backend_routing import default_router
        from .._tool_context import ChatStep
    except ImportError:
        try:
            from Core.harness.backend.backend_routing import default_router
            from Core.harness._tool_context import ChatStep
        except ImportError:
            return None

    router = default_router()

    def _chat(
        *, messages: Any, tools: Any = None, model: Any = None, **_kw: Any
    ) -> Any:  # noqa: ARG001
        result = router.chat(messages=messages, model=model)
        return ChatStep(text=getattr(result, "text", "") or "")

    return _chat


__all__ = [
    "STATE_PATH",
    "DEFAULT_POLL_INTERVAL_S",
    "CadenceConfig",
    "MemoryScheduler",
    "SchedulerState",
    "default_chat_fn",
]
