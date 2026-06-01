"""Tests for :mod:`Core.harness.turn._end_of_turn` — post-turn
extractor and the end-of-turn architectural-check sweep.

The post-turn pass runs after the model produces a final response
without further tool calls:

* The extractor reads the just-persisted conversation, asks the model
  what's worth promoting to long-term / workspace memory, and writes
  verdicts.  Each write fires a ``memory_written`` event on
  ``state.tool_events``.
* The architectural-check sweep re-runs the per-file ``wylde_check``
  rules over every file the turn wrote, surfacing ERROR findings as
  a single ``tool_warning`` event.
"""

from __future__ import annotations

import sys as _sys
from pathlib import Path
from typing import Any

import pytest

from .conftest import _empty_tools, _import_turn


# ── Post-turn extractor + memory_written event tests ───────────────────


def test_post_turn_extraction_writes_long_term(
    _isolated_data_for_extractor: Any,
) -> Any:
    deps = _isolated_data_for_extractor
    turn = deps["turn"]
    long_term = deps["long_term"]

    def driver_chat(*, messages: Any, tools: Any, model: Any, **_kw: Any) -> Any:
        return turn.ChatStep(text="kebab-case, got it.", tool_calls=[])

    def extractor_chat(
        *, messages: Any, tools: Any = None, model: Any = None, **_kw: Any
    ) -> Any:
        from types import SimpleNamespace

        return SimpleNamespace(
            text=(
                '{"action": "save_long_term", "body": '
                '"the Wylde user prefers kebab-case for new folder names.", '
                '"importance": 7}'
            )
        )

    turn.install_post_turn_extractor_chat_fn(extractor_chat, synchronous=True)

    result = turn.run_turn(
        user_message="By the way, I prefer kebab-case for new folder names.",
        conversation_id="conv_pte_long_term",
        chat_fn=driver_chat,
        tool_run=lambda n, a: {"ok": True, "data": None},
        list_tools_fn=lambda: [],
        timeout=10.0,
    )
    assert result.aborted is False

    records = long_term.list_records()
    bodies = [r.body for r in records]
    matching = [b for b in bodies if "kebab-case" in b]
    assert matching, f"no kebab-case entry; have {bodies!r}"

    state = turn.get_turn(result.turn_id)
    assert state is not None
    mem_events = [e for e in state.tool_events if e.type == "memory_written"]
    assert mem_events, (
        f"no memory_written event; saw {[e.type for e in state.tool_events]!r}"
    )
    payload = mem_events[0].data
    assert payload["source"] == "auto"
    assert payload["scope"] == "long_term"
    assert payload["importance"] == 7
    assert "kebab-case" in payload["body"]


def test_post_turn_extraction_noop_for_trivial_turn(
    _isolated_data_for_extractor: Any,
) -> Any:
    deps = _isolated_data_for_extractor
    turn = deps["turn"]
    long_term = deps["long_term"]

    pre_count = len(long_term.list_records(include_superseded=True))

    def driver_chat(*, messages: Any, tools: Any, model: Any, **_kw: Any) -> Any:
        return turn.ChatStep(text="okay.", tool_calls=[])

    def extractor_chat(
        *, messages: Any, tools: Any = None, model: Any = None, **_kw: Any
    ) -> Any:
        from types import SimpleNamespace

        return SimpleNamespace(text='{"action": "noop"}')

    turn.install_post_turn_extractor_chat_fn(extractor_chat, synchronous=True)

    result = turn.run_turn(
        user_message="hello",
        conversation_id="conv_pte_noop",
        chat_fn=driver_chat,
        tool_run=lambda n, a: {"ok": True, "data": None},
        list_tools_fn=lambda: [],
        timeout=10.0,
    )

    post_count = len(long_term.list_records(include_superseded=True))
    assert post_count == pre_count, (
        f"trivial turn wrote {post_count - pre_count} new entries"
    )

    state = turn.get_turn(result.turn_id)
    assert state is not None
    mem_events = [e for e in state.tool_events if e.type == "memory_written"]
    assert mem_events == [], f"unexpected memory_written events: {mem_events!r}"


def test_explicit_llm_save_emits_memory_written_event(
    _isolated_data_for_extractor: Any,
) -> Any:
    import importlib

    deps = _isolated_data_for_extractor
    turn = deps["turn"]
    long_term = deps["long_term"]

    def extractor_chat(
        *, messages: Any, tools: Any = None, model: Any = None, **_kw: Any
    ) -> Any:
        from types import SimpleNamespace

        return SimpleNamespace(text='{"action": "noop"}')

    turn.install_post_turn_extractor_chat_fn(extractor_chat, synchronous=True)

    calls = {"n": 0}

    def driver_chat(*, messages: Any, tools: Any, model: Any, **_kw: Any) -> Any:
        calls["n"] += 1
        if calls["n"] == 1:
            return turn.ChatStep(
                text="",
                tool_calls=[
                    turn.ToolCall(
                        id="call_explicit",
                        name="memory.long_term.save",  # dotted alias
                        args={
                            "body": "the Wylde user uses tabs over spaces in Go.",
                            "importance": 6,
                        },
                    )
                ],
            )
        return turn.ChatStep(text="saved.", tool_calls=[])

    try:
        _real_run_tool = importlib.import_module(
            "Core.harness.tooling.tool_runner"
        ).run_tool
    except ImportError:  # pragma: no cover
        _real_run_tool = importlib.import_module(
            "Wylde.Core.harness.tooling.tool_runner"
        ).run_tool
    result = turn.run_turn(
        user_message="Save this to long-term memory: I use tabs in Go.",
        conversation_id="conv_explicit_llm_save",
        chat_fn=driver_chat,
        tool_run=_real_run_tool,
        timeout=15.0,
    )
    assert result.aborted is False

    bodies = [r.body for r in long_term.list_records(include_superseded=True)]
    assert any("tabs over spaces" in b for b in bodies), (
        f"expected tabs/Go entry; have {bodies!r}"
    )

    state = turn.get_turn(result.turn_id)
    assert state is not None
    mem_events = [e for e in state.tool_events if e.type == "memory_written"]
    assert len(mem_events) >= 1, (
        f"expected memory_written event; saw {[e.type for e in state.tool_events]!r}"
    )
    explicit = next(
        (e for e in mem_events if e.data.get("source") == "llm_tool"),
        None,
    )
    assert explicit is not None, (
        f"no source=llm_tool event; events: {[e.data for e in mem_events]!r}"
    )
    assert explicit.data["scope"] == "long_term"
    assert "tabs over spaces" in explicit.data["body"]


def test_post_turn_extraction_skips_dupes_from_llm_tools(
    _isolated_data_for_extractor: Any,
) -> Any:
    """When the LLM explicitly saved content during the turn AND the
    extractor verdict mentions the same fact, the dedup guard skips
    the auto-save so we don't end up with two near-identical entries."""
    deps = _isolated_data_for_extractor
    long_term = deps["long_term"]
    pte = deps["post_turn_extractor"]

    pre_count = len(long_term.list_records(include_superseded=True))

    # Drive extract_post_turn directly (bypass the chat-turn driver) so
    # we can pass already_saved without orchestrating a real turn.
    # First, seed a conversation with a recent message so _build_context
    # has something to read.
    from Core.harness.memory import conversation as conv

    conv.save_conversation(
        conv_id="conv_dedup_test",
        messages=[
            {
                "role": "user",
                "content": "remember: I prefer kebab-case for new folder names.",
            },
            {"role": "assistant", "content": "Noted."},
        ],
    )

    # Synthetic extractor chat_fn returns a verdict with substantially
    # the same body the "LLM" already saved.
    def extractor_chat(
        *, messages: Any, tools: Any = None, model: Any = None, **_kw: Any
    ) -> Any:
        from types import SimpleNamespace

        return SimpleNamespace(
            text=(
                '{"action": "save_long_term", "body": '
                '"The user prefers kebab-case for new folder names.", '
                '"importance": 7}'
            )
        )

    already_saved = [
        {
            "memory_id": "fake_id_001",
            "body": "I prefer kebab-case for new folder names.",
        },
    ]

    result = pte.extract_post_turn(
        "conv_dedup_test",
        "turn_dedup_test",
        chat_fn=extractor_chat,
        already_saved=already_saved,
    )

    # The verdict was emitted, but the dedup guard skipped applying it.
    assert len(result.verdicts) == 1, f"verdicts: {result.verdicts!r}"
    assert result.verdicts[0].action == "save_long_term"
    assert result.written == [], f"unexpected writes: {result.written!r}"
    assert result.skipped is True

    post_count = len(long_term.list_records(include_superseded=True))
    assert post_count == pre_count, (
        f"dedup guard let through {post_count - pre_count} writes"
    )

    # Cleanup the seed conversation.
    conv.delete_conversation("conv_dedup_test")


# ── End-of-turn architectural check (wylde_check moved off per-write) ─


def test_end_of_turn_architectural_check_fires_once_per_multi_write_turn(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A turn that touches N files runs ``wylde_check`` ONCE at
    end-of-turn, not N times.  Load-bearing invariant of the
    per-write → end-of-turn refactor."""
    turn = _import_turn()

    # Point WYLDE_ROOT at tmp_path so check_one_file resolves the
    # tracked paths into in-tree relpaths the per-file rules see as
    # subject to the no_internal_http rule (outside Gateway).
    for mod_name in (
        "Wylde.Core.harness.dev.prewrite",
        "Core.harness.dev.prewrite",
    ):
        if mod_name in _sys.modules:
            monkeypatch.setattr(_sys.modules[mod_name], "wylde_root", lambda: tmp_path)
    for mod_name in (
        "Wylde.Core.harness.dev.wylde_check",
        "Core.harness.dev.wylde_check",
    ):
        if mod_name in _sys.modules:
            monkeypatch.setattr(_sys.modules[mod_name], "WYLDE_ROOT", tmp_path)

    bad_a = tmp_path / "Core" / "harness" / "evil_a.py"
    bad_b = tmp_path / "Core" / "harness" / "evil_b.py"
    clean = tmp_path / "Core" / "harness" / "good.py"
    for p in (bad_a, bad_b, clean):
        p.parent.mkdir(parents=True, exist_ok=True)
    bad_payload = "import requests\nrequests.post('http://127.0.0.1:8005/x')\n"
    bad_a.write_text(bad_payload, encoding="utf-8")
    bad_b.write_text(bad_payload, encoding="utf-8")
    clean.write_text("x = 1\n", encoding="utf-8")
    targets = [str(bad_a), str(bad_b), str(clean)]

    class _ScriptedChat:
        def __init__(self) -> None:
            self.calls = 0

        def __call__(self, *, messages: Any, tools: Any, model: Any) -> Any:
            self.calls += 1
            if self.calls == 1:
                return turn.ChatStep(
                    text="",
                    tool_calls=[
                        turn.ToolCall(
                            id=f"call_{i}",
                            name="fake_write",
                            args={"path": p},
                        )
                        for i, p in enumerate(targets)
                    ],
                )
            return turn.ChatStep(text="done", tool_calls=[])

    def _runner(name: Any, args: Any) -> Any:
        # Simulate the fs tool: file is already on disk; just record
        # the path via the canonical helper.  Going through the real
        # write_file would pull in a second module-graph copy of
        # ``turn`` under ``Core.harness.turn`` (vs the test's
        # ``Wylde.Core.harness.turn``) and split the thread-local
        # tool context across two modules.
        turn.record_file_written(args["path"])
        return {"ok": True, "data": {"status": "success", "path": args["path"]}}

    result = turn.run_turn(
        user_message="write three files",
        conversation_id="conv_eot_multi",
        chat_fn=_ScriptedChat(),
        tool_run=_runner,
        list_tools_fn=_empty_tools,
        timeout=10.0,
    )

    assert result.aborted is False
    state = turn.get_turn(result.turn_id)
    assert state is not None
    assert sorted(state.files_written) == sorted(targets)

    warnings = [
        e
        for e in state.tool_events
        if e.type == "tool_warning"
        and e.data.get("source") == "wylde_check_end_of_turn"
    ]
    assert len(warnings) == 1, (
        f"expected exactly ONE end-of-turn sweep, got {len(warnings)}"
    )
    findings = warnings[0].data["findings"]
    flagged = {f["file"] for f in findings}
    assert any("evil_a" in p for p in flagged), flagged
    assert any("evil_b" in p for p in flagged), flagged
    assert not any("good.py" in p for p in flagged), flagged
    assert sorted(warnings[0].data["files_checked"]) == sorted(targets)

    types_in_order = [e.type for e in state.turn_events]
    assert types_in_order[-1] == "turn_complete"


def test_end_of_turn_check_silent_when_no_files_written() -> Any:
    """A turn with no fs writes emits no end-of-turn ``tool_warning``."""
    turn = _import_turn()

    class _NoWriteChat:
        def __call__(self, *, messages: Any, tools: Any, model: Any) -> Any:
            return turn.ChatStep(text="just talking", tool_calls=[])

    result = turn.run_turn(
        user_message="say hi",
        conversation_id="conv_eot_none",
        chat_fn=_NoWriteChat(),
        tool_run=lambda n, a: {"ok": True, "data": None},
        list_tools_fn=_empty_tools,
        timeout=10.0,
    )

    state = turn.get_turn(result.turn_id)
    assert state is not None
    assert state.files_written == []
    warnings = [
        e
        for e in state.tool_events
        if e.type == "tool_warning"
        and e.data.get("source") == "wylde_check_end_of_turn"
    ]
    assert warnings == []


def test_end_of_turn_check_skipped_when_only_warning_findings(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> Any:
    """WARNING-severity per-file findings are dropped (they're low
    signal and would flood the tool stream).  Only ERRORs surface as a
    ``tool_warning`` event at end-of-turn."""
    turn = _import_turn()

    for mod_name in (
        "Wylde.Core.harness.dev.prewrite",
        "Core.harness.dev.prewrite",
    ):
        if mod_name in _sys.modules:
            monkeypatch.setattr(_sys.modules[mod_name], "wylde_root", lambda: tmp_path)
    for mod_name in (
        "Wylde.Core.harness.dev.wylde_check",
        "Core.harness.dev.wylde_check",
    ):
        if mod_name in _sys.modules:
            monkeypatch.setattr(_sys.modules[mod_name], "WYLDE_ROOT", tmp_path)

    # Wylde.Core.* import is a WARNING-severity finding from
    # check_import_paths — not an ERROR.
    warn_file = tmp_path / "Core" / "harness" / "warn_only.py"
    warn_file.parent.mkdir(parents=True, exist_ok=True)
    warn_file.write_text(
        "from Wylde.Core.shared import ipc\n",
        encoding="utf-8",
    )

    class _ScriptedChat:
        def __init__(self) -> None:
            self.calls = 0

        def __call__(self, *, messages: Any, tools: Any, model: Any) -> Any:
            self.calls += 1
            if self.calls == 1:
                return turn.ChatStep(
                    text="",
                    tool_calls=[
                        turn.ToolCall(
                            id="call_0",
                            name="fake_write",
                            args={"path": str(warn_file)},
                        ),
                    ],
                )
            return turn.ChatStep(text="done", tool_calls=[])

    def _runner(name: Any, args: Any) -> Any:
        turn.record_file_written(args["path"])
        return {"ok": True, "data": {"status": "success", "path": args["path"]}}

    result = turn.run_turn(
        user_message="write a warning file",
        conversation_id="conv_eot_warn",
        chat_fn=_ScriptedChat(),
        tool_run=_runner,
        list_tools_fn=_empty_tools,
        timeout=10.0,
    )
    assert result.aborted is False
    state = turn.get_turn(result.turn_id)
    eot_warnings = [
        e
        for e in state.tool_events
        if e.type == "tool_warning"
        and e.data.get("source") == "wylde_check_end_of_turn"
    ]
    assert eot_warnings == [], (
        f"warning-only files must not produce an end-of-turn event; got "
        f"{[e.data for e in eot_warnings]!r}"
    )
