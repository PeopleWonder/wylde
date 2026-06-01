"""browser_text — Playwright innerText extraction.

Output is capped at 10 KiB so a runaway page doesn't blow the LLM context
window. The full character count is reported alongside, so callers can
detect truncation.
"""

from __future__ import annotations

from typing import Any, Dict

from .._visual_lib import get_page

_MAX_CHARS = 10_000


def run_browser_text(params: Dict[str, Any]) -> Dict[str, Any]:
    selector = str(params.get("selector") or "body")
    try:
        timeout = int(params.get("timeout", 10000))
    except (TypeError, ValueError):
        timeout = 10000

    text = get_page().locator(selector).inner_text(timeout=timeout)
    truncated = len(text) > _MAX_CHARS
    return {
        "action": "browser_text",
        "selector": selector,
        "text": text[:_MAX_CHARS],
        "length": len(text),
        "truncated": truncated,
    }
