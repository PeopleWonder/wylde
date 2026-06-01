"""
system_prompts.py — Override store for every LLM system prompt in the platform.

Persisted at ``data/system_prompts.json``. Backend services read overrides
through this helper at workflow load / prompt construction time; the
Settings page in the GUI mutates the store via the ``harness.prompts.*``
pipe actions on wylde-orchestrator (which call into this module). Defaults  # wylde-check: dead-ref-ok
live in :mod:`shared.system_prompts_catalog` so a clean install with no
override file behaves the same as one carrying the catalog defaults.

File format (data/system_prompts.json)::

    {
      "version": 1,
      "overrides": {"<id>": "<text>", ...},
      "presets":   {"<name>": {"<id>": "<text>", ...}, ...},
      "active_preset": "Default"
    }

The reader is mtime-cached so hot paths don't re-parse JSON on every prompt
build. Writers drop the cache so the next read reflects the new state.
"""

from __future__ import annotations

import json
import logging
import os
import threading
from pathlib import Path
from typing import Any, Dict, List, Optional

try:
    # When core/shared/ is on sys.path (the orchestrator's bootstrap pattern),
    # the catalog is reachable as a top-level module.
    import system_prompts_catalog as _catalog
except ImportError:  # pragma: no cover — package-import fallback
    try:
        from shared import system_prompts_catalog as _catalog
    except ImportError:
        # Canonical Wylde import path — pytest from the repo root and any
        # in-process harness caller hits this branch.
        from Core.shared import system_prompts_catalog as _catalog

logger = logging.getLogger(__name__)

_WYLDE_ROOT = Path(os.getenv("WYLDE_ROOT", Path(__file__).parent.parent.parent))
_OVERRIDES_PATH = _WYLDE_ROOT / "data" / "system_prompts.json"

# Reentrant — every write path holds the lock and then calls read_store()
# at the end to return the post-write snapshot, which re-acquires the lock.
_lock = threading.RLock()
_cache: Optional[Dict[str, Any]] = None
_cache_mtime: float = 0.0


def overrides_path() -> Path:
    return _OVERRIDES_PATH


# ── Read path ────────────────────────────────────────────────────────────


def _empty_store() -> Dict[str, Any]:
    return {
        "version": 1,
        "overrides": {},
        "presets": {},
        "active_preset": "Default",
    }


def _load_store_locked() -> Dict[str, Any]:
    global _cache, _cache_mtime

    try:
        mtime = _OVERRIDES_PATH.stat().st_mtime
    except FileNotFoundError:
        _cache = _empty_store()
        _cache_mtime = 0.0
        return _cache

    if _cache is not None and mtime == _cache_mtime:
        return _cache

    try:
        raw = json.loads(_OVERRIDES_PATH.read_text(encoding="utf-8"))
    except Exception as exc:
        logger.warning("system_prompts: could not read %s: %s", _OVERRIDES_PATH, exc)
        _cache = _empty_store()
        _cache_mtime = mtime
        return _cache

    if not isinstance(raw, dict):
        raw = {}
    overrides = raw.get("overrides")
    if not isinstance(overrides, dict):
        overrides = {}
    presets = raw.get("presets")
    if not isinstance(presets, dict):
        presets = {}
    active = raw.get("active_preset")
    if not isinstance(active, str) or not active.strip():
        active = "Default"

    _cache = {
        "version": 1,
        "overrides": {
            k: v for k, v in overrides.items() if isinstance(v, str) and v.strip()
        },
        "presets": {
            name: {
                k: v
                for k, v in (bundle or {}).items()
                if isinstance(v, str) and v.strip()
            }
            for name, bundle in presets.items()
            if isinstance(bundle, dict)
        },
        "active_preset": active,
    }
    _cache_mtime = mtime
    return _cache


def read_store() -> Dict[str, Any]:
    """Return a deep-ish copy of the override + preset bundle."""
    with _lock:
        s = _load_store_locked()
        return {
            "version": s["version"],
            "overrides": dict(s["overrides"]),
            "presets": {k: dict(v) for k, v in s["presets"].items()},
            "active_preset": s["active_preset"],
        }


def get_override(prompt_id: str) -> Optional[str]:
    """Return the user override for ``prompt_id``, or ``None`` if none is set."""
    if not prompt_id:
        return None
    with _lock:
        result: Optional[str] = _load_store_locked()["overrides"].get(prompt_id)
        return result


def effective_prompt(prompt_id: str) -> str:
    """Return override text if set, else the catalog default."""
    text = get_override(prompt_id)
    if isinstance(text, str) and text.strip():
        return text
    default: str = _catalog.default_for(prompt_id)
    return default


def apply(prompt_id: str, default_text: Optional[str]) -> Optional[str]:
    """Return the override for ``prompt_id`` if present, else ``default_text``."""
    override = get_override(prompt_id)
    return override if override is not None else default_text


def reload() -> None:
    """Drop the cache. Next read re-parses the file from disk."""
    global _cache, _cache_mtime
    with _lock:
        _cache = None
        _cache_mtime = 0.0


# ── Write path ───────────────────────────────────────────────────────────


def _write_store_locked(store: Dict[str, Any]) -> None:
    """Persist the bundle to disk and refresh the in-memory cache."""
    global _cache, _cache_mtime

    _OVERRIDES_PATH.parent.mkdir(parents=True, exist_ok=True)
    text = json.dumps(store, indent=2, ensure_ascii=False)
    _OVERRIDES_PATH.write_text(text, encoding="utf-8")
    try:
        _cache_mtime = _OVERRIDES_PATH.stat().st_mtime
    except OSError:
        _cache_mtime = 0.0
    _cache = {
        "version": 1,
        "overrides": dict(store.get("overrides") or {}),
        "presets": {k: dict(v) for k, v in (store.get("presets") or {}).items()},
        "active_preset": store.get("active_preset") or "Default",
    }


def set_override(prompt_id: str, text: Optional[str]) -> Dict[str, Any]:
    """Save an override for ``prompt_id``. Pass ``None`` (or matching default
    text) to remove the override and fall back to the catalog default."""
    if not _catalog.entry_for(prompt_id):
        raise ValueError(f"Unknown prompt id: {prompt_id}")
    with _lock:
        store = dict(_load_store_locked())
        overrides = dict(store["overrides"])
        if (
            text is None
            or not str(text).strip()
            or str(text).strip() == _catalog.default_for(prompt_id).strip()
        ):
            overrides.pop(prompt_id, None)
        else:
            overrides[prompt_id] = str(text)
        store["overrides"] = overrides
        store["presets"] = dict(store["presets"])
        _write_store_locked(store)
        return read_store()


def clear_override(prompt_id: str) -> Dict[str, Any]:
    """Drop ``prompt_id``'s override; subsequent reads return the default."""
    return set_override(prompt_id, None)


def clear_all_overrides() -> Dict[str, Any]:
    """Reset to the built-in defaults across every prompt."""
    with _lock:
        store = dict(_load_store_locked())
        store["overrides"] = {}
        store["active_preset"] = "Default"
        store["presets"] = dict(store["presets"])
        _write_store_locked(store)
        return read_store()


# ── Presets ──────────────────────────────────────────────────────────────


def save_preset(name: str) -> Dict[str, Any]:
    """Snapshot the current overrides into a named preset and activate it."""
    trimmed = (name or "").strip()
    if not trimmed:
        raise ValueError("Preset name required.")
    if trimmed == "Default":
        raise ValueError('"Default" is reserved.')
    with _lock:
        store = dict(_load_store_locked())
        presets = dict(store["presets"])
        presets[trimmed] = dict(store["overrides"])
        store["presets"] = presets
        store["active_preset"] = trimmed
        store["overrides"] = dict(store["overrides"])
        _write_store_locked(store)
        return read_store()


def load_preset(name: str) -> Dict[str, Any]:
    """Replace the active overrides with the named preset's bundle."""
    if name == "Default":
        return clear_all_overrides()
    with _lock:
        store = dict(_load_store_locked())
        bundle = store["presets"].get(name)
        if bundle is None:
            raise LookupError(f"Preset not found: {name}")
        store["overrides"] = dict(bundle)
        store["active_preset"] = name
        store["presets"] = dict(store["presets"])
        _write_store_locked(store)
        return read_store()


def delete_preset(name: str) -> Dict[str, Any]:
    """Remove a named preset; falls back to Default if it was active."""
    if name == "Default":
        raise ValueError('"Default" cannot be deleted.')
    with _lock:
        store = dict(_load_store_locked())
        presets = dict(store["presets"])
        presets.pop(name, None)
        store["presets"] = presets
        if store["active_preset"] == name:
            store["active_preset"] = "Default"
        store["overrides"] = dict(store["overrides"])
        _write_store_locked(store)
        return read_store()


def preset_names() -> List[str]:
    with _lock:
        return ["Default"] + sorted(_load_store_locked()["presets"].keys())
