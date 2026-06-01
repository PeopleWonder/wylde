"""browser_fill — Playwright form-field fill."""

from __future__ import annotations

from typing import Any, Dict

from .._visual_lib import get_page


def run_browser_fill(params: Dict[str, Any]) -> Dict[str, Any]:
    selector = params.get("selector")
    if not selector:
        return {"status": "error", "error": "'selector' is required"}
    if "value" not in params:
        return {"status": "error", "error": "'value' is required"}

    value = str(params["value"])
    try:
        timeout = int(params.get("timeout", 10000))
    except (TypeError, ValueError):
        timeout = 10000

    get_page().fill(str(selector), value, timeout=timeout)
    return {
        "action": "browser_fill",
        "selector": str(selector),
        "value_length": len(value),
    }
