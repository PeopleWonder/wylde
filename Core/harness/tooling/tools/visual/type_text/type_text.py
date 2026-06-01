"""type_text — type a string via PyAutoGUI.

Falls back to ``write`` for non-ASCII text since ``typewrite`` is
ASCII-only on Windows.
"""

from __future__ import annotations

from typing import Any, Dict

from .._visual_lib import get_pyautogui


def run_type_text(params: Dict[str, Any]) -> Dict[str, Any]:
    if "text" not in params:
        return {"status": "error", "error": "'text' is required"}

    text = str(params["text"])
    try:
        interval = float(params.get("interval", 0.02))
    except (TypeError, ValueError):
        interval = 0.02

    pag = get_pyautogui()
    if text.isascii():
        pag.typewrite(text, interval=interval)
    else:
        pag.write(text)

    return {"action": "type", "characters_typed": len(text)}
