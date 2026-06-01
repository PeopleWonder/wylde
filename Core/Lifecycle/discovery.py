"""Filesystem-as-registry: walks Wylde/, diffs vs cache, integrates changes.

Discovery is the entry point for service registration. It runs once on app
startup. If nothing changed since last run, it exits in ~5ms. If new or
missing folders are detected, it spawns a background thread to integrate
them so the app's main startup isn't blocked.

Integration steps for a newly detected folder:
    1. Auto-generate manifest.json (via Lifecycle/manifest.py rules).
    2. Assign a port from the pool (sequential, slot reuse).
    3. Append a row to Network/services.yaml.

Integration steps for a missing folder:
    1. Remove the row from Network/services.yaml.
    2. (The manifest doesn't need cleanup — the folder is gone.)

Then write the new state to .wylde/discovery.cache so the next run is a no-op.
"""

from __future__ import annotations

import threading
import time

from . import manifest
from ._common import (
    assign_port,
    find_service,
    list_service_folders,
    load_services,
    logger,
    read_discovery_cache,
    save_services,
    write_discovery_cache,
)


def discover() -> None:
    """Main entry point. Call this from the app startup hook.

    Fast path (no folder changes): one directory listing + one cache read.
    Slow path (changes detected): spawns a daemon thread that integrates
    additions/removals in the background. The caller proceeds immediately.
    """
    current = _snapshot_current_folders()
    cached = read_discovery_cache()

    new_folders = sorted(current.keys() - cached.keys())
    missing_folders = sorted(cached.keys() - current.keys())

    if not new_folders and not missing_folders:
        return  # No-op fast path.

    logger.info(
        "discovery: %d new, %d missing — integrating in background",
        len(new_folders),
        len(missing_folders),
    )

    threading.Thread(
        target=_integrate_changes,
        args=(new_folders, missing_folders, current),
        name="wylde-discovery-integrate",
        daemon=True,
    ).start()


def _snapshot_current_folders() -> dict[str, float]:
    """Return {folder_name: mtime} for all current service folders."""
    return {p.name: p.stat().st_mtime for p in list_service_folders()}


def _integrate_changes(
    new_folders: list[str],
    missing_folders: list[str],
    current_state: dict[str, float],
) -> None:
    """Background worker. Mutates services.yaml + writes the cache."""
    services = load_services()

    for folder_name in new_folders:
        try:
            _integrate_new(folder_name, services)
        except Exception:  # noqa: BLE001 — keep the loop running per service
            logger.exception("failed to integrate new folder %s", folder_name)

    for folder_name in missing_folders:
        try:
            _integrate_missing(folder_name, services)
        except Exception:  # noqa: BLE001
            logger.exception("failed to integrate missing folder %s", folder_name)

    save_services(services)
    write_discovery_cache(current_state)
    logger.info("discovery: integration complete")


def _integrate_new(folder_name: str, services: list[dict]) -> None:
    """Auto-gen manifest, assign port, append row to services list."""
    from ._common import WYLDE_ROOT

    folder = WYLDE_ROOT / folder_name
    manifest.ensure_manifest(folder)

    # Skip if a row already exists (idempotent — service was registered
    # by hand or in a previous run that crashed mid-write)
    if find_service(services, folder_name) is not None:
        logger.debug("service %s already in services.yaml, skipping", folder_name)
        return

    port = assign_port(services)
    services.append(
        {
            "name": folder_name,
            "port": port,
            "endpoint": f"http://localhost:{port}",
            "enabled": False,  # safe default, GUI opts in
            "status": "stopped",
        }
    )
    logger.info("registered %s on port %d", folder_name, port)


def _integrate_missing(folder_name: str, services: list[dict]) -> None:
    """Remove the row for a vanished folder."""
    entry = find_service(services, folder_name)
    if entry is None:
        return
    services.remove(entry)
    logger.info("unregistered %s (folder removed)", folder_name)


# ─── CLI shim ─────────────────────────────────────────────────────────────


def main() -> int:
    from Core.shared.logging_setup import configure_logging

    configure_logging()
    discover()
    # Wait briefly so the background thread has time to log if changes existed
    time.sleep(0.5)
    return 0


if __name__ == "__main__":
    import sys

    sys.exit(main())
