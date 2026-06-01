"""Auto-generates manifest.json for newly detected service folders.

Rules (per Wylde service architecture):
  1. Generate ONLY if `manifest.json` is missing.
  2. Schema evolution → additively merge default values for missing keys.
     Never overwrite existing values.
  3. Malformed JSON is NOT treated as missing. Log + surface, don't auto-regen
     (would silently destroy hand-edited manifests).
  4. New folders default to `enabled: false` so half-built services don't crash startup.
  5. Manual regen is a separate explicit command (not part of normal startup).
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

from ._common import DEFAULT_SHUTDOWN_ORDER, logger


# Default schema for a newly auto-generated manifest. Folder owners can edit
# this freely after generation; subsequent runs will additively fill in any
# new keys we add to this default but never overwrite existing values.
#
# `entry_point` is the canonical "how to launch this" field — it IS the
# service's launch command / binary path (a string the launcher splits +
# spawns, or null for an in-process / library / pipe-only service). There
# is deliberately no separate `binary` key; one field, one source of
# truth. `shutdown_order` + `health_check` were added at the slice-11
# cutover (2026-05-29) so shutdown can order services declaratively and a
# slow-to-bind service can gate the launcher's next spawn — see
# Core/Lifecycle/shutdown.py and launcher.py, and the wylde_check rules
# `service_manifest_schema` / `shutdown_enumerates_services_from_manifests`.
DEFAULT_MANIFEST: dict[str, Any] = {
    "name": None,  # filled in to match folder name
    "description": "",
    "version": "0.1.0",
    "enabled": False,  # safe default — opt in via the GUI
    "entry_point": None,  # launch command / binary; null = library/in-process
    "depends_on": [],  # list of service names that must start first
    "tier": "standard",  # standard | core | optional | extension
    # Shutdown priority: services are stopped in ASCENDING order (lowest
    # first). User-facing / ingress services (GUI=10, Gateway=20) drain
    # before the infra they depend on (device_gate, broker, memgraph).
    # Absent → DEFAULT_SHUTDOWN_ORDER (reverse-launch order is the
    # fallback when no explicit slot is declared).
    "shutdown_order": DEFAULT_SHUTDOWN_ORDER,
    # Optional readiness probe the launcher waits on before spawning the
    # next service. Shapes: null (no gate), "pipe:wylde-<name>" (wait for
    # the named pipe to appear), or "http://host:port/path" (HTTP 200).
    "health_check": None,
}


def manifest_path(folder: Path) -> Path:
    return folder / "manifest.json"


def ensure_manifest(folder: Path) -> bool:
    """Ensure `folder/manifest.json` exists and has all default keys.

    Returns True if a write occurred (creation or additive merge), False if
    the manifest was already up to date or malformed (and thus skipped).
    """
    path = manifest_path(folder)

    if path.exists():
        return _additive_merge(path, folder.name)

    return _create_default(path, folder.name)


def _create_default(path: Path, folder_name: str) -> bool:
    """Write a fresh default manifest for a brand-new folder."""
    payload = {**DEFAULT_MANIFEST, "name": folder_name}
    path.write_text(json.dumps(payload, indent=2), encoding="utf-8")
    logger.info("auto-generated manifest for %s", folder_name)
    return True


def _additive_merge(path: Path, folder_name: str) -> bool:
    """Read existing manifest; add missing default keys; never overwrite."""
    try:
        existing = json.loads(path.read_text(encoding="utf-8"))
    except (json.JSONDecodeError, OSError) as e:
        logger.error(
            "manifest at %s is malformed, leaving untouched (%s). "
            "Service will be flagged in the GUI; user must repair manually.",
            path,
            e,
        )
        return False

    if not isinstance(existing, dict):
        logger.error("manifest at %s is not a JSON object, skipping", path)
        return False

    merged = {**DEFAULT_MANIFEST, **existing, "name": folder_name}
    if merged == existing:
        return False

    path.write_text(json.dumps(merged, indent=2), encoding="utf-8")
    logger.info("additively merged new keys into manifest for %s", folder_name)
    return True


def regenerate(folder: Path, *, force: bool = False) -> None:
    """Manual-regen entry point. Overwrites existing manifest only if force=True.

    Called by the explicit `wylde regen-manifest <service> --force` command,
    NOT part of normal startup. Without `force`, this behaves the same as
    `ensure_manifest` (additive merge only).
    """
    path = manifest_path(folder)
    if force or not path.exists():
        _create_default(path, folder.name)
    else:
        _additive_merge(path, folder.name)


def load_manifest(folder: Path) -> dict[str, Any] | None:
    """Read a manifest. Returns None if missing or malformed."""
    path = manifest_path(folder)
    if not path.exists():
        return None
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (json.JSONDecodeError, OSError) as e:
        logger.warning("can't read manifest at %s: %s", path, e)
        return None
    return data if isinstance(data, dict) else None


# Re-export for convenience
__all__ = [
    "DEFAULT_MANIFEST",
    "ensure_manifest",
    "regenerate",
    "load_manifest",
    "manifest_path",
]
