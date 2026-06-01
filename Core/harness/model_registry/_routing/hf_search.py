"""HuggingFace discovery , hf_search, discovery_status.

Opt-in: gated by ``MODEL_DISCOVERY_ENABLED``. Network calls only happen on
explicit invocation or via the scheduled discovery loop in ``ollama_watcher``.
"""

import json
import logging
import urllib.parse
import urllib.request
from typing import Any, Dict

from . import _DISCOVERY_FILE, DISCOVERY_ENABLED, DISCOVERY_SCHEDULE, _load_json

logger = logging.getLogger(__name__)


def hf_search(vram_gb: float, capability: str) -> list:
    """Query HuggingFace API for models fitting VRAM budget."""
    query = f"gguf {capability} language model"
    url = (
        f"https://huggingface.co/api/models"
        f"?search={urllib.parse.quote(query)}"
        f"&sort=downloads&limit=20&full=false"
    )
    try:
        with urllib.request.urlopen(url, timeout=10) as r:
            raw = r.read()
    except Exception as e:
        logger.warning("HuggingFace search failed: %s", e)
        return []
    try:
        models = json.loads(raw)
    except json.JSONDecodeError as e:
        logger.warning("HuggingFace response was not valid JSON: %s", e)
        return []
    if not isinstance(models, list):
        logger.warning("HuggingFace response was not a list: %r", type(models).__name__)
        return []

    results = []
    for m in models:
        model_id = m.get("modelId", "")
        tags = m.get("tags", [])
        results.append(
            {
                "model_id": model_id,
                "downloads": m.get("downloads", 0),
                "likes": m.get("likes", 0),
                "tags": tags,
                "ollama_pull_cmd": f"ollama pull {model_id.split('/')[-1].lower()}",
                "note": "Pull manually via Ollama to add to registry",
            }
        )
    return results


def discovery_status() -> Dict[str, Any]:
    info = _load_json(_DISCOVERY_FILE, {})
    return {
        "enabled": DISCOVERY_ENABLED,
        "schedule": DISCOVERY_SCHEDULE,
        "last_search_at": info.get("last_search_at"),
        "last_results_count": info.get("last_results_count", 0),
        "note": "Search is opt-in. Set MODEL_DISCOVERY_ENABLED=true to enable scheduled search.",
    }
