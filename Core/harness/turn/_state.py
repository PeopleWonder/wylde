"""Per-turn data shapes, thread-local tool context, and the process-wide
turn registry.

Pulled out of the monolithic ``turn.py`` so the data + state primitives
the rest of the driver builds on can be read in isolation. Every other
submodule of :mod:`Core.harness.turn` imports from here.

Public surface (re-exported via the package ``__init__``):

* :class:`ToolContext` — thread-local context the dispatched tool reads
  to learn the active conversation / turn / workspace.
* :func:`current_tool_context` / :func:`record_file_written` — read
  surface for tools.
* :class:`ToolCall`, :class:`ChatStep`, :class:`ChatFn` — wire shapes for
  the chat backend.
* :class:`TurnEvent`, :class:`TurnState`, :class:`AssistantTurn` — the
  driver's per-turn data.
* Registry: :func:`register_turn`, :func:`get_turn`, :func:`list_turns`,
  :func:`reap_turn`.
* Emit helpers: :func:`_emit_turn`, :func:`_emit_tool`, :func:`_mark_done`,
  :func:`_set_tool_context`.
"""

from __future__ import annotations

import logging
import threading
import time
from dataclasses import dataclass, field
from typing import Any, Callable, Dict, List, Optional, Protocol, Set

logger = logging.getLogger("wylde.harness.turn")


# ── Tool context (thread-local) ─────────────────────────────────────────
#
# When the driver dispatches a tool call it sets the per-thread tool
# context so the tool's Python module can read the active conversation_id,
# turn_id, and workspace_id without those being plumbed through the
# tool-runner's params dict. The memory tools (and any future
# context-aware tools) call :func:`current_tool_context` to read it.


@dataclass
class ToolContext:
    conversation_id: str = ""
    turn_id: str = ""
    workspace_id: str = ""


_tool_context = threading.local()


def current_tool_context() -> Optional[ToolContext]:
    """Read the active tool context for the calling thread.

    Returns ``None`` when no driver has set one (tests calling tools
    directly, or background callers). Tools should treat that as
    "no conversation in progress" and pick sensible defaults.
    """
    return getattr(_tool_context, "value", None)


def _set_tool_context(ctx: Optional[ToolContext]) -> None:
    _tool_context.value = ctx


def record_file_written(path: str) -> None:
    """Record a path the fs tools touched on the active turn.

    Called by ``write_file`` / ``edit_file`` after a successful write.
    The end-of-turn architectural check (see
    :func:`_run_end_of_turn_architectural_check`) drains this list and
    re-examines each file against the per-file ``wylde_check`` rules
    once.  Silent no-op when no turn is active (tests calling tools
    directly, background callers) — the file write itself is
    unaffected.

    De-dupes on append so a turn that edits the same file three times
    only pays for one check at end-of-turn.
    """
    if not isinstance(path, str) or not path:
        return
    ctx = current_tool_context()
    if ctx is None or not ctx.turn_id:
        return
    state = get_turn(ctx.turn_id)
    if state is None:
        return
    with state.cv:
        if path not in state.files_written:
            state.files_written.append(path)


# ── Result + state shapes ──────────────────────────────────────────────


@dataclass
class ToolCall:
    """One tool call extracted from an LLM response."""

    id: str
    name: str
    args: Dict[str, Any]


@dataclass
class ChatStep:
    """One LLM round trip's worth of output the driver consumes."""

    text: str = ""
    thinking: str = ""
    tool_calls: List[ToolCall] = field(default_factory=list)


class ChatFn(Protocol):
    """Pluggable LLM step. The default implementation streams from the
    Ollama daemon and pushes per-chunk text via ``on_token`` /
    ``on_thinking``; the smoke test injects a synthetic version that
    ignores those callbacks and returns a complete :class:`ChatStep`
    in one shot.

    ``on_token(text)`` is invoked for every assistant content chunk that
    arrives mid-stream. ``on_thinking(text)`` is invoked for every
    thinking-token chunk (only if the backend emits them and
    ``think_enabled=True`` is set on the body). Both callbacks are
    optional — synthetic implementations may take ``**kwargs`` and
    ignore them.

    The returned :class:`ChatStep` carries the *final* accumulated text
    and any tool calls extracted at end-of-stream. The driver MUST
    avoid double-emitting tokens that were already sent through the
    callbacks; ``_drive_turn_inner`` handles that by tracking whether
    the chat_fn streamed.
    """

    def __call__(
        self,
        *,
        messages: List[Dict[str, Any]],
        tools: List[Dict[str, Any]],
        model: Optional[str],
        on_token: Optional[Callable[[str], None]] = None,
        on_thinking: Optional[Callable[[str], None]] = None,
    ) -> ChatStep: ...


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
