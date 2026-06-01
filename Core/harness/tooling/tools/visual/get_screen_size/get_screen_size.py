"""get_screen_size — primary monitor resolution via PyAutoGUI."""

from __future__ import annotations

from typing import Any, Dict

from .._visual_lib import get_pyautogui


def run_get_screen_size(params: Dict[str, Any]) -> Dict[str, Any]:
    del params
    size = get_pyautogui().size()
    return {"width": size.width, "height": size.height}
