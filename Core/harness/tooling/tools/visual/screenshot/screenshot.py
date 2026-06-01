"""screenshot — desktop screenshot via PyAutoGUI.

Pulled forward from ``_legacy/.../visual_interact.py``. Returns a
base64-encoded PNG so a multimodal LLM can consume it directly.
"""

from __future__ import annotations

from typing import Any, Dict

from .._visual_lib import get_pyautogui, screenshot_to_b64


def run_screenshot(params: Dict[str, Any]) -> Dict[str, Any]:
    pag = get_pyautogui()
    region = params.get("region")

    if region and isinstance(region, (list, tuple)) and len(region) == 4:
        img = pag.screenshot(region=tuple(region))
    else:
        img = pag.screenshot()

    b64, width, height, size_bytes = screenshot_to_b64(img)
    return {
        "base64_image": b64,
        "width": width,
        "height": height,
        "format": "png",
        "size_bytes": size_bytes,
    }
