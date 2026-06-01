"""Ollama discovery & background daemons.

Hosts ``list_ollama_models`` (the read-side query against ``/api/tags``) plus
the two daemon threads kicked off from ``start_background_threads``:

* ``_model_watcher`` , polls Ollama for newly-pulled models and triggers an
  auto-bench + swap-prompt pipeline when one appears.
* ``_schedule_discovery`` , periodic HuggingFace search loop, dormant unless
  ``MODEL_DISCOVERY_ENABLED=true``.
"""

import json
import logging
import os
import threading
import time
import urllib.request
from datetime import datetime
from typing import Dict, List, Optional

from . import (
    DISCOVERY_ENABLED,
    DISCOVERY_SCHEDULE,
    OLLAMA_URL,
    _DISCOVERY_FILE,
    _MIN_DELTA_PCT,
    _load_json,
)
from .benchmarks import bench_model
from .churn import _queue_swap_prompt
from .hf_search import hf_search
from .profiles import get_profile, upsert_profile
from .slots import select_model

logger = logging.getLogger(__name__)


def _use_pipe() -> bool:
    return os.getenv("WYLDE_HARNESS_OLLAMA_TRANSPORT", "pipe").strip().lower() == "pipe"


def _ollama_get(path: str, timeout: int = 5) -> Optional[Dict]:
    """Read-only Ollama call. Pipe-gated when WYLDE_HARNESS_OLLAMA_TRANSPORT=pipe.

    The watcher polls only /api/tags and /api/ps, both of which are
    available as pipe actions (ollama.list_models, ollama.list_loaded).
    """
    if _use_pipe():
        try:
            from Core.shared import ipc

            action = {
                "/api/tags": "ollama.list_models",
                "/api/ps": "ollama.list_loaded",
            }.get(path)
            if action is not None:
                reply = ipc.send_action(
                    "wylde-ollama", action, {}, timeout=float(timeout)
                )
                if reply.ok:
                    return reply.data  # type: ignore[no-any-return]
                # pipe round-tripped but service errored — fall through to HTTP
        except Exception as e:  # noqa: BLE001
            logger.debug("Ollama pipe %s error: %s; falling back to HTTP", path, e)
    try:
        with urllib.request.urlopen(OLLAMA_URL + path, timeout=timeout) as r:
            data: Dict = json.loads(r.read())
            return data
    except Exception as e:
        logger.debug("Ollama %s error: %s", path, e)
        return None


def list_ollama_models() -> List[str]:
    data = _ollama_get("/api/tags")
    if not data:
        return []
    return [m["name"] for m in data.get("models", [])]


# ── Polling: detect new Ollama models ─────────────────────────────────────────

_known_models: set = set()


def _model_watcher() -> None:
    global _known_models
    time.sleep(15)
    _known_models = set(list_ollama_models())
    while True:
        time.sleep(60)
        current = set(list_ollama_models())
        new = current - _known_models
        for name in new:
            logger.info("New model detected: %s, running auto-benchmark", name)
            threading.Thread(
                target=_auto_benchmark_new_model,
                args=(name,),
                daemon=True,
                name=f"bench-{name[:20]}",
            ).start()
        _known_models = current


def _auto_benchmark_new_model(name: str) -> None:
    try:
        scores = bench_model(name)
        upsert_profile(
            name,
            lambda existing: {
                "last_benchmarked": datetime.utcnow().isoformat(),
                "benchmark_scores": scores,
                "benchmark_runs": existing.get("benchmark_runs", 0) + 1,
                "status": "candidate",
                "capabilities": [scores["primary_capability"]],
            },
        )
        cap = scores["primary_capability"]
        current = select_model(cap)
        if current and current != name:
            c_score = scores["task_scores"].get(cap, 0)
            i_profile = get_profile(current) or {}
            i_score = (
                i_profile.get("benchmark_scores", {}).get("task_scores", {}).get(cap, 0)
            )
            tok_s = scores.get("tok_s_gen", 0)
            delta = (c_score - i_score) / max(i_score, 0.01) * 100
            logger.info(
                "AUTO-BENCH %s: cap=%s score=%.3f tok/s=%.1f | current %s: score=%.3f delta=%.1f%%",
                name,
                cap,
                c_score,
                tok_s,
                current,
                i_score,
                delta,
            )
            if delta >= _MIN_DELTA_PCT * 100:
                _queue_swap_prompt(cap, name, current, delta)
                logger.info(
                    "Queued swap suggestion: %s → %s for '%s' (%.1f%%)",
                    current,
                    name,
                    cap,
                    delta,
                )
    except Exception as e:
        logger.error("Auto-benchmark failed for %s: %s", name, e)


def _schedule_discovery() -> None:
    if not DISCOVERY_ENABLED:
        return
    while True:
        info = _load_json(_DISCOVERY_FILE, {})
        last = info.get("last_search_at")
        if last:
            try:
                since = (datetime.utcnow() - datetime.fromisoformat(last)).days
                if DISCOVERY_SCHEDULE == "weekly" and since < 7:
                    time.sleep(3600)
                    continue
            except Exception:
                pass
        logger.info("Scheduled model discovery running (user-enabled)")
        hf_search(vram_gb=16, capability="")
        time.sleep(3600)


def start_background_threads() -> None:
    """Start model-watcher and scheduled-discovery daemon threads.

    Call once at startup.
    """
    threading.Thread(target=_model_watcher, daemon=True, name="model-watcher").start()
    threading.Thread(
        target=_schedule_discovery, daemon=True, name="model-discovery"
    ).start()
