"""Score gating + insufficient-context detection for the RAG pipeline.

Ported from ``_legacy/core/wylde-rag/reranker.py::score_threshold`` and
``_legacy/core/wylde-rag/citation.py::insufficient_context_response``.

The gate is the pipeline's confidence check. After the final cross-encoder
rerank, :func:`score_threshold` keeps only the candidates whose score clears
a *relative* bar — ``top_score × cutoff`` — and reports whether the gate
"fired". A fired gate means retrieval did not surface anything confidently
relevant, and the orchestrator should return an insufficient-context
response instead of handing weak candidates to the LLM.

Why a *relative* threshold rather than the legacy absolute ``GATE_SCORE_MIN``:
the headline ``score`` on a :class:`~retrieval.RetrievalHit` is whichever
stage ran last — a cross-encoder logit (roughly -11..+11) when
sentence-transformers is installed, or a small positive RRF fusion score
(~0.02-0.05) in the degraded path. An absolute floor calibrated for one is
meaningless for the other. A relative cutoff adapts to both:

* When the best candidate scores **non-positive**, nothing is confidently
  relevant — the gate fires.
* Otherwise the top candidate always clears ``top × cutoff`` (for a cutoff
  in ``(0, 1]``), so the degraded RRF path — all-positive scores — never
  spuriously fires the gate. It just trims the weak tail.

Cutoff is env-overridable via ``WYLDE_RAG_GATE_CUTOFF`` (default 0.5).
"""

from __future__ import annotations

import os
from typing import Any, Dict, List, Optional, Tuple

from .retrieval import RetrievalHit


def _env_float(name: str, default: float) -> float:
    """Read a float env var, falling back to ``default`` on unset / garbage."""
    raw = os.getenv(name)
    if raw is None or not raw.strip():
        return default
    try:
        return float(raw)
    except ValueError:
        return default


# Fraction of the top score a candidate must reach to survive the gate.
GATE_CUTOFF: float = _env_float("WYLDE_RAG_GATE_CUTOFF", 0.5)


def score_threshold(
    hits: List[RetrievalHit],
    *,
    cutoff: Optional[float] = None,
) -> Tuple[List[RetrievalHit], bool]:
    """Apply the confidence gate to a ranked hit list.

    Returns ``(kept, gate_fired)``:

    * ``kept`` — hits whose ``score`` is at least ``top_score × cutoff``.
    * ``gate_fired`` — ``True`` when retrieval produced nothing confidently
      relevant: an empty input, a non-positive top score, or (defensively)
      an empty kept set. When ``True`` the caller should return an
      insufficient-context response rather than generating an answer.

    ``cutoff`` overrides :data:`GATE_CUTOFF` for this call.
    """
    cut = GATE_CUTOFF if cutoff is None else float(cutoff)
    if not hits:
        return [], True

    top = max(h.score for h in hits)
    if top <= 0.0:
        # Best candidate is not positively relevant — no confident context.
        return [], True

    threshold = top * cut
    kept = [h for h in hits if h.score >= threshold]
    return kept, len(kept) == 0


def insufficient_context_response(
    query: str,
    hits: List[RetrievalHit],
) -> Dict[str, Any]:
    """Build the standard insufficient-context response body.

    The pipeline returns this shape (instead of ``status="ok"``) when the
    gate fires or retrieval found nothing. The top few hits that *were*
    retrieved are still surfaced as ``hints`` so the LLM can say "I didn't
    find enough to answer confidently, but these look related" — strictly
    more useful than a bare failure.

    The caller (``rag_pipeline``) stamps ``query_id`` and ``trace`` onto the
    returned dict before handing it back.
    """
    return {
        "status": "insufficient_context",
        "query": query,
        "answer": None,
        "hits": [h.to_dict() for h in (hits or [])[:3]],
        "count": 0,
        "message": (
            "Retrieval confidence was below threshold for this query. The "
            "indexed corpus either does not cover this topic, or the "
            "question needs rephrasing. Consider widening the query, "
            "indexing more paths, or falling back to a non-memory tool."
        ),
    }


__all__ = ["GATE_CUTOFF", "score_threshold", "insufficient_context_response"]
