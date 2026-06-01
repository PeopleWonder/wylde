"""Orphan-detection sweep for daemon-managed services.

Post manifest-ownership refactor, services own their data/manifests/
files (write_manifest at startup, mark_stopped on graceful shutdown).
The daemon's job is:

  1. Track what it *spawned* — the spawn record stores the service
     name, the pid it expected, and the spawn time. This is the
     daemon's source of truth that "I started this thing", separate
     from whether the service got far enough to write its manifest.

  2. Sweep periodically. For every alive-marked manifest whose pid
     is no longer running, call mark_orphan_dead(); for every spawn
     record older than the grace window with no matching manifest,
     log a failed-to-launch warning. Both run from one thread that
     ticks on the unified 60s heartbeat cadence.

  3. Reap at shutdown. :func:`reap_manifest_orphans` is the safety
     net for the case the periodic sweep can't fix — a service whose
     manifest claims ``alive`` AND whose pid is still in the process
     table, but the daemon has no Popen handle for it (orphan from a
     prior crashed daemon session). The sweep would leave the pid
     alone because its check fires only when the pid is gone; the
     reaper terminates the live orphan and flips the manifest.

Spawn records live in-memory only. They reset on every daemon boot,
which is the right behaviour: a fresh daemon doesn't inherit stale
spawn expectations from the prior session.

State and the test-patchable helpers (``_pid_alive``,
``_SPAWN_GRACE_SECONDS``, ``_MANIFEST_DIR``) live in the package's
``__init__.py``. This module looks them up via the canonical
``daemon_state`` module at *call time* so monkeypatches on the package
flow through correctly.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import threading
import time
from typing import Any, Dict, List

from .. import daemon_state as _ds
from .._common import logger as _lc_logger


def sweep_orphans() -> Dict[str, Any]:
    """One pass of the orphan-detection sweep.

    Walks every ``data/manifests/*.json`` file. For each manifest with
    ``status.state == "alive"`` whose pid is no longer running, calls
    :func:`Core.shared.manifest.mark_orphan_dead`. Also checks each
    in-flight spawn record — if past the grace window with no manifest
    on disk, logs a failed-to-launch warning (does NOT mark anything;
    there's nothing to mark).

    Returns a structured summary suitable for logging / smoke tests.
    """
    try:
        from Core.shared import manifest as _service_manifest
    except ImportError:
        return {"orphans": [], "failed_to_launch": [], "checked": 0}

    orphans: list[str] = []
    failed: list[str] = []
    checked = 0

    if _ds._MANIFEST_DIR.exists():
        for path in sorted(_ds._MANIFEST_DIR.glob("*.json")):
            checked += 1
            try:
                data = json.loads(path.read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError):
                continue
            status = data.get("status") or {}
            state = status.get("state")
            pid = status.get("pid")
            # Treat manifests that predate the ``state`` field as alive
            # if they have a recent heartbeat — same UX as the old behaviour.
            if state not in ("alive", None):
                continue
            if not isinstance(pid, int) or pid <= 0:
                continue
            if _ds._pid_alive(pid):
                continue
            service_name = data.get("service")
            if not isinstance(service_name, str):
                continue
            _service_manifest.mark_orphan_dead(service_name)
            orphans.append(service_name)
            _lc_logger.warning(
                "orphan_sweep: %s (pid=%d) is no longer running — marked dead-orphan",
                service_name,
                pid,
            )

    # Failed-to-launch check: spawn records older than the grace window
    # with no live pid AND no on-disk manifest. The service either died
    # before reaching write_manifest() or never started Python at all.
    now = time.monotonic()
    with _ds._spawn_lock:
        for name, rec in list(_ds._spawn_records.items()):
            if rec.grace_satisfied:
                continue
            manifest_path = _ds._manifest_path(name)
            if manifest_path.exists():
                rec.grace_satisfied = True
                continue
            if (now - rec.spawn_time) < _ds._SPAWN_GRACE_SECONDS:
                continue
            if _ds._pid_alive(rec.pid):
                # Pid is alive but no manifest yet — probably still in
                # startup. Skip; we'll catch it next sweep.
                continue
            failed.append(name)
            _lc_logger.warning(
                "orphan_sweep: %s (pid=%d) failed to launch — no manifest "
                "after %.0fs grace and pid is gone",
                name,
                rec.pid,
                _ds._SPAWN_GRACE_SECONDS,
            )
            # Don't repeat the warning every tick.
            rec.grace_satisfied = True

    return {"orphans": orphans, "failed_to_launch": failed, "checked": checked}


def start_orphan_sweep(interval: float = 60.0) -> None:
    """Spawn the daemon's orphan-detection thread (idempotent).

    Called once from :func:`Core.Lifecycle.daemon.serve_forever` after
    services are spawned. The thread sleeps on a stop event so
    :func:`stop_orphan_sweep` can drain it cleanly during shutdown.
    """
    if _ds._orphan_sweep_stop is not None:
        return  # already running
    stop = threading.Event()
    _ds._orphan_sweep_stop = stop

    def _loop() -> None:
        while not stop.wait(timeout=interval):
            try:
                sweep_orphans()
            except Exception:  # noqa: BLE001
                _lc_logger.exception("orphan_sweep: tick raised")

    threading.Thread(target=_loop, name="lifecycle-orphan-sweep", daemon=True).start()
    _lc_logger.info("orphan_sweep: started (interval=%ss)", interval)


def stop_orphan_sweep() -> None:
    """Signal the orphan-detection thread to exit (idempotent)."""
    stop = _ds._orphan_sweep_stop
    _ds._orphan_sweep_stop = None
    if stop is not None:
        stop.set()


# ── Shutdown-time live-orphan reaper ─────────────────────────────────


def _force_kill_pid(pid: int, *, grace_seconds: float = 5.0) -> bool:
    """Force-terminate ``pid``. Returns True if the pid is gone after.

    Uses psutil's terminate-then-kill flow when available so a service
    that handles SIGTERM / WM_CLOSE gets a chance to clean up. Falls
    back to ``taskkill /F /PID`` on Windows or SIGKILL on POSIX when
    psutil isn't installed.

    Best-effort: swallows AccessDenied / NoSuchProcess, returns the
    post-condition liveness check so the caller knows whether the
    process actually died.
    """
    try:
        import psutil

        try:
            proc = psutil.Process(pid)
        except psutil.NoSuchProcess:
            return True
        try:
            proc.terminate()
        except psutil.NoSuchProcess:
            return True
        except psutil.AccessDenied:
            pass
        try:
            proc.wait(timeout=grace_seconds)
            return True
        except psutil.TimeoutExpired:
            pass
        except psutil.NoSuchProcess:
            return True
        try:
            proc.kill()
            proc.wait(timeout=2.0)
        except (psutil.NoSuchProcess, psutil.TimeoutExpired, psutil.AccessDenied):
            pass
        return not _ds._pid_alive(pid)
    except ImportError:
        pass

    if sys.platform == "win32":
        try:
            subprocess.run(
                ["taskkill", "/F", "/PID", str(pid)],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=False,
                timeout=5.0,
            )
        except (OSError, subprocess.TimeoutExpired):
            pass
        return not _ds._pid_alive(pid)

    import signal as _signal

    try:
        os.kill(pid, _signal.SIGTERM)
    except OSError:
        return not _ds._pid_alive(pid)
    deadline = time.monotonic() + min(grace_seconds, 5.0)
    while time.monotonic() < deadline and _ds._pid_alive(pid):
        time.sleep(0.1)
    if _ds._pid_alive(pid):
        try:
            os.kill(pid, _signal.SIGKILL)
        except OSError:
            pass
    return not _ds._pid_alive(pid)


def reap_manifest_orphans(*, grace_seconds: float = 5.0) -> List[Dict[str, Any]]:
    """Walk ``data/manifests/*.json`` and force-kill every live orphan.

    The shutdown safety net for the case the in-memory Popen handles
    miss: a service whose manifest claims ``alive`` with a pid still
    in the process table. The classic source is an orphan from a
    prior crashed daemon session — `stop_<service>` short-circuits
    because ``_<service>_proc`` is ``None`` on the fresh daemon, the
    periodic sweep does nothing because the pid IS alive, and the
    process survives every shutdown until something hard-kills it.

    For each surviving pid, ``_force_kill_pid`` is invoked (graceful
    SIGTERM/terminate first; SIGKILL after the grace) and the
    manifest is flipped to ``dead-orphan`` via
    :func:`Core.shared.manifest.mark_orphan_dead`. Returns one entry
    per service the reap touched — ``{"name", "pid", "killed"}``.

    Safe to call when no manifests exist (returns empty list) or when
    nothing is alive (returns empty list).
    """
    reaped: List[Dict[str, Any]] = []
    if not _ds._MANIFEST_DIR.exists():
        return reaped

    try:
        from Core.shared import manifest as _service_manifest
    except ImportError:
        _service_manifest = None  # type: ignore[assignment]

    for path in sorted(_ds._MANIFEST_DIR.glob("*.json")):
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            continue
        if not isinstance(data, dict):
            continue
        service = data.get("service") or data.get("name")
        if not isinstance(service, str) or not service:
            continue
        status = data.get("status") or {}
        state = status.get("state")
        pid = status.get("pid")
        # Anything not explicitly terminal is treated as potentially
        # alive — pre-state-field manifests carry no state at all and
        # must still be reaped if the pid is in the table.
        if state in ("stopped", "dead-orphan", "crashed"):
            continue
        if not isinstance(pid, int) or pid <= 0:
            continue
        if not _ds._pid_alive(pid):
            continue

        killed = _force_kill_pid(pid, grace_seconds=grace_seconds)
        if _service_manifest is not None:
            try:
                _service_manifest.mark_orphan_dead(service)
            except Exception:  # noqa: BLE001
                _lc_logger.exception(
                    "reap_manifest_orphans: mark_orphan_dead %s raised", service
                )
        reaped.append({"name": service, "pid": pid, "killed": killed})
        _lc_logger.warning(
            "reap_manifest_orphans: %s (pid=%d) force-terminated (killed=%s)",
            service,
            pid,
            killed,
        )
    return reaped
