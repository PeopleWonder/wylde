"""updater.* action handlers — persist the GUI's auto-update preferences.

The Settings panel's Updates section reads and writes these two verbs
through the lifecycle pipe (``updater.get_prefs`` / ``updater.set_prefs``;
see ``Core/GUI/Frontend/Panels/Settings/src/ipc.rs``).

Wylde is privacy-first: the daemon never performs an update check on
its own.  These prefs only record the user's *stated intent* so the GUI
can reflect it and a future updater slice can honour it — turning the
master toggle off means "make no network calls", which is the on-disk
default.  Kept out of ``control.py`` so that file stays under the
700-line cap (rule ``file_size_limit``); registered into the daemon's
``ACTIONS`` map from there.

State lives at ``<wylde_root>/data/preferences/updater.json`` — the same
``data/preferences`` directory the harness uses for ``consent.json``.
The Lifecycle daemon legitimately owns cross-service state, so writing
here is allowed by the ``service_owns_its_state`` rule.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

from ._common import WYLDE_ROOT, logger

# Where the GUI's update preferences are persisted.  Mirrors the
# resolution the harness uses for consent (``data/preferences/<name>.json``).
_PREFS_PATH: Path = WYLDE_ROOT / "data" / "preferences" / "updater.json"

# Allowed background-check cadences.  Mirrors the Svelte alpha's
# frequency picker (``Core/GUI/src/pages/Settings.svelte``) and the
# default the Rust ``UpdatePrefs`` mirror falls back to.
_VALID_FREQUENCIES: frozenset[str] = frozenset({"daily", "weekly", "monthly"})

# Canonical default shape.  Byte-for-byte what the Rust
# ``UpdatePrefs::from_value(&{})`` produces so a missing file and an
# empty file round-trip to the same view-side state.
_DEFAULTS: dict[str, Any] = {
    "enabled": False,
    "auto_check": False,
    "frequency": "weekly",
    "last_checked": None,
}


def _bad_request(message: str) -> Exception:
    """Build the structured error the pipe dispatcher honours.

    Imported lazily so this module stays importable in unit tests that
    don't have the pywin32 IPC stack available.
    """
    from Core.shared.ipc import IpcError

    return IpcError("bad_request", message)


def _read_prefs() -> dict[str, Any]:
    """Return the persisted prefs merged over :data:`_DEFAULTS`.

    A missing or unreadable/corrupt file degrades to defaults rather
    than raising — the Updates section must still render when the user
    has never touched it.  Unknown keys on disk are dropped so a stale
    field from an older build can't leak into the reply.
    """
    merged = dict(_DEFAULTS)
    if not _PREFS_PATH.exists():
        return merged
    try:
        raw = json.loads(_PREFS_PATH.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as e:
        logger.warning("updater_prefs: %s unreadable, using defaults: %s", _PREFS_PATH, e)
        return merged
    if not isinstance(raw, dict):
        logger.warning("updater_prefs: %s is not an object, using defaults", _PREFS_PATH)
        return merged
    for key in _DEFAULTS:
        if key in raw:
            merged[key] = raw[key]
    return merged


def _write_prefs(prefs: dict[str, Any]) -> None:
    """Atomically persist ``prefs`` (temp file + replace)."""
    _PREFS_PATH.parent.mkdir(parents=True, exist_ok=True)
    tmp = _PREFS_PATH.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(prefs, indent=2), encoding="utf-8")
    tmp.replace(_PREFS_PATH)


def _coerce_patch(patch: dict[str, Any], current: dict[str, Any]) -> dict[str, Any]:
    """Validate and merge a partial ``patch`` over ``current``.

    Only the four known keys are accepted; each is type-checked so a bad
    write can't corrupt the on-disk shape.  Raises ``bad_request`` on any
    malformed field.  Returns the merged, validated shape.
    """
    merged = dict(current)
    for key in ("enabled", "auto_check"):
        if key in patch:
            value = patch[key]
            if not isinstance(value, bool):
                raise _bad_request(f"{key} must be a boolean")
            merged[key] = value
    if "frequency" in patch:
        freq = patch["frequency"]
        if not isinstance(freq, str) or freq not in _VALID_FREQUENCIES:
            allowed = ", ".join(sorted(_VALID_FREQUENCIES))
            raise _bad_request(f"frequency must be one of: {allowed}")
        merged["frequency"] = freq
    if "last_checked" in patch:
        ts = patch["last_checked"]
        # bool is an int subclass — reject it explicitly so a stray
        # `true` can't masquerade as a timestamp.
        if ts is not None and (isinstance(ts, bool) or not isinstance(ts, int) or ts < 0):
            raise _bad_request("last_checked must be a non-negative integer or null")
        merged["last_checked"] = ts
    return merged


def updater_get_prefs_action(_payload: Any = None) -> dict[str, Any]:
    """Return the persisted update preferences merged over the defaults.

    Reply shape ``{enabled, auto_check, frequency, last_checked}`` — the
    Settings panel's ``UpdatePrefs::from_value`` mirror consumes it
    directly.  Never raises for a missing file; an unconfigured install
    reports the privacy-first defaults (everything off, weekly cadence).
    """
    return _read_prefs()


def updater_set_prefs_action(payload: Any = None) -> dict[str, Any]:
    """Merge a partial prefs patch into the on-disk shape and persist it.

    Payload is a partial object — the Settings toggles send a single key
    at a time (e.g. ``{"enabled": true}``).  Unknown keys are ignored;
    known keys are type-validated.  Returns the full merged shape so the
    caller can reconcile its view state without a follow-up read.
    """
    if payload is None:
        patch: dict[str, Any] = {}
    elif isinstance(payload, dict):
        patch = payload
    else:
        raise _bad_request("payload must be an object")
    merged = _coerce_patch(patch, _read_prefs())
    _write_prefs(merged)
    return merged


# Map of canonical action name → handler.  ``control.py`` folds this into
# its ``ACTIONS`` map at registration time.
ACTIONS: dict[str, Any] = {
    "updater.get_prefs": updater_get_prefs_action,
    "updater.set_prefs": updater_set_prefs_action,
}


__all__ = [
    "ACTIONS",
    "updater_get_prefs_action",
    "updater_set_prefs_action",
]
