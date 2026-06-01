"""Reader→writer graph feedback — retrieval outcomes teach the graph.

A lightweight take on the SAGE "self-evolving" idea: the *outcome* of a
retrieval is itself a signal, and feeding it back into the knowledge graph
makes future retrievals better without any offline training step.

:func:`record_outcome` is called by :func:`rag_pipeline.ask` at both
terminal branches, right after the miss-log write:

* **Success (``status="ok"``)** — the cited chunks answered the query, so
  the edges between the query's entities and those chunk ids get
  *strengthened*: an :func:`memgraph.upsert_edge` per ``(entity, chunk)``
  pair with a positive weight delta. A repeated/related query over the same
  entities then ranks those chunks higher via the graph-expansion stage.

* **Failure (anything else — ``insufficient_context``, a future
  citation-failure status)** — retrieval came up short. Two things happen:
  a structured ``weak_retrieval`` marker is written to the miss log (richer
  than the bare ``log_query`` row — it carries the query entities), and a
  *low-weight* "miss" edge is drawn from each query entity to a sentinel
  ``RetrievalMiss`` node. Over time a recurring blind spot shows up as a
  cluster of entities all pointing at that sentinel — a queryable,
  self-evolving record of where the corpus is thin.

Best-effort throughout: every Memgraph call is guarded, an unreachable
graph degrades to a debug log and a zero-edge result, and the function
never raises — a graph write must never break a chat turn.
"""

from __future__ import annotations

from typing import Any, Dict, List, Optional

from . import miss_log
from ._common import logger

# Sentinel node every retrieval miss points at — see module docstring.
MISS_SENTINEL = "RetrievalMiss"

# Edge labels. CITED_IN: entity → chunk that cited it (strengthened on ok).
# RETRIEVAL_MISS: entity → MISS_SENTINEL (low-weight, drawn on failure).
_CITED_EDGE = "CITED_IN"
_MISS_EDGE = "RETRIEVAL_MISS"

# Weight deltas — a citation is a strong positive signal; a miss is a faint
# negative one, so a single lucky hit later easily outweighs it.
_OK_WEIGHT = 1.0
_MISS_WEIGHT = 0.25


def _memgraph() -> Any:
    """Lazy import of the Memgraph client; ``None`` when unimportable."""
    try:
        from . import memgraph as _mg

        return _mg
    except Exception:  # noqa: BLE001
        return None


def _upsert_all(
    edges: List[tuple[str, str, str]],
    weight_delta: float,
) -> int:
    """Apply a batch of ``(source, label, target)`` edge upserts. Returns the
    count that the graph acknowledged. Best-effort — unreachable graph or a
    client without ``upsert_edge`` yields 0."""
    mg = _memgraph()
    upsert = getattr(mg, "upsert_edge", None) if mg is not None else None
    if upsert is None:
        logger.debug("rag_feedback: memgraph upsert_edge unavailable; skipping")
        return 0
    written = 0
    for source, label, target in edges:
        try:
            reply = upsert(source, label, target, weight_delta=weight_delta)
        except Exception as exc:  # noqa: BLE001
            logger.debug("rag_feedback: upsert_edge failed (%s)", exc)
            continue
        if getattr(reply, "ok", False):
            written += 1
    return written


def _record_weak_marker(query: str, query_id: str, entities: List[str]) -> bool:
    """Write a structured ``weak_retrieval`` marker into the miss log."""
    try:
        miss_log.record_miss(
            query,
            {
                "event": "weak_retrieval",
                "query_id": query_id,
                "entities": entities[:8],
            },
        )
        return True
    except Exception as exc:  # noqa: BLE001
        logger.debug("rag_feedback: weak-retrieval marker failed (%s)", exc)
        return False


def record_outcome(
    query: str,
    *,
    status: str,
    query_entities: Optional[List[str]] = None,
    chunk_ids: Optional[List[str]] = None,
    query_id: str = "",
) -> Dict[str, Any]:
    """Feed a terminal RAG outcome back into the knowledge graph.

    Parameters
    ----------
    query:
        The original user query.
    status:
        The pipeline's terminal status — ``"ok"`` strengthens edges,
        anything else is treated as a retrieval miss.
    query_entities:
        Entities extracted from the query (see :mod:`rag_entities`). The
        source side of every feedback edge.
    chunk_ids:
        Cited chunk ids — the target side of the strengthen edges on an
        ``ok`` outcome. Ignored on a miss.
    query_id:
        The miss-log id of the originating query, recorded on the marker.

    Returns a small trace dict — ``{"graph_edges", "miss_recorded",
    "graph_ok"}`` — folded into the pipeline trace. Never raises.
    """
    entities = [e for e in (query_entities or []) if e and str(e).strip()]
    trace: Dict[str, Any] = {
        "graph_edges": 0,
        "miss_recorded": False,
        "graph_ok": False,
    }

    if status == "ok":
        chunks = [c for c in (chunk_ids or []) if c]
        if entities and chunks:
            edges = [
                (ent, _CITED_EDGE, chunk_id) for ent in entities for chunk_id in chunks
            ]
            written = _upsert_all(edges, _OK_WEIGHT)
            trace["graph_edges"] = written
            trace["graph_ok"] = written > 0
        return trace

    # Non-ok terminal state — record the weak-retrieval signal.
    trace["miss_recorded"] = _record_weak_marker(query, query_id, entities)
    if entities:
        edges = [(ent, _MISS_EDGE, MISS_SENTINEL) for ent in entities]
        written = _upsert_all(edges, _MISS_WEIGHT)
        trace["graph_edges"] = written
        trace["graph_ok"] = written > 0
    return trace


__all__ = ["MISS_SENTINEL", "record_outcome"]
