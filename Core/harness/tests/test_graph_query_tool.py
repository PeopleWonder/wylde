"""Smoke for the graph_query meta-tool.

The tool now combines a vector pass (``rag.search``) with graph-
distance expansion (``graph_retrieval.expand_by_graph``). Tests
patch out the two backends with synthetic stubs so we don't need a
running Memgraph or a populated vector index, then assert:

* Pure vector hits appear with ``source="vector"`` and a
  ``combined_score`` derived from their similarity.
* Pure graph hits (chunks the vector pass missed but the graph
  surfaced) appear with ``source="graph"``.
* Hits that BOTH retrievers found get ``source="vector+graph"`` and
  a higher combined score than either pure source.
* Final ranking puts agreeing-retriever hits above pure-vector or
  pure-graph hits, all else equal.
* The legacy entity-only path still works when no ``q`` is given.
"""

from __future__ import annotations

import importlib
import sys
from pathlib import Path
from typing import Any

import pytest


_HERE = Path(__file__).resolve()
_VAULT_ROOT = _HERE.parents[4]
if str(_VAULT_ROOT) not in sys.path:
    sys.path.insert(0, str(_VAULT_ROOT))


def _import_graph_query() -> Any:
    try:
        graph_query = importlib.import_module(
            "Core.harness.tooling.tools.meta.graph_query.graph_query"
        )

    except ImportError:  # pragma: no cover
        graph_query = importlib.import_module(
            "Wylde.Core.harness.tooling.tools.meta.graph_query.graph_query"
        )
    return graph_query


def _import_graph_retrieval() -> Any:
    try:
        graph_retrieval = importlib.import_module("Core.harness.memory.graph_retrieval")

    except ImportError:  # pragma: no cover
        graph_retrieval = importlib.import_module(
            "Wylde.Core.harness.memory.graph_retrieval"
        )
    return graph_retrieval


def test_vector_only_no_graph_backend(monkeypatch: pytest.MonkeyPatch) -> None:
    """When the graph backend returns nothing, vector hits flow
    through with ``source="vector"`` and combined_score = alpha * sim."""
    gq = _import_graph_query()
    monkeypatch.setattr(
        gq.rag,
        "search",
        lambda q, **_: [
            {"id": "chunk_a", "similarity": 0.8, "body": "alpha"},
            {"id": "chunk_b", "similarity": 0.6, "body": "beta"},
        ],
    )
    monkeypatch.setattr(
        gq.graph_retrieval, "expand_by_graph", lambda candidates, **_: []
    )

    result = gq.run_graph_query({"q": "anything", "limit": 5})

    assert result["count"] == 2
    assert result["source"] == "hybrid"
    ids = [r["id"] for r in result["results"]]
    assert ids == ["chunk_a", "chunk_b"]  # higher similarity first
    for r in result["results"]:
        assert r["source"] == "vector"
        assert r["graph_hops"] is None
        assert r["combined_score"] > 0


def test_graph_expansion_adds_neighbours(monkeypatch: pytest.MonkeyPatch) -> Any:
    """Graph-only hits (chunks the vector pass missed) appear with
    ``source="graph"`` and a hop-count."""
    gq = _import_graph_query()
    gr = _import_graph_retrieval()

    monkeypatch.setattr(
        gq.rag,
        "search",
        lambda q, **_: [
            {"id": "seed", "similarity": 0.7, "body": "the seed"},
        ],
    )

    def _fake_expand(candidates: Any, **kw: Any) -> Any:
        return [
            gr.GraphHit(
                id="neighbour_1",
                path="p1",
                content="c1",
                hops=1,
                via_entities=["e1"],
                similarity=0.5,
            ),
            gr.GraphHit(
                id="neighbour_2",
                path="p2",
                content="c2",
                hops=2,
                via_entities=["e2"],
                similarity=0.33,
            ),
        ]

    monkeypatch.setattr(gq.graph_retrieval, "expand_by_graph", _fake_expand)

    result = gq.run_graph_query({"q": "anything", "limit": 10})

    ids = [r["id"] for r in result["results"]]
    assert "seed" in ids
    assert "neighbour_1" in ids
    assert "neighbour_2" in ids

    by_id = {r["id"]: r for r in result["results"]}
    assert by_id["seed"]["source"] == "vector"
    assert by_id["neighbour_1"]["source"] == "graph"
    assert by_id["neighbour_1"]["graph_hops"] == 1
    assert by_id["neighbour_2"]["graph_hops"] == 2

    # vector_seeds reports the hits we sent into expansion.
    assert result["vector_seeds"] == [{"id": "seed", "similarity": 0.7}]


def test_agreement_bonus_lifts_dual_source_hits(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A chunk that BOTH retrievers surface lands above otherwise-
    similar pure-vector or pure-graph hits — the +0.05 agreement
    bonus does the work."""
    gq = _import_graph_query()
    gr = _import_graph_retrieval()

    monkeypatch.setattr(
        gq.rag,
        "search",
        lambda q, **_: [
            {"id": "agreement", "similarity": 0.5, "body": "shared"},
            {"id": "vector_only", "similarity": 0.6, "body": "vec"},
        ],
    )
    monkeypatch.setattr(
        gq.graph_retrieval,
        "expand_by_graph",
        lambda c, **_: [
            gr.GraphHit(
                id="agreement", path="p", content="shared", hops=1, similarity=0.5
            ),
            gr.GraphHit(id="graph_only", path="g", content="g", hops=1, similarity=0.5),
        ],
    )

    result = gq.run_graph_query({"q": "anything"})
    by_id = {r["id"]: r for r in result["results"]}
    assert by_id["agreement"]["source"] == "vector+graph"
    # Agreement-bonus pushes 'agreement' above 'vector_only' even though
    # vector_only's vector_similarity is higher (0.6 vs 0.5).
    ranks = [r["id"] for r in result["results"]]
    assert ranks.index("agreement") < ranks.index("vector_only")


def test_explicit_entities_path_skips_vector(monkeypatch: pytest.MonkeyPatch) -> None:
    """When the caller passes ``entities`` directly, no vector pass
    runs; the result envelope's ``vector_seeds`` is empty."""
    gq = _import_graph_query()

    def _no_vector(*a: Any, **kw: Any) -> None:
        raise AssertionError("rag.search should NOT be called on entity-only path")

    monkeypatch.setattr(gq.rag, "search", _no_vector)

    # Stub the memgraph client to simulate a successful traverse.
    monkeypatch.setattr(
        gq,
        "_via_memgraph",
        lambda entities, max_hops, limit: {
            "entities": entities,
            "results": [{"id": "x", "content": "c"}],
            "count": 1,
            "source": "memgraph",
        },
    )

    result = gq.run_graph_query({"entities": ["alice"], "max_hops": 2})
    assert result["source"] == "memgraph"
    assert result["count"] == 1
    assert result["vector_seeds"] == []


def test_no_q_no_entities_errors(monkeypatch: pytest.MonkeyPatch) -> None:
    gq = _import_graph_query()
    result = gq.run_graph_query({})
    assert "error" in result
    assert "either" in result["error"].lower()
