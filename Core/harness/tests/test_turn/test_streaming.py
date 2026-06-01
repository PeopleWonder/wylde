"""Tests for :mod:`Core.harness.turn._streaming` — chat-backend wiring
and the assistant-content tool-call salvage parser.

Covers:

* Option A streaming contract (driver does not pass ``on_token`` /
  ``on_thinking`` to chat_fn — the salvage parser can't classify partial
  chunks yet).
* :func:`_extract_tool_calls_from_content` over each detection shape:
  fenced JSON, tag-wrapped, bare balanced-brace JSON.
* Prose-JSON guard (``"name":`` substring requirement).
* Per-turn ``(name, args)`` dedupe across structured-and-content tool
  calls.
* Registry alias resolution for dotted ↔ snake-case names.
"""

from __future__ import annotations

import importlib
from typing import Any

from .conftest import _drive_one_turn_with, _empty_tools, _import_turn


def test_streaming_chat_fn_emits_single_bulk_token_event_under_option_a() -> Any:
    """Option A: per-chunk streaming is disabled until the salvage
    parser handles partial chunks.  The driver no longer passes
    on_token/on_thinking to chat_fn, so even a chat_fn that *would*
    stream gets the legacy one-event-at-end behaviour.  Re-enable
    when the parser learns streaming-buffer classification.
    """
    turn = _import_turn()

    chunk_calls = []

    def streaming_chat(
        *,
        messages: Any,
        tools: Any,
        model: Any,
        on_token: Any = None,
        on_thinking: Any = None,
    ) -> Any:
        # The driver's call site no longer passes these callbacks, so
        # on_token stays None and this loop never fires.  We record
        # whether the kwarg arrived to assert the contract.
        chunk_calls.append(on_token is not None)
        for chunk in ("hello ", "from ", "the ", "harness"):
            if on_token:
                on_token(chunk)
        return turn.ChatStep(
            text="hello from the harness",
            tool_calls=[],
        )

    result = turn.run_turn(
        user_message="say hi",
        conversation_id="streaming-test-1",
        chat_fn=streaming_chat,
        tool_run=lambda n, a: {"ok": True, "data": None},
        list_tools_fn=_empty_tools,
        timeout=10.0,
    )
    assert result.aborted is False
    assert result.final_message == "hello from the harness"
    assert chunk_calls == [False], (
        f"driver should not pass on_token under Option A, saw {chunk_calls!r}"
    )

    state = turn.get_turn(result.turn_id)
    assert state is not None
    token_events = [e for e in state.turn_events if e.type == "token"]
    # One bulk token event carrying the full assembled text — emitted
    # by the post-call double-emit path because the chat_fn could not
    # stream chunks (no callback was passed).
    assert len(token_events) == 1, (
        f"expected 1 bulk token event under Option A, got {len(token_events)} "
        f"({[e.data['text'] for e in token_events]!r})"
    )
    assert token_events[0].data["text"] == "hello from the harness"
    assert state.turn_events[-1].type == "turn_complete"


# ── Tool-call salvage parser tests (Option A + dedupe) ─────────────────


def test_extract_bare_json_tool_call_recovered() -> None:
    turn = _import_turn()
    alias_map = {
        "memory_long_term_save": "memory_long_term_save",
        "memory.long_term.save": "memory_long_term_save",
    }
    text = '{"name": "memory.long_term.save", "arguments": {"body": "kebab"}}'
    cleaned, recovered, unrecognised = turn._extract_tool_calls_from_content(
        text, alias_map
    )
    assert cleaned == "", f"cleaned should be empty after scrub, got {cleaned!r}"
    assert len(recovered) == 1
    assert recovered[0]["name"] == "memory_long_term_save"
    assert recovered[0]["args"] == {"body": "kebab"}
    assert recovered[0]["raw_name"] == "memory.long_term.save"
    assert unrecognised == []


def test_extract_tag_wrapped_recovered() -> None:
    turn = _import_turn()
    alias_map = {"git_status": "git_status"}
    text = (
        "I'll check that.\n"
        '<tool_call>{"name": "git_status", "arguments": {}}</tool_call>\n'
        "Done."
    )
    cleaned, recovered, unrecognised = turn._extract_tool_calls_from_content(
        text, alias_map
    )
    assert "git_status" not in cleaned
    assert "I'll check that." in cleaned
    assert "Done." in cleaned
    assert len(recovered) == 1
    assert recovered[0]["name"] == "git_status"
    assert recovered[0]["args"] == {}
    assert unrecognised == []


def test_extract_fenced_json_recovered() -> None:
    turn = _import_turn()
    alias_map = {"rag_ask": "rag_ask"}
    text = (
        'Here you go:\n```json\n{"name": "rag_ask", "arguments": {"q": "test"}}\n```\n'
    )
    cleaned, recovered, unrecognised = turn._extract_tool_calls_from_content(
        text, alias_map
    )
    assert "```" not in cleaned
    assert "rag_ask" not in cleaned
    assert "Here you go:" in cleaned
    assert len(recovered) == 1
    assert recovered[0]["name"] == "rag_ask"
    assert recovered[0]["args"] == {"q": "test"}
    assert unrecognised == []


def test_extract_unrecognised_name() -> None:
    turn = _import_turn()
    alias_map = {"git_status": "git_status"}
    text = '{"name": "nonexistent_tool", "arguments": {"x": 1}}'
    cleaned, recovered, unrecognised = turn._extract_tool_calls_from_content(
        text, alias_map
    )
    # JSON is still scrubbed (chat bubble must not render raw call JSON),
    # but the call lands in unrecognised so the caller can fire a
    # tool_error rather than dispatch it.
    assert cleaned == ""
    assert recovered == []
    assert len(unrecognised) == 1
    assert unrecognised[0]["name"] == "nonexistent_tool"
    assert unrecognised[0]["args"] == {"x": 1}


def test_extract_mixed_prose_and_tool_call() -> None:
    turn = _import_turn()
    alias_map = {"git_diff": "git_diff"}
    text = (
        "Let me check the diff for you. "
        '{"name": "git_diff", "arguments": {"path": "."}} '
        "Be right back!"
    )
    cleaned, recovered, unrecognised = turn._extract_tool_calls_from_content(
        text, alias_map
    )
    assert "git_diff" not in cleaned
    assert "Let me check the diff for you." in cleaned
    assert "Be right back!" in cleaned
    assert len(recovered) == 1
    assert recovered[0]["name"] == "git_diff"
    assert recovered[0]["args"] == {"path": "."}


def test_extract_does_not_falsepositive_on_prose_json() -> None:
    """JSON that doesn't carry a top-level "name" key must NOT be
    treated as a tool call.  Models sometimes emit example JSON in
    explanations; scrubbing it would corrupt the assistant's reply."""
    turn = _import_turn()
    alias_map = {"git_status": "git_status"}
    text = 'The forecast is {"weather": "sunny", "temp": 72}. Nothing else to report.'
    cleaned, recovered, unrecognised = turn._extract_tool_calls_from_content(
        text, alias_map
    )
    # Span has no "name": substring → not even tried, stays in cleaned.
    assert cleaned == text.strip()
    assert recovered == []
    assert unrecognised == []


def test_dedupe_structured_then_text() -> Any:
    """Model emits the SAME call as both a structured tool_calls entry
    AND inline content JSON on the same step.  Structured dispatches;
    the text duplicate fires a tool_error with reason
    ``tool_call_text_duplicate`` and never re-runs the tool."""
    turn = _import_turn()

    # Single tool catalog so the salvage parser recognises the inline name.
    def tools_fn() -> Any:
        return [
            {
                "id": "git_status",
                "name": "git_status",
                "description": "git status",
                "parameters": [],
            }
        ]

    call_count = {"n": 0}

    def chat(*, messages: Any, tools: Any, model: Any) -> Any:
        call_count["n"] += 1
        if call_count["n"] == 1:
            # Same call expressed two ways: structured + content.  Driver
            # should run it once, surface the duplicate as a tool_error.
            return turn.ChatStep(
                text='{"name": "git_status", "arguments": {}}',
                tool_calls=[turn.ToolCall(id="call_0", name="git_status", args={})],
            )
        return turn.ChatStep(text="all clean", tool_calls=[])

    state, result = _drive_one_turn_with(
        chat,
        conversation_id="dedupe-mixed-1",
        list_tools_fn=tools_fn,
    )
    assert result.aborted is False

    tool_types = [e.type for e in state.tool_events]
    # tool_dispatched + tool_result for the structured call ...
    assert "tool_dispatched" in tool_types
    assert "tool_result" in tool_types
    # ... and tool_error for the dedup'd content twin.
    errors = [
        e
        for e in state.tool_events
        if e.type == "tool_error" and e.data.get("reason") == "tool_call_text_duplicate"
    ]
    assert len(errors) == 1, (
        f"expected one duplicate tool_error, got "
        f"{[e.data for e in state.tool_events if e.type == 'tool_error']!r}"
    )

    # Wire-level disjointness preserved.
    turn_types = [e.type for e in state.turn_events]
    assert not (set(turn_types) & {"tool_dispatched", "tool_result", "tool_error"}), (
        f"tool event leaked into chat.stream_turn: {turn_types!r}"
    )


def test_dedupe_two_text_emissions_same_call() -> Any:
    """A model that emits the same tool call as content twice in the
    SAME step runs the tool once and emits a tool_error for the
    second occurrence."""
    turn = _import_turn()

    def tools_fn() -> Any:
        return [
            {
                "id": "git_status",
                "name": "git_status",
                "description": "git status",
                "parameters": [],
            }
        ]

    call_count = {"n": 0}

    def chat(*, messages: Any, tools: Any, model: Any) -> Any:
        call_count["n"] += 1
        if call_count["n"] == 1:
            return turn.ChatStep(
                text=(
                    '{"name": "git_status", "arguments": {}}\n'
                    '{"name": "git_status", "arguments": {}}'
                ),
                tool_calls=[],
            )
        return turn.ChatStep(text="done", tool_calls=[])

    state, result = _drive_one_turn_with(
        chat,
        conversation_id="dedupe-text-twin-1",
        list_tools_fn=tools_fn,
    )
    assert result.aborted is False

    dispatched = [e for e in state.tool_events if e.type == "tool_dispatched"]
    results_ev = [e for e in state.tool_events if e.type == "tool_result"]
    duplicate_errors = [
        e
        for e in state.tool_events
        if e.type == "tool_error" and e.data.get("reason") == "tool_call_text_duplicate"
    ]
    assert len(dispatched) == 1, (
        f"expected one dispatch (dedup'd), got {len(dispatched)}: "
        f"{[e.data for e in dispatched]!r}"
    )
    assert len(results_ev) == 1
    assert len(duplicate_errors) == 1


def test_tool_name_alias_dot_to_underscore_resolves() -> None:
    try:
        tool_registry = importlib.import_module("Core.harness.tooling.tool_registry")
    except ImportError:  # pragma: no cover
        tool_registry = importlib.import_module(
            "Wylde.Core.harness.tooling.tool_registry"
        )
    tool_registry.invalidate_cache()
    catalog = tool_registry.list_tools()
    a = catalog.get("memory_long_term_save")
    b = catalog.get("memory.long_term.save")
    assert a is not None, "memory_long_term_save missing"
    assert b is not None, "dotted alias not aliased"
    assert a is b, "alias must be a shared reference, not a copy"
    canonical = tool_registry.list_canonical_tools()
    assert "memory_long_term_save" in canonical
    assert "memory.long_term.save" not in canonical
