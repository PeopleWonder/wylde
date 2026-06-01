"""MRU cap settings — single home for ``MRU_LIMIT`` and related constants.

The MRU cap is persisted to ``workspace_settings.json`` so the user
can tune it from the Settings UI without restarting the harness.
Activation logic in ``_store.py`` calls :func:`get_mru_limit` to read
the effective value; clamping ensures garbage on disk falls back to
the default rather than crashing the registry.
"""

from __future__ import annotations

import json
import logging
from pathlib import Path
from typing import Any, Dict

from .._common import DATA_DIR, ensure_dir

logger = logging.getLogger("wylde.harness.memory.workspaces")

# MRU cap. Activating one past the cap evicts the oldest. The user can
# tune this from the Settings UI; ``MRU_LIMIT`` is the *default* the
# constant kept for back-compat with callers that didn't get the memo,
# but eviction logic and ``get_mru_limit()`` always read the persisted
# value below.
MRU_LIMIT_DEFAULT: int = 5
MRU_LIMIT_MIN: int = 1
MRU_LIMIT_MAX: int = 20
MRU_LIMIT: int = MRU_LIMIT_DEFAULT  # back-compat shim

SETTINGS_PATH: Path = DATA_DIR / "workspace_settings.json"


def _clamp_mru(value: Any) -> int:
    """Validate the MRU cap. Raises ``ValueError`` on garbage so the
    pipe surface can return a structured ``bad_request``."""
    if isinstance(value, bool):
        # bool is a subclass of int; reject explicitly so True/False
        # don't sneak through as 1/0.
        raise ValueError("mru limit must be an integer, not bool")
    try:
        n = int(value)
    except (TypeError, ValueError):
        raise ValueError(f"mru limit must be an integer, got {value!r}")
    if n < MRU_LIMIT_MIN or n > MRU_LIMIT_MAX:
        raise ValueError(
            f"mru limit must be in [{MRU_LIMIT_MIN}, {MRU_LIMIT_MAX}], got {n}"
        )
    return n


def _read_settings() -> Dict[str, Any]:
    if not SETTINGS_PATH.exists():
        return {}
    try:
        raw = json.loads(SETTINGS_PATH.read_text(encoding="utf-8"))
    except Exception as exc:  # noqa: BLE001
        logger.warning("workspaces: settings unreadable, using defaults: %s", exc)
        return {}
    return raw if isinstance(raw, dict) else {}


def _write_settings(settings: Dict[str, Any]) -> None:
    ensure_dir(SETTINGS_PATH.parent)
    tmp = SETTINGS_PATH.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(settings, indent=2), encoding="utf-8")
    tmp.replace(SETTINGS_PATH)


def get_mru_limit() -> int:
    """Effective MRU cap — reads the persisted value, falls back to
    :data:`MRU_LIMIT_DEFAULT` if no setting is on disk yet."""
    raw = _read_settings().get("mru_limit")
    if raw is None:
        return MRU_LIMIT_DEFAULT
    try:
        return _clamp_mru(raw)
    except ValueError:
        return MRU_LIMIT_DEFAULT


def set_mru_limit(value: Any) -> int:
    """Persist a new MRU cap. If the new cap is smaller than the
    current MRU count, evict the oldest workspaces immediately —
    deleting their index folders but preserving the durable workspace
    memory (same semantics as ``_evict_past_mru``).
    """
    # Lazy import to avoid the _store → _mru → _store cycle at module load.
    from ._store import _evict_past_mru, _load_registry, _registry_lock, _save_registry

    n = _clamp_mru(value)
    with _registry_lock:
        settings = _read_settings()
        settings["mru_limit"] = n
        _write_settings(settings)
        # Apply immediately if the cap shrank below the current count.
        workspaces = _load_registry()
        evicted = _evict_past_mru(workspaces, limit=n)
        if evicted:
            _save_registry(workspaces)
    if evicted:
        logger.info(
            "workspaces: mru_limit set to %d — evicted %d older workspace(s): %s",
            n,
            len(evicted),
            ", ".join(evicted),
        )
    else:
        logger.info("workspaces: mru_limit set to %d", n)
    return n
