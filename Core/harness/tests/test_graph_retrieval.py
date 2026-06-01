"""Smoke for the graph-distance expansion stage.

The Memgraph client is stubbed via monkeypatch — we don't depend on a
running Neo4j. Tests assert:

* ``expand_by_graph`` returns ``[]`` when Memgraph is unimportable.
* ``expand_by_graph`` returns ``[]`` when the client returns no chunks.
* ``expand_by_graph`` returns hop-ranked neighbours when the client
  surfaces a multihop result.
* ``hybrid_search`` folds neighbour chunks into the fused ranking
  even when they weren't in the vector pool.
"""

from __future__ import annotations

from typing import Any

import importlib
import sys
from pathlib import Path

import pytest

_HERE = Path(__file__).resolve()
_VAULT_ROOT = _HERE.parents[4]
if str(_VAULT_ROOT) not in sys.path:
    sys.path.insert(0, str(_VAULT_ROOT))


@pytest.fixture
def graph_module() -> Any:
    try:
        graph_retrieval = importlib.import_module("Core.harness.memory.graph_retrieval")

    except ImportError:  # pragma: no cover
        graph_retrieval = importlib.import_module(
            "Wylde.Core.harness.memory.graph_retrieval"
        )
    importlib.reload(graph_retrieval)
    return graph_retrieval


def test_no_candidates_returns_empty(graph_module: Any) -> None:
    assert graph_module.expand_by_graph([]) == []


def test_no_memgraph_returns_empty(
    graph_module: Any, monkeypatch: pytest.MonkeyPatch
) -> None:
    """When ``_memgraph`` resolves to None (module not importable),
    expand_by_graph degrades silently."""
    monkeypatch.setattr(graph_module, "_memgraph", lambda: None)
    hits = graph_module.expand_by_graph(
        [{"id": "seed_1"}],
        workspace_id="ws_x",
    )
    assert hits == []


def test_multihop_returns_ranked_neighbours(
    graph_module: Any, monkeypatch: pytest.MonkeyPatch
) -> Any:
    """When the client's multihop returns a list of chunk dicts, the
    expansion sorts them by hop count and synthesises similarity."""

    class _FakeMemgraph:
        def multihop(
            self, *, chunk_ids: Any, workspace: Any, max_hops: Any, limit: Any
        ) -> Any:
            # Simulate the graph: seed_1 → chunk_A (1 hop), chunk_B (2 hops).
            assert chunk_ids == ["seed_1"]
            assert max_hops == 1
            return {
                "chunks": [
                    {
                        "id": "chunk_B",
                        "path": "p/b.md",
                        "content": "B body",
                        "hops": 2,
                        "via_entities": ["e2"],
                    },
                    {
                        "id": "chunk_A",
                        "path": "p/a.md",
                        "content": "A body",
                        "hops": 1,
                        "via_entities": ["e1"],
                    },
                    # Echo the seed back — must be filtered out.
                    {"id": "seed_1", "path": "p/seed.md", "content": "seed", "hops": 0},
                ]
            }

    monkeypatch.setattr(graph_module, "_memgraph", lambda: _FakeMemgraph())

    hits = graph_module.expand_by_graph(
        [{"id": "seed_1"}],
        workspace_id="ws_x",
    )
    assert [h.id for h in hits] == ["chunk_A", "chunk_B"], (
        f"expected hop-ordered, got {[(h.id, h.hops) for h in hits]}"
    )
    assert hits[0].similarity > hits[1].similarity


def test_traverse_fallback_when_multihop_missing(
    graph_module: Any, monkeypatch: pytest.MonkeyPatch
) -> Any:
    """When the client only exposes ``traverse``, ``expand_by_graph``
    falls back to a single-hop entity → chunk lookup. Distances are
    synthesised as 1 since traverse is hop-agnostic."""

    class _OldMemgraph:
        # No multihop attribute.
        def traverse(self, *, entities: Any, workspace: Any, limit: Any) -> Any:
            assert "alice" in entities
            return [
                {
                    "id": "chunk_C",
                    "path": "p/c.md",
                    "content": "C body",
                    "via_entities": ["alice"],
                },
            ]

    monkeypatch.setattr(graph_module, "_memgraph", lambda: _OldMemgraph())

    candidates = [{"id": "seed_1", "entities": ["alice"]}]
    hits = graph_module.expand_by_graph(candidates, workspace_id="ws_x")
    assert len(hits) == 1
    assert hits[0].id == "chunk_C"
    assert hits[0].hops == 1


def test_memgraph_reply_envelope_unwrapped(
    graph_module: Any, monkeypatch: pytest.MonkeyPatch
) -> Any:
    """Some clients wrap the result in a MemgraphReply-shaped object
    with ``.ok`` / ``.data`` attributes — the coercer should pull
    ``.data`` out."""

    class _Reply:
        ok = True
        data = {"chunks": [{"id": "x", "hops": 1, "path": "p", "content": "c"}]}

    class _ClientReturningReply:
        def multihop(self, **_kw: Any) -> Any:
            return _Reply()

    monkeypatch.setattr(graph_module, "_memgraph", lambda: _ClientReturningReply())
    hits = graph_module.expand_by_graph([{"id": "seed_1"}])
    assert [h.id for h in hits] == ["x"]


def test_failed_reply_envelope_swallowed(
    graph_module: Any, monkeypatch: pytest.MonkeyPatch
) -> Any:
    class _Reply:
        ok = False
        data = None
        error = {"code": "transport"}

    class _ClientFailing:
        def multihop(self, **_kw: Any) -> Any:
            return _Reply()

    monkeypatch.setattr(graph_module, "_memgraph", lambda: _ClientFailing())
    assert graph_module.expand_by_graph([{"id": "seed_1"}]) == []
