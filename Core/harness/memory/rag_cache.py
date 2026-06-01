"""Semantic query-result cache for the RAG pipeline.

A simplified port of ``_legacy/core/wylde-rag/generate.py::_QueryCache``.

**Why this cache exists — and why it is a different layer from what the
harness already has.** :mod:`retrieval` caches the *cross-encoder model*
object (a few hundred MB, loaded once per process). That is a model cache.
It does nothing for *query results*: two near-identical questions asked a
minute apart each pay the full pipeline cost again. The expensive part of
that cost is the LLM round-trips — HyDE expansion, decomposition, and
multi-hop follow-up synthesis all call ``chat_fn``. This module adds the
missing layer: a small in-memory cache keyed on the query *embedding*, so a
repeated or paraphrased question short-circuits the whole pipeline (LLM
calls included) and returns the prior result.

It is keyed on embedding cosine similarity rather than exact text so
"how do I shut the services down" and "how to shutdown the services" hit
the same entry. Unit-normalised vectors are stored, so the dot product
*is* the cosine similarity — no per-comparison division.

Simplifications vs. the legacy 120-line version: no numpy dependency (pure-
Python normalise + dot, consistent with :mod:`embeddings`), and a single
flat entry list with LRU + TTL eviction instead of the legacy's separate
bookkeeping. Tunables are env-overridable:

* ``WYLDE_RAG_CACHE_MAX``        — max entries (default 256)
* ``WYLDE_RAG_CACHE_TTL_S``      — entry lifetime in seconds (default 600)
* ``WYLDE_RAG_CACHE_SIMILARITY`` — cosine-similarity hit threshold (default 0.95)
"""

from __future__ import annotations

import math
import os
import threading
import time
from typing import Any, Dict, List, Optional, Tuple


def _env_int(name: str, default: int) -> int:
    raw = os.getenv(name)
    if raw is None or not raw.strip():
        return default
    try:
        return int(raw)
    except ValueError:
        return default


def _env_float(name: str, default: float) -> float:
    raw = os.getenv(name)
    if raw is None or not raw.strip():
        return default
    try:
        return float(raw)
    except ValueError:
        return default


CACHE_MAX_SIZE: int = max(1, _env_int("WYLDE_RAG_CACHE_MAX", 256))
CACHE_TTL_S: float = _env_float("WYLDE_RAG_CACHE_TTL_S", 600.0)
CACHE_SIMILARITY: float = _env_float("WYLDE_RAG_CACHE_SIMILARITY", 0.95)


def _unit(vec: List[float]) -> Optional[List[float]]:
    """L2-normalise ``vec``. Returns ``None`` for an empty or zero vector."""
    if not vec:
        return None
    norm = math.sqrt(sum(x * x for x in vec))
    if norm <= 0.0:
        return None
    inv = 1.0 / norm
    return [x * inv for x in vec]


def _dot(a: List[float], b: List[float]) -> float:
    """Dot product of two equal-length vectors (cosine sim when both unit)."""
    if len(a) != len(b):
        return 0.0
    return sum(x * y for x, y in zip(a, b))


# One cache entry: (inserted_at, unit_vector, result_dict).
_Entry = Tuple[float, List[float], Dict[str, Any]]


class _QueryCache:
    """Thread-safe LRU + TTL cache keyed on query-embedding cosine similarity.

    Entries are ordered oldest-first. A lookup hit moves its entry to the
    tail (most-recently-used); an insert past ``max_size`` evicts the head.
    Expired entries are pruned lazily on every lookup, so a quiet cache
    never accumulates stale results.
    """

    def __init__(self, max_size: int, ttl_s: float, sim_threshold: float) -> None:
        self._max = max(1, max_size)
        self._ttl = ttl_s
        self._thresh = sim_threshold
        self._entries: List[_Entry] = []
        self._lock = threading.Lock()

    def lookup(self, query_vec: List[float]) -> Optional[Dict[str, Any]]:
        """Return the cached result for a vector within the similarity
        threshold of a live entry, or ``None``. Best (highest-sim) match
        wins when several entries qualify."""
        unit = _unit(query_vec)
        if unit is None:
            return None
        now = time.time()
        cutoff = now - self._ttl
        with self._lock:
            # Drop expired entries in place.
            self._entries = [e for e in self._entries if e[0] >= cutoff]
            best_idx = -1
            best_sim = self._thresh
            for idx, (_, uv, _) in enumerate(self._entries):
                sim = _dot(unit, uv)
                if sim >= best_sim:
                    best_sim = sim
                    best_idx = idx
            if best_idx < 0:
                return None
            # LRU touch — move the hit entry to the tail.
            entry = self._entries.pop(best_idx)
            self._entries.append((now, entry[1], entry[2]))
            return dict(entry[2])

    def insert(self, query_vec: List[float], result: Dict[str, Any]) -> None:
        """Store ``result`` keyed on ``query_vec``. Evicts the LRU entry
        when the cache is full. A non-normalisable vector is ignored."""
        unit = _unit(query_vec)
        if unit is None:
            return
        with self._lock:
            if len(self._entries) >= self._max:
                self._entries.pop(0)
            self._entries.append((time.time(), unit, dict(result)))

    def clear(self) -> None:
        with self._lock:
            self._entries.clear()

    def size(self) -> int:
        with self._lock:
            return len(self._entries)


# Process-wide singleton — built from the env-resolved tunables at import.
_CACHE = _QueryCache(CACHE_MAX_SIZE, CACHE_TTL_S, CACHE_SIMILARITY)


def _lookup(query_vec: List[float]) -> Optional[Dict[str, Any]]:
    """Module-level lookup against the shared cache. Returns a copy of the
    cached result dict, or ``None`` on miss."""
    return _CACHE.lookup(query_vec)


def _insert(query_vec: List[float], result: Dict[str, Any]) -> None:
    """Module-level insert into the shared cache."""
    _CACHE.insert(query_vec, result)


def clear() -> None:
    """Drop every cached entry. Used by tests and after an index rebuild."""
    _CACHE.clear()


def size() -> int:
    """Current number of live entries — for diagnostics and tests."""
    return _CACHE.size()


__all__ = [
    "CACHE_MAX_SIZE",
    "CACHE_TTL_S",
    "CACHE_SIMILARITY",
    "_lookup",
    "_insert",
    "clear",
    "size",
]
