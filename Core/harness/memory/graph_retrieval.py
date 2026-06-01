"""Graph-distance expansion for the retrieval pipeline.

Sits between Stage 2 (hybrid vector + BM25) and Stage 3 (RRF fusion)
in :mod:`Core.harness.memory.retrieval`. After the vector layer
produces a candidate pool, we walk Memgraph from those candidates'
entities to surface NEIGHBOURS — chunks that share entity edges with
the seeds. The neighbours are scored by graph distance (inverse hop
count) and folded into the fused ranking.

The whole stage is best-effort:

* Memgraph unreachable → returns an empty expansion. Original
  candidate pool flows through unchanged.
* Memgraph reachable but graph empty (no entities written for this
  workspace yet) → returns empty.
* Per-seed lookups fail individually → those failures are logged at
  DEBUG and the rest of the expansion proceeds.

The integration point is :func:`Core.harness.memory.retrieval.hybrid_search`,
which calls :func:`expand_by_graph` if the workspace has any entity
records. Callers can also call :func:`expand_by_graph` directly when
they want graph-distance neighbours without the BM25 step.
"""

from __future__ import annotations

import logging
import os
from dataclasses import dataclass, field
from typing import Any, Dict, List, Optional

logger = logging.getLogger("wylde.harness.memory.graph_retrieval")


# Default expansion knobs; override via env at boot if a deployment has
# different graph density.
DEFAULT_HOPS = int(os.getenv("WYLDE_GRAPH_HOPS", "1"))
DEFAULT_MAX_EXTRA = int(os.getenv("WYLDE_GRAPH_MAX_EXTRA", "20"))


@dataclass
class GraphHit:
    """One chunk surfaced via graph traversal. ``hops`` is the shortest
    edge distance from any seed; ``via_entities`` lists which entities
    bridged seed → neighbour."""

    id: str
    path: str
    content: str
    hops: int
    via_entities: List[str] = field(default_factory=list)
    similarity: float = 0.0  # for RRF parity with vector hits

    def to_dict(self) -> Dict[str, Any]:
        return {
            "id": self.id,
            "path": self.path,
            "content": self.content,
            "hops": self.hops,
            "via_entities": list(self.via_entities),
            "similarity": self.similarity,
        }


def _memgraph() -> Any:
    """Lazy import of the Memgraph client.

    Returns the module on success, ``None`` if Memgraph isn't
    importable in this env (no msgpack / pywin32) — the rest of the
    pipeline degrades silently when this is None.
    """
    try:
        from . import memgraph as _mg

        return _mg
    except ImportError:
        try:
            from Core.harness.memory import memgraph as _mg

            return _mg
        except ImportError:
            return None


def expand_by_graph(
    candidates: List[Dict[str, Any]],
    *,
    workspace_id: str = "",
    hops: int = DEFAULT_HOPS,
    max_extra: int = DEFAULT_MAX_EXTRA,
    seed_entities: Optional[List[str]] = None,
) -> List[GraphHit]:
    """Take a candidate pool's entity edges and return up-to-N neighbour
    chunks ranked by graph distance.

    ``candidates`` are the vector-stage hits (each must carry an ``id``
    that matches a Memgraph chunk node). ``workspace_id`` filters
    neighbours to the same workspace if the graph layer tracks that.

    ``seed_entities`` is the "soft addressing" hook: entity names extracted
    from the *user query* (see :mod:`rag_entities`). When non-empty they are
    used as additional traverse seeds — Memgraph is walked from those
    entities directly, and the neighbours are merged with the
    candidate-derived expansion. Empty / ``None`` leaves behaviour
    unchanged.

    Returns ``[]`` on any error path so callers can splice the result
    into their hit list unconditionally.
    """
    entity_seeds = [
        str(e).strip() for e in (seed_entities or []) if e and str(e).strip()
    ]
    if not candidates and not entity_seeds:
        return []
    mg = _memgraph()
    if mg is None:
        return []

    seed_ids: List[str] = [str(c.get("id")) for c in candidates if c.get("id")]

    raw: List[Dict[str, Any]] = []
    # The Memgraph client's :func:`multihop` is the right primitive — it
    # walks N hops from a seed set and returns chunk ids ordered by the
    # shortest distance to any seed. We try it first; if the deployment
    # has only the simpler :func:`traverse` (entity → chunk lookup), we
    # fall back to that and synthesise hop=1 neighbours.
    if seed_ids:
        chunk_raw = _try_multihop(mg, seed_ids, workspace_id, hops, max_extra)
        if chunk_raw is None:
            chunk_raw = _try_traverse(mg, candidates, workspace_id, max_extra)
        if chunk_raw:
            raw.extend(chunk_raw)

    # Soft addressing — query-derived entities are additional traverse
    # seeds, walked directly regardless of the candidate-pool expansion.
    if entity_seeds:
        entity_raw = _traverse_from_entities(mg, entity_seeds, workspace_id, max_extra)
        if entity_raw:
            raw.extend(entity_raw)

    if not raw:
        return []

    seed_set = set(seed_ids)
    seen_out: set[str] = set()
    out: List[GraphHit] = []
    for entry in raw:
        chunk_id = entry.get("id") or entry.get("chunk_id") or ""
        if not chunk_id or chunk_id in seed_set or chunk_id in seen_out:
            # Don't surface seeds back as their own graph neighbours, and
            # dedup chunks that both expansion paths surfaced.
            continue
        seen_out.add(chunk_id)
        hop_count = int(entry.get("hops") or entry.get("distance") or 1)
        out.append(
            GraphHit(
                id=chunk_id,
                path=str(entry.get("path") or ""),
                content=str(entry.get("content") or entry.get("body") or ""),
                hops=hop_count,
                via_entities=list(
                    entry.get("via_entities") or entry.get("entities") or []
                ),
                similarity=_hops_to_similarity(hop_count),
            )
        )
        if len(out) >= max_extra:
            break
    out.sort(key=lambda h: h.hops)
    return out


def _try_multihop(
    mg: Any,
    seed_ids: List[str],
    workspace_id: str,
    hops: int,
    limit: int,
) -> Optional[List[Dict[str, Any]]]:
    """Prefer the multihop primitive if the client exposes it."""
    fn = getattr(mg, "multihop", None)
    if fn is None:
        return None
    try:
        # Different deployments accept slightly different kwargs; try
        # the most explicit signature first, fall back to positional.
        try:
            reply = fn(
                chunk_ids=seed_ids,
                workspace=workspace_id or None,
                max_hops=hops,
                limit=limit,
            )
        except TypeError:
            reply = fn(seed_ids, hops, limit)
    except Exception as exc:  # noqa: BLE001
        logger.debug("graph_retrieval: multihop failed (%s); falling back", exc)
        return None
    return _coerce_chunks(reply)


def _try_traverse(
    mg: Any,
    candidates: List[Dict[str, Any]],
    workspace_id: str,
    limit: int,
) -> Optional[List[Dict[str, Any]]]:
    """Single-hop fallback. For each candidate's entity list (if we know
    them), find chunks that mention the same entities."""
    fn = getattr(mg, "traverse", None)
    if fn is None:
        return None
    seed_entities: List[str] = []
    seen_ents: set = set()
    for c in candidates:
        for e in c.get("entities") or c.get("via_entities") or []:
            if e and e not in seen_ents:
                seen_ents.add(e)
                seed_entities.append(e)
    if not seed_entities:
        return []
    try:
        try:
            reply = fn(
                entities=seed_entities,
                workspace=workspace_id or None,
                limit=limit,
            )
        except TypeError:
            reply = fn(seed_entities, limit)
    except Exception as exc:  # noqa: BLE001
        logger.debug("graph_retrieval: traverse failed (%s)", exc)
        return None
    coerced = _coerce_chunks(reply)
    if coerced is None:
        return None
    # All single-hop neighbours are distance 1.
    for entry in coerced:
        entry.setdefault("hops", 1)
    return coerced


def _traverse_from_entities(
    mg: Any,
    entities: List[str],
    workspace_id: str,
    limit: int,
) -> Optional[List[Dict[str, Any]]]:
    """Single-hop traverse seeded directly by query-derived entity names.

    Unlike :func:`_try_traverse` (which mines entities out of the candidate
    pool), this takes the entity list straight from the caller — the "soft
    addressing" path. Distances are synthesised as 1 since traverse is
    hop-agnostic. Returns ``None`` when the client has no ``traverse``."""
    fn = getattr(mg, "traverse", None)
    if fn is None:
        return None
    try:
        try:
            reply = fn(entities=entities, workspace=workspace_id or None, limit=limit)
        except TypeError:
            reply = fn(entities, limit)
    except Exception as exc:  # noqa: BLE001
        logger.debug("graph_retrieval: entity-seed traverse failed (%s)", exc)
        return None
    coerced = _coerce_chunks(reply)
    if coerced is None:
        return None
    for entry in coerced:
        entry.setdefault("hops", 1)
    return coerced


def _coerce_chunks(reply: Any) -> Optional[List[Dict[str, Any]]]:
    """Normalise whatever the client returned into ``[{id, path, ...}]``.

    Memgraph clients vary by version: some return a ``MemgraphReply``
    dataclass with ``.ok`` and ``.data``; some return a bare dict or
    list. We accept all of them.
    """
    if reply is None:
        return None
    # MemgraphReply or similar — pull .data if present.
    data = getattr(reply, "data", None)
    if data is not None:
        if hasattr(reply, "ok") and not getattr(reply, "ok"):
            return None
        reply = data
    if isinstance(reply, dict):
        for key in ("chunks", "results", "hits", "data"):
            if isinstance(reply.get(key), list):
                reply = reply[key]
                break
        else:
            return None
    if not isinstance(reply, list):
        return None
    out: List[Dict[str, Any]] = []
    for entry in reply:
        if not isinstance(entry, dict):
            continue
        out.append(entry)
    return out


def _hops_to_similarity(hops: int) -> float:
    """Convert hop count to a 0..1 similarity-style score for RRF fusion.

    Closer entities are more similar; a one-hop neighbour scores ~0.5,
    two hops ~0.25, etc. Caller can override the curve later if the
    graph turns out to be denser / sparser than expected.
    """
    h = max(1, int(hops))
    return 1.0 / (1.0 + h)


__all__ = [
    "DEFAULT_HOPS",
    "DEFAULT_MAX_EXTRA",
    "GraphHit",
    "expand_by_graph",
]
