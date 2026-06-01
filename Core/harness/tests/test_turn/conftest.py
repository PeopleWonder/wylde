"""Shared fixtures and helpers for the ``test_turn`` test package.

Mirrors the production split of ``Core.harness.turn`` into per-phase
submodules: each ``test_*.py`` file exercises one submodule of the
driver. Boilerplate the legacy single-file ``test_turn.py`` repeated
(import-fallback shim, scripted chat helpers, isolated-data fixture)
lives here.
"""

from __future__ import annotations

import importlib
import sys
from pathlib import Path
from typing import Any, Generator

import pytest

# Pytest's rootdir scan from ``Wylde/`` doesn't add the parent to
# ``sys.path``, so tests that want ``Wylde.Core.X`` imports need to do
# it themselves. Done at conftest import time so every collected test
# in this package inherits the resolution.
_HERE = Path(__file__).resolve()
_VAULT_ROOT = _HERE.parents[5]
if str(_VAULT_ROOT) not in sys.path:
    sys.path.insert(0, str(_VAULT_ROOT))


def _import_turn() -> Any:
    """Resolve the turn module under either import root.

    Two import paths exist for the harness because the project supports
    being imported as ``Wylde.Core.X`` (when the parent of the vault is
    on ``sys.path``) and as ``Core.X`` (when the vault itself is the
    cwd). Tests must work in both shapes.
    """
    try:
        from Wylde.Core.harness import turn

        return turn
    except ImportError:
        from Core.harness import turn

        return turn


def _empty_tools() -> Any:
    """No-op tool catalog — most tests don't care about the registry."""
    return []


def _drive_one_turn_with(
    chat_fn: Any,
    *,
    conversation_id: Any,
    list_tools_fn: Any = None,
) -> Any:
    """Helper: drive one turn-cap-bounded run_turn with the supplied
    chat_fn and return the (state, result) pair so tests can assert on
    events without re-wiring boilerplate.
    """
    turn = _import_turn()
    result = turn.run_turn(
        user_message="test",
        conversation_id=conversation_id,
        chat_fn=chat_fn,
        tool_run=lambda n, a: {"ok": True, "data": None},
        list_tools_fn=list_tools_fn or _empty_tools,
        timeout=10.0,
    )
    state = turn.get_turn(result.turn_id)
    return state, result


def _scripted_chat_with_tool_call(turn: Any, tool_id: str, *, args: Any = None) -> Any:
    """ScriptedChat that returns one tool call then a finalising
    response. ``tool_id`` is whatever the runner is keyed by — both
    snake_case ids and dotted aliases resolve through the registry."""
    state = {"calls": 0}

    def _chat(*, messages: Any, tools: Any, model: Any, **_kw: Any) -> Any:
        state["calls"] += 1
        if state["calls"] == 1:
            return turn.ChatStep(
                text="",
                tool_calls=[
                    turn.ToolCall(
                        id="call_t1",
                        name=tool_id,
                        args=args or {},
                    )
                ],
            )
        return turn.ChatStep(text="done", tool_calls=[])

    return _chat


@pytest.fixture
def _isolated_data_for_extractor(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> Generator[Any, None, None]:
    """Point the harness memory layer at tmp dirs + reload modules so
    the extractor's writes don't leak into the user's real store."""
    data_dir = tmp_path / "data"
    monkeypatch.setenv("WYLDE_DATA_DIR", str(data_dir))
    monkeypatch.setenv("CONVERSATIONS_DIR", str(data_dir / "conversations"))

    try:
        _common = importlib.import_module("Core.harness.memory._common")
        embeddings = importlib.import_module("Core.harness.memory.embeddings")
        long_term = importlib.import_module("Core.harness.memory.long_term")
        workspaces = importlib.import_module("Core.harness.memory.workspaces")
        workspace_memory = importlib.import_module(
            "Core.harness.memory.workspace_memory"
        )
        conversation = importlib.import_module("Core.harness.memory.conversation")
        scoring = importlib.import_module("Core.harness.memory.scoring")
        post_turn_extractor = importlib.import_module(
            "Core.harness.memory.post_turn_extractor"
        )
        turn = importlib.import_module("Core.harness.turn")
    except ImportError:  # pragma: no cover
        _common = importlib.import_module("Wylde.Core.harness.memory._common")
        embeddings = importlib.import_module("Wylde.Core.harness.memory.embeddings")
        long_term = importlib.import_module("Wylde.Core.harness.memory.long_term")
        workspaces = importlib.import_module("Wylde.Core.harness.memory.workspaces")
        workspace_memory = importlib.import_module(
            "Wylde.Core.harness.memory.workspace_memory"
        )
        conversation = importlib.import_module("Wylde.Core.harness.memory.conversation")
        scoring = importlib.import_module("Wylde.Core.harness.memory.scoring")
        post_turn_extractor = importlib.import_module(
            "Wylde.Core.harness.memory.post_turn_extractor"
        )
        turn = importlib.import_module("Wylde.Core.harness.turn")
    for mod in (
        _common,
        embeddings,
        scoring,
        conversation,
        long_term,
        workspaces,
        workspace_memory,
        post_turn_extractor,
        turn,
    ):
        importlib.reload(mod)

    dim = _common.EMBED_DIM
    monkeypatch.setattr(embeddings, "embed", lambda texts: [[0.1] * dim for _ in texts])
    monkeypatch.setattr(embeddings, "embed_one", lambda t: [0.1] * dim)

    yield {
        "long_term": long_term,
        "workspace_memory": workspace_memory,
        "workspaces": workspaces,
        "conversation": conversation,
        "post_turn_extractor": post_turn_extractor,
        "turn": turn,
    }
    turn.install_post_turn_extractor_chat_fn(None, synchronous=False)
