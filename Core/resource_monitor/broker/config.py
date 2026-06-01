"""Environment-driven configuration constants for the VRAM broker."""

from __future__ import annotations

import os
from pathlib import Path

# Safety margin: never hand out the last N bytes. NVIDIA drivers allocate
# ~256MB of scratch for CUDA kernels that no reservation covers.
_SAFETY_MARGIN = int(os.getenv("WYLDE_VRAM_SAFETY_MB", "512")) * 1024 * 1024

_DEFAULT_TTL = float(os.getenv("WYLDE_VRAM_TTL", "60"))
_OLLAMA_URL = os.getenv("OLLAMA_URL", "http://127.0.0.1:11434")
_OLLAMA_POLL_S = float(os.getenv("WYLDE_VRAM_OLLAMA_POLL", "5"))
_REAPER_POLL_S = float(os.getenv("WYLDE_VRAM_REAPER_POLL", "2"))
_MANIFEST_POLL_S = float(os.getenv("WYLDE_VRAM_MANIFEST_POLL", "2"))

_EVICT_TIMEOUT_S = float(os.getenv("WYLDE_VRAM_EVICT_TIMEOUT", "3"))

# Soft-eviction grace period: when the broker decides a lease must yield, it
# sends /vram/please-evict first and gives the owner this many seconds to
# finish in-flight work before the hard /vram/evict kicks in. Set to 0 to
# disable graceful eviction entirely (legacy fire-and-forget behaviour).
_GRACE_PERIOD_S = float(os.getenv("WYLDE_VRAM_GRACE_PERIOD", "10"))

# Model cache: per-(service, model) "keep warm for N seconds" hints. When a
# lease releases, we record the (service, model) in a soft-LRU. Subsequent
# reservations for the same (service, model) get a small priority boost so
# we don't pay another cold start unnecessarily.
_MODEL_CACHE_TTL_S = float(os.getenv("WYLDE_VRAM_MODEL_CACHE_TTL", "1800"))

# This file lives at Core/resource_monitor/broker/config.py, so the repo root
# is four parents up (broker/ → resource_monitor/ → Core/ → repo root).
_WYLDE_ROOT = Path(
    os.getenv("WYLDE_ROOT", Path(__file__).resolve().parent.parent.parent.parent)
)
_STATE_PATH = _WYLDE_ROOT / "data" / "state" / "vram-broker.json"
