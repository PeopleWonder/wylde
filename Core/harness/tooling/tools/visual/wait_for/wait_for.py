"""wait_for — Playwright wait_for_selector wrapper."""

from __future__ import annotations

from typing import Any, Dict

from .._visual_lib import get_page


def run_wait_for(params: Dict[str, Any]) -> Dict[str, Any]:
    selector = params.get("selector")
    if not selector:
        return {"status": "error", "error": "'selector' is required"}

    state = str(params.get("state", "visible"))
    if state not in ("visible", "hidden", "attached", "detached"):
        return {
            "status": "error",
            "error": "'state' must be one of: visible, hidden, attached, detached",
        }

    try:
        timeout = int(params.get("timeout", 10000))
    except (TypeError, ValueError):
        timeout = 10000

    get_page().wait_for_selector(str(selector), state=state, timeout=timeout)
    return {
        "action": "wait_for",
        "selector": str(selector),
        "state": state,
        "found": True,
    }
