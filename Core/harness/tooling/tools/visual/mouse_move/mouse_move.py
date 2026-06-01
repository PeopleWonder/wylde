"""mouse_move — move cursor without clicking via PyAutoGUI."""

from __future__ import annotations

from typing import Any, Dict

from .._visual_lib import get_pyautogui


def run_mouse_move(params: Dict[str, Any]) -> Dict[str, Any]:
    if "x" not in params or "y" not in params:
        return {"status": "error", "error": "'x' and 'y' are required"}

    try:
        x = int(params["x"])
        y = int(params["y"])
    except (TypeError, ValueError):
        return {"status": "error", "error": "'x' and 'y' must be coercible to int"}

    try:
        duration = float(params.get("duration", 0.2))
    except (TypeError, ValueError):
        duration = 0.2

    get_pyautogui().moveTo(x, y, duration=duration)
    return {"action": "mouse_move", "x": x, "y": y, "duration": duration}
