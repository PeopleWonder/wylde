"""Tool-facing per-turn context + chat-backend wire shapes.

Phase 5.D rehome
----------------

These helpers used to live in :mod:`Core.harness.turn._state`, but they
are *not* part of the chat-turn driver — the tooling, memory, and
scheduler layers depend on them directly. Pulling them out of the
``turn`` package lets that package be deleted once the Rust harness owns
the chat-turn loop without breaking every tool that reads the active
context.

``turn/_state.py`` re-imports every name below and re-exports it, so the
legacy ``from Core.harness.turn import current_tool_context`` (and the
``ChatStep`` / ``ToolCall`` imports) keep resolving for any caller or
test that still reaches through the package root.

Public surface:

* :class:`ToolContext` — thread-local context the dispatched tool reads
  to learn the active conversation / turn / workspace.
* :func:`current_tool_context` / :func:`record_file_written` — read +
  record surface for tools.
* :class:`ToolCall`, :class:`ChatStep`, :class:`ChatFn` — wire shapes for
  the chat backend.

Nothing here imports the turn driver at module-load time.
:func:`record_file_written` reaches the (driver-owned) turn registry
through a guarded lazy import so it degrades to a silent no-op once the
driver is gone — and in the default Rust mode no Python driver ever sets
a tool context, so the registry lookup is never even reached.
"""

from __future__ import annotations

import threading
from dataclasses import dataclass, field
from typing import Any, Callable, Dict, List, Optional, Protocol


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
    :func:`Core.harness.turn._run_end_of_turn_architectural_check`) drains
    this list and re-examines each file against the per-file
    ``wylde_check`` rules once. Silent no-op when no turn is active (tests
    calling tools directly, background callers) — the file write itself is
    unaffected.

    De-dupes on append so a turn that edits the same file three times
    only pays for one check at end-of-turn.

    The active turn lives in the chat-turn driver's process-wide registry
    (``Core.harness.turn``). That import is deliberately lazy and guarded:
    in the default Rust mode no Python driver sets a tool context, so the
    early ``ctx is None`` return fires first and the registry is never
    imported; once the Python driver is deleted entirely the
    ``ImportError`` guard keeps this a clean no-op.
    """
    if not isinstance(path, str) or not path:
        return
    ctx = current_tool_context()
    if ctx is None or not ctx.turn_id:
        return
    try:
        from .turn import get_turn
    except ImportError:
        # Python chat-turn driver removed → nothing to record against.
        return
    state = get_turn(ctx.turn_id)
    if state is None:
        return
    with state.cv:
        if path not in state.files_written:
            state.files_written.append(path)


# ── Chat-backend wire shapes ───────────────────────────────────────────


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


__all__ = [
    "ChatFn",
    "ChatStep",
    "ToolCall",
    "ToolContext",
    "current_tool_context",
    "record_file_written",
]
