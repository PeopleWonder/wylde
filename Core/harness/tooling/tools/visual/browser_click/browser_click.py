"""browser_click — Playwright click via selector or viewport coordinates."""

from __future__ import annotations

from typing import Any, Dict

from .._visual_lib import get_page


def run_browser_click(params: Dict[str, Any]) -> Dict[str, Any]:
    selector = params.get("selector")
    button = str(params.get("button", "left"))
    if button not in ("left", "right", "middle"):
        return {
            "status": "error",
            "error": "'button' must be 'left', 'right', or 'middle'",
        }

    try:
        timeout = int(params.get("timeout", 10000))
    except (TypeError, ValueError):
        timeout = 10000

    page = get_page()

    if selector:
        page.click(str(selector), button=button, timeout=timeout)
        return {
            "action": "browser_click",
            "selector": str(selector),
            "button": button,
        }

    if "x" in params and "y" in params:
        try:
            x = float(params["x"])
            y = float(params["y"])
        except (TypeError, ValueError):
            return {"status": "error", "error": "'x' and 'y' must be numbers"}
        page.mouse.click(x, y, button=button)
        return {"action": "browser_click", "x": x, "y": y, "button": button}

    return {
        "status": "error",
        "error": "either 'selector' or both 'x' and 'y' are required",
    }
