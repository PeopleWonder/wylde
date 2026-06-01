"""preload_model — load a model into VRAM without generating tokens."""

from __future__ import annotations

import urllib.error
from typing import Any, Dict

from .._ollama_lib import DEFAULT_KEEP_ALIVE, post


def run_preload_model(params: Dict[str, Any]) -> Dict[str, Any]:
    model = params.get("model")
    if not model:
        return {"status": "error", "error": "'model' is required"}
    keep_alive = params.get("keep_alive", DEFAULT_KEEP_ALIVE)
    try:
        out = post(
            "/api/generate",
            {
                "model": str(model),
                "prompt": "",
                "stream": False,
                "keep_alive": keep_alive,
            },
            timeout=300,
        )
    except urllib.error.URLError as exc:
        return {"status": "error", "error": f"ollama unreachable: {exc}"}
    except Exception as exc:
        return {"status": "error", "error": str(exc)}
    return {
        "status": "success",
        "model": model,
        "keep_alive": keep_alive,
        "loaded": True,
        "ollama": {
            k: v for k, v in out.items() if k in ("done", "done_reason", "model")
        },
    }
