"""Concurrency smoke for build_memory_block.

Each of the three retrieval branches (core_context / search /
workspace_memory.search) is patched to sleep 0.5s before returning
its (empty) result. Sequential execution would take >=1.5s; the
parallel implementation should finish well under 1s. We assert
< 1.0s wall-clock as the boundary — comfortable margin against
scheduler jitter without being so loose the test would pass even
if parallelism broke.
"""

from __future__ import annotations

import importlib
import sys
import time
from pathlib import Path
from typing import Any

import pytest


_HERE = Path(__file__).resolve()
_VAULT_ROOT = _HERE.parents[4]
if str(_VAULT_ROOT) not in sys.path:
    sys.path.insert(0, str(_VAULT_ROOT))


def _import_rag() -> Any:
    try:
        rag = importlib.import_module("Core.harness.memory.rag")

    except ImportError:  # pragma: no cover
        rag = importlib.import_module("Wylde.Core.harness.memory.rag")
    return rag


def test_build_memory_block_runs_retrievals_in_parallel(
    monkeypatch: pytest.MonkeyPatch,
) -> Any:
    rag = _import_rag()

    timestamps = {}

    def _slow_core(*a: Any, **kw: Any) -> Any:
        timestamps["core_start"] = time.monotonic()
        time.sleep(0.5)
        timestamps["core_end"] = time.monotonic()
        return "core block"

    def _slow_search(query: Any, *a: Any, **kw: Any) -> Any:
        timestamps["search_start"] = time.monotonic()
        time.sleep(0.5)
        timestamps["search_end"] = time.monotonic()
        return [{"memory_type": "semantic", "content": "search hit"}]

    monkeypatch.setattr(rag, "core_context", _slow_core)
    monkeypatch.setattr(rag, "search", _slow_search)

    # Stub workspace_memory.search via the import path build_memory_block
    # actually uses (lazy import inside the helper). Patch the module
    # so the lazy-import returns our stub.
    try:
        wm = importlib.import_module("Core.harness.memory.workspace_memory")
    except ImportError:  # pragma: no cover
        wm = importlib.import_module("Wylde.Core.harness.memory.workspace_memory")

    def _slow_ws(workspace_id: Any, query: Any, *a: Any, **kw: Any) -> Any:
        timestamps["ws_start"] = time.monotonic()
        time.sleep(0.5)
        timestamps["ws_end"] = time.monotonic()
        return [{"body": "ws hit"}]

    monkeypatch.setattr(wm, "search", _slow_ws)

    t0 = time.monotonic()
    out = rag.build_memory_block(
        "tell me everything",
        workspace_id="ws_test",
        deadline_s=5.0,
    )
    elapsed = time.monotonic() - t0

    # All three branches ran (we see all three start/end timestamps).
    assert {"core_start", "search_start", "ws_start"} <= set(timestamps), (
        f"missing branch timestamps: {timestamps!r}"
    )

    # Output contains content from each branch.
    assert "core block" in out
    assert "search hit" in out
    assert "ws hit" in out

    # Wall-clock should be ~0.5s, certainly under 1.0s. Sequential
    # execution would be >= 1.5s.
    assert elapsed < 1.0, (
        f"build_memory_block took {elapsed:.2f}s — looks sequential "
        f"(timestamps: {timestamps!r})"
    )

    # All three branches started before any of them finished — the
    # canonical signature of true parallelism.
    starts = [
        timestamps["core_start"],
        timestamps["search_start"],
        timestamps["ws_start"],
    ]
    ends = [timestamps["core_end"], timestamps["search_end"], timestamps["ws_end"]]
    assert max(starts) < min(ends), (
        f"branches didn't overlap: starts={starts!r} ends={ends!r}"
    )


def test_build_memory_block_one_branch_failing_others_complete(
    monkeypatch: pytest.MonkeyPatch,
) -> Any:
    """If one retrieval raises, the other two still produce output."""
    rag = _import_rag()

    def _broken_core(*a: Any, **kw: Any) -> None:
        raise RuntimeError("simulated core failure")

    def _ok_search(query: Any, *a: Any, **kw: Any) -> Any:
        return [{"memory_type": "semantic", "content": "search worked"}]

    monkeypatch.setattr(rag, "core_context", _broken_core)
    monkeypatch.setattr(rag, "search", _ok_search)

    try:
        wm = importlib.import_module("Core.harness.memory.workspace_memory")
    except ImportError:  # pragma: no cover
        wm = importlib.import_module("Wylde.Core.harness.memory.workspace_memory")
    monkeypatch.setattr(wm, "search", lambda *a, **kw: [{"body": "ws worked"}])

    out = rag.build_memory_block("anything", workspace_id="ws_x")
    assert "search worked" in out
    assert "ws worked" in out
    # core block is missing (it raised) — output doesn't contain it.
    assert "Persistent context" not in out


def test_build_memory_block_no_workspace_id_skips_workspace_branch(
    monkeypatch: pytest.MonkeyPatch,
) -> Any:
    """When workspace_id is empty, workspace_memory.search isn't called
    at all — no timeout cost from a branch we can't run."""
    rag = _import_rag()
    monkeypatch.setattr(rag, "core_context", lambda *a, **kw: "core x")
    monkeypatch.setattr(rag, "search", lambda *a, **kw: [])

    try:
        wm = importlib.import_module("Core.harness.memory.workspace_memory")
    except ImportError:  # pragma: no cover
        wm = importlib.import_module("Wylde.Core.harness.memory.workspace_memory")
    called = {"n": 0}

    def _ws_search(*a: Any, **kw: Any) -> Any:
        called["n"] += 1
        return []

    monkeypatch.setattr(wm, "search", _ws_search)

    out = rag.build_memory_block("anything")  # no workspace_id
    assert "core x" in out
    assert called["n"] == 0
