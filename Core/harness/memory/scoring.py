"""Importance + recency-decay scoring shared across the memory layers.

Used by long-term, workspace, and short-term layers when ranking
candidates during retrieval. The formula is the one the Wylde user specified:

    score = similarity * importance * exp(-age_days / decay)

* ``similarity`` is the upstream relevance signal (cosine for vector,
  RRF / BM25 / overlap-token for keyword paths). Caller passes it in.
* ``importance`` is 0..10, supplied at write time by the LLM (or a
  fallback heuristic — see :func:`heuristic_importance`).
* ``age_days`` is how long since the memory was last touched. The
  decay constant defaults to 30 (memory loses ~63% of its weight at
  one month) but is configurable per call.

Fallback heuristic for when the LLM declines to set importance: a
length+entity heuristic capped at 8 (so heuristic-marked memories
never crowd out genuinely-important hand-tagged ones). Pure function,
deterministic, easy to test.
"""

from __future__ import annotations

import math
import time
from typing import Any, Iterable, Optional


DEFAULT_DECAY_DAYS = 30.0
SECONDS_PER_DAY = 86400.0


def combined_score(
    similarity: float,
    importance: float,
    last_used_at: float,
    *,
    decay_days: float = DEFAULT_DECAY_DAYS,
    now: Optional[float] = None,
) -> float:
    """the Wylde user's formula: ``similarity * importance * exp(-age_days / decay)``.

    ``importance`` is normalised to 0..1 by dividing by 10 so the output
    stays comparable across importance values; the LLM's 0..10 scale
    is preserved for inspection / sorting in the Settings UI.

    A memory with similarity 0.5, importance 8, and age 30 days returns:
        0.5 * 0.8 * exp(-1) ≈ 0.147

    Same memory at age 0:
        0.5 * 0.8 * 1.0 = 0.40
    """
    now = now if now is not None else time.time()
    age_days = max(0.0, (now - float(last_used_at)) / SECONDS_PER_DAY)
    importance_norm = max(0.0, min(1.0, float(importance) / 10.0))
    decay = math.exp(-age_days / max(1e-6, float(decay_days)))
    return float(similarity) * importance_norm * decay


def heuristic_importance(body: str, *, entity_count: int = 0) -> int:
    """Crude importance estimator for memories the LLM didn't tag.

    Capped at 8 deliberately — the 9–10 band is reserved for
    explicitly-flagged user identity / hard preferences. Returns an
    integer in [1, 8] so storage can keep the same int8-shaped column
    whether the value came from the LLM or here.

    Heuristic: 3 base + 1 per 100 chars (up to 4) + 1 per entity
    (up to 3). No model in the loop.
    """
    if not isinstance(body, str):
        body = str(body or "")
    length_pts = min(4, len(body) // 100)
    entity_pts = min(3, max(0, int(entity_count)))
    score = 3 + length_pts + entity_pts
    return max(1, min(8, score))


def normalize_importance(raw: Any, body: str = "", *, entity_count: int = 0) -> int:
    """Coerce an LLM-supplied importance to the int 1..10 range.

    Falls back to :func:`heuristic_importance` when the input isn't
    numeric (e.g. the model dropped the field). Always returns a value
    in [1, 10] — never 0, since 0 implies "filed as not worth keeping"
    which the storage layer interprets as "skip this entirely".
    """
    try:
        n = float(raw)
    except (TypeError, ValueError):
        return heuristic_importance(body, entity_count=entity_count)
    if n != n:  # NaN
        return heuristic_importance(body, entity_count=entity_count)
    return max(1, min(10, int(round(n))))


def rank_by_score(
    candidates: Iterable[dict],
    *,
    decay_days: float = DEFAULT_DECAY_DAYS,
    now: Optional[float] = None,
    similarity_key: str = "similarity",
    importance_key: str = "importance",
    last_used_key: str = "last_used_at",
    score_key: str = "score",
) -> list:
    """Annotate each candidate with a combined ``score`` and sort desc.

    Mutates a copy of each input dict (adds the score field); the
    originals are left alone. Returns a new list — caller's iterable
    is not consumed in place.
    """
    out = []
    for c in candidates:
        s = combined_score(
            similarity=c.get(similarity_key, 0.0),
            importance=c.get(importance_key, 5),
            last_used_at=c.get(last_used_key, 0.0),
            decay_days=decay_days,
            now=now,
        )
        annotated = dict(c)
        annotated[score_key] = s
        out.append(annotated)
    out.sort(key=lambda r: r.get(score_key, 0.0), reverse=True)
    return out


__all__ = [
    "DEFAULT_DECAY_DAYS",
    "combined_score",
    "heuristic_importance",
    "normalize_importance",
    "rank_by_score",
]
