"""auto_evict_lru — sweep loaded models, evict LRU until under threshold."""

from __future__ import annotations

import urllib.error
from typing import Any, Dict, List

from .._ollama_lib import VRAM_EVICT_THRESHOLD_MB, get, post


def run_auto_evict_lru(params: Dict[str, Any]) -> Dict[str, Any]:
    try:
        threshold_mb = int(params.get("threshold_mb", VRAM_EVICT_THRESHOLD_MB))
    except (TypeError, ValueError):
        threshold_mb = VRAM_EVICT_THRESHOLD_MB
    dry_run = bool(params.get("dry_run", False))

    try:
        data = get("/api/ps")
    except urllib.error.URLError as exc:
        return {"status": "error", "error": f"ollama unreachable: {exc}"}
    except Exception as exc:
        return {"status": "error", "error": str(exc)}

    models = data.get("models", [])
    if not models:
        return {
            "status": "success",
            "message": "no models loaded",
            "evicted": [],
            "vram_mb": 0,
        }

    total_vram_mb = sum((m.get("size_vram") or 0) for m in models) / (1024 * 1024)
    if total_vram_mb <= threshold_mb:
        return {
            "status": "success",
            "message": f"VRAM {total_vram_mb:.0f} MiB below threshold {threshold_mb} MiB — nothing to evict",
            "evicted": [],
            "vram_mb": round(total_vram_mb),
        }

    # Soonest-to-expire first = least recently used.
    sortable = sorted(models, key=lambda m: m.get("expires_at") or "")
    evicted: List[Dict[str, Any]] = []
    for m in sortable:
        if total_vram_mb <= threshold_mb:
            break
        name = m.get("name", "")
        if not name:
            continue
        vram_mb = (m.get("size_vram") or 0) / (1024 * 1024)
        if dry_run:
            total_vram_mb -= vram_mb
            evicted.append(
                {"model": name, "vram_freed_mb": round(vram_mb), "dry_run": True}
            )
            continue
        try:
            post(
                "/api/generate",
                {"model": name, "prompt": "", "stream": False, "keep_alive": 0},
                timeout=60,
            )
            total_vram_mb -= vram_mb
            evicted.append({"model": name, "vram_freed_mb": round(vram_mb)})
        except Exception as exc:
            evicted.append({"model": name, "vram_freed_mb": 0, "error": str(exc)})

    return {
        "status": "success",
        "evicted": evicted,
        "vram_after_mb": round(total_vram_mb),
        "threshold_mb": threshold_mb,
    }
