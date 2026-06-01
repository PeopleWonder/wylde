"""get_mouse_position — current cursor coordinates via PyAutoGUI."""

from __future__ import annotations

from typing import Any, Dict

from .._visual_lib import get_pyautogui


def run_get_mouse_position(params: Dict[str, Any]) -> Dict[str, Any]:
    del params
    pos = get_pyautogui().position()
    return {"x": pos.x, "y": pos.y}
