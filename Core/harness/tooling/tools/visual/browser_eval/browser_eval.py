"""browser_eval — Playwright page.evaluate wrapper."""

from __future__ import annotations

from typing import Any, Dict

from .._visual_lib import get_page


def run_browser_eval(params: Dict[str, Any]) -> Dict[str, Any]:
    expression = params.get("expression")
    if not expression:
        return {"status": "error", "error": "'expression' is required"}

    result = get_page().evaluate(str(expression))
    return {"action": "browser_eval", "result": result}
