"""Background worker threads: reaper, Ollama poller, manifest-state writer."""

from __future__ import annotations

import json
import logging
import os
import threading
import time
import urllib.error
import urllib.request
import uuid
from dataclasses import dataclass, field
from typing import Any, Dict, List, Optional

from .config import (
    _DEFAULT_TTL,
    _GRACE_PERIOD_S,
    _MANIFEST_POLL_S,
    _MODEL_CACHE_TTL_S,
    _OLLAMA_POLL_S,
    _OLLAMA_URL,
    _REAPER_POLL_S,
    _SAFETY_MARGIN,
    _STATE_PATH,
)
from .model_cache import _model_cache
from .registry import Lease, _refresh_nvml, _registry

logger = logging.getLogger(__name__)


def _poll_ollama() -> List[Lease]:
    """Query Ollama /api/ps and produce synthetic leases for each model.
    Failures are swallowed — Ollama being down is a normal state."""
    try:
        req = urllib.request.Request(_OLLAMA_URL + "/api/ps")
        with urllib.request.urlopen(req, timeout=2.0) as resp:
            raw = resp.read().decode("utf-8", errors="replace")
        data = json.loads(raw)
    except (urllib.error.URLError, OSError, ValueError):
        return []

    now = time.time()
    leases: List[Lease] = []
    for m in data.get("models", []) or []:
        name = m.get("name") or m.get("model") or "unknown"
        size_vram = int(m.get("size_vram") or m.get("size") or 0)
        if size_vram <= 0:
            continue
        leases.append(
            Lease(
                lease_id=f"ollama:{uuid.uuid4().hex}",
                service="ollama",
                model=name,
                bytes=size_vram,
                priority=100,
                granted_at=now,
                # Synthetic leases are rebuilt on every poll; the TTL only
                # exists so the shape matches real leases.
                expires_at=now + 3600,
                heartbeat_at=now,
                pid=0,
                synthetic=True,
            )
        )
    return leases


@dataclass
class _Threads:
    reaper: Optional[threading.Thread] = None
    ollama: Optional[threading.Thread] = None
    manifest: Optional[threading.Thread] = None
    stop: threading.Event = field(default_factory=threading.Event)


_threads = _Threads()


def _reaper_loop() -> None:
    cache_prune_every_n = max(1, int(60.0 / max(_REAPER_POLL_S, 1.0)))
    tick = 0
    while not _threads.stop.wait(timeout=_REAPER_POLL_S):
        _refresh_nvml()
        removed = _registry.reap_expired()
        for lease in removed:
            logger.info(
                "vram_broker: reaped expired lease %s (%s/%s %d bytes)",
                lease.lease_id[:8],
                lease.service,
                lease.model,
                lease.bytes,
            )
        tick += 1
        if tick % cache_prune_every_n == 0:
            pruned = _model_cache.prune()
            if pruned:
                logger.debug("vram_broker: pruned %d stale model-cache entries", pruned)


def _ollama_loop() -> None:
    while not _threads.stop.wait(timeout=_OLLAMA_POLL_S):
        leases = _poll_ollama()
        _registry.replace_synthetic("ollama", leases)


def _manifest_loop() -> None:
    # Write once immediately so the GUI has something even before the
    # first poll interval passes.
    _write_state()
    while not _threads.stop.wait(timeout=_MANIFEST_POLL_S):
        _write_state()


def _write_state() -> None:
    try:
        state = _state_snapshot()
        _STATE_PATH.parent.mkdir(parents=True, exist_ok=True)
        tmp = _STATE_PATH.with_suffix(".tmp")
        tmp.write_text(json.dumps(state, indent=2), encoding="utf-8")
        os.replace(tmp, _STATE_PATH)
    except Exception as e:
        logger.debug("vram_broker: state write failed: %s", e)


def _state_snapshot() -> Dict[str, Any]:
    now = time.time()
    leases = _registry.all_leases()
    by_service: Dict[str, Dict[str, Any]] = {}
    for lease in leases:
        row = by_service.setdefault(
            lease.service,
            {
                "service": lease.service,
                "bytes": 0,
                "count": 0,
                "priority": lease.priority,
                "synthetic": lease.synthetic,
            },
        )
        row["bytes"] += lease.bytes
        row["count"] += 1
        row["priority"] = max(row["priority"], lease.priority)
    total = _registry.total()
    reserved = _registry.reserved_total()
    nvml_ts = _registry.nvml_last_update()
    cache_entries = _model_cache.all()
    return {
        "generated_at": now,
        "gpu": {
            "total_bytes": total,
            "actual_used_bytes": _registry.actual_used(),
            "reserved_bytes": reserved,
            "free_for_grant": _registry.free_for_grant(),
            "safety_margin": _SAFETY_MARGIN,
            "name": _registry.gpu_name(),
            "nvml_fresh_s": now - nvml_ts if nvml_ts else None,
        },
        "leases": [lease.to_wire() for lease in leases],
        "by_service": sorted(by_service.values(), key=lambda r: -r["priority"]),
        "model_cache": {
            "ttl_s": _MODEL_CACHE_TTL_S,
            "entries": [
                {
                    "service": e.service,
                    "model": e.model,
                    "bytes": e.bytes,
                    "last_used": e.last_used,
                    "warm_for": max(0.0, _MODEL_CACHE_TTL_S - (now - e.last_used)),
                }
                for e in sorted(cache_entries, key=lambda x: -x.last_used)
            ],
        },
        "config": {
            "safety_margin_bytes": _SAFETY_MARGIN,
            "default_ttl": _DEFAULT_TTL,
            "ollama_poll_s": _OLLAMA_POLL_S,
            "grace_period_s": _GRACE_PERIOD_S,
            "model_cache_ttl_s": _MODEL_CACHE_TTL_S,
        },
    }


def _start_background() -> None:
    if _threads.reaper is not None:
        return
    _threads.stop.clear()
    _threads.reaper = threading.Thread(
        target=_reaper_loop,
        name="vram-reaper",
        daemon=True,
    )
    _threads.reaper.start()

    _threads.ollama = threading.Thread(
        target=_ollama_loop,
        name="vram-ollama-poll",
        daemon=True,
    )
    _threads.ollama.start()

    _threads.manifest = threading.Thread(
        target=_manifest_loop,
        name="vram-manifest",
        daemon=True,
    )
    _threads.manifest.start()
    logger.info(
        "vram_broker: background threads started (reaper %.1fs, ollama %.1fs, manifest %.1fs)",
        _REAPER_POLL_S,
        _OLLAMA_POLL_S,
        _MANIFEST_POLL_S,
    )


def _reset() -> None:
    # Tests call this between cases. Signal old loops to exit, drop the
    # thread refs so _start_background() will spin up fresh ones, and swap
    # in a clean stop Event so the next round starts unset. Old daemon
    # threads end up waiting on the new event — harmless until process exit.
    _threads.stop.set()
    _threads.reaper = None
    _threads.ollama = None
    _threads.manifest = None
    _threads.stop = threading.Event()
