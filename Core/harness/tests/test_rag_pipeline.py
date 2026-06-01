"""Tests for the RAG pipeline orchestrator (``rag_pipeline.ask``).

Exercises the full compose — cache → decompose → retrieve (single- or
multi-hop) → rerank → score gate → miss-log — against a real LanceDB-backed
workspace index.

Two things are stubbed to keep the suite fast and hermetic:

* The cross-encoder rerank is replaced with a deterministic fake. The real
  ``sentence-transformers`` model is heavyweight and exercised (or skipped)
  by ``test_retrieval``; here we only need a predictable reorder, and the
  gate-firing test needs to drive scores negative on demand.
* Embeddings use a deterministic pseudo-random fake — identical text embeds
  identically (so the semantic cache can hit), different text embeds near-
  orthogonally (so it cannot wrongly hit).

The HyDE / decomposition / multi-hop LLM helpers are driven by a synthetic
``chat_fn`` that branches on the system prompt.
"""

from __future__ import annotations

import importlib
import math
import random
import sys
from pathlib import Path
from types import SimpleNamespace
from typing import Any, Callable, Dict, List

import pytest

_HERE = Path(__file__).resolve()
_VAULT_ROOT = _HERE.parents[4]
if str(_VAULT_ROOT) not in sys.path:
    sys.path.insert(0, str(_VAULT_ROOT))


# ── Synthetic chat backend ─────────────────────────────────────────────


class _Step:
    """Minimal stand-in for the harness ``ChatStep`` — only ``.text`` is read
    by HyDE / decompose / multi-hop."""

    def __init__(self, text: str) -> None:
        self.text = text
        self.thinking = ""
        self.tool_calls: List[Any] = []


def make_chat_fn(*, follow_up: str = "memgraph bolt port") -> Callable[..., Any]:
    """Build a synthetic ``chat_fn`` that answers each pipeline LLM helper.

    Branches on the system prompt: the gap prompt gets a follow-up query
    (or ``NONE`` when ``follow_up`` is empty), the decompose prompt gets a
    two-element JSON array, the HyDE prompt gets a hypothetical passage.
    """

    def chat_fn(*, messages: Any, tools: Any, model: Any, **_: Any) -> _Step:
        system = ""
        if messages:
            system = str(messages[0].get("content") or "").lower()
        if "gap" in system or "follow-up search" in system:
            return _Step(follow_up if follow_up else "NONE")
        if "named entit" in system:  # entity NER (soft addressing)
            return _Step("Lifecycle daemon\nshutdown\nservices")
        if "sub-quer" in system:
            return _Step('["shut the services down", "drain the running daemons"]')
        if "plausible" in system:  # HyDE
            return _Step("The lifecycle daemon drains every running service cleanly.")
        return _Step("")

    return chat_fn


# ── Deterministic rerank fakes ─────────────────────────────────────────


def _passthrough_rerank(query: Any, hits: Any, **_: Any) -> Any:
    """Rerank that preserves order + scores — keeps the gate satisfied."""
    return list(hits)


def _negative_rerank(query: Any, hits: Any, **_: Any) -> Any:
    """Rerank that drives every score negative — forces the gate to fire."""
    out = list(hits)
    for h in out:
        h.score = -5.0
        h.rerank_score = -5.0
    return out


# ── Workspace + reloaded-module fixture ────────────────────────────────


def _det_seed(text: str) -> int:
    """Stable 64-bit FNV-1a hash so identical text seeds an identical RNG."""
    h = 1469598103934665603
    for ch in text:
        h = ((h ^ ord(ch)) * 1099511628211) % (2**64)
    return h


@pytest.fixture
def pipeline_env(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Any:
    pytest.importorskip("lancedb")
    data_dir = tmp_path / "data"
    monkeypatch.setenv("WYLDE_DATA_DIR", str(data_dir))
    monkeypatch.setenv("CONVERSATIONS_DIR", str(data_dir / "conversations"))

    # Resolve the memory package prefix — "Core.*" when Wylde/ is on
    # sys.path, "Wylde.Core.*" otherwise (standalone-file pytest runs).
    try:
        importlib.import_module("Core.harness.memory._common")
        prefix = "Core.harness.memory"
    except ImportError:  # pragma: no cover
        prefix = "Wylde.Core.harness.memory"

    # Reload in dependency order so each module re-reads WYLDE_DATA_DIR and
    # binds to the freshly-reloaded siblings.
    names = [
        "_common",
        "embeddings",
        "workspaces._store",
        "workspaces._mru",
        "workspaces._index",
        "workspaces._search",
        "workspaces",
        "miss_log",
        "memgraph",
        "graph_retrieval",
        "retrieval",
        "rag_gate",
        "rag_cache",
        "rag_decompose",
        "rag_entities",
        "rag_multihop",
        "rag_feedback",
        "rag_pipeline",
    ]
    mods = {}
    for name in names:
        mod = importlib.import_module(f"{prefix}.{name}")
        importlib.reload(mod)
        mods[name] = mod

    _common = mods["_common"]
    embeddings = mods["embeddings"]
    workspaces = mods["workspaces"]
    dim = _common.EMBED_DIM

    def _fake_embed(texts: Any) -> Any:
        """Deterministic unit vectors — identical text → identical vector,
        different text → near-orthogonal."""
        out = []
        for t in texts:
            rng = random.Random(_det_seed(str(t or "").strip().lower()))
            v = [rng.gauss(0.0, 1.0) for _ in range(dim)]
            norm = math.sqrt(sum(x * x for x in v)) or 1.0
            out.append([x / norm for x in v])
        return out

    monkeypatch.setattr(embeddings, "embed", _fake_embed)
    monkeypatch.setattr(embeddings, "embed_one", lambda t: _fake_embed([t])[0])

    # Default-stub the Memgraph write so the suite never touches the
    # network — the feedback tests override this with their own recorder.
    monkeypatch.setattr(
        mods["memgraph"],
        "upsert_edge",
        lambda *a, **k: SimpleNamespace(ok=False, data=None, error=None),
    )

    folder = tmp_path / "code"
    folder.mkdir()
    (folder / "lifecycle.py").write_text(
        "def shutdown_all(): pass  # gracefully drains every running service",
        encoding="utf-8",
    )
    (folder / "harness.md").write_text(
        "The harness pipe exposes chat.start_turn / chat.run_turn / chat.stream_turn",
        encoding="utf-8",
    )
    (folder / "memgraph.txt").write_text(
        "Memgraph runs Neo4j on bolt://127.0.0.1:7687 and serves the graph pipe",
        encoding="utf-8",
    )

    record = workspaces.activate(str(folder))
    return SimpleNamespace(
        rag_pipeline=mods["rag_pipeline"],
        retrieval=mods["retrieval"],
        miss_log=mods["miss_log"],
        rag_cache=mods["rag_cache"],
        memgraph=mods["memgraph"],
        rag_entities=mods["rag_entities"],
        rag_feedback=mods["rag_feedback"],
        ws_id=record.id,
    )


# ── Tests ──────────────────────────────────────────────────────────────


def test_happy_path_all_stages(
    pipeline_env: Any, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Decompose + multi-hop + cache all enabled → ok result with citations."""
    env = pipeline_env
    monkeypatch.setattr(env.retrieval, "rerank", _passthrough_rerank)

    out = env.rag_pipeline.ask(
        "how do I shut down the services and what graph port is used?",
        workspace_id=env.ws_id,
        chat_fn=make_chat_fn(),
        limit=4,
        decompose=True,
        multi_hop=True,
        use_cache=True,
    )

    assert out["status"] == "ok"
    assert out["count"] >= 1
    assert out["hits"]
    assert "[1]" in out["citation_block"]
    assert out["query_id"]
    # Decomposition split the query into the two synthetic sub-queries.
    assert len(out["trace"]["sub_queries"]) == 2
    # Multi-hop synthesised + retrieved at least one follow-up.
    assert out["trace"]["follow_ups"]
    assert out["trace"]["hops"] >= 2


def test_cache_hit_short_circuits(
    pipeline_env: Any, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A repeated query hits the semantic cache and skips retrieval entirely."""
    env = pipeline_env
    monkeypatch.setattr(env.retrieval, "rerank", _passthrough_rerank)

    calls = {"n": 0}
    real_retrieve = env.retrieval.retrieve

    def counting_retrieve(*a: Any, **kw: Any) -> Any:
        calls["n"] += 1
        return real_retrieve(*a, **kw)

    monkeypatch.setattr(env.retrieval, "retrieve", counting_retrieve)
    chat_fn = make_chat_fn(follow_up="")  # no follow-up — keep call counts low

    query = "how do I shut down the services?"
    first = env.rag_pipeline.ask(
        query, workspace_id=env.ws_id, chat_fn=chat_fn, limit=4, use_cache=True
    )
    assert first["status"] == "ok"
    after_first = calls["n"]
    assert after_first > 0

    second = env.rag_pipeline.ask(
        query, workspace_id=env.ws_id, chat_fn=chat_fn, limit=4, use_cache=True
    )
    assert second.get("cache_hit") is True
    assert second["query_id"] == first["query_id"]
    # The cache short-circuited the pipeline — no further retrieval ran.
    assert calls["n"] == after_first


def test_decompose_disabled_single_subquery(
    pipeline_env: Any, monkeypatch: pytest.MonkeyPatch
) -> None:
    """decompose=False → exactly one sub-query (the original)."""
    env = pipeline_env
    monkeypatch.setattr(env.retrieval, "rerank", _passthrough_rerank)

    query = "how do I shut down the services?"
    out = env.rag_pipeline.ask(
        query,
        workspace_id=env.ws_id,
        chat_fn=make_chat_fn(),
        limit=4,
        decompose=False,
        multi_hop=False,
        use_cache=False,
    )

    assert out["status"] == "ok"
    assert out["trace"]["sub_queries"] == [query]


def test_multihop_disabled_single_hop(
    pipeline_env: Any, monkeypatch: pytest.MonkeyPatch
) -> None:
    """multi_hop=False → one retrieval hop, no follow-up queries."""
    env = pipeline_env
    monkeypatch.setattr(env.retrieval, "rerank", _passthrough_rerank)

    out = env.rag_pipeline.ask(
        "how do I shut down the services?",
        workspace_id=env.ws_id,
        chat_fn=make_chat_fn(),  # would emit follow-ups if multi-hop ran
        limit=4,
        decompose=False,
        multi_hop=False,
        use_cache=False,
    )

    assert out["status"] == "ok"
    assert out["trace"]["hops"] == 1
    assert out["trace"]["follow_ups"] == []


def test_gate_fires_insufficient_context(
    pipeline_env: Any, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Low rerank scores trip the gate → insufficient_context response."""
    env = pipeline_env
    monkeypatch.setattr(env.retrieval, "rerank", _negative_rerank)

    out = env.rag_pipeline.ask(
        "how do I shut down the services?",
        workspace_id=env.ws_id,
        chat_fn=make_chat_fn(follow_up=""),
        limit=4,
        decompose=False,
        multi_hop=False,
        use_cache=False,
    )

    assert out["status"] == "insufficient_context"
    assert out["answer"] is None
    assert out["trace"]["gate_fired"] is True
    # Candidates WERE retrieved — the gate fired on score, not on emptiness.
    assert out["trace"]["candidate_count"] > 0
    assert out["query_id"]


def test_no_chat_fn_degraded_but_functional(
    pipeline_env: Any, monkeypatch: pytest.MonkeyPatch
) -> None:
    """No chat_fn → HyDE/decompose/multi-hop all fall back to query-as-is."""
    env = pipeline_env
    monkeypatch.setattr(env.retrieval, "rerank", _passthrough_rerank)

    query = "how do I shut down the services?"
    out = env.rag_pipeline.ask(
        query,
        workspace_id=env.ws_id,
        chat_fn=None,
        limit=4,
        decompose=True,
        multi_hop=True,
        use_cache=False,
    )

    assert out["status"] == "ok"
    assert out["hits"]
    assert out["trace"]["chat_fn"] is False
    assert out["trace"]["sub_queries"] == [query]
    assert out["trace"]["follow_ups"] == []
    assert out["trace"]["hops"] == 1


def test_miss_log_records_terminal_states(
    pipeline_env: Any, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Both terminal outcomes are logged; only the IC query counts as a miss."""
    env = pipeline_env
    chat_fn = make_chat_fn(follow_up="")

    monkeypatch.setattr(env.retrieval, "rerank", _passthrough_rerank)
    ok = env.rag_pipeline.ask(
        "how do I shut down the services?",
        workspace_id=env.ws_id,
        chat_fn=chat_fn,
        limit=4,
        decompose=False,
        multi_hop=False,
        use_cache=False,
    )
    assert ok["status"] == "ok"
    assert ok["query_id"]

    monkeypatch.setattr(env.retrieval, "rerank", _negative_rerank)
    insufficient = env.rag_pipeline.ask(
        "what is the airspeed of an unladen swallow?",
        workspace_id=env.ws_id,
        chat_fn=chat_fn,
        limit=4,
        decompose=False,
        multi_hop=False,
        use_cache=False,
    )
    assert insufficient["status"] == "insufficient_context"
    assert insufficient["query_id"]

    miss_queries = [m.get("query") for m in env.miss_log.list_misses(limit=100)]
    # The gate-fired (IC) query is logged as a miss; the ok query is not.
    assert "what is the airspeed of an unladen swallow?" in miss_queries
    assert "how do I shut down the services?" not in miss_queries


# ── Item 7: reader→writer graph feedback ───────────────────────────────


def _recording_upsert(calls: List[Any]) -> Callable[..., Any]:
    """An upsert_edge stub that records every call and reports success."""

    def fake_upsert(
        source: Any, label: Any, target: Any, *, weight_delta: float = 1.0, **_: Any
    ) -> Any:
        calls.append((source, label, target, weight_delta))
        return SimpleNamespace(ok=True, data=None, error=None)

    return fake_upsert


def test_feedback_ok_strengthens_edges(
    pipeline_env: Any, monkeypatch: pytest.MonkeyPatch
) -> None:
    """An ok result strengthens entity→chunk edges with a positive weight."""
    env = pipeline_env
    monkeypatch.setattr(env.retrieval, "rerank", _passthrough_rerank)
    calls: List[Any] = []
    monkeypatch.setattr(env.memgraph, "upsert_edge", _recording_upsert(calls))

    out = env.rag_pipeline.ask(
        "how does the Lifecycle daemon shut the services down?",
        workspace_id=env.ws_id,
        chat_fn=make_chat_fn(follow_up=""),
        limit=4,
        decompose=False,
        multi_hop=False,
        use_cache=False,
    )

    assert out["status"] == "ok"
    assert calls, "expected entity→chunk edges to be strengthened"
    assert all(label == "CITED_IN" for (_, label, _, _) in calls)
    assert all(weight > 0 for (_, _, _, weight) in calls)
    assert out["trace"]["feedback"]["graph_edges"] == len(calls)
    assert out["trace"]["feedback"]["graph_ok"] is True


def test_feedback_ic_records_miss_marker(
    pipeline_env: Any, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A gate-fired result records a weak-retrieval marker + low-weight
    'miss' edges to the RetrievalMiss sentinel node."""
    env = pipeline_env
    monkeypatch.setattr(env.retrieval, "rerank", _negative_rerank)
    calls: List[Any] = []
    monkeypatch.setattr(env.memgraph, "upsert_edge", _recording_upsert(calls))

    out = env.rag_pipeline.ask(
        "how does the Lifecycle daemon shut the services down?",
        workspace_id=env.ws_id,
        chat_fn=make_chat_fn(follow_up=""),
        limit=4,
        decompose=False,
        multi_hop=False,
        use_cache=False,
    )

    assert out["status"] == "insufficient_context"
    assert out["trace"]["feedback"]["miss_recorded"] is True
    # Miss edges point every query entity at the sentinel, low-weight.
    assert calls
    assert all(
        label == "RETRIEVAL_MISS" and target == "RetrievalMiss"
        for (_, label, target, _) in calls
    )
    assert all(weight < 1.0 for (_, _, _, weight) in calls)
    # A structured weak-retrieval marker landed in the miss log.
    misses = env.miss_log.list_misses(limit=100)
    assert any(
        (m.get("context") or {}).get("event") == "weak_retrieval" for m in misses
    )


# ── Item 8: soft addressing (query-entity graph seeds) ─────────────────


def test_soft_addressing_threads_entities_to_hybrid_search(
    pipeline_env: Any, monkeypatch: pytest.MonkeyPatch
) -> None:
    """chat_fn-extracted entities are threaded down to hybrid_search."""
    env = pipeline_env
    monkeypatch.setattr(env.retrieval, "rerank", _passthrough_rerank)
    seen: Dict[str, Any] = {}
    real_hybrid = env.retrieval.hybrid_search

    def recording_hybrid(workspace_id: Any, query_text: Any, **kw: Any) -> Any:
        seen["query_entities"] = kw.get("query_entities")
        return real_hybrid(workspace_id, query_text, **kw)

    monkeypatch.setattr(env.retrieval, "hybrid_search", recording_hybrid)

    out = env.rag_pipeline.ask(
        "how does the Lifecycle daemon shut the services down?",
        workspace_id=env.ws_id,
        chat_fn=make_chat_fn(follow_up=""),
        limit=4,
        decompose=False,
        multi_hop=False,
        use_cache=False,
    )

    assert out["trace"]["query_entities"], "entities should have been extracted"
    assert seen.get("query_entities") == out["trace"]["query_entities"]


def test_soft_addressing_regex_fallback_without_chat_fn(pipeline_env: Any) -> None:
    """With no chat_fn, entity extraction falls back to the regex extractor."""
    env = pipeline_env
    entities = env.rag_entities.extract_entities(
        'how does the WyldeLink VPN expose "device tokens"?',
        chat_fn=None,
    )
    assert entities, "regex fallback should surface some entities"
    joined = " ".join(entities).lower()
    assert "wyldelink" in joined or "vpn" in joined
    assert any("device tokens" in e.lower() for e in entities)


def test_soft_addressing_empty_entities_no_seed_effect(
    pipeline_env: Any, monkeypatch: pytest.MonkeyPatch
) -> None:
    """No extractable entities → empty seed list, graph expansion unchanged."""
    env = pipeline_env
    monkeypatch.setattr(env.retrieval, "rerank", _passthrough_rerank)
    seen: Dict[str, Any] = {}
    real_hybrid = env.retrieval.hybrid_search

    def recording_hybrid(workspace_id: Any, query_text: Any, **kw: Any) -> Any:
        seen["query_entities"] = kw.get("query_entities")
        return real_hybrid(workspace_id, query_text, **kw)

    monkeypatch.setattr(env.retrieval, "hybrid_search", recording_hybrid)

    # All-lowercase, no quotes/paths, no chat_fn → regex finds nothing.
    out = env.rag_pipeline.ask(
        "how do i drain the things quietly",
        workspace_id=env.ws_id,
        chat_fn=None,
        limit=4,
        decompose=False,
        multi_hop=False,
        use_cache=False,
    )

    assert out["trace"]["query_entities"] == []
    # Empty entity list reaches hybrid_search — no extra graph seeds.
    assert not seen.get("query_entities")
