"""Capability slots & smart routing , CAPABILITY_SLOTS, select_model.

A *slot* is one capability label (``code``, ``reasoning``, …); each slot tracks
one "current best" active model. ``select_model`` is the read-side API: given a
capability and budget mode, pick the highest-scoring active model.
"""

from datetime import datetime
from typing import Any, Dict, Optional

from . import _INCUMBENT_BONUS
from .profiles import _profiles

# Capability slots — each slot tracks one "current best" model
CAPABILITY_SLOTS = ["code", "reasoning", "extraction", "creative", "chat"]


def select_model(
    capability: str = "chat", budget_mode: str = "normal"
) -> Optional[str]:
    """Return the best active model name for the requested capability."""
    profiles = _profiles()
    candidates = [
        p
        for p in profiles.values()
        if p.get("status") == "active" and capability in p.get("capabilities", [])
    ]
    if not candidates:
        candidates = [p for p in profiles.values() if p.get("status") == "active"]
    if not candidates:
        return None

    def _score(p: Dict[str, Any]) -> float:
        scores = p.get("benchmark_scores", {}).get("task_scores", {})
        base = scores.get(capability, 0.0)
        if p.get("first_active_at"):
            try:
                age_days = (
                    datetime.utcnow() - datetime.fromisoformat(p["first_active_at"])
                ).days
            except Exception:
                age_days = 0
            if age_days > 30:
                base *= 1 + _INCUMBENT_BONUS
        if budget_mode == "compact":
            size_gb = p.get("size_gb") or 7
            base /= max(size_gb / 7, 1)
        return float(base)

    best = max(candidates, key=_score)
    name: str = best["name"]
    return name
