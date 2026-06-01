"""RAG pipeline orchestrator — the public ``ask`` entry point.

This is the in-process successor to ``_legacy/core/wylde-rag/generate.py``.
It composes the already-ported retrieval primitives into one call:

    cache lookup → extract query entities → decompose
                 → retrieve (single- or multi-hop, per sub-query)
                 → merge → cross-encoder rerank → score gate
                 → miss-log → reader→writer graph feedback
                 → cache insert → return

Two SAGE-inspired touches sit inside that flow. **Soft addressing**:
:mod:`rag_entities` pulls the named entities out of the query and they are
threaded down to the graph-expansion stage as explicit traverse seeds, so
the graph walk starts from what the query is *about*. **Reader→writer
feedback**: :mod:`rag_feedback` turns each terminal outcome into a graph
write — a cited success strengthens the entity→chunk edges that produced
it, a miss leaves a low-weight trail — so retrieval quality compounds over
time without an offline training step.

**What this does NOT do: generation.** The legacy pipeline ended with an LLM
call that produced a cited answer. Per the Wylde user's directive there is no separate
generation model in the harness — the chat-turn LLM already loaded for the
user's turn does generation, inside the active inference loop. So ``ask``
stops at *ranked, cited candidates*: each surviving hit carries an ``[N]``
citation label and the result includes a ``citation_block`` ready to drop
into the prompt. The LLM cites those labels in its reply; an optional
citation-resolution step can run later in the harness turn driver.

**Internal LLM helpers reuse the turn's ``chat_fn``.** HyDE expansion (inside
:func:`retrieval.retrieve`), query decomposition, and multi-hop follow-up
synthesis all call the *same* ``chat_fn`` the harness already has loaded for
the chat turn. No separate orchestrator/generation model, no extra model
load. When ``chat_fn`` is ``None`` every one of those helpers degrades to
"use the query as-is", so the pipeline still returns ranked hits.

Every stage is wrapped so a single failing helper never aborts the call —
the same degrade-in-place contract :func:`retrieval.retrieve` honours.
"""

from __future__ import annotations

import os
import time
from typing import Any, Callable, Dict, List, Optional

from . import (
    embeddings,
    miss_log,
    rag_cache,
    rag_decompose,
    rag_entities,
    rag_feedback,
    rag_gate,
    rag_multihop,
    retrieval,
)
from ._common import logger
from .retrieval import RetrievalHit


def _env_bool(name: str, default: bool) -> bool:
    raw = os.getenv(name)
    if raw is None or not raw.strip():
        return default
    return raw.strip().lower() in {"1", "true", "yes", "on"}


# Pipeline-stage defaults. Each is overridable per-call via the ``ask``
# kwargs; the env vars set the default when the caller passes ``None``.
_DECOMPOSE_DEFAULT = _env_bool("WYLDE_RAG_DECOMPOSE", True)
_MULTIHOP_DEFAULT = _env_bool("WYLDE_RAG_MULTIHOP", True)
_CACHE_DEFAULT = _env_bool("WYLDE_RAG_CACHE_ENABLED", True)

_OK_NOTE = (
    "Ranked candidate chunks, each tagged with an [N] citation label. "
    "Ground the answer in these passages and cite the [N] labels for every "
    "factual claim — do not cite passages you did not use."
)


def _safe_single_hop(
    workspace_id: str,
    query: str,
    limit: int,
    chat_fn: Optional[Callable[..., Any]],
    query_entities: List[str],
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
        logger.debug("rag_pipeline: single-hop retrieve failed (%s)", exc)
        return []


def _safe_record_outcome(
    query: str,
    status: str,
    query_entities: List[str],
    chunk_ids: List[str],
    query_id: str,
) -> Dict[str, Any]:
    """Reader→writer graph feedback — guarded so a graph write can never
    break the turn (``rag_feedback`` is itself best-effort; this is the
    outermost belt-and-braces wrap)."""
    try:
        return rag_feedback.record_outcome(
            query,
            status=status,
            query_entities=query_entities,
            chunk_ids=chunk_ids,
            query_id=query_id,
        )
    except Exception as exc:  # noqa: BLE001
        logger.debug("rag_pipeline: feedback record_outcome failed (%s)", exc)
        return {"graph_edges": 0, "miss_recorded": False, "graph_ok": False}


def _log_query(query: str, workspace_id: str, hits: List[Dict[str, Any]]) -> str:
    """Record the query in the miss log (best-effort). Returns the query id."""
    try:
        return miss_log.log_query(query, workspace_id=workspace_id, hits=hits)
    except Exception:  # noqa: BLE001
        logger.debug("rag_pipeline: miss_log.log_query failed", exc_info=True)
        return ""


def _finish_insufficient(
    query: str,
    workspace_id: str,
    hint_hits: List[RetrievalHit],
    query_entities: List[str],
    trace: Dict[str, Any],
    t0: float,
) -> Dict[str, Any]:
    """Build + log an insufficient-context terminal result, then feed the
    weak-retrieval signal back into the graph."""
    trace["gate_fired"] = True
    trace["kept_count"] = 0
    query_id = _log_query(query, workspace_id, [])
    trace["feedback"] = _safe_record_outcome(
        query, "insufficient_context", query_entities, [], query_id
    )
    trace["total_ms"] = int((time.time() - t0) * 1000)
    resp = rag_gate.insufficient_context_response(query, hint_hits)
    resp["query_id"] = query_id
    resp["trace"] = trace
    return resp


def ask(
    query: str,
    *,
    workspace_id: str = "default",
    chat_fn: Optional[Callable[..., Any]] = None,
    limit: int = 6,
    decompose: Optional[bool] = None,
    multi_hop: Optional[bool] = None,
    use_cache: Optional[bool] = None,
) -> Dict[str, Any]:
    """Run the full RAG pipeline for ``query`` and return a result dict.

    Parameters
    ----------
    query:
        The user's natural-language question.
    workspace_id:
        Workspace whose file index is searched. ``"default"`` when unset.
    chat_fn:
        Harness ``ChatFn`` used for HyDE / decomposition / multi-hop
        follow-up synthesis. ``None`` runs the pipeline in degraded mode
        (every LLM helper falls back to the query as-is).
    limit:
        Max ranked candidates returned (clamped to 1..50).
    decompose / multi_hop / use_cache:
        Per-call stage overrides. ``None`` uses the env-configured default.

    Returns
    -------
    A dict with ``status="ok"`` (``hits``, ``citation_block``, ``query_id``,
    ``count``, ``trace``) or ``status="insufficient_context"`` (``hits`` as
    hints, ``message``, ``query_id``, ``trace``). A cache hit returns the
    prior result dict with ``cache_hit=True`` added. ``status="error"`` is
    returned only for an empty query.
    """
    t0 = time.time()
    q = (query or "").strip()
    if not q:
        return {"status": "error", "query": query, "error": "query must be non-empty"}

    ws = (workspace_id or "default").strip() or "default"
    try:
        lim = max(1, min(50, int(limit)))
    except (TypeError, ValueError):
        lim = 6

    do_decompose = _DECOMPOSE_DEFAULT if decompose is None else bool(decompose)
    do_multihop = _MULTIHOP_DEFAULT if multi_hop is None else bool(multi_hop)
    do_cache = _CACHE_DEFAULT if use_cache is None else bool(use_cache)

    trace: Dict[str, Any] = {
        "workspace_id": ws,
        "decompose": do_decompose,
        "multi_hop": do_multihop,
        "use_cache": do_cache,
        "chat_fn": chat_fn is not None,
    }

    # ── (a) Cache lookup ────────────────────────────────────────────────
    # Embed once; the same vector keys both the lookup and the later insert.
    query_vec: Optional[List[float]] = None
    if do_cache:
        try:
            query_vec = embeddings.embed_one(q)
        except Exception as exc:  # noqa: BLE001
            logger.debug("rag_pipeline: cache embed failed (%s)", exc)
            query_vec = None
        if query_vec is not None:
            try:
                cached = rag_cache._lookup(query_vec)
            except Exception:  # noqa: BLE001
                cached = None
            if cached is not None:
                logger.debug("rag_pipeline: cache hit for %r", q[:60])
                hit_result = dict(cached)
                hit_result["cache_hit"] = True
                return hit_result

    # ── (a2) Soft addressing — extract query entities once ──────────────
    # Done after the cache check so a cache hit never pays the NER call.
    # The entities seed graph expansion in every retrieval below, and the
    # reader→writer feedback at both terminal branches.
    query_entities: List[str] = []
    try:
        query_entities = rag_entities.extract_entities(q, chat_fn=chat_fn)
    except Exception as exc:  # noqa: BLE001
        logger.debug("rag_pipeline: entity extraction failed (%s)", exc)
        query_entities = []
    trace["query_entities"] = query_entities

    # ── (b) Decomposition ───────────────────────────────────────────────
    sub_queries: List[str] = [q]
    if do_decompose:
        try:
            sub_queries = rag_decompose.decompose_query(q, chat_fn=chat_fn) or [q]
        except Exception as exc:  # noqa: BLE001
            logger.debug("rag_pipeline: decompose failed (%s)", exc)
            sub_queries = [q]
    trace["sub_queries"] = list(sub_queries)

    # ── (c) Retrieve per sub-query, accumulate + dedup by id ────────────
    # Over-fetch so the final rerank + gate have a real candidate pool.
    pool = max(lim * 2, 8)
    accum: Dict[str, RetrievalHit] = {}
    hops = 1
    follow_ups: List[str] = []
    for sub_query in sub_queries:
        if do_multihop:
            try:
                hits, mh_trace = rag_multihop.multi_hop_retrieve(
                    sub_query,
                    workspace_id=ws,
                    chat_fn=chat_fn,
                    limit=pool,
                    query_entities=query_entities,
                )
            except Exception as exc:  # noqa: BLE001
                logger.debug("rag_pipeline: multi-hop failed (%s)", exc)
                hits = _safe_single_hop(ws, sub_query, pool, chat_fn, query_entities)
                mh_trace = {"hops": 1, "follow_ups": []}
            hops = max(hops, int(mh_trace.get("hops") or 1))
            mh_follow_ups = mh_trace.get("follow_ups")
            if isinstance(mh_follow_ups, list):
                follow_ups.extend(str(x) for x in mh_follow_ups)
        else:
            hits = _safe_single_hop(ws, sub_query, pool, chat_fn, query_entities)
        for hit in hits:
            if hit.id and hit.id not in accum:
                accum[hit.id] = hit

    trace["hops"] = hops
    trace["follow_ups"] = follow_ups
    trace["candidate_count"] = len(accum)

    candidates = list(accum.values())
    if not candidates:
        trace["reason"] = "no_candidates"
        return _finish_insufficient(q, ws, [], query_entities, trace, t0)

    # ── (d) Final cross-encoder rerank, against the ORIGINAL query ──────
    try:
        reranked = retrieval.rerank(q, candidates)
    except Exception as exc:  # noqa: BLE001
        logger.debug("rag_pipeline: rerank failed (%s)", exc)
        reranked = candidates

    # ── (e) Score gate ──────────────────────────────────────────────────
    try:
        kept, gate_fired = rag_gate.score_threshold(reranked)
    except Exception as exc:  # noqa: BLE001
        logger.debug("rag_pipeline: gate failed (%s)", exc)
        kept, gate_fired = reranked, False

    if gate_fired or not kept:
        trace["reason"] = "gate_fired"
        return _finish_insufficient(q, ws, reranked[:3], query_entities, trace, t0)

    # ── Terminal: ok ────────────────────────────────────────────────────
    final = retrieval.label_citations(kept[:lim])
    citation_block = retrieval.format_for_prompt(final)
    hit_dicts = [h.to_dict() for h in final]
    trace["kept_count"] = len(final)
    trace["gate_fired"] = False
    trace["total_ms"] = int((time.time() - t0) * 1000)

    query_id = _log_query(q, ws, hit_dicts)
    # Reader→writer graph feedback — strengthen the entity→chunk edges that
    # produced this cited result so related future queries rank them higher.
    trace["feedback"] = _safe_record_outcome(
        q,
        "ok",
        query_entities,
        [str(h["id"]) for h in hit_dicts if h.get("id")],
        query_id,
    )

    result: Dict[str, Any] = {
        "status": "ok",
        "query": q,
        "hits": hit_dicts,
        "count": len(hit_dicts),
        "citation_block": citation_block,
        "query_id": query_id,
        "note": _OK_NOTE,
        "trace": trace,
    }

    # ── (g) Cache insert (ok results only) ──────────────────────────────
    if do_cache and query_vec is not None:
        try:
            rag_cache._insert(query_vec, result)
        except Exception:  # noqa: BLE001
            logger.debug("rag_pipeline: cache insert failed", exc_info=True)

    return result


__all__ = ["ask"]
