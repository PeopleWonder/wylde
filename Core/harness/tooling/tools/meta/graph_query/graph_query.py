"""tools/meta/graph_query — entity-anchored traversal over the knowledge graph.

Locked-in-memory replacement for the legacy ``graph_query_fallback`` tool
that lived in the orchestrator. The legacy tool tried two HTTP backends in
order:

1. ``wylde-rag``'s ``GraphQueryTool`` — hydrated chunk text and re-ranked
   results by graph proximity.
2. ``wylde-memgraph``'s ``/traverse`` endpoint — chunk ids only, no hydration.

Both have collapsed into in-process modules under ``Wylde.Core.harness``:
:mod:`Wylde.Core.harness.memory.memgraph` (the graph traversal client) and
:mod:`Wylde.Core.harness.memory.rag` (vector / tier-aware semantic search).

Two notable changes from the legacy implementation:

* No HTTP loopback — :func:`memgraph.traverse` runs over the named-pipe
  client (with HTTP fallback handled inside the client itself, transparent
  to this module).
* The "via rag" path is intentionally skipped for now. Legacy ``wylde-rag``
  shipped a dedicated ``GraphQueryTool`` that re-ranked chunks by their
  proximity to traversal entities; the new ``rag.search`` is a tier-aware
  vector search and does not yet have graph-proximity reranking. Adding a
  proper hybrid (graph traversal + rag vector hits) belongs in a later
  pass — see TODO below. Until then we go straight to ``memgraph.traverse``.

Failure model: empty ``results`` and ``count=0`` when the graph backend is
unreachable or the entity set is empty. Never raises; the tool is meant to
fail soft so a planner doesn't abort over a missing memory layer.

Renamed from ``graph_query_fallback`` because the duplicate-name collision
that triggered the legacy rename (Ollama's tools array requires unique
function names) no longer exists — there is only one ``graph_query`` tool.
"""

from __future__ import annotations

import logging
import re
from typing import Any, Dict, List, Optional

from .....memory import graph_retrieval, memgraph, rag

logger = logging.getLogger(__name__)

# How many vector hits to seed graph expansion with by default. Bigger
# pulls more 1-hop neighbours, but each adds a small Memgraph round-
# trip; 5 is the legacy GraphQueryTool's default.
_DEFAULT_VECTOR_K = 5

# Combined score = ``alpha * vector_similarity + (1 - alpha) * graph_score``.
# Vector hits without graph neighbours keep their raw similarity;
# graph-only hits use the inverse-hop score from
# :func:`graph_retrieval._hops_to_similarity`.
_COMBINED_ALPHA = 0.6

_QUERY_IDENT_RE = re.compile(r"\b([A-Za-z_][A-Za-z0-9_]{2,})\b")
_STOP = {
    "the",
    "what",
    "how",
    "why",
    "when",
    "where",
    "does",
    "find",
    "show",
    "tell",
    "about",
    "with",
    "for",
    "and",
    "that",
    "this",
    "from",
    "into",
    "are",
    "you",
    "can",
    "use",
    "uses",
    "using",
}


def _extract_entities(query: str, limit: int = 12) -> List[str]:
    out, seen = [], set()
    for m in _QUERY_IDENT_RE.finditer(query):
        tok = m.group(1)
        low = tok.lower()
        if low in _STOP or low in seen:
            continue
        seen.add(low)
        out.append(tok)
        if len(out) >= limit:
            break
    return out


def _normalize_chunks(data: Any) -> List[Any]:
    """Coerce the ``MemgraphReply.data`` payload into a list of chunks.

    The Memgraph service's ``/traverse`` route historically returned
    ``{"chunks": [...]}`` and that's what the pipe transport propagates
    today. Be defensive in case the wire shape evolves: accept either a
    ``chunks`` field or a bare list, and fall back to ``[]``.
    """
    if isinstance(data, dict):
        chunks = data.get("chunks")
        if isinstance(chunks, list):
            return chunks
        # Some pipe handlers wrap the response one layer deeper.
        nested = data.get("data")
        if isinstance(nested, dict) and isinstance(nested.get("chunks"), list):
            nested_chunks: List[Any] = nested["chunks"]
            return nested_chunks
        return []
    if isinstance(data, list):
        return data
    return []


def _via_memgraph(
    entities: List[str], max_hops: int, limit: int
) -> Optional[Dict[str, Any]]:
    """Direct in-process call to the Memgraph client.

    Returns ``None`` when the backend is unreachable so the caller can fall
    through to a default empty-result envelope (matches the legacy
    fail-soft contract).
    """
    if not entities:
        return None
    try:
        reply = memgraph.traverse(entities, max_hops=max_hops, limit=limit)
    except Exception as exc:  # pragma: no cover — client is itself fail-soft
        logger.debug("graph_query: memgraph.traverse raised: %s", exc)
        return None
    if not reply.ok:
        err = reply.error or {}
        logger.debug(
            "graph_query: memgraph.traverse not-ok (%s): %s",
            err.get("code", "?"),
            err.get("message", ""),
        )
        return None
    chunks = _normalize_chunks(reply.data)
    return {
        "entities": entities,
        "results": chunks,
        "count": len(chunks),
        "source": "memgraph",
    }


def run_graph_query(params: Dict[str, Any]) -> Dict[str, Any]:
    """Hybrid graph + vector retrieval.

    Two seeding paths:

    * ``q`` (natural language) → vector pass via ``rag.search`` → top-K
      hit ids feed ``graph_retrieval.expand_by_graph`` for 1-hop entity
      neighbours. Vector hits and graph hits union into a single result
      list ordered by combined score (vector similarity + inverse-hop
      graph score, weighted by :data:`_COMBINED_ALPHA`).
    * ``entities`` (explicit list) → skip the vector pass, run
      ``memgraph.traverse`` directly. Same result envelope shape, but
      ``vector_seeds`` will be empty.

    params:
      q:           natural-language query (vector + graph)
      entities:    explicit entity-name list (graph-only)
      max_hops:    graph expansion depth (default 1, clamped 1..4)
      limit:       max chunks returned (default 10)
      vector_k:    how many vector hits to seed expansion with
                   (default 5, only used when ``q`` is provided)
      tier:        rag.search tier filter ("episodic" | "semantic" |
                   "procedural" | None for all)
      workspace_id: filters graph neighbours to this workspace
    """
    query = str(params.get("q", "")).strip()
    explicit_entities = list(params.get("entities") or [])
    workspace_id = str(params.get("workspace_id") or "")
    tier = params.get("tier") if isinstance(params.get("tier"), str) else None
    try:
        max_hops = max(1, min(4, int(params.get("max_hops", 1))))
        limit = max(1, min(50, int(params.get("limit", 10))))
        vector_k = max(1, min(20, int(params.get("vector_k", _DEFAULT_VECTOR_K))))
    except (TypeError, ValueError):
        max_hops, limit, vector_k = 1, 10, _DEFAULT_VECTOR_K

    if explicit_entities:
        # Entity-only path — same as the legacy behaviour.
        out = _via_memgraph(explicit_entities, max_hops, limit)
        if out is not None:
            out["vector_seeds"] = []
            return out
        return _empty(explicit_entities, "graph backend unavailable")

    if not query:
        return {"error": "either 'q' or 'entities' is required"}

    # ── Vector pass ────────────────────────────────────────────────
    vector_hits: List[Dict[str, Any]] = []
    try:
        vector_hits = rag.search(query, tier=tier, limit=vector_k) or []
    except Exception as exc:  # noqa: BLE001
        logger.debug("graph_query: rag.search raised: %s", exc)
        vector_hits = []

    # ── Graph expansion from vector seeds ──────────────────────────
    graph_hits = []
    try:
        graph_hits = graph_retrieval.expand_by_graph(
            vector_hits,
            workspace_id=workspace_id,
            hops=max_hops,
            max_extra=limit,
        )
    except Exception as exc:  # noqa: BLE001
        logger.debug("graph_query: expand_by_graph raised: %s", exc)
        graph_hits = []

    # Fall back to keyword-extraction → memgraph.traverse if both
    # backends came up empty AND the query has plausible entities in
    # it. Keeps the legacy "extract names then walk" behaviour as a
    # last-ditch path; mirrors the old graph_query_fallback shape.
    if not vector_hits and not graph_hits:
        derived_entities = _extract_entities(query)
        if derived_entities:
            out = _via_memgraph(derived_entities, max_hops, limit)
            if out is not None:
                out["vector_seeds"] = []
                return out

    # ── Union + combined-score ranking ────────────────────────────
    results = _merge_and_rank(vector_hits, graph_hits, limit=limit)
    return {
        "q": query,
        "vector_seeds": [
            {"id": h.get("id"), "similarity": h.get("similarity") or h.get("score")}
            for h in vector_hits
        ],
        "results": results,
        "count": len(results),
        "source": "hybrid" if (vector_hits or graph_hits) else "none",
    }


def _empty(entities: List[str], err: str) -> Dict[str, Any]:
    return {
        "entities": entities,
        "results": [],
        "count": 0,
        "source": "none",
        "error": err,
    }


def _merge_and_rank(
    vector_hits: List[Dict[str, Any]],
    graph_hits: List[Any],
    *,
    limit: int,
) -> List[Dict[str, Any]]:
    """Combine vector + graph hits into one ranked list.

    Hits that appear in BOTH (same chunk id) get the higher of the two
    scores plus a small bonus, since two retrievers agreeing is a
    stronger signal than either alone. Otherwise pure vector hits
    keep their similarity; pure graph hits use their inverse-hop
    score. Sort descending, trim to ``limit``.
    """
    by_id: Dict[str, Dict[str, Any]] = {}

    for h in vector_hits:
        if not isinstance(h, dict):
            continue
        cid = h.get("id")
        if not cid:
            continue
        sim = float(h.get("similarity") or h.get("score") or 0.0)
        by_id[cid] = {
            **h,
            "id": cid,
            "vector_similarity": sim,
            "graph_hops": None,
            "graph_similarity": 0.0,
            "combined_score": _COMBINED_ALPHA * sim,
            "source": "vector",
        }

    for hit in graph_hits:
        as_dict = hit.to_dict() if hasattr(hit, "to_dict") else dict(hit)
        cid = as_dict.get("id")
        if not cid:
            continue
        graph_sim = float(as_dict.get("similarity") or 0.0)
        hops = as_dict.get("hops")
        if cid in by_id:
            existing = by_id[cid]
            existing["graph_hops"] = hops
            existing["graph_similarity"] = graph_sim
            # Both retrievers agree → use the better score plus a
            # small agreement bonus.
            existing["combined_score"] = (
                max(
                    existing["combined_score"],
                    _COMBINED_ALPHA * existing["vector_similarity"]
                    + (1.0 - _COMBINED_ALPHA) * graph_sim,
                )
                + 0.05
            )
            existing["source"] = "vector+graph"
        else:
            by_id[cid] = {
                **as_dict,
                "id": cid,
                "vector_similarity": 0.0,
                "graph_hops": hops,
                "graph_similarity": graph_sim,
                "combined_score": (1.0 - _COMBINED_ALPHA) * graph_sim,
                "source": "graph",
            }

    ranked = sorted(by_id.values(), key=lambda r: r["combined_score"], reverse=True)
    return ranked[:limit]
