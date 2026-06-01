"""browser_screenshot — Playwright page or element screenshot."""

from __future__ import annotations

from typing import Any, Dict

from .._visual_lib import encode_b64, get_page


def run_browser_screenshot(params: Dict[str, Any]) -> Dict[str, Any]:
    full_page = bool(params.get("full_page", False))
    selector = params.get("selector")

    page = get_page()
    if selector:
        img_bytes = page.locator(str(selector)).screenshot()
    else:
        img_bytes = page.screenshot(full_page=full_page)

    return {
        "base64_image": encode_b64(img_bytes),
        "format": "png",
        "size_bytes": len(img_bytes),
        "url": page.url,
        "selector": selector or None,
        "full_page": full_page,
    }
