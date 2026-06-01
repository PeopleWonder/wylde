"""scroll — desktop mouse-wheel scroll via PyAutoGUI."""

from __future__ import annotations

from typing import Any, Dict

from .._visual_lib import get_pyautogui


def run_scroll(params: Dict[str, Any]) -> Dict[str, Any]:
    if "amount" not in params:
        return {"status": "error", "error": "'amount' is required"}

    try:
        amount = int(params["amount"])
    except (TypeError, ValueError):
        return {"status": "error", "error": "'amount' must be an integer"}

    pag = get_pyautogui()
    x = params.get("x")
    y = params.get("y")

    if x is not None and y is not None:
        try:
            pag.scroll(amount, int(x), int(y))
        except (TypeError, ValueError):
            return {"status": "error", "error": "'x' and 'y' must be coercible to int"}
        return {"action": "scroll", "amount": amount, "x": int(x), "y": int(y)}

    pag.scroll(amount)
    return {"action": "scroll", "amount": amount}
