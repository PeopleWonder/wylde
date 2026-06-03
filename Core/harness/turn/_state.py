"""Per-turn driver data shapes and the process-wide turn registry.

Pulled out of the monolithic ``turn.py`` so the data + state primitives
the rest of the driver builds on can be read in isolation. Every other
submodule of :mod:`Core.harness.turn` imports from here.

The *non-driver* helpers — :class:`ToolContext`, :func:`current_tool_context`,
:func:`record_file_written`, and the chat wire shapes :class:`ToolCall` /
:class:`ChatStep` / :class:`ChatFn` — were rehomed to
:mod:`Core.harness._tool_context` in Phase 5.D so they survive the
deletion of this package. They are re-imported below and re-exported via
the package ``__init__`` so ``from Core.harness.turn import X`` keeps
resolving for any caller or test that still reaches through the root.

Public surface (re-exported via the package ``__init__``):

* :class:`TurnEvent`, :class:`TurnState`, :class:`AssistantTurn` — the
  driver's per-turn data.
* Registry: :func:`register_turn`, :func:`get_turn`, :func:`list_turns`,
  :func:`reap_turn`.
* Emit helpers: :func:`_emit_turn`, :func:`_emit_tool`, :func:`_mark_done`.
* Re-exported from :mod:`Core.harness._tool_context`: :class:`ToolContext`,
  :func:`current_tool_context`, :func:`_set_tool_context`,
  :func:`record_file_written`, :class:`ToolCall`, :class:`ChatStep`,
  :class:`ChatFn`.
"""

from __future__ import annotations

import logging
import threading
import time
from dataclasses import dataclass, field
from typing import Any, Dict, List, Optional, Set

# Rehomed to Core.harness._tool_context (Phase 5.D) so these non-driver
# helpers outlive this package; re-exported here for back-compat.
from .._tool_context import (  # noqa: F401 — re-exported for package-root access
    ChatFn,
    ChatStep,
    ToolCall,
    ToolContext,
    _set_tool_context,
    _tool_context,
    current_tool_context,
    record_file_written,
)

logger = logging.getLogger("wylde.harness.turn")


# ── Result + state shapes ──────────────────────────────────────────────


@dataclass
class TurnEvent:
    type: str
    data: Dict[str, Any]


@dataclass
class TurnState:
    turn_id: str
    conversation_id: str
    workspace_id: str = ""
    # ``"text"`` (default) or ``"voice"`` — Voice service starts turns
    # with ``modality="voice"`` so the system-prompt builder folds in a
    # voice-friendly prelude (concise, conversational, avoids markdown).
    modality: str = "text"
    # Per-device permission tier — Gateway forwards this from the
    # Bearer-token verify result so tool calls can be gated mid-loop:
    #   * "read_only"               — no tools may run
    #   * "tool_use"                — non-destructive tools only
    #   * "destructive_tool_access" — full surface
    # Defaults to "tool_use" because in-process callers (Voice, the
    # desktop GUI via the local pipe) don't carry a device token —
    # they're already inside the trust boundary, and the read-only
    # restriction would block routine tool use.
    device_tier: str = "tool_use"
    turn_events: List[TurnEvent] = field(default_factory=list)
    tool_events: List[TurnEvent] = field(default_factory=list)
    done: bool = False
    cancel_event: threading.Event = field(default_factory=threading.Event)
    cv: threading.Condition = field(default_factory=threading.Condition)
    final_message: str = ""
    tool_calls_summary: List[Dict[str, Any]] = field(default_factory=list)
    started_at: float = field(default_factory=time.monotonic)
    completed_at: Optional[float] = None
    # Per-turn dedupe set for tool dispatches.  Keys are
    # sha256(name + json(args, sort_keys=True)) — see :func:`_call_hash`.
    # Both structured tool_calls returned by the chat backend AND tool
    # calls salvaged from assistant content go through the same hash
    # check, so a model that emits the same call in both slots only
    # runs once per turn.  Duplicates fire a ``tool_error`` with
    # ``reason="tool_call_text_duplicate"``.
    _dispatched_call_hashes: Set[str] = field(default_factory=set)
    # Paths the fs tools (write_file / edit_file) touched this turn.
    # Drained by ``_run_end_of_turn_architectural_check`` to bound the
    # wylde_check pass to files that actually changed instead of
    # re-sweeping the whole tree per turn.  De-duped on append via
    # :func:`record_file_written`.
    files_written: List[str] = field(default_factory=list)


@dataclass
class AssistantTurn:
    turn_id: str
    conversation_id: str
    final_message: str
    tool_calls_summary: List[Dict[str, Any]]
    aborted: bool = False
    abort_reason: Optional[str] = None


# ── Process-wide turn registry ─────────────────────────────────────────


_turns: Dict[str, TurnState] = {}
_turns_lock = threading.Lock()


def register_turn(state: TurnState) -> None:
    with _turns_lock:
        _turns[state.turn_id] = state


def get_turn(turn_id: str) -> Optional[TurnState]:
    with _turns_lock:
        return _turns.get(turn_id)


def list_turns() -> List[str]:
    with _turns_lock:
        return list(_turns.keys())


def reap_turn(turn_id: str) -> None:
    """Remove a completed turn from the registry. Long-poll callers must
    have already drained their cursor past the final event before this is
    safe to call."""
    with _turns_lock:
        _turns.pop(turn_id, None)


# ── Emit helpers — call these from the driver thread only ─────────────


def _emit_turn(state: TurnState, event_type: str, data: Dict[str, Any]) -> None:
    """Append a user-facing event and wake any long-poll waiters.

    Tokens, thinking-tokens, and the terminal completion / abort frames
    all flow through here. Tool events MUST go through :func:`_emit_tool`
    instead — wire-level separation is the whole point.
    """
    with state.cv:
        state.turn_events.append(TurnEvent(type=event_type, data=dict(data)))
        state.cv.notify_all()


def _emit_tool(state: TurnState, event_type: str, data: Dict[str, Any]) -> None:
    """Append a tool-activity event and wake any long-poll waiters."""
    with state.cv:
        state.tool_events.append(TurnEvent(type=event_type, data=dict(data)))
        state.cv.notify_all()


def _mark_done(state: TurnState) -> None:
    with state.cv:
        state.done = True
        state.completed_at = time.monotonic()
        state.cv.notify_all()
