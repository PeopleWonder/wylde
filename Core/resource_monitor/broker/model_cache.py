"""Soft LRU keep-warm hints for recently-leased (service, model) pairs."""

from __future__ import annotations

import threading
import time
from dataclasses import dataclass
from typing import Dict, List

from .config import _MODEL_CACHE_TTL_S


@dataclass
class _ModelCacheEntry:
    service: str
    model: str
    bytes: int
    last_used: float
    last_priority: int = 0


class _ModelCache:
    """Soft LRU of (service, model) → recent use timestamp.

    The cache does not own VRAM, it only remembers which models were recently
    leased so that:
      • repeated reserves for the same (service, model) re-use the previous
        lease's accounting if the original is still live;
      • eviction picks lower-priority *or* less-recently-used victims when ties
        in priority occur.
    """

    def __init__(self, ttl_s: float):
        self._ttl_s = ttl_s
        self._lock = threading.Lock()
        self._entries: Dict[str, _ModelCacheEntry] = {}

    @staticmethod
    def _key(service: str, model: str) -> str:
        return f"{service}:{model}"

    def touch(self, service: str, model: str, nbytes: int, priority: int) -> None:
        with self._lock:
            self._entries[self._key(service, model)] = _ModelCacheEntry(
                service=service,
                model=model,
                bytes=nbytes,
                last_used=time.time(),
                last_priority=priority,
            )

    def last_used(self, service: str, model: str) -> float:
        with self._lock:
            ent = self._entries.get(self._key(service, model))
            return ent.last_used if ent else 0.0

    def warm_for(self, service: str, model: str) -> bool:
        with self._lock:
            ent = self._entries.get(self._key(service, model))
            if ent is None:
                return False
            return (time.time() - ent.last_used) < self._ttl_s

    def all(self) -> List[_ModelCacheEntry]:
        cutoff = time.time() - self._ttl_s
        with self._lock:
            return [e for e in self._entries.values() if e.last_used >= cutoff]

    def prune(self) -> int:
        cutoff = time.time() - self._ttl_s
        with self._lock:
            stale = [k for k, e in self._entries.items() if e.last_used < cutoff]
            for k in stale:
                self._entries.pop(k, None)
        return len(stale)


_model_cache = _ModelCache(ttl_s=_MODEL_CACHE_TTL_S)
