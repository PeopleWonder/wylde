"""model_state — runtime knowledge about a model the harness has talked to.

Distinct from connection health (``ollama_client.check_health``):

* health = "is the daemon up?"
* model_state = "what do we know about *this model* that's useful for routing?"

Two concerns share this file:

1. **Capability cache** — process-local. Sticky-fallback for tool support: if
   a model returns "does not support tools" on the first call, we remember
   that and stop sending the ``tools`` field on follow-ups. Without it,
   switching mid-session to a tool-capable model would also be permanently
   capped.
2. **Active-model selection** — persisted to ``$DATA_DIR/active_model.json``
   so the choice survives orchestrator restarts and is observable across
   processes (via pipe action). The InferenceBar dropdown and any non-GUI
   caller share this single value.
"""

from __future__ import annotations

import json
import logging
import os
import threading
from pathlib import Path
from typing import Dict, Optional

logger = logging.getLogger(__name__)


# ─── Capability cache (process-local) ──────────────────────────────────────

_lock = threading.Lock()
_tool_failures: Dict[str, bool] = {}


def model_supports_tools(model: str) -> bool:
    """Return False only if we previously saw ``model`` reject the ``tools`` field."""
    if not model:
        return True
    with _lock:
        return not _tool_failures.get(model, False)


def mark_tool_failure(model: str) -> None:
    """Record that ``model`` does not handle the ``tools`` field — strip it next time."""
    if not model:
        return
    with _lock:
        _tool_failures[model] = True


def forget_model(model: str) -> None:
    """Drop any cached capability state for a single model (e.g. on model swap)."""
    if not model:
        return
    with _lock:
        _tool_failures.pop(model, None)


def reset_capabilities() -> None:
    """Drop the entire capability cache (e.g. test setup, full reload)."""
    with _lock:
        _tool_failures.clear()


# ─── Active-model selection (persisted) ────────────────────────────────────

# When flattening from the legacy ``model_state/active.py`` + ``capabilities.py``
# we collide on the private name ``_lock``. Capabilities owns ``_lock`` (set
# above); active-model state takes ``_active_lock``.
_PATH = Path(
    os.getenv("ACTIVE_MODEL_PATH")
    or Path(os.getenv("DATA_DIR", "data")) / "active_model.json"
)
_active_lock = threading.Lock()
_cached: Optional[str] = None
_loaded = False


def _read_disk() -> Optional[str]:
    try:
        if _PATH.exists():
            data = json.loads(_PATH.read_text(encoding="utf-8"))
            name = data.get("model")
            if isinstance(name, str) and name.strip():
                return name.strip()
    except Exception as exc:
        logger.warning("active_model load failed (%s); starting empty", exc)
    return None


def _write_disk(name: Optional[str]) -> None:
    try:
        _PATH.parent.mkdir(parents=True, exist_ok=True)
        _PATH.write_text(
            json.dumps({"model": name or ""}, indent=2),
            encoding="utf-8",
        )
    except Exception as exc:
        logger.warning("active_model save failed: %s", exc)


def get_active_model() -> Optional[str]:
    """Return the persisted active model, or ``None`` if none chosen yet."""
    global _cached, _loaded
    with _active_lock:
        if not _loaded:
            _cached = _read_disk()
            _loaded = True
        return _cached


# ─── Default-model selection (persisted, env fallback) ──────────────────

# The "default model" is the user's starred preference — distinct from
# the *active* model (the dropdown's current pick). It survives restarts
# and falls back to ``WYLDE_DEFAULT_MODEL`` when the user hasn't starred
# one yet. Stored next to ``active_model.json`` in the harness data dir.
_DEFAULT_PATH = Path(
    os.getenv("DEFAULT_MODEL_PATH")
    or Path(os.getenv("DATA_DIR", "data")) / "default_model.json"
)
_default_lock = threading.Lock()
_default_cached: Optional[str] = None
_default_loaded = False


def _read_default_disk() -> Optional[str]:
    try:
        if _DEFAULT_PATH.exists():
            data = json.loads(_DEFAULT_PATH.read_text(encoding="utf-8"))
            name = data.get("model")
            if isinstance(name, str) and name.strip():
                return name.strip()
    except Exception as exc:
        logger.warning("default_model load failed (%s); starting empty", exc)
    return None


def _write_default_disk(name: Optional[str]) -> None:
    try:
        _DEFAULT_PATH.parent.mkdir(parents=True, exist_ok=True)
        _DEFAULT_PATH.write_text(
            json.dumps({"model": name or ""}, indent=2),
            encoding="utf-8",
        )
    except Exception as exc:
        logger.warning("default_model save failed: %s", exc)


def get_default_model() -> Optional[str]:
    """Return the user's starred default model.

    Resolution order: persisted choice → ``WYLDE_DEFAULT_MODEL`` env →
    ``None``. The env fallback means a fresh install honours the
    deployment's configured default before the user stars anything.
    """
    global _default_cached, _default_loaded
    with _default_lock:
        if not _default_loaded:
            _default_cached = _read_default_disk()
            _default_loaded = True
        if _default_cached:
            return _default_cached
    env = (os.getenv("WYLDE_DEFAULT_MODEL") or "").strip()
    return env or None


def set_default_model(name: Optional[str]) -> Optional[str]:
    """Persist ``name`` as the starred default. Empty / ``None`` clears
    it (subsequent reads fall back to ``WYLDE_DEFAULT_MODEL`` then
    ``None``). Returns the persisted value (the cleaned string or
    ``None``)."""
    global _default_cached, _default_loaded
    cleaned = (name or "").strip() or None
    with _default_lock:
        _default_cached = cleaned
        _default_loaded = True
        _write_default_disk(cleaned)
    return cleaned


def set_active_model(name: Optional[str]) -> Optional[str]:
    """Persist ``name`` as the active model and clear capability state for the
    previously-active model so a future swap back doesn't inherit stale flags.

    Pass an empty string or ``None`` to unset.
    """
    global _cached, _loaded
    cleaned = (name or "").strip() or None
    with _active_lock:
        previous = _cached if _loaded else _read_disk()
        _cached = cleaned
        _loaded = True
        _write_disk(cleaned)
    if previous and previous != cleaned:
        forget_model(previous)
    return cleaned


__all__ = [
    "model_supports_tools",
    "mark_tool_failure",
    "forget_model",
    "reset_capabilities",
    "get_active_model",
    "set_active_model",
    "get_default_model",
    "set_default_model",
]
