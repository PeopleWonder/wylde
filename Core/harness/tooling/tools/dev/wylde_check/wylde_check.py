"""wylde_check tool — thin wrapper exposing the architectural checker.

The rules themselves live at :mod:`Core.harness.dev.wylde_check`.  This
module just adapts the function signature to the tool-runner contract
(``run_<id>(params) -> envelope``).
"""

from __future__ import annotations

from typing import Any, Dict, List, Optional


def _normalise_only(value: Any) -> Optional[List[str]]:
    if value is None:
        return None
    if isinstance(value, str):
        return [value]
    if isinstance(value, (list, tuple)):
        return [str(v) for v in value]
    return None


def run_wylde_check(params: Dict[str, Any]) -> Dict[str, Any]:
    from Core.harness.dev import wylde_check as _wc

    only = _normalise_only((params or {}).get("only"))
    result = _wc.run_all(only=only)
    # The architectural checker already emits the canonical envelope
    # shape; just stamp the tool name into the summary for parity with
    # the other dev/ tools.
    data = result.get("data") or {}
    summary = data.get("summary") or {}
    summary.setdefault("tool", "wylde_check")
    return result
