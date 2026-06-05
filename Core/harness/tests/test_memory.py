"""Smoke for the three-layer memory architecture.

Same isolation as test_workspaces: tmp_path-backed DATA_DIR, fake
embedder. Covers long-term save/update/delete/search/history,
workspace memory, short-term append/get/clear, importance scoring,
supersession behaviour, reflection cycles.
"""

from __future__ import annotations

from typing import Any

import importlib
import sys
import time as _time
from pathlib import Path

import pytest

_HERE = Path(__file__).resolve()
_VAULT_ROOT = _HERE.parents[4]
if str(_VAULT_ROOT) not in sys.path:
    sys.path.insert(0, str(_VAULT_ROOT))


@pytest.fixture
def isolated_memory(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Any:
    data_dir = tmp_path / "data"
    monkeypatch.setenv("WYLDE_DATA_DIR", str(data_dir))
    monkeypatch.setenv("CONVERSATIONS_DIR", str(data_dir / "conversations"))

    import importlib as _importlib

    try:
        _common = _importlib.import_module("Core.harness.memory._common")
        embeddings = _importlib.import_module("Core.harness.memory.embeddings")
        long_term = _importlib.import_module("Core.harness.memory.long_term")
        # workspaces: removed in config-file-backed redesign (2026-06-05) —
        # Rust now owns the workspace registry/MRU index.
        workspace_memory = _importlib.import_module(
            "Core.harness.memory.workspace_memory"
        )
        conversation = _importlib.import_module("Core.harness.memory.conversation")
        scoring = _importlib.import_module("Core.harness.memory.scoring")
        reflection = _importlib.import_module("Core.harness.memory.reflection")
    except ImportError:
        _common = _importlib.import_module("Wylde.Core.harness.memory._common")
        embeddings = _importlib.import_module("Wylde.Core.harness.memory.embeddings")
        long_term = _importlib.import_module("Wylde.Core.harness.memory.long_term")
        workspace_memory = _importlib.import_module(
            "Wylde.Core.harness.memory.workspace_memory"
        )
        conversation = _importlib.import_module(
            "Wylde.Core.harness.memory.conversation"
        )
        scoring = _importlib.import_module("Wylde.Core.harness.memory.scoring")
        reflection = _importlib.import_module("Wylde.Core.harness.memory.reflection")
    # Reload _common first so DATA_DIR reflects the env vars above.
    # Then reload subpackage submodules so their module-level path
    # constants (REGISTRY_PATH, SETTINGS_PATH, INDEXES_DIR,
    # WORKSPACE_MEMORIES_DIR) re-read DATA_DIR — reloading just the
    # package shim leaves the submodules pointing at the previous
    # test's tmp dir, which used to leak test memory writes to the
    # real .wylde/data/ directory.
    importlib.reload(_common)
    importlib.reload(embeddings)
    # workspaces: removed in config-file-backed redesign (2026-06-05) —
    # only workspace_memory submodules need reloading now.
    for _name in (
        f"{workspace_memory.__name__}._store",
        f"{workspace_memory.__name__}._search",
        f"{workspace_memory.__name__}._curate",
    ):
        _sub = sys.modules.get(_name)
        if _sub is not None:
            importlib.reload(_sub)
    for mod in (
        scoring,
        conversation,
        long_term,
        workspace_memory,
        reflection,
    ):
        importlib.reload(mod)

    dim = _common.EMBED_DIM

    def _fake_embed(texts: Any) -> Any:
        out = []
        for t in texts:
            t = str(t or "")
            v = [0.1] * dim
            v[0] = (len(t) % 31) / 31.0
            v[1] = (sum(ord(c) for c in t[:8]) % 19) / 19.0
            out.append(v)
        return out

    monkeypatch.setattr(embeddings, "embed", _fake_embed)
    monkeypatch.setattr(embeddings, "embed_one", lambda t: _fake_embed([t])[0])

    return {
        "long_term": long_term,
        "workspace_memory": workspace_memory,
        "conversation": conversation,
        "scoring": scoring,
        "reflection": reflection,
    }


# ── Long-term ──────────────────────────────────────────────────────────


def test_long_term_save_search_roundtrip(isolated_memory: Any) -> None:
    pytest.importorskip("lancedb")
    lt = isolated_memory["long_term"]
    a = lt.save(
        "the Wylde user prefers pipes over loopback HTTP between services.",
        source="test",
        importance=9,
    )
    b = lt.save(
        "The MEMGRAPH default service name is wylde-memgraph.",
        source="test",
        importance=6,
    )
    records = lt.list_records()
    assert {r.id for r in records} == {a.id, b.id}
    # list_records sorts by importance desc, so a should be first.
    assert records[0].id == a.id

    hits = lt.search("memgraph default")
    assert hits, "expected at least one hit"
    assert hits[0]["id"] in {a.id, b.id}


def test_long_term_update_marks_supersession(isolated_memory: Any) -> None:
    lt = isolated_memory["long_term"]
    original = lt.save("first version", source="test", importance=5)
    revised = lt.update(original.id, body="second version", importance=7)
    assert revised is not None
    assert revised.id != original.id

    # Default list omits superseded records.
    visible = {r.id for r in lt.list_records()}
    assert revised.id in visible
    assert original.id not in visible

    # include_superseded surfaces both.
    full = {r.id for r in lt.list_records(include_superseded=True)}
    assert {original.id, revised.id} <= full

    # history walks the chain.
    chain = lt.history(original.id)
    chain_ids = [r.id for r in chain]
    assert original.id in chain_ids
    assert revised.id in chain_ids


def test_long_term_importance_clamped(isolated_memory: Any) -> None:
    lt = isolated_memory["long_term"]
    a = lt.save("ten", source="t", importance=99)  # clamped to 10
    b = lt.save("zero", source="t", importance=-3)  # clamped to >= 1
    c = lt.save("missing", source="t", importance=None)  # heuristic
    assert a.importance == 10
    assert b.importance >= 1
    assert 1 <= c.importance <= 8


def test_long_term_delete_removes_chain(isolated_memory: Any) -> None:
    lt = isolated_memory["long_term"]
    a = lt.save("first", source="t", importance=5)
    b = lt.update(a.id, body="second", importance=6)
    assert b is not None
    # Deleting the new record should also drop predecessors that
    # superseded into it.
    assert lt.delete(b.id)
    remaining = {r.id for r in lt.list_records(include_superseded=True)}
    assert a.id not in remaining
    assert b.id not in remaining


# ── Workspace memory ───────────────────────────────────────────────────
#
# workspaces: removed in config-file-backed redesign (2026-06-05) —
# the workspace registry/MRU index (activate, list_workspaces,
# delete_workspace, INDEXES_DIR eviction) moved to Rust, so the tests
# that drove it through the Python `workspaces` module were removed:
#   - test_workspace_memory_isolated_per_workspace
#   - test_workspace_memory_persists_across_eviction
#   - test_explicit_workspace_delete_removes_both
# (workspace_memory itself still exists; its curation behaviour is
# covered indirectly via the reflection tests below.)


# ── Short-term ─────────────────────────────────────────────────────────


def test_short_term_append_get_clear(isolated_memory: Any) -> None:
    conv = isolated_memory["conversation"]
    cid = "short_term_test_1"
    conv.append_working_memory(cid, {"kind": "tool", "data": {"name": "git_status"}})
    conv.append_working_memory(cid, {"kind": "decision", "data": "use SQLite"})
    entries = conv.get_working_memory(cid)
    assert len(entries) == 2
    assert entries[0]["kind"] == "tool"
    assert entries[1]["kind"] == "decision"

    assert conv.clear_working_memory(cid)
    assert conv.get_working_memory(cid) == []


def test_short_term_persists_across_in_memory_close(isolated_memory: Any) -> None:
    """Short-term lives on the conversation's JSON record on disk —
    same place chat history lives. So an "app restart" (which means
    `read_conversation` reloading the file from scratch) must surface
    the same entries that were appended before the restart.

    the Wylde user explicitly confirmed: short-term DIES with the conversation
    (when ``delete_conversation`` is called), but survives normal app
    close + reopen. This test asserts both halves.
    """
    import importlib

    conv = isolated_memory["conversation"]
    cid = "persist_test_1"

    conv.append_working_memory(cid, {"kind": "tool", "data": {"name": "fs_read"}})
    conv.append_working_memory(cid, {"kind": "decision", "data": "use lancedb"})
    conv.append_working_memory(cid, {"kind": "summary", "data": "found the bug"})

    # Simulate app restart: reload the module so any in-memory caches
    # are cleared. The JSON file on disk is the durable state.
    importlib.reload(conv)

    after_reload = conv.get_working_memory(cid)
    assert len(after_reload) == 3
    kinds = [e["kind"] for e in after_reload]
    assert kinds == ["tool", "decision", "summary"]

    # The full document also reads back without error.
    doc = conv.read_conversation(cid)
    assert isinstance(doc.get("working_memory"), list)
    assert len(doc["working_memory"]) == 3

    # Short-term DIES with the conversation (intentional — see docstring).
    conv.delete_conversation(cid)
    importlib.reload(conv)
    assert conv.get_working_memory(cid) == []


# workspaces: removed in config-file-backed redesign (2026-06-05) —
# test_workspace_curation_supersedes_stale and
# test_workspace_curation_skipped_without_chat_fn drove curation through
# a workspace activated via the deleted `workspaces` module; removed with
# that module. (workspace_memory.curate itself is unchanged.)


def test_set_workspace_binding(isolated_memory: Any) -> None:
    conv = isolated_memory["conversation"]
    cid = "bind_test"
    assert conv.get_workspace(cid) == ""
    conv.set_workspace(cid, "ws_xyz")
    assert conv.get_workspace(cid) == "ws_xyz"


# ── Reflection ─────────────────────────────────────────────────────────


def test_reflection_no_chat_fn_skipped(isolated_memory: Any) -> None:
    refl = isolated_memory["reflection"]
    result = refl.reflect("long_term")
    assert result.skipped
    assert "no chat_fn" in result.skip_reason.lower()


def test_reflection_synthesises_inputs(isolated_memory: Any) -> Any:
    """Synthetic chat_fn returns a fixed reflection; we expect a new
    long-term memory with importance >= the inputs', and the inputs to
    be marked superseded by the reflection."""
    lt = isolated_memory["long_term"]
    refl = isolated_memory["reflection"]

    a = lt.save("the Wylde user likes monospaced fonts in the editor.", source="t", importance=4)
    b = lt.save("the Wylde user prefers Iosevka over Fira Code.", source="t", importance=5)
    c = lt.save("the Wylde user sometimes uses JetBrains Mono.", source="t", importance=3)
    inputs_before = {a.id, b.id, c.id}

    # Synthetic chat_fn — ignores inputs, returns a canned reflection.
    class _Step:
        def __init__(self, text: Any) -> None:
            self.text = text

    def _chat(*, messages: Any, tools: Any, model: Any, **_kw: Any) -> Any:
        return _Step(
            "the Wylde user has settled font preferences favouring monospaced families with Iosevka as the lead choice."
        )

    result = refl.reflect("long_term", chat_fn=_chat, min_inputs=2, window_days=999)
    assert not result.skipped, f"reflection skipped: {result.skip_reason}"
    assert result.reflection_id
    assert result.inputs_considered == 3
    assert set(result.superseded_ids) == inputs_before

    # The reflection record should now be visible in the active list,
    # the inputs should not be (they're superseded).
    visible = {r.id for r in lt.list_records()}
    assert result.reflection_id in visible
    for old in inputs_before:
        assert old not in visible

    # And include_superseded surfaces them again.
    full = {r.id for r in lt.list_records(include_superseded=True)}
    assert inputs_before <= full


# workspaces: removed in config-file-backed redesign (2026-06-05) —
# test_conversation_reflection_promotes_to_workspace activated a workspace
# via the deleted `workspaces` module to bind a conversation to it; removed
# with that module. The no-workspace reflection path is still covered by
# test_conversation_reflection_promotes_to_long_term_when_no_workspace below.


def test_conversation_reflection_promotes_to_long_term_when_no_workspace(
    isolated_memory: Any,
) -> None:
    """Without a workspace binding, the synthesis lands in long-term
    so the durable insight isn't lost."""
    pytest.importorskip("lancedb")
    refl = isolated_memory["reflection"]
    conv = isolated_memory["conversation"]
    lt = isolated_memory["long_term"]

    cid = "conv_no_ws"
    conv.append_working_memory(cid, {"kind": "tool", "data": {"name": "git_log"}})
    conv.append_working_memory(
        cid, {"kind": "decision", "data": "split the PR by concern"}
    )
    conv.append_working_memory(
        cid, {"kind": "summary", "data": "review pass identified 2 follow-ups"}
    )

    class _Step:
        def __init__(self, text: Any) -> None:
            self.text = text

    SYNTH = "the Wylde user prefers PRs split by concern; review passes commonly surface follow-up tasks."

    def _chat(*, messages: Any, tools: Any, model: Any, **_kw: Any) -> Any:
        return _Step(SYNTH)

    result = refl.reflect(f"conversation:{cid}", chat_fn=_chat, min_inputs=2)
    assert not result.skipped, f"reflection skipped: {result.skip_reason}"
    assert result.reflection_id

    visible = {r.id: r for r in lt.list_records()}
    assert result.reflection_id in visible
    saved = visible[result.reflection_id]
    assert saved.body == SYNTH
    # Long-term reflections carry the REFLECTION_TAG so future reflection
    # rounds skip them (no infinite escalator).
    assert refl.REFLECTION_TAG in saved.tags

    # Working memory entries marked superseded.
    raw = conv.read_conversation(cid)["working_memory"]
    assert all(e.get("superseded_by") == result.reflection_id for e in raw)


def test_conversation_reflection_skips_if_already_superseded(
    isolated_memory: Any,
) -> Any:
    """Running the same reflection twice doesn't double-count: the
    second call sees no fresh inputs and returns skipped."""
    refl = isolated_memory["reflection"]
    conv = isolated_memory["conversation"]

    cid = "conv_double_reflect"
    conv.append_working_memory(cid, {"kind": "tool", "data": {"name": "a"}})
    conv.append_working_memory(cid, {"kind": "tool", "data": {"name": "b"}})
    conv.append_working_memory(cid, {"kind": "decision", "data": "ship it"})

    class _Step:
        def __init__(self, text: Any) -> None:
            self.text = text

    def _chat(*, messages: Any, tools: Any, model: Any, **_kw: Any) -> Any:
        return _Step("First-pass insight.")

    first = refl.reflect(f"conversation:{cid}", chat_fn=_chat, min_inputs=2)
    if not first.skipped:
        # If the first reflection consumed inputs, the second pass with
        # the same ``min_inputs`` floor must skip cleanly.
        second = refl.reflect(f"conversation:{cid}", chat_fn=_chat, min_inputs=2)
        assert second.skipped
        assert "need" in second.skip_reason.lower()


# ── Scoring math ───────────────────────────────────────────────────────


def test_scoring_combined_decays_with_age(isolated_memory: Any) -> None:
    sc = isolated_memory["scoring"]
    now = _time.time()
    fresh = sc.combined_score(0.5, 8, now, now=now, decay_days=10)
    old = sc.combined_score(
        0.5, 8, now - 10 * sc.SECONDS_PER_DAY, now=now, decay_days=10
    )
    assert fresh > old > 0.0


def test_scoring_normalize_handles_garbage(isolated_memory: Any) -> None:
    sc = isolated_memory["scoring"]
    assert sc.normalize_importance(7.4) == 7
    assert sc.normalize_importance("nine") in range(1, 11)  # heuristic
    assert sc.normalize_importance(
        None, "a longer body to push the heuristic up"
    ) in range(1, 9)
