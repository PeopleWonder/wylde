"""Graceful stop. Persists last-enabled state so the daemon resumes correctly.

Behavior:
    1. Iterate over every running subprocess tracked by the daemon.
    2. Send a graceful signal (Ctrl-Break on Windows, SIGTERM on POSIX).
    3. Wait up to a per-service timeout. Force-kill if it overstays.
    4. Update services.yaml with the final status (`stopped` or `crashed`).

Note: `enabled: true|false` is NOT changed during shutdown. The flag means
"user wants this running"; on next start the daemon resumes everything
that was enabled. Shutdown only updates the runtime status.
"""

from __future__ import annotations

import signal
import subprocess
import sys
from typing import Any

from . import launcher
from . import manifest as manifest_mod
from ._common import (
    DEFAULT_SHUTDOWN_ORDER,
    WYLDE_ROOT,
    load_services,
    logger,
    save_services,
)


# Per-service grace period before force-kill (seconds).
SHUTDOWN_TIMEOUT: int = 10


def _shutdown_order(name: str) -> int:
    """Resolve a service's shutdown slot from its manifest.

    Lower stops earlier. A missing / malformed manifest, or a manifest
    with no (or a non-int) ``shutdown_order``, falls back to
    ``DEFAULT_SHUTDOWN_ORDER`` so the reverse-launch tiebreak decides.
    """
    mf = manifest_mod.load_manifest(WYLDE_ROOT / name)
    order = (mf or {}).get("shutdown_order")
    return order if isinstance(order, int) else DEFAULT_SHUTDOWN_ORDER


def _shutdown_sequence(names: list[str]) -> list[str]:
    """Order service names for shutdown, manifest-driven.

    The launcher spawns in dependency (topological) order, so the
    *default* shutdown order is its reverse — dependents stop before the
    services they depend on. A manifest's ``shutdown_order`` overrides
    that slot; a stable sort over the reversed launch order means
    services sharing a slot keep the reverse-launch relationship.

    Pure over the passed names + the on-disk manifests, so the ordering
    is unit-testable without a live process table.
    """
    reversed_launch = list(reversed(names))  # reverse-topo default
    reversed_launch.sort(key=_shutdown_order)  # stable; manifest override
    return reversed_launch


def shutdown_all() -> None:
    """Stop every running service tracked by the launcher.

    Services are stopped in the manifest-driven order computed by
    :func:`_shutdown_sequence` — reverse-launch (reverse-topo) by
    default, with a per-service ``shutdown_order`` manifest override.
    This is the canonical drain the GUI reaches via the
    ``lifecycle.shutdown_all`` action; the ``wylde_check`` rule
    ``shutdown_enumerates_services_from_manifests`` guards that it stays
    manifest-driven rather than walking a hardcoded service list.
    """
    running = launcher.get_running()
    if not running:
        logger.info("shutdown: no running services")
        return

    services = load_services()

    for name in _shutdown_sequence(list(running.keys())):
        proc = running.get(name)
        if proc is None:
            continue
        try:
            _stop_one(name, proc, services)
        except Exception:  # noqa: BLE001
            logger.exception("error stopping %s", name)
            _mark_status(services, name, "crashed")

    # Clear the tracking dict so a subsequent launch_all starts fresh
    running.clear()
    save_services(services)
    logger.info("shutdown: complete")


def _stop_one(
    name: str, proc: subprocess.Popen, services: list[dict[str, Any]]
) -> None:
    """Send a graceful signal, wait, force-kill if needed."""
    if proc.poll() is not None:
        # Already exited
        _mark_status(services, name, "stopped")
        return

    logger.info("stopping %s (pid=%d)", name, proc.pid)
    _send_graceful_signal(proc)

    try:
        proc.wait(timeout=SHUTDOWN_TIMEOUT)
        logger.info("%s exited cleanly", name)
        _mark_status(services, name, "stopped")
    except subprocess.TimeoutExpired:
        logger.warning(
            "%s did not exit after %ds, force-killing", name, SHUTDOWN_TIMEOUT
        )
        proc.kill()
        try:
            proc.wait(timeout=2)
        except subprocess.TimeoutExpired:
            pass
        _mark_status(services, name, "stopped")  # killed counts as stopped


def _send_graceful_signal(proc: subprocess.Popen) -> None:
    """Send the OS-appropriate graceful-stop signal."""
    if sys.platform == "win32":
        # CTRL_BREAK_EVENT works because launcher spawned with
        # CREATE_NEW_PROCESS_GROUP. CTRL_C_EVENT would also break the parent.
        try:
            proc.send_signal(signal.CTRL_BREAK_EVENT)
        except (OSError, ValueError):
            proc.terminate()
    else:
        proc.terminate()  # SIGTERM


def _mark_status(services: list[dict[str, Any]], name: str, status: str) -> None:
    for svc in services:
        if svc.get("name") == name:
            svc["status"] = status
            return


def main() -> int:
    from Core.shared.logging_setup import configure_logging

    configure_logging()
    shutdown_all()
    return 0


if __name__ == "__main__":
    sys.exit(main())
