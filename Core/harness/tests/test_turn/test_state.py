"""Tests for :mod:`Core.harness.turn._state` — registry lookups and
cancellation semantics.

Exercises the read surface of the process-wide turn registry
(``get_turn`` / ``cancel_turn`` on unknown ids) and the cancellation
flow that produces a ``turn_aborted`` event on the user-facing stream.
"""

from __future__ import annotations

import threading
import time
from typing import Any

from .conftest import _empty_tools, _import_turn


def test_cancel_aborts_turn_with_turn_aborted_event() -> Any:
    turn = _import_turn()

    cancel_now = threading.Event()
    proceed = threading.Event()

    def slow_chat(*, messages: Any, tools: Any, model: Any) -> Any:
        # Block until the test signals: simulates a long LLM call.
        proceed.set()
        if not cancel_now.wait(timeout=5.0):
            return turn.ChatStep(text="should not be reached", tool_calls=[])
        # Driver loops back to check cancel after we return; give it a
        # response that LOOKS like more work so it'd keep going if not
        # cancelled, then it'll see the cancel flag on the next iteration.
        return turn.ChatStep(
            text="",
            tool_calls=[
                turn.ToolCall(id="call_x", name="never.runs", args={}),
            ],
        )

    state = turn.start_turn(
        user_message="hello",
        conversation_id="conv_cancel_1",
        chat_fn=slow_chat,
        tool_run=lambda n, a: {"ok": True, "data": None},
        list_tools_fn=_empty_tools,
    )

    # Wait for the driver thread to enter the chat call.
    assert proceed.wait(timeout=2.0), "driver never entered chat_fn"

    # Issue cancel — the driver checks cancel between iterations.
    assert turn.cancel_turn(state.turn_id) is True
    cancel_now.set()

    # Wait for the driver to wrap up.
    deadline = time.monotonic() + 5.0
    with state.cv:
        while not state.done and time.monotonic() < deadline:
            state.cv.wait(timeout=0.5)
    assert state.done, "driver should have terminated after cancel"

    types = [e.type for e in state.turn_events]
    assert "turn_aborted" in types, f"expected turn_aborted, got {types}"
    aborted = [e for e in state.turn_events if e.type == "turn_aborted"][0]
    assert aborted.data.get("reason") == "cancelled"

    # No turn_complete on a cancelled turn.
    assert "turn_complete" not in types

    # Tool stream should be empty (we cancelled before any tool ran).
    tool_types = [e.type for e in state.tool_events]
    assert tool_types == [], f"no tool events expected on cancel, got {tool_types}"


def test_unknown_turn_id_streams_raise_not_found() -> None:
    turn = _import_turn()
    # Sanity: the driver uses an in-memory registry; bogus turn_ids
    # raise via the pipe action layer. Test it at the API level —
    # cancel_turn returns False for unknown ids, get_turn returns None.
    assert turn.get_turn("definitely-not-a-turn") is None
    assert turn.cancel_turn("definitely-not-a-turn") is False
