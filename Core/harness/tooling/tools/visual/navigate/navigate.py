"""navigate — Playwright page navigation."""

from __future__ import annotations

from typing import Any, Dict

from .._visual_lib import get_page


def run_navigate(params: Dict[str, Any]) -> Dict[str, Any]:
    url = params.get("url")
    if not url:
        return {"status": "error", "error": "'url' is required"}

    wait_until = str(params.get("wait_until", "load"))
    if wait_until not in ("load", "domcontentloaded", "networkidle", "commit"):
        return {
            "status": "error",
            "error": "'wait_until' must be one of: load, domcontentloaded, networkidle, commit",
        }

    try:
        timeout = int(params.get("timeout", 30000))
    except (TypeError, ValueError):
        timeout = 30000

    page = get_page()
    page.goto(str(url), wait_until=wait_until, timeout=timeout)
    return {
        "action": "navigate",
        "url": page.url,
        "title": page.title(),
        "wait_until": wait_until,
    }
