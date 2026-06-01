"""Tests for :mod:`Core.harness.turn._driver` — the end-to-end loop
that walks the LLM-tool-call cycle.

Covers the architectural invariants the driver upholds:

* Two LLM round trips (tool call + final response) yield the right
  shape of events on each stream.
* User-facing and tool-activity streams stay wire-level disjoint.
* Multi-turn conversations thread history through ``chat_fn`` correctly.
* Sibling pipe actions (``tools.list`` / ``tools.run`` /
  ``models.list`` / ``models.get_profile``) dispatch without going
  through the chat driver — sanity check that they're unaffected by
  the turn refactor.
"""

from __future__ import annotations

import importlib
from pathlib import Path
from typing import Any

import pytest

from .conftest import _empty_tools, _import_turn


class _ScriptedChat:
    """Returns a different ``ChatStep`` on each call. Mirrors what the
    backend router would produce for a real Ollama tool-calling response.
    """

    def __init__(self, turn_module: Any) -> None:
        self._turn = turn_module
        self.calls = 0

    def __call__(self, *, messages: Any, tools: Any, model: Any) -> Any:
        self.calls += 1
        if self.calls == 1:
            # First round: model wants a tool. Use meta.tool_search-ish
            # name; the synthetic runner below intercepts by name so the
            # real tool catalog isn't consulted.
            return self._turn.ChatStep(
                text="",
                tool_calls=[
                    self._turn.ToolCall(
                        id="call_0", name="meta.tool_search", args={"q": "git"}
                    ),
                ],
            )
        # Second round: final response.
        return self._turn.ChatStep(text="here you go", tool_calls=[])


class _SyntheticRunner:
    """In-process tool runner that returns canned envelopes for known
    names and an error envelope for anything else."""

    def __init__(self) -> None:
        self.invocations: list[Any] = []

    def __call__(self, name: Any, args: Any) -> Any:
        self.invocations.append((name, dict(args)))
        if name == "meta.tool_search":
            return {
                "ok": True,
                "data": {"matches": ["git_status", "git_diff", "git_log"]},
            }
        return {
            "ok": False,
            "error": {"code": "not_found", "message": f"no synthetic for {name}"},
        }


def test_run_turn_drives_tool_then_final_response() -> None:
    turn = _import_turn()
    chat = _ScriptedChat(turn)
    runner = _SyntheticRunner()

    result = turn.run_turn(
        user_message="show me git tools",
        conversation_id="conv_test_1",
        chat_fn=chat,
        tool_run=runner,
        list_tools_fn=_empty_tools,
        timeout=10.0,
    )

    # Driver invoked the tool exactly once with the model's args.
    assert runner.invocations == [("meta.tool_search", {"q": "git"})], (
        f"runner saw {runner.invocations!r}"
    )
    # Two LLM round trips: tool call, then final.
    assert chat.calls == 2

    # Final answer carries the assistant's text response.
    assert result.final_message == "here you go"
    assert result.aborted is False

    state = turn.get_turn(result.turn_id)
    assert state is not None, "turn state should still be in the registry"

    # User-facing stream content: a token event + turn_complete.
    turn_types = [e.type for e in state.turn_events]
    assert "token" in turn_types, f"expected a token event, got {turn_types}"
    assert turn_types[-1] == "turn_complete", (
        f"last event should be turn_complete, got {turn_types}"
    )

    # Tool-activity stream content: dispatch then result.
    tool_types = [e.type for e in state.tool_events]
    assert tool_types == ["tool_dispatched", "tool_result"], (
        f"expected dispatched→result, got {tool_types}"
    )

    # Wire-level disjointness: tool event types must NOT appear on the
    # user-facing stream and token-style types must NOT appear on the
    # tool stream. This is the architectural invariant.
    assert not (set(turn_types) & {"tool_dispatched", "tool_result", "tool_error"})
    assert not (
        set(tool_types) & {"token", "thinking", "turn_complete", "turn_aborted"}
    )

    # tool_calls_summary records the one successful call.
    assert len(result.tool_calls_summary) == 1
    assert result.tool_calls_summary[0]["ok"] is True
    assert result.tool_calls_summary[0]["name"] == "meta.tool_search"


def test_pipe_tools_and_models_actions_dispatch() -> None:
    """The non-chat harness actions (tools.list, tools.run, models.list,
    models.get_profile) should resolve through the action handlers
    without going near a real pipe — we drive them by calling the
    dispatcher directly the same way the smoke test does for chat.*."""
    try:
        from Wylde.Core.harness import pipe as harness_pipe
    except ImportError:
        from Core.harness import pipe as harness_pipe

    # tools.list returns the live in-process catalog. We don't assert
    # an exact tool count (it grows as new tools land) — just that the
    # response shape is a list of catalog entries.
    resp = harness_pipe._tools_list_action(None)
    assert isinstance(resp.get("tools"), list)
    assert resp.get("count") == len(resp["tools"])
    if resp["tools"]:
        sample = resp["tools"][0]
        assert isinstance(sample, dict)

    # tools.run on an unknown tool returns the runner's structured
    # error envelope, not an exception.
    resp = harness_pipe._tools_run_action(
        {"name": "definitely-not-a-real-tool", "args": {}}
    )
    assert isinstance(resp, dict)
    assert resp.get("ok") is False

    # tools.run with bad input raises the structured action error.
    with pytest.raises(Exception) as exc_info:
        harness_pipe._tools_run_action({"args": {}})
    assert "bad_request" in str(exc_info.value) or "name is required" in str(
        exc_info.value
    )

    # models.list shape — kind filter optional.
    resp = harness_pipe._models_list_action(None)
    assert "models" in resp
    assert resp.get("kind") == "all"

    # models.get_profile requires a name.
    with pytest.raises(Exception):
        harness_pipe._models_get_profile_action({})


def test_conversation_history_threads_across_turns(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> Any:
    """Two consecutive turns on the same conversation_id: the second
    turn's chat_fn should see the first turn's user + assistant
    messages in its history."""
    # Point the conversations store at a fresh tmp dir so the test is
    # isolated from any real history on disk.
    monkeypatch.setenv("WYLDE_DATA_DIR", str(tmp_path))
    monkeypatch.setenv("CONVERSATIONS_DIR", str(tmp_path / "conversations"))

    # Reload the conversations module so the env vars take effect.
    # _common reads paths at import time; we have to re-run that read.
    try:
        from Wylde.Core.harness.memory import _common as _mem_common
        from Wylde.Core.harness.memory import conversation as _conv
    except ImportError:
        from Core.harness.memory import _common as _mem_common
        from Core.harness.memory import conversation as _conv
    importlib.reload(_mem_common)
    importlib.reload(_conv)

    turn = _import_turn()
    importlib.reload(turn)  # pick up the freshly-reloaded conversation module

    # Capture what each chat call saw.
    seen_messages = []

    def echo_chat(*, messages: Any, tools: Any, model: Any) -> Any:
        # Snapshot the (non-system) history visible to this round-trip.
        seen_messages.append(
            [
                {"role": m["role"], "content": m.get("content", "")}
                for m in messages
                if m["role"] != "system"
            ]
        )
        return turn.ChatStep(text=f"reply{len(seen_messages)}", tool_calls=[])

    conv_id = "history-roundtrip-1"

    r1 = turn.run_turn(
        user_message="first question",
        conversation_id=conv_id,
        chat_fn=echo_chat,
        tool_run=lambda n, a: {"ok": True, "data": None},
        list_tools_fn=_empty_tools,
        timeout=10.0,
    )
    assert r1.aborted is False
    assert r1.final_message == "reply1"

    r2 = turn.run_turn(
        user_message="follow-up question",
        conversation_id=conv_id,
        chat_fn=echo_chat,
        tool_run=lambda n, a: {"ok": True, "data": None},
        list_tools_fn=_empty_tools,
        timeout=10.0,
    )
    assert r2.aborted is False
    assert r2.final_message == "reply2"

    # First turn saw only its own user message.
    assert seen_messages[0] == [
        {"role": "user", "content": "first question"},
    ], f"first-turn history: {seen_messages[0]!r}"

    # Second turn saw the first turn's user + assistant THEN its own user.
    assert seen_messages[1] == [
        {"role": "user", "content": "first question"},
        {"role": "assistant", "content": "reply1"},
        {"role": "user", "content": "follow-up question"},
    ], f"second-turn history: {seen_messages[1]!r}"

    # The persisted document carries everything.
    doc = _conv.read_conversation(conv_id)
    persisted_roles = [m["role"] for m in doc["messages"]]
    assert persisted_roles == ["user", "assistant", "user", "assistant"], (
        f"persisted: {persisted_roles!r}"
    )
