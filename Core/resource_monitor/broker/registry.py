"""Lease store, nvml accounting, and the registry-level reset hook."""

from __future__ import annotations

import logging
import threading
import time
from dataclasses import asdict, dataclass
from typing import Any, Dict, List, Optional

from .config import _SAFETY_MARGIN

logger = logging.getLogger(__name__)


@dataclass
class Lease:
    lease_id: str
    service: str
    model: str
    bytes: int
    priority: int
    granted_at: float
    expires_at: float
    heartbeat_at: float
    pid: int = 0
    synthetic: bool = False  # Ollama-reflected lease
    client_nonce: str = ""

    def to_wire(self) -> Dict[str, Any]:
        return asdict(self)


class _Registry:
    """Thread-safe lease store + headroom accounting.

    The nvml-reported free bytes drift from our accounting (due to driver
    overhead, un-brokered loads, allocator fragmentation). So the broker
    grants against *reserved* bytes for predictability, but exposes nvml
    values separately so the GUI can show the divergence.
    """

    def __init__(self) -> None:
        self._leases: Dict[str, Lease] = {}
        self._by_nonce: Dict[str, str] = {}  # client_nonce -> lease_id
        self._lock = threading.RLock()
        self._total_bytes = 0  # from nvml
        self._actual_used_bytes = 0  # from nvml
        self._gpu_name = ""
        self._nvml_last_update = 0.0

    # ─── nvml wrappers ────────────────────────────────────────────────
    def set_gpu(self, total: int, used: int, name: str) -> None:
        with self._lock:
            self._total_bytes = int(total)
            self._actual_used_bytes = int(used)
            self._gpu_name = name
            self._nvml_last_update = time.time()

    def total(self) -> int:
        with self._lock:
            return self._total_bytes

    def actual_used(self) -> int:
        with self._lock:
            return self._actual_used_bytes

    def gpu_name(self) -> str:
        with self._lock:
            return self._gpu_name

    def nvml_last_update(self) -> float:
        with self._lock:
            return self._nvml_last_update

    def reserved_total(self) -> int:
        with self._lock:
            return sum(lease.bytes for lease in self._leases.values())

    def free_for_grant(self) -> int:
        """Bytes we're willing to hand out right now."""
        with self._lock:
            return max(0, self._total_bytes - self.reserved_total() - _SAFETY_MARGIN)

    # ─── lease CRUD ───────────────────────────────────────────────────
    def find_by_nonce(self, nonce: str) -> Optional[Lease]:
        if not nonce:
            return None
        with self._lock:
            lid = self._by_nonce.get(nonce)
            return self._leases.get(lid) if lid else None

    def add(self, lease: Lease) -> None:
        with self._lock:
            self._leases[lease.lease_id] = lease
            if lease.client_nonce:
                self._by_nonce[lease.client_nonce] = lease.lease_id

    def get(self, lease_id: str) -> Optional[Lease]:
        with self._lock:
            return self._leases.get(lease_id)

    def remove(self, lease_id: str) -> Optional[Lease]:
        with self._lock:
            lease = self._leases.pop(lease_id, None)
            if lease and lease.client_nonce:
                self._by_nonce.pop(lease.client_nonce, None)
            return lease

    def touch(self, lease_id: str, ttl: float) -> Optional[Lease]:
        with self._lock:
            lease = self._leases.get(lease_id)
            if lease is None:
                return None
            now = time.time()
            lease.heartbeat_at = now
            lease.expires_at = now + ttl
            return lease

    def all_leases(self) -> List[Lease]:
        with self._lock:
            return list(self._leases.values())

    def reap_expired(self) -> List[Lease]:
        now = time.time()
        removed: List[Lease] = []
        with self._lock:
            for lid, lease in list(self._leases.items()):
                if lease.synthetic:
                    continue  # synthetic leases are reconciled by the Ollama poller
                if now > lease.expires_at:
                    self._leases.pop(lid, None)
                    if lease.client_nonce:
                        self._by_nonce.pop(lease.client_nonce, None)
                    removed.append(lease)
        return removed

    def replace_synthetic(self, service: str, new_leases: List[Lease]) -> None:
        """Atomically swap all synthetic leases for one service. Used by the
        Ollama poller — we don't try to track each model's identity across
        polls, we just rebuild the set each tick."""
        with self._lock:
            for lid, lease in list(self._leases.items()):
                if lease.synthetic and lease.service == service:
                    self._leases.pop(lid, None)
                    if lease.client_nonce:
                        self._by_nonce.pop(lease.client_nonce, None)
            for lease in new_leases:
                self._leases[lease.lease_id] = lease


_registry = _Registry()


# ── nvml bridge ───────────────────────────────────────────────────────
_pynvml = None


def _init_nvml() -> bool:
    global _pynvml
    if _pynvml is not None:
        return True
    try:
        import pynvml as _p

        _p.nvmlInit()
        _pynvml = _p
        return True
    except Exception as e:
        logger.warning(
            "vram_broker: pynvml unavailable, headroom accounting will use env total only: %s",
            e,
        )
        return False


def _refresh_nvml() -> None:
    if _pynvml is None:
        return
    try:
        h = _pynvml.nvmlDeviceGetHandleByIndex(0)
        name = _pynvml.nvmlDeviceGetName(h)
        if isinstance(name, bytes):
            name = name.decode(errors="replace")
        mem = _pynvml.nvmlDeviceGetMemoryInfo(h)
        _registry.set_gpu(total=mem.total, used=mem.used, name=name)
    except Exception as e:
        logger.debug("vram_broker: nvml refresh failed: %s", e)


def _reset() -> None:
    # Clear in place rather than rebinding _registry — keeps the shim's
    # re-export pointing at the same live instance across test runs.
    with _registry._lock:
        _registry._leases.clear()
        _registry._by_nonce.clear()
        _registry._total_bytes = 0
        _registry._actual_used_bytes = 0
        _registry._gpu_name = ""
        _registry._nvml_last_update = 0.0
