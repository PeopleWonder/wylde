"""Churn prevention , promote_model, swap eligibility, pending-swap state.

Promotion is gated on minimum benchmark runs, a delta threshold over the
incumbent, and a per-week swap cap. When a candidate beats the incumbent by
enough but auto-promotion is disabled, the swap is queued in
``pending_swaps.json`` for the user to confirm.
"""

import logging
import os
import threading
from datetime import datetime, timedelta
from pathlib import Path
from typing import Any, Dict

from . import (
    _FALLBACK_DAYS,
    _INCUMBENT_BONUS,
    _MAX_SWAP_PER_WEEK,
    _MIN_BENCHMARK_RUNS,
    _MIN_DELTA_PCT,
    _PENDING_SWAPS_FILE,
    _SWAPS_FILE,
    _load_json,
    _lock,
    _save_json,
)
from .profiles import _profiles, _save_profiles, get_profile
from .slots import select_model

logger = logging.getLogger(__name__)


# ── Pending swap suggestions ──────────────────────────────────────────────────


def load_pending_swaps() -> Dict[str, Dict]:
    data: Dict[str, Dict] = _load_json(_PENDING_SWAPS_FILE, {})
    return data


def _queue_swap_prompt(
    capability: str, candidate: str, incumbent: str, delta_pct: float
) -> None:
    with _lock:
        swaps = load_pending_swaps()
        swaps[capability] = {
            "capability": capability,
            "candidate": candidate,
            "incumbent": incumbent,
            "delta_pct": round(delta_pct, 1),
            "queued_at": datetime.utcnow().isoformat(),
        }
        _save_json(_PENDING_SWAPS_FILE, swaps)


def clear_swap_prompt(capability: str) -> None:
    with _lock:
        swaps = load_pending_swaps()
        swaps.pop(capability, None)
        _save_json(_PENDING_SWAPS_FILE, swaps)


def _notify_orchestrator_reset(capability: str) -> None:
    """Reset autotuner scores for all tracked nodes after a capability swap.

    Previously a cross-process HTTP call; now a direct in-process call since
    the model registry lives inside the orchestrator.
    """
    try:
        import autotuner as at_mod

        autotuner_dir = Path(os.getenv("AUTOTUNER_DIR", "/autotuner"))
        reset_count = 0
        try:
            for wf_dir in autotuner_dir.iterdir():
                if not wf_dir.is_dir():
                    continue
                for node_dir in wf_dir.iterdir():
                    if not node_dir.is_dir():
                        continue
                    at_mod.reset_scores(wf_dir.name, node_dir.name)
                    reset_count += 1
        except FileNotFoundError:
            pass
        logger.info(
            "Autotuner scores reset after model swap (capability=%r, nodes=%d)",
            capability,
            reset_count,
        )
    except Exception as e:
        logger.warning("Autotuner reset failed: %s", e)


# ── Swap eligibility & promotion ──────────────────────────────────────────────


def _can_promote(candidate: Dict, incumbent: Dict, capability: str) -> tuple[bool, str]:
    if candidate.get("benchmark_runs", 0) < _MIN_BENCHMARK_RUNS:
        return (
            False,
            f"Needs {_MIN_BENCHMARK_RUNS} benchmark runs (has {candidate.get('benchmark_runs', 0)})",
        )

    c_score = (
        candidate.get("benchmark_scores", {}).get("task_scores", {}).get(capability, 0)
    )
    i_score = (
        incumbent.get("benchmark_scores", {}).get("task_scores", {}).get(capability, 0)
    )

    if incumbent.get("first_active_at"):
        try:
            age_days = (
                datetime.utcnow() - datetime.fromisoformat(incumbent["first_active_at"])
            ).days
        except Exception:
            age_days = 0
        if age_days > 30:
            i_score *= 1 + _INCUMBENT_BONUS

    delta = (c_score - i_score) / max(i_score, 0.01)
    if delta < _MIN_DELTA_PCT:
        return False, f"Delta {delta * 100:.1f}% < required {_MIN_DELTA_PCT * 100:.0f}%"

    swaps = _load_json(_SWAPS_FILE, [])
    week_ago = (datetime.utcnow() - timedelta(days=7)).isoformat()
    recent = [
        s
        for s in swaps
        if s.get("capability") == capability and s.get("swapped_at", "") > week_ago
    ]
    if len(recent) >= _MAX_SWAP_PER_WEEK:
        return False, f"Swap limit reached for '{capability}' this week"

    return True, "eligible"


def promote_model(name: str, capability: str, force: bool = False) -> Dict[str, Any]:
    """Promote a candidate model to active for a capability slot."""
    candidate = get_profile(name)
    if candidate is None:
        return {"error": "Model not profiled", "status": 404}

    incumbent_name = None
    if not force:
        incumbent_name = select_model(capability)
        if incumbent_name:
            incumbent = get_profile(incumbent_name) or {}
            ok, reason = _can_promote(candidate, incumbent, capability)
            if not ok:
                return {"error": f"Promotion blocked: {reason}", "status": 409}

    with _lock:
        profiles = _profiles()
        for m, p in profiles.items():
            if p.get("status") == "active" and capability in p.get("capabilities", []):
                p["status"] = "fallback"
                p["fallback_until"] = (
                    datetime.utcnow() + timedelta(days=_FALLBACK_DAYS)
                ).isoformat()

        if name not in profiles:
            profiles[name] = candidate
        profiles[name]["status"] = "active"
        profiles[name]["first_active_at"] = (
            profiles[name].get("first_active_at") or datetime.utcnow().isoformat()
        )
        if capability and capability not in profiles[name].get("capabilities", []):
            profiles[name].setdefault("capabilities", []).append(capability)

        _save_profiles(profiles)

    swaps = _load_json(_SWAPS_FILE, [])
    swaps.append(
        {
            "capability": capability,
            "from": incumbent_name,
            "to": name,
            "swapped_at": datetime.utcnow().isoformat(),
            "forced": force,
        }
    )
    _save_json(_SWAPS_FILE, swaps)

    threading.Thread(
        target=_notify_orchestrator_reset,
        args=(capability,),
        daemon=True,
        name=f"reset-scores-{capability}",
    ).start()

    return {"status": "promoted", "model": name, "capability": capability}
