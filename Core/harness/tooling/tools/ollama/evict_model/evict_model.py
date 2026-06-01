"""evict_model — unload a model from VRAM (keep_alive=0)."""

from __future__ import annotations

import urllib.error
from typing import Any, Dict

from .._ollama_lib import post


def run_evict_model(params: Dict[str, Any]) -> Dict[str, Any]:
    model = params.get("model")
    if not model:
        return {"status": "error", "error": "'model' is required"}
    try:
        post(
            "/api/generate",
            {"model": str(model), "prompt": "", "stream": False, "keep_alive": 0},
            timeout=60,
        )
    except urllib.error.URLError as exc:
        return {"status": "error", "error": f"ollama unreachable: {exc}"}
    except Exception as exc:
        return {"status": "error", "error": str(exc)}
    return {"status": "success", "model": model, "evicted": True}
