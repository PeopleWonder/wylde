"""Retrieval pipeline: HyDE → hybrid (vector + BM25) → cross-encoder rerank → forced citations.

Composes the workspace file index (and, optionally, workspace memory)
into a ranked context block ready for prompt injection. Each stage
degrades gracefully so the pipeline still produces sensible output
when the model / library / data isn't available.

Pipeline stages
---------------

1. **HyDE** (Hypothetical Document Embeddings) — ask the LLM to draft
   a short hypothetical answer to the query, embed THAT instead of the
   raw query, and search with the resulting vector. The hypothetical
   answer matches the corpus's vocabulary better than a question
   does. If no ``chat_fn`` is wired, we skip HyDE and embed the raw
   query — degraded but still functional.

2. **Hybrid retrieval** — vector search via the workspace's LanceDB
   files table + a BM25-style keyword search over the same chunks.
   The BM25 implementation is a lightweight in-process scorer (no
   external dependency) — it walks the chunk list once and scores by
   token-frequency / inverse-document-frequency. Good enough for
   workspace-sized corpora; if ``rank_bm25`` becomes available we'd
   swap to it transparently.

3. **Reciprocal Rank Fusion** combines the two ranked lists. Cheap,
   parameter-free, and competitive with learned fusion in published
   benchmarks.

4. **Cross-encoder rerank** (optional) — if ``sentence-transformers``
   is installed, run the top-N fused candidates through a cross-encoder
   for a final reorder. We reach for the classic ``ms-marco-MiniLM-L-6-v2``
   when nothing else is configured. If sentence-transformers isn't
   installed, this stage is a no-op and the fused score wins.

5. **Forced citations** — every result keeps its source path + chunk
   index so prompt-block formatters can stamp ``[1] path:line`` markers
   the LLM is told to cite. The actual citation enforcement happens
   in the system prompt; this module just preserves the metadata.

Public surface: :func:`retrieve` is the one-shot pipeline; lower-level
:func:`hyde_query`, :func:`hybrid_search`, :func:`rerank` are exposed
for tests and bespoke callers.
"""

from __future__ import annotations

import logging
import math
import re
from collections import Counter
from dataclasses import dataclass
from typing import Any, Callable, Dict, List, Optional

# workspaces: removed in config-file-backed redesign (2026-06-05) —
# workspace file RAG is now owned by Rust; this module no longer sources
# vector candidates from a Python workspace index.

logger = logging.getLogger("wylde.harness.memory.retrieval")


# ── Result shape ───────────────────────────────────────────────────────


@dataclass
class RetrievalHit:
    """One ranked chunk. ``score`` is the post-rerank value; the
    component scores survive for inspection / tuning."""

    id: str
    path: str
    chunk_idx: int
    content: str
    score: float
    vector_score: float = 0.0
    bm25_score: float = 0.0
    rerank_score: float = 0.0
    citation_label: str = ""

    def to_dict(self) -> Dict[str, Any]:
        return {
            "id": self.id,
            "path": self.path,
            "chunk_idx": self.chunk_idx,
            "content": self.content,
            "score": self.score,
            "vector_score": self.vector_score,
            "bm25_score": self.bm25_score,
            "rerank_score": self.rerank_score,
            "citation_label": self.citation_label,
        }


# ── Stage 1: HyDE ──────────────────────────────────────────────────────


_HYDE_SYSTEM = (
    "You write short, plausible-looking answers to the user's question, "
    "in 2-3 sentences max, using the kind of vocabulary that would appear "
    "in a real source document. Do not refuse, do not hedge, do not ask "
    "for clarification. Just produce the hypothetical answer text."
)


def hyde_query(
    query: str,
    *,
    chat_fn: Optional[Callable[..., Any]] = None,
    model: Optional[str] = None,
) -> str:
    """Generate a hypothetical answer for ``query`` and return the text.

    ``chat_fn`` matches the harness driver's :class:`ChatFn` shape — the
    function takes ``messages``, ``tools``, ``model`` (and optional
    streaming callbacks) and returns an object with a ``.text`` attr.
    Falls back to the raw query when ``chat_fn`` is None or the call
    raises.
    """
    if chat_fn is None:
        return query
    messages = [
        {"role": "system", "content": _HYDE_SYSTEM},
        {"role": "user", "content": query},
    ]
    try:
        step = chat_fn(messages=messages, tools=[], model=model)
    except Exception as exc:  # noqa: BLE001
        logger.debug("retrieval: HyDE call failed (%s); using raw query", exc)
        return query
    text = getattr(step, "text", None)
    if not isinstance(text, str) or not text.strip():
        return query
    # Combine HyDE with the query so token-overlap BM25 still sees the
    # original keywords. Vector search uses the combined embedding.
    return f"{text}\n\n{query}"


# ── Stage 2 + 3: hybrid (vector + BM25 + RRF) ──────────────────────────


def hybrid_search(
    workspace_id: str,
    query_text: str,
    *,
    raw_query: str = "",
    limit: int = 8,
    candidate_pool: int = 32,
    use_graph: bool = True,
    graph_hops: int = 1,
    graph_max_extra: int = 12,
    query_entities: Optional[List[str]] = None,
) -> List[RetrievalHit]:
    """Vector + BM25 fusion over the workspace's file index, optionally
    expanded by Memgraph entity-edge graph distance.

    ``query_text`` is what we embed (post-HyDE); ``raw_query`` is the
    original user text used for BM25 token overlap. If ``raw_query`` is
    empty, BM25 falls back to ``query_text``.

    ``use_graph`` (default True) adds graph-distance neighbours of the
    vector pool's chunks to the fusion. Best-effort: if Memgraph isn't
    reachable, the graph stage is a silent no-op and the vector + BM25
    fusion runs unchanged.

    ``query_entities`` is the "soft addressing" hook — entity names
    extracted from the user query are passed to the graph-expansion stage
    as additional traverse seeds, so the graph walk starts from what the
    query is *about*, not only from the vector pool's chunks. Empty /
    ``None`` leaves graph expansion unchanged.
    """
    if not query_text:
        return []
    raw = raw_query or query_text

    # workspaces: removed in config-file-backed redesign (2026-06-05) —
    # the workspace file index that previously sourced the vector
    # candidate pool now lives in Rust. With no Python workspace RAG
    # source, this branch yields no candidates (clean break, pointer-only).
    vector_hits: List[Any] = []
    if not vector_hits:
        return []

    # Build BM25 over the vector pool. Scoring chunks the vector layer
    # didn't surface would require enumerating the whole table; for
    # workspace-sized corpora the vector top-N is a good candidate set.
    bm25_scores = _bm25_over(raw, vector_hits)

    # Stage 2.5: graph-distance expansion. After Memgraph entity edges
    # have been written via workspace_memory.save(entities=[...]), this
    # surfaces chunks that share entity ties with the seed set even
    # when their text didn't match the query directly.
    graph_neighbours: List[Any] = []
    _graph: Any = None
    if use_graph:
        try:
            from . import graph_retrieval as _graph_mod

            _graph = _graph_mod
        except ImportError:
            try:
                from Core.harness.memory import graph_retrieval as _graph_mod

                _graph = _graph_mod
            except ImportError:
                _graph = None
        if _graph is not None:
            try:
                graph_neighbours = _graph.expand_by_graph(
                    vector_hits,
                    workspace_id=workspace_id,
                    hops=graph_hops,
                    max_extra=graph_max_extra,
                    seed_entities=query_entities,
                )
            except Exception as exc:  # noqa: BLE001
                logger.debug("retrieval: graph expansion failed (%s)", exc)
                graph_neighbours = []

    # Reciprocal Rank Fusion — k=60 is the conventional constant. The
    # graph stage gets its own rank dimension (sorted by hop count
    # ascending = closest first).
    vector_rank = {h["id"]: idx for idx, h in enumerate(vector_hits)}
    bm25_rank = {
        hid: idx
        for idx, (hid, _) in enumerate(
            sorted(bm25_scores.items(), key=lambda kv: kv[1], reverse=True)
        )
    }
    graph_rank = {gh.id: idx for idx, gh in enumerate(graph_neighbours)}

    # Union the candidate ids — graph neighbours add NEW chunks we
    # haven't seen in the vector pool, so the fused dict needs to span
    # both worlds.
    all_ids = set(vector_rank) | set(graph_rank)
    K = 60.0
    fused: Dict[str, float] = {}
    for hid in all_ids:
        score = 0.0
        score += 1.0 / (K + vector_rank.get(hid, candidate_pool))
        score += 1.0 / (K + bm25_rank.get(hid, candidate_pool))
        if hid in graph_rank:
            score += 1.0 / (K + graph_rank[hid])
        fused[hid] = score

    # Build the hit objects sorted by fused score.
    by_id_vector = {h["id"]: h for h in vector_hits}
    by_id_graph = {gh.id: gh for gh in graph_neighbours}
    sorted_ids = sorted(fused, key=lambda k: fused[k], reverse=True)[:limit]
    out: List[RetrievalHit] = []
    for hid in sorted_ids:
        if hid in by_id_vector:
            h = by_id_vector[hid]
            out.append(
                RetrievalHit(
                    id=hid,
                    path=h.get("path", ""),
                    chunk_idx=int(h.get("chunk_idx", 0)),
                    content=h.get("content", ""),
                    score=fused[hid],
                    vector_score=float(h.get("similarity", 0.0)),
                    bm25_score=float(bm25_scores.get(hid, 0.0)),
                )
            )
        elif hid in by_id_graph:
            gh = by_id_graph[hid]
            out.append(
                RetrievalHit(
                    id=hid,
                    path=gh.path,
                    chunk_idx=0,
                    content=gh.content,
                    score=fused[hid],
                    vector_score=0.0,
                    bm25_score=0.0,
                )
            )
    return out


def _bm25_over(query: str, hits: List[Dict[str, Any]]) -> Dict[str, float]:
    """Simple in-process BM25 over the candidate pool. Good enough.

    Uses k1=1.5, b=0.75 — Okapi defaults. Tokenisation is lowercase
    word-split, no stemming. Scoring is per-chunk-id so the caller can
    align with the vector hit list.
    """
    if not hits:
        return {}

    docs: Dict[str, List[str]] = {}
    for h in hits:
        docs[h["id"]] = _tokenize(h.get("content", ""))
    avg_len = sum(len(t) for t in docs.values()) / max(1, len(docs))

    # Document frequency for each query token.
    q_tokens = _tokenize(query)
    if not q_tokens:
        return {hid: 0.0 for hid in docs}
    df: Counter = Counter()
    for tokens in docs.values():
        unique = set(tokens)
        for tok in q_tokens:
            if tok in unique:
                df[tok] += 1
    n_docs = len(docs)

    K1 = 1.5
    B = 0.75
    scores: Dict[str, float] = {}
    for hid, tokens in docs.items():
        tf = Counter(tokens)
        score = 0.0
        for tok in q_tokens:
            if df[tok] == 0:
                continue
            idf = math.log(1 + (n_docs - df[tok] + 0.5) / (df[tok] + 0.5))
            num = tf[tok] * (K1 + 1)
            denom = tf[tok] + K1 * (1 - B + B * (len(tokens) / max(1, avg_len)))
            score += idf * (num / denom)
        scores[hid] = score
    return scores


_TOKEN_RE = re.compile(r"[A-Za-z0-9_]+")


def _tokenize(text: str) -> List[str]:
    if not isinstance(text, str):
        return []
    return [t.lower() for t in _TOKEN_RE.findall(text)]


# ── Stage 4: cross-encoder rerank ──────────────────────────────────────


_DEFAULT_CROSS_ENCODER = "cross-encoder/ms-marco-MiniLM-L-6-v2"
_cross_encoder_cache: Dict[str, Any] = {}


def rerank(
    query: str,
    hits: List[RetrievalHit],
    *,
    model_name: str = _DEFAULT_CROSS_ENCODER,
    top_k: Optional[int] = None,
) -> List[RetrievalHit]:
    """Cross-encoder rerank when ``sentence-transformers`` is available.

    Loads a model the first time it's used and caches it for the
    process lifetime — the cross-encoder is a few hundred MB and we
    don't want to pay that hit on every call. If sentence-transformers
    isn't installed, returns the input list unchanged.
    """
    if not hits or not query:
        return hits
    try:
        from sentence_transformers import CrossEncoder
    except ImportError:
        logger.debug("retrieval: sentence-transformers not installed; skip rerank")
        return hits
    try:
        model = _cross_encoder_cache.get(model_name)
        if model is None:
            model = CrossEncoder(model_name)
            _cross_encoder_cache[model_name] = model
    except Exception as exc:  # noqa: BLE001
        logger.warning(
            "retrieval: failed to load cross-encoder %s: %s", model_name, exc
        )
        return hits

    pool = hits[: top_k or len(hits)]
    pairs = [(query, h.content) for h in pool]
    try:
        scores = model.predict(pairs)
    except Exception as exc:  # noqa: BLE001
        logger.warning("retrieval: cross-encoder predict failed: %s", exc)
        return hits

    reranked: List[RetrievalHit] = []
    for h, s in zip(pool, scores):
        h.rerank_score = float(s)
        h.score = float(s)  # promote rerank to the headline score
        reranked.append(h)
    reranked.sort(key=lambda x: x.rerank_score, reverse=True)
    # Append any tail hits that weren't reranked — preserves the
    # candidate set's original ordering for the rest.
    if top_k is not None and len(hits) > top_k:
        reranked.extend(hits[top_k:])
    return reranked


# ── Stage 5: citation labelling ────────────────────────────────────────


def label_citations(hits: List[RetrievalHit]) -> List[RetrievalHit]:
    """Assign ``[1]`` / ``[2]`` / ... labels to each hit. The system
    prompt tells the LLM to cite using these labels — labelling here
    keeps the formatter and the prompt instruction in sync.
    """
    for idx, h in enumerate(hits, start=1):
        h.citation_label = f"[{idx}]"
    return hits


# ── Pipeline entry point ───────────────────────────────────────────────


def retrieve(
    workspace_id: str,
    query: str,
    *,
    limit: int = 6,
    chat_fn: Optional[Callable[..., Any]] = None,
    hyde_model: Optional[str] = None,
    do_rerank: bool = True,
    query_entities: Optional[List[str]] = None,
) -> List[RetrievalHit]:
    """One-shot pipeline: HyDE → hybrid → rerank → citations.

    Each stage is independently bypassable so a degraded environment
    (no LLM for HyDE, no sentence-transformers for rerank) still
    produces ranked hits.

    ``query_entities`` (soft addressing) is forwarded to
    :func:`hybrid_search` as additional graph-expansion seeds; ``None``
    leaves graph behaviour unchanged.
    """
    if not workspace_id or not query:
        return []
    expanded = hyde_query(query, chat_fn=chat_fn, model=hyde_model)
    hybrid = hybrid_search(
        workspace_id,
        expanded,
        raw_query=query,
        limit=max(limit * 2, 8),
        query_entities=query_entities,
    )
    if do_rerank:
        hybrid = rerank(query, hybrid, top_k=min(len(hybrid), limit * 2))
    final = label_citations(hybrid[:limit])
    return final


def format_for_prompt(hits: List[RetrievalHit]) -> str:
    """Render hits as a citations-ready block. Intended for direct
    drop-in to the system prompt's RAG slot.
    """
    if not hits:
        return ""
    lines = []
    for h in hits:
        snippet = h.content.strip().replace("\n", " ")
        if len(snippet) > 600:
            snippet = snippet[:600] + " …"
        # Citation marker the LLM is told to use, plus the source so it
        # can verify it's not making things up.
        lines.append(f"{h.citation_label} {h.path} (chunk {h.chunk_idx}): {snippet}")
    return "\n".join(lines)


__all__ = [
    "RetrievalHit",
    "hyde_query",
    "hybrid_search",
    "rerank",
    "label_citations",
    "retrieve",
    "format_for_prompt",
]
