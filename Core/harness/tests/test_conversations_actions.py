"""Smoke for the ``conversations.*`` pipe actions.

Drives the action handlers directly (no real pipe) against an isolated
``CONVERSATIONS_DIR`` so the tests don't touch ``~/.wylde/data``. The
persistence module is reloaded under the temp env var so its
module-level ``CONVERSATIONS_DIR`` constant picks up the override.
"""

from __future__ import annotations

import importlib
import sys
from pathlib import Path
from typing import Any, Generator

import pytest

_HERE = Path(__file__).resolve()
_VAULT_ROOT = _HERE.parents[4]
if str(_VAULT_ROOT) not in sys.path:
    sys.path.insert(0, str(_VAULT_ROOT))


@pytest.fixture
def isolated_conversations(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> Generator[Any, None, None]:
    """Point the conversation store at a fresh ``tmp_path`` and reload
    the modules that capture ``CONVERSATIONS_DIR`` at import time."""
    conv_dir = tmp_path / "conversations"
    monkeypatch.setenv("WYLDE_DATA_DIR", str(tmp_path / "data"))
    monkeypatch.setenv("CONVERSATIONS_DIR", str(conv_dir))
    try:
        _common = importlib.import_module("Core.harness.memory._common")
        conv = importlib.import_module("Core.harness.memory.conversation")
        harness_pipe = importlib.import_module("Core.harness.pipe")
    except ImportError:
        _common = importlib.import_module("Wylde.Core.harness.memory._common")
        conv = importlib.import_module("Wylde.Core.harness.memory.conversation")
        harness_pipe = importlib.import_module("Wylde.Core.harness.pipe")
    importlib.reload(_common)
    importlib.reload(conv)
    # pipe.py looks up the module lazily via _conv_module(), so a fresh
    # `conv` reference at the module level is enough.
    yield harness_pipe, conv


def test_conversations_new_returns_filename_safe_id(
    isolated_conversations: Any,
) -> None:
    harness_pipe, _ = isolated_conversations
    resp = harness_pipe._conversations_new_action(None)
    assert isinstance(resp.get("id"), str)
    assert resp["id"]
    # The minted id should be safe to feed back to read_conversation
    # (rejecting it would mean the format and the validator disagree).
    import re

    assert re.match(r"^[A-Za-z0-9_-]+$", resp["id"])


def test_conversations_list_empty(isolated_conversations: Any) -> None:
    harness_pipe, _ = isolated_conversations
    resp = harness_pipe._conversations_list_action(None)
    assert resp == {"conversations": [], "count": 0}


def test_conversations_list_returns_saved_chats(isolated_conversations: Any) -> None:
    harness_pipe, conv = isolated_conversations
    conv.save_conversation(
        conv_id="t1",
        messages=[{"role": "user", "content": "hi there"}],
    )
    conv.save_conversation(
        conv_id="t2",
        messages=[{"role": "user", "content": "second chat"}],
    )
    resp = harness_pipe._conversations_list_action(None)
    assert resp["count"] == 2
    ids = {c["id"] for c in resp["conversations"]}
    assert ids == {"t1", "t2"}
    # listing exposes a title (derived from the user message)
    titles = {c["title"] for c in resp["conversations"]}
    assert {"hi there", "second chat"} <= titles


def test_conversations_get_round_trips(isolated_conversations: Any) -> None:
    harness_pipe, conv = isolated_conversations
    conv.save_conversation(
        conv_id="abc",
        messages=[{"role": "user", "content": "the body"}],
    )
    resp = harness_pipe._conversations_get_action({"id": "abc"})
    assert resp["id"] == "abc"
    assert resp["messages"] == [{"role": "user", "content": "the body"}]


def test_conversations_get_missing_raises_not_found(
    isolated_conversations: Any,
) -> None:
    harness_pipe, _ = isolated_conversations
    with pytest.raises(Exception) as exc_info:
        harness_pipe._conversations_get_action({"id": "nope"})
    assert getattr(exc_info.value, "code", None) == "not_found"


def test_conversations_get_requires_id(isolated_conversations: Any) -> None:
    harness_pipe, _ = isolated_conversations
    with pytest.raises(Exception) as exc_info:
        harness_pipe._conversations_get_action({})
    assert getattr(exc_info.value, "code", None) == "bad_request"


def test_conversations_delete_removes_file(isolated_conversations: Any) -> None:
    harness_pipe, conv = isolated_conversations
    conv.save_conversation(
        conv_id="todelete",
        messages=[{"role": "user", "content": "x"}],
    )
    resp = harness_pipe._conversations_delete_action({"id": "todelete"})
    assert resp == {"ok": True, "id": "todelete"}
    # Calling again on the now-missing file returns ok=False, not an error.
    resp2 = harness_pipe._conversations_delete_action({"id": "todelete"})
    assert resp2 == {"ok": False, "id": "todelete"}


def test_conversations_actions_registered_on_pipe(isolated_conversations: Any) -> None:
    """Each handler must be reachable through the ``_ACTIONS`` dispatch
    table — this is what guards against silent ``action_not_found`` on
    the wire."""
    harness_pipe, _ = isolated_conversations
    for name in (
        "conversations.new",
        "conversations.list",
        "conversations.get",
        "conversations.delete",
    ):
        assert name in harness_pipe._ACTIONS, f"{name} missing from _ACTIONS"
