"""Tests for :mod:`Core.harness.turn._tool_round` — per-device-tier
gating around tool dispatch.

Three permission tiers gate tool execution:

* ``read_only`` — every tool is blocked.
* ``tool_use`` — tools whose manifest sets ``requires_confirmation: true``
  are blocked.
* ``destructive_tool_access`` — full surface.

These tests cover each axis plus the "no tier specified → default to
``tool_use``" fallback the in-process callers rely on.
"""

from __future__ import annotations

import importlib
from typing import Any

from .conftest import _import_turn, _scripted_chat_with_tool_call


def test_chat_turn_read_only_blocks_all_tools() -> Any:
    turn = _import_turn()
    runner_calls: list = []

    def _runner(name: str, args: Any) -> Any:
        runner_calls.append((name, args))
        return {"ok": True, "data": {"echo": args}}

    chat = _scripted_chat_with_tool_call(turn, "git_status", args={"path": "."})

    result = turn.run_turn(
        user_message="check status",
        conversation_id="conv_tier_readonly",
        chat_fn=chat,
        tool_run=_runner,
        device_tier="read_only",
        timeout=5.0,
    )
    assert result.aborted is False

    # The runner was NEVER reached — the tier-gate fired first.
    assert runner_calls == [], f"runner was called: {runner_calls!r}"

    state = turn.get_turn(result.turn_id)
    tool_types = [e.type for e in state.tool_events]
    assert "tool_dispatched" in tool_types
    assert "tool_error" in tool_types
    err_event = next(e for e in state.tool_events if e.type == "tool_error")
    assert err_event.data.get("reason") == "tier_read_only"
    assert "read_only" in err_event.data.get("error", "")
    # tool_calls_summary records the block.
    summary = result.tool_calls_summary
    assert len(summary) == 1
    assert summary[0]["ok"] is False
    assert summary[0].get("reason") == "tier_read_only"


def test_chat_turn_tool_use_blocks_destructive() -> Any:
    """tool_use tier blocks tools whose manifest sets
    requires_confirmation: true. We use the real tool catalog so we
    pick a real manifest with that flag set."""
    turn = _import_turn()

    # Pick a real destructive tool from the catalog. The memory_delete
    # manifest has requires_confirmation: true (confirmed earlier in
    # the rework). If that manifest changes the test catches it.
    try:
        list_tools = importlib.import_module(
            "Core.harness.tooling.tool_registry"
        ).list_tools
    except ImportError:  # pragma: no cover
        list_tools = importlib.import_module(
            "Wylde.Core.harness.tooling.tool_registry"
        ).list_tools
    catalog = list_tools()
    destructive_id = next(
        (
            k
            for k, v in catalog.items()
            if v.get("requires_confirmation") and v.get("id") == k
        ),
        None,
    )
    assert destructive_id, "no destructive tool found in catalog"

    runner_calls: list = []

    def _runner(name: Any, args: Any) -> Any:
        runner_calls.append((name, args))
        return {"ok": True, "data": None}

    chat = _scripted_chat_with_tool_call(turn, destructive_id)

    result = turn.run_turn(
        user_message="run destructive",
        conversation_id="conv_tier_use_destructive",
        chat_fn=chat,
        tool_run=_runner,
        device_tier="tool_use",
        timeout=5.0,
    )
    assert result.aborted is False
    assert runner_calls == [], f"runner saw {runner_calls!r}"
    state = turn.get_turn(result.turn_id)
    err_event = next(e for e in state.tool_events if e.type == "tool_error")
    assert err_event.data.get("reason") == "tier_tool_use_blocked_destructive"


def test_chat_turn_destructive_runs_anything() -> Any:
    """destructive_tool_access tier — even requires_confirmation:true
    tools dispatch successfully. We exercise via a synthetic runner
    so the real destructive tool's side effects don't fire."""
    turn = _import_turn()
    try:
        list_tools = importlib.import_module(
            "Core.harness.tooling.tool_registry"
        ).list_tools
    except ImportError:  # pragma: no cover
        list_tools = importlib.import_module(
            "Wylde.Core.harness.tooling.tool_registry"
        ).list_tools
    catalog = list_tools()
    destructive_id = next(
        (
            k
            for k, v in catalog.items()
            if v.get("requires_confirmation") and v.get("id") == k
        ),
        None,
    )
    assert destructive_id

    runner_calls: list = []

    def _runner(name: Any, args: Any) -> Any:
        runner_calls.append((name, args))
        return {"ok": True, "data": {"ran": True}}

    chat = _scripted_chat_with_tool_call(turn, destructive_id)

    result = turn.run_turn(
        user_message="run destructive",
        conversation_id="conv_tier_destructive_ok",
        chat_fn=chat,
        tool_run=_runner,
        device_tier="destructive_tool_access",
        timeout=5.0,
    )
    assert result.aborted is False
    assert len(runner_calls) == 1
    assert runner_calls[0][0] == destructive_id
    state = turn.get_turn(result.turn_id)
    tool_types = [e.type for e in state.tool_events]
    assert "tool_result" in tool_types
    assert "tool_error" not in tool_types


def test_chat_turn_no_tier_defaults_to_tool_use() -> Any:
    """No device_tier passed → defaults to tool_use. Non-destructive
    tools run; destructive tools are blocked. We exercise the
    non-destructive path here (a tool whose manifest doesn't carry
    requires_confirmation=true)."""
    turn = _import_turn()
    try:
        list_tools = importlib.import_module(
            "Core.harness.tooling.tool_registry"
        ).list_tools
    except ImportError:  # pragma: no cover
        list_tools = importlib.import_module(
            "Wylde.Core.harness.tooling.tool_registry"
        ).list_tools
    catalog = list_tools()
    safe_id = next(
        (
            k
            for k, v in catalog.items()
            if not v.get("requires_confirmation") and v.get("id") == k
        ),
        None,
    )
    assert safe_id, "no non-destructive tool found in catalog"

    runner_calls: list = []

    def _runner(name: Any, args: Any) -> Any:
        runner_calls.append((name, args))
        return {"ok": True, "data": {}}

    chat = _scripted_chat_with_tool_call(turn, safe_id)

    result = turn.run_turn(
        user_message="run safe",
        conversation_id="conv_tier_default",
        chat_fn=chat,
        tool_run=_runner,
        # No device_tier kwarg — should default to "tool_use".
        timeout=5.0,
    )
    assert result.aborted is False
    assert len(runner_calls) == 1
    state = turn.get_turn(result.turn_id)
    assert state.device_tier == "tool_use"
