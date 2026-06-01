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
        workspaces = _importlib.import_module("Core.harness.memory.workspaces")
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
        workspaces = _importlib.import_module("Wylde.Core.harness.memory.workspaces")
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
    for _name in (
        f"{workspaces.__name__}._mru",
        f"{workspaces.__name__}._store",
        f"{workspaces.__name__}._index",
        f"{workspaces.__name__}._search",
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
        workspaces,
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
        "workspaces": workspaces,
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


def test_workspace_memory_isolated_per_workspace(
    isolated_memory: Any, tmp_path: Path
) -> None:
    ws = isolated_memory["workspaces"]
    wm = isolated_memory["workspace_memory"]

    fa = tmp_path / "ws_A"
    fa.mkdir()
    (fa / "f.txt").write_text("alpha", encoding="utf-8")
    fb = tmp_path / "ws_B"
    fb.mkdir()
    (fb / "f.txt").write_text("beta", encoding="utf-8")

    ra = ws.activate(str(fa))
    rb = ws.activate(str(fb))

    wm.save(ra.id, "memory for A only", importance=5)
    wm.save(rb.id, "memory for B only", importance=5)

    a_rows = wm.list_records(ra.id)
    b_rows = wm.list_records(rb.id)
    assert len(a_rows) == 1 and a_rows[0].body == "memory for A only"
    assert len(b_rows) == 1 and b_rows[0].body == "memory for B only"


def test_workspace_memory_persists_across_eviction(
    isolated_memory: Any, tmp_path: Path
) -> None:
    """MRU eviction deletes the index folder but workspace memory
    survives at ``workspace_memories/<slug>/`` — that's the durable
    storage the Wylde user locked in the design correction. The LLM's curated
    insights about a project shouldn't die just because the file
    cache was evicted."""
    ws = isolated_memory["workspaces"]
    wm = isolated_memory["workspace_memory"]

    folder = tmp_path / "ws_durable"
    folder.mkdir()
    (folder / "x.txt").write_text("the file", encoding="utf-8")
    record = ws.activate(str(folder))
    durable_slug = record.id

    # Drop a memory entry into the durable workspace memory store.
    saved = wm.save(
        durable_slug, "key insight about this project", importance=7, source="test"
    )
    memory_dir = wm.WORKSPACE_MEMORIES_DIR / durable_slug
    assert memory_dir.exists(), "durable memory dir should exist after save"

    # Activate 5 more workspaces — the durable one falls off the MRU.
    for i in range(5):
        f = tmp_path / f"filler_{i}"
        f.mkdir()
        (f / "x.txt").write_text(f"filler {i}", encoding="utf-8")
        ws.activate(str(f))

    # Confirm eviction: the durable workspace is no longer in the registry.
    remaining_ids = {w.id for w in ws.list_workspaces()}
    assert durable_slug not in remaining_ids, "durable workspace should be evicted"

    # Index folder must be gone (eviction removed it).
    assert not (ws.INDEXES_DIR / durable_slug).exists(), (
        "index folder should be removed by MRU eviction"
    )

    # But the durable memory folder MUST still exist.
    assert memory_dir.exists(), "workspace memory folder must survive MRU eviction"

    # And the saved entry is still readable.
    survivors = wm.list_records(durable_slug)
    assert any(r.id == saved.id for r in survivors), (
        "saved memory entry should survive eviction"
    )


def test_explicit_workspace_delete_removes_both(
    isolated_memory: Any, tmp_path: Path
) -> None:
    """The explicit-delete path (user clicks "remove this workspace")
    should take BOTH the index folder AND the durable memory folder."""
    ws = isolated_memory["workspaces"]
    wm = isolated_memory["workspace_memory"]

    folder = tmp_path / "ws_kill"
    folder.mkdir()
    (folder / "x.txt").write_text("doomed", encoding="utf-8")
    record = ws.activate(str(folder))
    wm.save(
        record.id, "memory in soon-to-be-deleted workspace", importance=5, source="test"
    )

    index_dir = ws.INDEXES_DIR / record.id
    memory_dir = wm.WORKSPACE_MEMORIES_DIR / record.id
    assert index_dir.exists() or memory_dir.exists()

    assert ws.delete_workspace(record.id)

    # Both must be gone.
    assert not index_dir.exists(), "index folder should be removed"
    assert not memory_dir.exists(), "durable memory folder should be removed"
    assert not any(w.id == record.id for w in ws.list_workspaces())


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


def test_workspace_curation_supersedes_stale(
    isolated_memory: Any, tmp_path: Path
) -> Any:
    """Synthetic chat_fn votes ``supersede`` on every memory whose body
    starts with ``Stale:`` and ``keep`` on the rest. The curator must
    mark exactly the staled ones with a tombstone supersession (audit
    trail intact, hidden from default retrieval, visible via
    ``include_superseded=True``)."""
    ws = isolated_memory["workspaces"]
    wm = isolated_memory["workspace_memory"]

    folder = tmp_path / "ws_curate"
    folder.mkdir()
    (folder / "x.txt").write_text("anything", encoding="utf-8")
    record = ws.activate(str(folder))

    bodies = [
        "the Wylde user prefers tabs over spaces.",
        "Stale: the legacy rag service is at port 5000.",
        "The harness uses lancedb for vector search.",
        "Stale: NSSM is required to install services on Windows.",
        "Workspaces are MRU-capped at 5.",
        "Stale: fletch-web is the canonical HTTP gateway.",
    ]
    saved = [wm.save(record.id, b, importance=5, source="test") for b in bodies]

    class _Step:
        def __init__(self, text: Any) -> None:
            self.text = text

    def fake_chat(*, messages: Any, tools: Any, model: Any, **_kw: Any) -> Any:
        # Pull the user prompt; emit one verdict per indexed line.
        user_msg = messages[-1]["content"] if messages else ""
        out = []
        for raw in user_msg.splitlines():
            raw = raw.strip()
            if not raw or "." not in raw:
                continue
            try:
                idx = int(raw.split(".", 1)[0])
            except ValueError:
                continue
            if "Stale:" in raw:
                out.append(
                    f'{{"index": {idx}, "verdict": "supersede", '
                    f'"reason": "no longer relevant"}}'
                )
            else:
                out.append(f'{{"index": {idx}, "verdict": "keep"}}')
        return _Step("\n".join(out))

    result = wm.curate(record.id, chat_fn=fake_chat)
    assert not result.skipped, f"curation skipped: {result.skip_reason}"
    assert result.inputs_considered == 6

    superseded_ids = {s["old_id"] for s in result.superseded}
    expected_stale = {s.id for s, b in zip(saved, bodies) if b.startswith("Stale:")}
    assert superseded_ids == expected_stale, (
        f"expected {expected_stale}, got {superseded_ids}"
    )

    # Default list hides them.
    visible_default = {r.id for r in wm.list_records(record.id)}
    assert visible_default.isdisjoint(expected_stale)

    # include_superseded surfaces them — audit trail intact.
    visible_full = {r.id for r in wm.list_records(record.id, include_superseded=True)}
    assert expected_stale <= visible_full

    # Tombstone supersession pointer.
    for old_id in expected_stale:
        rec = wm.get(record.id, old_id)
        assert rec is not None
        assert rec.superseded_by.startswith("tombstone:"), (
            f"expected tombstone supersession, got {rec.superseded_by!r}"
        )


def test_workspace_curation_skipped_without_chat_fn(
    isolated_memory: Any, tmp_path: Path
) -> None:
    wm = isolated_memory["workspace_memory"]
    ws = isolated_memory["workspaces"]
    folder = tmp_path / "ws_curate_noop"
    folder.mkdir()
    (folder / "x.txt").write_text("anything", encoding="utf-8")
    record = ws.activate(str(folder))
    wm.save(record.id, "anything", importance=5)

    result = wm.curate(record.id, chat_fn=None)
    assert result.skipped
    assert "no chat_fn" in result.skip_reason.lower()


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


def test_conversation_reflection_promotes_to_workspace(
    isolated_memory: Any, tmp_path: Path
) -> Any:
    """A conversation bound to a workspace consolidates its working
    memory into THAT workspace's memory store; long-term stays clean."""
    pytest.importorskip("lancedb")
    refl = isolated_memory["reflection"]
    conv = isolated_memory["conversation"]
    ws = isolated_memory["workspaces"]
    wm = isolated_memory["workspace_memory"]
    lt = isolated_memory["long_term"]

    folder = tmp_path / "ws_conv_reflect"
    folder.mkdir()
    (folder / "x.txt").write_text("any", encoding="utf-8")
    record = ws.activate(str(folder))

    cid = "conv_with_ws"
    conv.set_workspace(cid, record.id)
    conv.append_working_memory(cid, {"kind": "tool", "data": {"name": "fs_read"}})
    conv.append_working_memory(cid, {"kind": "decision", "data": "use lancedb"})
    conv.append_working_memory(
        cid, {"kind": "summary", "data": "found the bug in scorer"}
    )

    class _Step:
        def __init__(self, text: Any) -> None:
            self.text = text

    SYNTH = (
        "When debugging the harness scorer, prefer LanceDB-backed traces over fs reads."
    )

    def _chat(*, messages: Any, tools: Any, model: Any, **_kw: Any) -> Any:
        return _Step(SYNTH)

    result = refl.reflect(f"conversation:{cid}", chat_fn=_chat, min_inputs=2)
    assert not result.skipped, f"reflection skipped: {result.skip_reason}"
    assert result.reflection_id, "reflection should produce a record id"
    assert result.inputs_considered == 3
    assert result.reflection_body == SYNTH

    # The synthesis lives in workspace memory, NOT long-term.
    ws_records = wm.list_records(record.id)
    assert any(r.id == result.reflection_id for r in ws_records), (
        "synthesis missing from workspace memory"
    )
    saved = next(r for r in ws_records if r.id == result.reflection_id)
    assert saved.body == SYNTH
    assert saved.source.startswith("reflection:conversation:")
    assert saved.importance >= 7

    lt_records = lt.list_records(include_superseded=True)
    assert all(r.id != result.reflection_id for r in lt_records), (
        "synthesis should not have leaked into long-term"
    )

    # Working-memory entries are now flagged as superseded by the
    # reflection id — visible on the document but filtered from the
    # chat-turn short-term slot.
    raw = conv.read_conversation(cid)["working_memory"]
    assert len(raw) == 3
    for e in raw:
        assert e.get("superseded_by") == result.reflection_id, (
            f"entry {e} should be marked superseded"
        )


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
