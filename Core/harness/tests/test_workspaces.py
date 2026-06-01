"""Smoke for workspace registry + per-workspace file index + MRU.

Tests run with a temp DATA_DIR so they don't touch the real
``.wylde/data/`` directory. Embedding is monkey-patched to a
deterministic fixed-vector function so we don't depend on Ollama.
"""

from __future__ import annotations

import importlib
import sys
from pathlib import Path
from typing import Any, List

import pytest

_HERE = Path(__file__).resolve()
_VAULT_ROOT = _HERE.parents[4]
if str(_VAULT_ROOT) not in sys.path:
    sys.path.insert(0, str(_VAULT_ROOT))


# ── Fixtures ───────────────────────────────────────────────────────────


@pytest.fixture
def isolated_data_dir(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Any:
    """Point all memory-layer paths at a fresh tmp_path and stub the
    embedder to a deterministic fixed vector. Reload the relevant
    modules so the env vars take effect (paths are read at import)."""
    data_dir = tmp_path / "data"
    monkeypatch.setenv("WYLDE_DATA_DIR", str(data_dir))
    monkeypatch.setenv("CONVERSATIONS_DIR", str(data_dir / "conversations"))

    # Reload the modules that read paths at import time, in the right
    # order (embeddings depends on _common, workspaces depends on _common
    # and embeddings).
    try:
        _common = importlib.import_module("Core.harness.memory._common")

        embeddings = importlib.import_module("Core.harness.memory.embeddings")

        workspaces = importlib.import_module("Core.harness.memory.workspaces")

    except ImportError:  # pragma: no cover
        _common = importlib.import_module("Wylde.Core.harness.memory._common")

        embeddings = importlib.import_module("Wylde.Core.harness.memory.embeddings")

        workspaces = importlib.import_module("Wylde.Core.harness.memory.workspaces")
    importlib.reload(_common)
    importlib.reload(embeddings)
    # Reload workspaces submodules BEFORE the package shim so their
    # module-level path constants (REGISTRY_PATH, SETTINGS_PATH,
    # INDEXES_DIR) re-read DATA_DIR from the freshly reloaded _common.
    # Reloading just the package's __init__.py would leave the
    # submodules pointing at the previous test's tmp dir, which used to
    # contaminate MRU-cap state across tests.
    _pkg = workspaces.__name__
    for _sub in (f"{_pkg}._mru", f"{_pkg}._store", f"{_pkg}._index", f"{_pkg}._search"):
        _mod = sys.modules.get(_sub)
        if _mod is not None:
            importlib.reload(_mod)
    importlib.reload(workspaces)

    # Stub embeddings to deterministic fixed vectors. The store cares
    # about dimensionality (EMBED_DIM) but not about quality for these
    # tests — every call returns a 1.0-magnitude vector with content
    # mixed in just enough to vary across inputs.
    dim = _common.EMBED_DIM

    def _fake_embed(texts: List[str]) -> List[List[float]]:
        out = []
        for t in texts:
            t = str(t or "")
            seed = (sum(ord(c) for c in t) % 7) / 10.0  # 0..0.6
            vec = [seed] * dim
            # Salt position 0 with a body-length variation so different
            # texts get distinguishable vectors.
            vec[0] = (len(t) % 31) / 31.0
            out.append(vec)
        return out

    def _fake_embed_one(text: str) -> List[float]:
        return _fake_embed([text])[0]

    monkeypatch.setattr(embeddings, "embed", _fake_embed)
    monkeypatch.setattr(embeddings, "embed_one", _fake_embed_one)

    yield workspaces


# ── Helpers ────────────────────────────────────────────────────────────


def _seed_folder(root: Path, files: dict) -> Path:
    """Create a workspace folder with the given filename → content map."""
    root.mkdir(parents=True, exist_ok=True)
    for relpath, content in files.items():
        target = root / relpath
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content, encoding="utf-8")
    return root


# ── Tests ──────────────────────────────────────────────────────────────


def test_activate_indexes_a_new_folder(isolated_data_dir: Any, tmp_path: Path) -> None:
    pytest.importorskip("lancedb")
    ws = isolated_data_dir
    folder = _seed_folder(
        tmp_path / "ws1",
        {
            "alpha.txt": "the quick brown fox",
            "subdir/beta.md": "lazy dogs and notes",
        },
    )

    record = ws.activate(str(folder))

    assert record.path == str(folder.resolve())
    assert record.id  # slug populated
    assert record.file_count >= 2

    # Search should find chunks now.
    hits = ws.search_files(record.id, "quick brown", limit=5)
    assert any("quick" in h["content"] or "brown" in h["content"] for h in hits)


def test_delta_refresh_skips_unchanged_files(
    isolated_data_dir: Any, tmp_path: Path
) -> None:
    pytest.importorskip("lancedb")
    ws = isolated_data_dir
    folder = _seed_folder(
        tmp_path / "ws_delta",
        {
            "a.txt": "alpha content",
            "b.txt": "beta content",
        },
    )
    ws.activate(str(folder))
    record = ws.list_workspaces()[0]
    pre = ws.search_files(record.id, "alpha", limit=5)
    assert pre, "alpha should be findable after first activate"

    # Touch only b.txt; alpha's chunk shouldn't be re-embedded but
    # delta should still notice b changed.
    import time as _t

    _t.sleep(0.02)
    (folder / "b.txt").write_text("beta UPDATED content", encoding="utf-8")
    ws.refresh_workspace(record.id)

    hits = ws.search_files(record.id, "UPDATED", limit=5)
    assert any("UPDATED" in (h.get("content") or "") for h in hits)


def test_full_reindex_drops_stale_rows(isolated_data_dir: Any, tmp_path: Path) -> None:
    pytest.importorskip("lancedb")
    ws = isolated_data_dir
    folder = _seed_folder(
        tmp_path / "ws_full",
        {
            "before.txt": "before content here",
        },
    )
    ws.activate(str(folder))
    record = ws.list_workspaces()[0]

    # Replace content + remove file to force a stale-row scenario.
    (folder / "before.txt").unlink()
    (folder / "after.txt").write_text("after content reset", encoding="utf-8")
    ws.reindex_workspace(record.id)

    hits = ws.search_files(record.id, "before", limit=10)
    # No row should mention 'before' after a clean reindex.
    assert not any("before" in (h.get("content") or "") for h in hits)


def test_mru_eviction_at_six(isolated_data_dir: Any, tmp_path: Path) -> None:
    ws = isolated_data_dir
    paths = []
    for i in range(6):
        f = _seed_folder(tmp_path / f"ws_{i}", {"only.txt": f"workspace {i}"})
        paths.append(f)
        ws.activate(str(f))

    workspaces_now = ws.list_workspaces()
    cap = ws.get_mru_limit()
    assert len(workspaces_now) == cap, (
        f"expected MRU cap {cap}, got {len(workspaces_now)}"
    )
    # The oldest workspace (index 0) is the one that should have been evicted.
    evicted_path = str(paths[0].resolve())
    remaining_paths = {w.path for w in workspaces_now}
    assert evicted_path not in remaining_paths

    # Its index folder must be gone from disk.
    evicted_index = ws.INDEXES_DIR
    found = list(evicted_index.iterdir()) if evicted_index.exists() else []
    # All remaining index dirs should still exist; the evicted one is missing.
    found_slugs = {p.name for p in found}
    remaining_slugs = {w.id for w in workspaces_now}
    assert found_slugs <= remaining_slugs, (
        f"unexpected leftover index dirs: {found_slugs - remaining_slugs}"
    )


def test_per_conversation_binding(
    isolated_data_dir: Any, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """activate() with conversation_id should persist the binding so
    the next turn against that conversation picks up the workspace."""
    folder = _seed_folder(tmp_path / "ws_bound", {"file.txt": "bound content"})

    # Drive activate() through the pipe layer — that's the surface
    # callers actually hit when they pass conversation_id.
    try:
        harness_pipe = importlib.import_module("Core.harness.pipe")
        conv = importlib.import_module("Core.harness.memory.conversation")
    except ImportError:
        harness_pipe = importlib.import_module("Wylde.Core.harness.pipe")
        conv = importlib.import_module("Wylde.Core.harness.memory.conversation")
    importlib.reload(conv)

    resp = harness_pipe._rag_workspaces_activate_action(
        {
            "path": str(folder),
            "conversation_id": "convo_bind_1",
        }
    )
    assert resp["id"]
    bound = conv.get_workspace("convo_bind_1")
    assert bound == resp["id"], f"expected {resp['id']}, got {bound!r}"


def test_persona_set_and_get(isolated_data_dir: Any, tmp_path: Path) -> None:
    ws = isolated_data_dir
    folder = _seed_folder(tmp_path / "ws_persona", {"a.txt": "anything"})
    ws.activate(str(folder))
    record = ws.list_workspaces()[0]

    assert ws.set_persona(record.id, "You are an architect reviewing this codebase.")
    assert "architect" in ws.get_persona(record.id)


def test_mru_limit_default_and_persistence(isolated_data_dir: Any) -> None:
    ws = isolated_data_dir
    # Fresh data dir → defaults.
    assert ws.get_mru_limit() == ws.MRU_LIMIT_DEFAULT

    # Setting a new value persists.
    ws.set_mru_limit(8)
    assert ws.get_mru_limit() == 8


def test_mru_limit_validates_input(isolated_data_dir: Any) -> None:
    ws = isolated_data_dir
    with pytest.raises(ValueError):
        ws.set_mru_limit(0)
    with pytest.raises(ValueError):
        ws.set_mru_limit(ws.MRU_LIMIT_MAX + 1)
    with pytest.raises(ValueError):
        ws.set_mru_limit("five")
    with pytest.raises(ValueError):
        ws.set_mru_limit(True)
    # Original setting unchanged after each rejection.
    assert ws.get_mru_limit() == ws.MRU_LIMIT_DEFAULT


def test_mru_limit_change_evicts_excess_workspaces(
    isolated_data_dir: Any, tmp_path: Path
) -> None:
    ws = isolated_data_dir

    # Seed at the default cap of 5 so we have a full registry.
    paths = []
    for i in range(5):
        f = _seed_folder(tmp_path / f"ws_evict_{i}", {"only.txt": f"workspace {i}"})
        paths.append(f)
        ws.activate(str(f))
    assert len(ws.list_workspaces()) == 5

    # Lower the cap to 3 — the two oldest should be evicted now,
    # their index folders removed, but workspace memory preserved.
    new_limit = ws.set_mru_limit(3)
    assert new_limit == 3

    remaining = ws.list_workspaces()
    assert len(remaining) == 3, f"expected 3 after shrink, got {len(remaining)}"
    # Most recently activated wins — the last 3 paths survive.
    surviving_paths = {w.path for w in remaining}
    expected_survivors = {str(p.resolve()) for p in paths[-3:]}
    assert surviving_paths == expected_survivors

    # Index folders for the evicted workspaces are gone.
    evicted_slugs = {ws._slug_for(str(p)) for p in paths[:2]}
    found_slugs = (
        {p.name for p in ws.INDEXES_DIR.iterdir()} if ws.INDEXES_DIR.exists() else set()
    )
    assert evicted_slugs.isdisjoint(found_slugs), (
        f"evicted slugs still on disk: {evicted_slugs & found_slugs}"
    )

    # Raising the cap doesn't evict anything.
    ws.set_mru_limit(10)
    after_raise = ws.list_workspaces()
    assert len(after_raise) == 3
    assert {w.path for w in after_raise} == expected_survivors
