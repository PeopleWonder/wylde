"""Multi-hop retrieval — iterative gap-filling over the workspace index.

Ported from the multi-hop logic in ``_legacy/core/wylde-rag/retrieval.py``
and the gap-identification prompt in ``_legacy/core/wylde-rag/hyde.py``.

A single retrieval pass answers a focused question well, but misses the
follow-on context a broader question needs ("how does X work" often needs
both the definition of X *and* the thing that calls it). Multi-hop closes
that gap:

1. Retrieve once for the original query (hop 0) via :func:`retrieval.retrieve`.
2. Show the LLM (via ``chat_fn``) the query plus the passages found so far
   and ask for **one** follow-up search that would fill the biggest gap —
   or ``NONE`` if the context is already sufficient.
3. Retrieve for that follow-up (hop 1), fuse both hit lists with Reciprocal
   Rank Fusion, and repeat until the LLM says ``NONE`` or the hop cap is hit.

Bounded by :data:`MULTIHOP_MAX_HOPS` (env-overridable, default 2 — i.e. the
original query plus at most one follow-up).

Degraded modes, mirroring :mod:`retrieval`'s degrade-in-place philosophy:

* No ``chat_fn`` → there is no way to synthesise a follow-up, so this
  collapses to a plain single-hop :func:`retrieval.retrieve` call.
* A failing ``chat_fn`` / retrieval call mid-loop → the loop stops early
  and returns whatever was fused so far.

Per-hop retrieval runs with ``do_rerank=False`` so the fusion combines raw
hybrid rankings; the orchestrator applies the cross-encoder rerank once,
against the original query, after all hops are merged.
"""

from __future__ import annotations

import os
from typing import Any, Callable, Dict, List, Optional, Tuple

from . import retrieval
from ._common import logger
from .retrieval import RetrievalHit


def _env_int(name: str, default: int) -> int:
    raw = os.getenv(name)
    if raw is None or not raw.strip():
        return default
    try:
        return int(raw)
    except ValueError:
        return default


# Total retrieval rounds per query (original + follow-ups). 2 → one follow-up.
MULTIHOP_MAX_HOPS: int = max(1, _env_int("MULTIHOP_MAX_HOPS", 2))

# RRF constant — the conventional k=60 from the published fusion benchmarks.
_RRF_K = 60.0

_GAP_SYSTEM = (
    "You analyse retrieved context for gaps. Given a user's question and "
    "the passages already retrieved for it, decide whether ONE additional "
    "search would materially improve a complete answer. If yes, reply with "
    "ONLY that follow-up search query, on a single line, no prose. If the "
    "retrieved context is already sufficient, reply with exactly: NONE"
)


def _rrf_fuse(hit_lists: List[List[RetrievalHit]]) -> List[RetrievalHit]:
    """Reciprocal Rank Fusion of several ranked hit lists into one.

    Each hit's fused score is ``sum(1 / (k + rank))`` over the lists it
    appears in. Cheap, parameter-free, and competitive with learned fusion.
    The fused score is written back onto the surviving hit objects so the
    downstream rerank/gate see a consistent headline ``score``.
    """
    scores: Dict[str, float] = {}
    objs: Dict[str, RetrievalHit] = {}
    for hits in hit_lists:
        for rank, hit in enumerate(hits):
            if not hit.id:
                continue
            scores[hit.id] = scores.get(hit.id, 0.0) + 1.0 / (_RRF_K + rank)
            objs.setdefault(hit.id, hit)
    fused = sorted(objs.values(), key=lambda h: scores[h.id], reverse=True)
    for h in fused:
        h.score = scores[h.id]
    return fused


def _single_hop(
    workspace_id: str,
    query: str,
    limit: int,
    chat_fn: Optional[Callable[..., Any]],
    query_entities: Optional[List[str]],
) -> List[RetrievalHit]:
    """One :func:`retrieval.retrieve` pass, rerank deferred to the caller."""
    try:
        return retrieval.retrieve(
            workspace_id,
            query,
            limit=limit,
            chat_fn=chat_fn,
            do_rerank=False,
            query_entities=query_entities,
        )
    except Exception as exc:  # noqa: BLE001
        logger.debug("rag_multihop: retrieve failed for %r (%s)", query, exc)
        return []


def _synthesize_follow_up(
    query: str,
    hits: List[RetrievalHit],
    chat_fn: Callable[..., Any],
) -> Optional[str]:
    """Ask the LLM for one follow-up query that fills a gap in ``hits``.

    Returns the follow-up text, or ``None`` when the model says ``NONE``,
    echoes the original query, or the call fails / yields nothing usable.
    """
    if not hits:
        return None
    context = "\n".join(
        f"- {h.path} (chunk {h.chunk_idx}): "
        f"{h.content.strip().replace(chr(10), ' ')[:300]}"
        for h in hits[:8]
    )
    user = (
        f"Question: {query}\n\n"
        f"--- Passages retrieved so far ---\n{context}\n--- End passages ---\n\n"
        f"Follow-up search query (or NONE):"
    )
    messages = [
        {"role": "system", "content": _GAP_SYSTEM},
        {"role": "user", "content": user},
    ]
    try:
        step = chat_fn(messages=messages, tools=[], model=None)
    except Exception as exc:  # noqa: BLE001
        logger.debug("rag_multihop: gap chat_fn failed (%s)", exc)
        return None

    text = getattr(step, "text", None)
    if not isinstance(text, str):
        return None
    # First non-empty line, stripped of bullet / quote noise.
    follow_up = ""
    for line in text.splitlines():
        candidate = line.strip().lstrip("-*0123456789. \t").strip().strip('"`')
        if candidate:
            follow_up = candidate
            break
    if not follow_up or follow_up.upper() == "NONE":
        return None
    if follow_up.lower() == query.strip().lower() or len(follow_up) < 4:
        return None
    return follow_up


def multi_hop_retrieve(
    query: str,
    *,
    workspace_id: str,
    chat_fn: Optional[Callable[..., Any]] = None,
    limit: int = 6,
    max_hops: Optional[int] = None,
    query_entities: Optional[List[str]] = None,
) -> Tuple[List[RetrievalHit], Dict[str, Any]]:
    """Iteratively retrieve, gap-fill, and RRF-fuse for ``query``.

    Returns ``(fused_hits, trace)`` where ``trace`` is
    ``{"hops": int, "follow_ups": List[str]}`` — ``hops`` counts the
    retrieval rounds actually run, ``follow_ups`` lists the synthesised
    follow-up queries in order.

    ``query_entities`` (soft addressing) is forwarded to every per-hop
    :func:`retrieval.retrieve` call as graph-expansion seeds.

    Falls back to a single :func:`retrieval.retrieve` pass when ``chat_fn``
    is missing or the hop cap is 1.
    """
    q = (query or "").strip()
    if not q or not workspace_id:
        return [], {"hops": 0, "follow_ups": []}

    cap = MULTIHOP_MAX_HOPS if max_hops is None else max(1, int(max_hops))

    first = _single_hop(workspace_id, q, limit, chat_fn, query_entities)
    if chat_fn is None or cap <= 1:
        return first, {"hops": 1, "follow_ups": []}

    hit_lists: List[List[RetrievalHit]] = [first]
    follow_ups: List[str] = []
    fused = first

    for _ in range(1, cap):
        follow_up = _synthesize_follow_up(q, fused, chat_fn)
        if not follow_up:
            break
        follow_ups.append(follow_up)
        next_hits = _single_hop(workspace_id, follow_up, limit, chat_fn, query_entities)
        if next_hits:
            hit_lists.append(next_hits)
            fused = _rrf_fuse(hit_lists)

    return fused, {"hops": len(hit_lists), "follow_ups": follow_ups}


__all__ = ["MULTIHOP_MAX_HOPS", "multi_hop_retrieve"]
