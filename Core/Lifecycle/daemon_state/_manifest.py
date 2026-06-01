"""Manifest writers for daemon-managed services.

Core's runtime manifest lives at ``data/manifests/core.json``. Core is one
logical service in the dashboard — its internal pipes (wylde-lifecycle,
wylde-harness, wylde-memgraph) are NOT individually surfaced. Registry
probes each constituent pipe live; this manifest only carries pid /
started_at / heartbeat so the dashboard can show uptime and a fresh
heartbeat indicator.

Daemon-managed top-level services (Voice, device_gate) still publish
their own runtime manifest at ``data/manifests/wylde-<name>.json``, with
a per-service heartbeat thread. Memgraph's wrapper writes its own too
(we don't author wylde-memgraph.json from here — registry filters it
out anyway as a Core constituent).
"""

from __future__ import annotations

import json
import os
import threading
from typing import Any, Callable, Dict, Optional

from .._common import logger as _lc_logger


def _write_daemon_manifest(
    name: str,
    pid: int,
    *,
    description: str = "",
    category: str = "service",
    pipe: Optional[str] = None,
    port: Optional[int] = None,
    contributes: Optional[Dict[str, Any]] = None,
    version: str = "1.0.0",
) -> None:
    """Write data/manifests/<name>.json so service.list can see this service.

    Preserves ``status.started_at`` across writes within the same session
    (so the dashboard's uptime field stays honest after a heartbeat update).
    """
    from . import _atomic_write_json, _manifest_path, _now_iso

    path = _manifest_path(name)
    started_at = _now_iso()
    if path.exists():
        try:
            existing = json.loads(path.read_text(encoding="utf-8"))
            prev_status = existing.get("status") or {}
            if prev_status.get("started_at"):
                started_at = prev_status["started_at"]
        except (OSError, json.JSONDecodeError):
            pass
    manifest = {
        "service": name,
        "version": version,
        "kind": "daemon-managed",
        "pipe": pipe,
        "port": port,
        "category": category,
        "description": description,
        "contributes": contributes or {},
        "status": {
            "pid": pid,
            "started_at": started_at,
            "heartbeat": _now_iso(),
        },
    }
    try:
        _atomic_write_json(path, manifest)
    except OSError as exc:
        _lc_logger.warning("manifest: write failed for %s: %s", name, exc)
        return
    _lc_logger.info("manifest: wrote %s (pid=%d)", name, pid)


def _start_daemon_heartbeat(
    name: str,
    pid_provider: Callable[[], Optional[int]],
    interval: float = 60.0,
) -> None:
    """Refresh manifest heartbeat + pid every ``interval`` seconds.

    ``pid_provider`` returns the live PID or ``None`` if the service is dead;
    a None return is a soft skip (manifest not refreshed this tick) so the
    classifier flips it to ``stale`` naturally.
    """
    from . import _atomic_write_json, _heartbeat_stops, _manifest_path, _now_iso

    existing = _heartbeat_stops.pop(name, None)
    if existing is not None:
        existing.set()
    stop = threading.Event()
    _heartbeat_stops[name] = stop
    path = _manifest_path(name)

    def _loop() -> None:
        while not stop.wait(timeout=interval):
            try:
                current_pid = pid_provider()
                if current_pid is None:
                    continue
                if not path.exists():
                    continue
                manifest = json.loads(path.read_text(encoding="utf-8"))
                status = manifest.setdefault("status", {})
                status["heartbeat"] = _now_iso()
                if status.get("pid") != current_pid:
                    status["pid"] = current_pid
                _atomic_write_json(path, manifest)
            except (OSError, json.JSONDecodeError, ValueError):
                continue
            except Exception:  # noqa: BLE001
                continue

    threading.Thread(
        target=_loop,
        name=f"manifest-hb-{name}",
        daemon=True,
    ).start()


def _stop_daemon_heartbeat(name: str) -> None:
    from . import _heartbeat_stops

    stop = _heartbeat_stops.pop(name, None)
    if stop is not None:
        stop.set()


def register_core_manifest() -> None:
    """Publish Core's runtime manifest as a single ``core.json``.

    Replaces the previous per-pipe manifests (wylde-lifecycle, wylde-harness,
    wylde-memgraph, wylde-memory-scheduler). Core is one service in the
    dashboard — registry probes constituent pipes live, so this manifest
    only carries identity + heartbeat. Idempotent: callers can re-invoke it
    after the harness pipe comes up to refresh.

    Also clears stale per-pipe manifest files from prior daemon versions
    so the dashboard doesn't surface them as peers during the transition.
    """
    from . import _DEPRECATED_CORE_SUB_MANIFESTS, _delete_manifest

    for stale in _DEPRECATED_CORE_SUB_MANIFESTS:
        _delete_manifest(stale)
        _stop_daemon_heartbeat(stale)
    _write_daemon_manifest(
        "wylde-core",
        pid=os.getpid(),
        description="Wylde core infrastructure (lifecycle, harness, memgraph, memory scheduler). "
        "Constituent pipes: \\\\.\\pipe\\wylde-lifecycle, "
        "\\\\.\\pipe\\wylde-harness, \\\\.\\pipe\\wylde-memgraph.",
        category="core",
        pipe=None,
        contributes={
            "dashboard": {"label": "Core", "icon": "cpu", "color": "blue"},
        },
    )
    _start_daemon_heartbeat("wylde-core", lambda: os.getpid())


def unregister_core_manifest() -> None:
    from . import _delete_manifest

    _stop_daemon_heartbeat("wylde-core")
    _delete_manifest("wylde-core")
