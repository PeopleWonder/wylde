"""list_loaded_models — list models currently held in memory by Ollama."""

from __future__ import annotations

import urllib.error
from typing import Any, Dict

from .._ollama_lib import get


def run_list_loaded_models(params: Dict[str, Any]) -> Dict[str, Any]:
    del params  # unused
    try:
        data = get("/api/ps")
    except urllib.error.URLError as exc:
        return {"status": "error", "error": f"ollama unreachable: {exc}"}
    except Exception as exc:
        return {"status": "error", "error": str(exc)}
    models = [
        {
            "name": m.get("name"),
            "size": m.get("size"),
            "size_vram": m.get("size_vram"),
            "expires_at": m.get("expires_at"),
        }
        for m in data.get("models", [])
    ]
    return {"status": "success", "models": models, "count": len(models)}
