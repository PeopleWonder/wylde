"""click — desktop mouse click via PyAutoGUI."""

from __future__ import annotations

from typing import Any, Dict

from .._visual_lib import get_pyautogui


def run_click(params: Dict[str, Any]) -> Dict[str, Any]:
    if "x" not in params or "y" not in params:
        return {"status": "error", "error": "'x' and 'y' are required"}

    pag = get_pyautogui()
    try:
        x = int(params["x"])
        y = int(params["y"])
    except (TypeError, ValueError):
        return {"status": "error", "error": "'x' and 'y' must be coercible to int"}

    button = str(params.get("button", "left"))
    if button not in ("left", "right", "middle"):
        return {
            "status": "error",
            "error": "'button' must be 'left', 'right', or 'middle'",
        }

    try:
        clicks = max(1, int(params.get("clicks", 1)))
    except (TypeError, ValueError):
        clicks = 1

    pag.click(x, y, button=button, clicks=clicks)
    return {"action": "click", "x": x, "y": y, "button": button, "clicks": clicks}
