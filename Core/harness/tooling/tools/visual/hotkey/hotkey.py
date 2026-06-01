"""hotkey — press a key combo via PyAutoGUI."""

from __future__ import annotations

from typing import Any, Dict

from .._visual_lib import get_pyautogui


def run_hotkey(params: Dict[str, Any]) -> Dict[str, Any]:
    raw = params.get("keys")
    if not raw:
        return {
            "status": "error",
            "error": "'keys' is required (space-separated key names)",
        }

    keys = str(raw).split()
    if not keys:
        return {"status": "error", "error": "'keys' must contain at least one key name"}

    get_pyautogui().hotkey(*keys)
    return {"action": "hotkey", "keys": keys}
