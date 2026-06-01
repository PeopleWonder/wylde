r"""Runtime control plane for Core/Lifecycle.

The launcher boots services once; this module exposes the *post-boot*
operations the GUI needs — start, stop, wake, list, health — over the
``\\.\pipe\wylde-lifecycle`` named pipe. Pipe wiring lives in
:mod:`Core.Lifecycle.daemon`; here we keep the control logic pure so it
can be tested without msgpack / pywin32 in scope.

Design notes
~~~~~~~~~~~~
* **Source of truth.** The set of registered services lives in
  ``Core/Network/services.yaml``. Live runtime state (PIDs, heartbeats)
  lives in ``data/manifests/{service}.json``, written by each service.
  ``launcher._running`` is the daemon's own in-memory record of children
  it has spawned in *this* process — authoritative for stop/wake on
  daemon-launched services, advisory for everything else.

* **Status classification.** Mirrors the GUI's ``deriveStatus``
  (``Core/GUI/src/lib/manifests.js``) and the legacy Gateway services
  route: heartbeat fresh < 35 s = ``active``, pipe-alive = ``active``,
  manifest mtime < 90 s = ``stale``, else ``inactive``.

* **Idempotence.** ``start`` on a running service is a no-op success;
  ``stop`` on a stopped service is a no-op success. Callers can retry.

* **Errors.** Handlers return plain dicts on success and raise
  :class:`ControlError` for predictable failures. The pipe layer wraps
  raises into the standard ``{ok: false, error: ...}`` envelope so
  callers see a structured error instead of a 500.
"""

from __future__ import annotations

import json
import os
import signal
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from . import launcher as _launcher
from . import updater_prefs as _updater_prefs
from ._common import (
    WYLDE_ROOT,
    find_service,
    load_services,
    logger,
    save_services,
)


# ─── Constants ──────────────────────────────────────────────────────────────

# Heartbeat-age thresholds.  Wylde's unified heartbeat tick is 60 s
# (see ``Core/shared/manifest.start_heartbeat`` and
# ``daemon_state._start_daemon_heartbeat``); we tolerate one missed tick
# plus 30 s of slack before flipping out of "active".  Past 5 min with no
# heartbeat the service is treated as gone.  GUI mirrors in
# ``Core/GUI/src/lib/manifests.js``.
_ACTIVE_MAX_AGE = 90.0
_STALE_MAX_AGE = 300.0

# Per-service grace window for stop. Mirrors shutdown.SHUTDOWN_TIMEOUT.
_STOP_GRACE_SECONDS = 10.0

# Where service manifests are written. Mirrors the resolution order of the
# old Gateway services route — both data/manifests and Core/shared/manifests.
_MANIFEST_DIRS = (
    WYLDE_ROOT / "data" / "manifests",
    WYLDE_ROOT / "Core" / "shared" / "manifests",
)


# ─── Errors ─────────────────────────────────────────────────────────────────


class ControlError(Exception):
    """Predictable failure surfaced over the pipe as a structured error.

    The pipe action dispatcher catches this and wraps it as
    ``{ok: false, error: {code, message}}``. ``code`` defaults to
    ``"control_error"`` if not specified.
    """

    def __init__(self, message: str, code: str = "control_error") -> None:
        super().__init__(message)
        self.code = code


# ─── Manifest reading ───────────────────────────────────────────────────────


def _read_runtime_manifests() -> dict[str, tuple[dict[str, Any], Path]]:
    """Map service-name → (manifest dict, manifest path).

    Multiple entries can collide — e.g. a name in both data/manifests and
    Core/shared/manifests. The first directory in ``_MANIFEST_DIRS`` wins;
    later directories don't overwrite. This matches the precedence the
    Gateway route used (data/manifests before Core/shared/manifests).
    """
    out: dict[str, tuple[dict[str, Any], Path]] = {}
    for d in _MANIFEST_DIRS:
        if not d.exists():
            continue
        for path in sorted(d.glob("*.json")):
            try:
                m = json.loads(path.read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError):
                continue
            if not isinstance(m, dict):
                continue
            name = m.get("service") or m.get("name")
            if not isinstance(name, str) or not name:
                continue
            out.setdefault(name, (m, path))
    return out


def _heartbeat_age(heartbeat: str | None) -> float:
    """Seconds since the heartbeat. ``inf`` if missing or malformed."""
    if not heartbeat:
        return float("inf")
    try:
        ts = datetime.fromisoformat(heartbeat.replace("Z", "+00:00"))
    except ValueError:
        return float("inf")
    return (datetime.now(timezone.utc) - ts).total_seconds()


def _pipe_path(service_name: str) -> str:
    """Named pipe path for ``service_name``. Mirrors core/shared/ipc.py."""
    suffix = service_name.removeprefix("wylde-")
    return rf"\\.\pipe\wylde-{suffix}"


def _pipe_alive(service_name: str) -> bool:
    r"""Is ``\\.\pipe\wylde-<name>`` accepting connections right now?

    Cheap existence check on Windows. Always False off-Windows since
    Wylde's pipe transport is win32-only.
    """
    if os.name != "nt":
        return False
    try:
        return os.path.exists(_pipe_path(service_name))
    except OSError:
        return False


def _classify(service_name: str, manifest: dict[str, Any], path: Path) -> str:
    """Bucket the service into ``active`` | ``stale`` | ``inactive``."""
    status = manifest.get("status") or {}
    age = _heartbeat_age(status.get("heartbeat"))
    if age < _ACTIVE_MAX_AGE:
        return "active"

    # Heartbeat looks stale. If the pipe is still accepting, the service
    # is alive even though its heartbeat thread is delayed.
    if _pipe_alive(service_name):
        return "active"

    # Manifest mtime is a secondary signal — the heartbeat tick rewrites
    # the manifest, so a recent mtime implies a recent live process.
    try:
        mtime_age = datetime.now(timezone.utc).timestamp() - path.stat().st_mtime
    except OSError:
        mtime_age = float("inf")
    age = min(age, mtime_age)

    if age < _STALE_MAX_AGE:
        return "stale"
    return "inactive"


# ─── Action handlers ────────────────────────────────────────────────────────
#
# Each handler takes the action `payload` (a dict, possibly None) and
# returns a JSON-serialisable result, or raises ControlError. The pipe
# layer in daemon.py registers these with ipc.register_action() under
# the names "service.start" / "service.stop" / etc.


def _require_name(payload: Any) -> str:
    """Pull and validate ``payload['name']``. Raises ControlError on bad input."""
    if not isinstance(payload, dict):
        raise ControlError("payload must be an object", code="bad_request")
    name = payload.get("name")
    if not isinstance(name, str) or not name.strip():
        raise ControlError("payload.name is required", code="bad_request")
    return name.strip()


def list_services_action(_payload: Any = None) -> dict[str, Any]:
    """Return ``{services: [...], counts: {...}}``.

    Thin shaper over :func:`Core.Lifecycle.registry.list_services`.
    Registry walks both declarative manifests (every ``<service>/manifest.json``
    in top-level Wylde/ plus the Core/ infrastructure list) and runtime
    heartbeat manifests under ``data/manifests/*.json``, then probes each
    service's pipe/port for live state. We map ``running=true`` → ``active``
    and pair it with the runtime ``heartbeat``-age fallback (in case a
    service heartbeat-thread is alive but the pipe blip'd for a tick).
    """
    from . import registry  # local import to keep control.py importable in tests

    infos = registry.list_services()
    out: list[dict[str, Any]] = []
    counts: dict[str, int] = {"active": 0, "stale": 0, "inactive": 0}

    for info in infos:
        # Bucket: probe says running → active. Otherwise fall back to
        # the heartbeat-age classifier for services whose pipe might be
        # briefly unreachable but heartbeat is fresh.
        if info.running:
            bucket = "active"
        else:
            age = _heartbeat_age(info.heartbeat)
            if age < _ACTIVE_MAX_AGE:
                bucket = "active"
            elif age < _STALE_MAX_AGE:
                bucket = "stale"
            else:
                bucket = "inactive"

        counts[bucket] = counts.get(bucket, 0) + 1
        # `pipe` from the registry is the full path (\\.\pipe\X) when the
        # manifest carried a short name. The GUI is content with either
        # shape — it doesn't try to open the pipe directly.
        out.append(
            {
                "name": info.name,
                "version": info.version,
                "category": info.kind,
                "description": info.description,
                "port": info.port,
                "endpoint": None,
                "enabled": info.enabled,
                "pipe": info.pipe,
                "status": bucket,
                "running": info.running,
                "pid": info.pid,
                "started_at": info.started_at or "",
                "heartbeat": info.heartbeat or "",
                "contributes": info.contributes or {},
                "tracked": info.name in _launcher.get_running(),
                "source": info.source,
            }
        )

    return {"services": out, "counts": counts}


def health_action(payload: Any = None) -> dict[str, Any]:
    """Pipe-call the named service's ``/health`` endpoint.

    Returns the service's reply on success. Raises ControlError if the
    service is unreachable — the pipe envelope surfaces that as a
    structured error rather than letting an exception bubble.
    """
    name = _require_name(payload)
    # Imported lazily so daemon-side tests can stub out IPC.
    from Core.shared import ipc

    try:
        reply = ipc.send(name, "/health", http_verb="GET", timeout=5.0)
    except Exception as e:  # noqa: BLE001
        raise ControlError(f"health probe failed: {e}", code="probe_failed") from e

    if not getattr(reply, "ok", False):
        err = getattr(reply, "error", None) or {"message": "unknown"}
        raise ControlError(
            f"{name} replied not-ok: {err.get('message', err)}",
            code="service_unhealthy",
        )
    return {"name": name, "reply": getattr(reply, "data", None)}


def start_action(payload: Any = None) -> dict[str, Any]:
    """Launch ``payload.name`` as a subprocess. Idempotent on already-running.

    Looks up the service in services.yaml, validates that we have the
    metadata to spawn it, then delegates to ``launcher._spawn``. On
    success the new Popen lands in ``launcher._running`` and the yaml
    is updated to ``status: running`` so a subsequent restart resumes
    the same set.
    """
    name = _require_name(payload)
    services = load_services()
    svc = find_service(services, name)
    if svc is None:
        raise ControlError(f"unknown service {name!r}", code="not_registered")

    # Already tracked → nothing to do. We probe poll() to catch the case
    # where _running has a stale Popen for a child that has since died.
    proc = _launcher.get_running().get(name)
    if proc is not None and proc.poll() is None:
        return {"name": name, "status": "running", "pid": proc.pid, "started": False}

    env_overlay = _launcher._build_env_overlay(services)
    try:
        new_proc = _launcher._spawn(svc, env_overlay)
    except Exception as e:  # noqa: BLE001
        logger.exception("control: failed to spawn %s", name)
        raise ControlError(f"spawn failed: {e}", code="spawn_failed") from e

    if new_proc is None:
        # _spawn returns None for library/internal services (no entry_point)
        # and for missing folders — either way there's nothing to start.
        raise ControlError(
            f"{name} has no entry_point or folder is missing",
            code="not_launchable",
        )

    _launcher.get_running()[name] = new_proc
    svc["status"] = "running"
    svc["enabled"] = True  # user explicitly started it
    save_services(services)

    return {"name": name, "status": "running", "pid": new_proc.pid, "started": True}


def stop_action(payload: Any = None) -> dict[str, Any]:
    """Graceful stop with force-kill fallback.

    Behaviour parity with ``shutdown._stop_one`` but scoped to a single
    service. If the daemon doesn't track the service (it wasn't spawned
    by us this session), we try to taskkill by manifest PID.
    """
    name = _require_name(payload)
    services = load_services()
    svc = find_service(services, name)

    proc = _launcher.get_running().pop(name, None)
    pid_killed: int | None = None

    if proc is not None and proc.poll() is None:
        # Daemon-tracked: send graceful signal, wait, force-kill if needed.
        logger.info("control: stopping %s (pid=%d)", name, proc.pid)
        _send_graceful_signal(proc)
        try:
            proc.wait(timeout=_STOP_GRACE_SECONDS)
        except subprocess.TimeoutExpired:
            logger.warning(
                "control: %s overstayed %.0fs grace, killing", name, _STOP_GRACE_SECONDS
            )
            proc.kill()
            try:
                proc.wait(timeout=2)
            except subprocess.TimeoutExpired:
                pass
        pid_killed = proc.pid
    else:
        # Not tracked — fall back to manifest PID. This catches services
        # started outside this daemon (e.g. by a legacy supervisor).
        rt = _read_runtime_manifests().get(name)
        if rt is not None:
            manifest_pid = (rt[0].get("status") or {}).get("pid")
            if isinstance(manifest_pid, int):
                pid_killed = _kill_pid(manifest_pid)

    if svc is not None:
        svc["status"] = "stopped"
        svc["enabled"] = False  # user explicitly stopped it
        save_services(services)

    return {"name": name, "status": "stopped", "pid_killed": pid_killed}


def shutdown_all_action(_payload: Any = None) -> dict[str, Any]:
    """Stop every running service the daemon is tracking — launcher-
    spawned AND daemon-special-cased.

    Two surfaces feed this:

    * :mod:`Core.Lifecycle.shutdown` walks ``launcher.get_running()``
      (services started via ``manifest.json`` + ``services.yaml``).
    * :func:`daemon.stop_all_daemon_managed` tears down the three
      tier=core subprocesses (Memgraph, Voice, device_gate) plus the
      memory scheduler thread — all of which the daemon spawns
      directly outside the launcher's tracked-services set.

    Both paths run; the response payload merges what each one stopped
    so the GUI can render a single "shut down Wylde" confirmation.
    The daemon-managed path is best-effort: if the daemon module isn't
    importable here (e.g. control.py used standalone in a test), we
    just skip that half and return what the launcher took down.
    """
    from . import shutdown as _shutdown

    running_before = list(_launcher.get_running().keys())
    try:
        _shutdown.shutdown_all()
    except Exception:  # noqa: BLE001
        logger.exception("control: shutdown.shutdown_all() raised")

    daemon_summary: dict[str, Any] = {"stopped": [], "failed": [], "count": 0}
    daemon_will_exit = False
    try:
        # Import from daemon_state, NOT daemon. daemon.py is loaded as
        # __main__ when launched via `python -m Core.Lifecycle.daemon`,
        # so a `from . import daemon` here would resolve to a different
        # module instance with all globals at None — and the action
        # would silently report nothing stopped. daemon_state is only
        # ever imported (never run as __main__), so every caller hits
        # the same module object and sees the live Popen handles.
        from . import daemon_state as _daemon_state

        daemon_summary = _daemon_state.stop_all_daemon_managed()
        # After tearing down the four managed components, ask the
        # daemon itself to exit so wylde-lifecycle and wylde-harness
        # (the in-process pipes) also disappear. The deferred-exit
        # thread waits ~500ms before flipping the daemon's stop event
        # so this action's response gets through first.
        daemon_will_exit = _daemon_state.request_daemon_exit()
    except Exception:  # noqa: BLE001
        logger.exception("control: stop_all_daemon_managed() raised")

    stopped = list(running_before) + list(daemon_summary.get("stopped", []))
    return {
        "stopped": stopped,
        "count": len(stopped),
        "launcher_stopped": running_before,
        "daemon_managed_stopped": daemon_summary.get("stopped", []),
        "daemon_managed_failed": daemon_summary.get("failed", []),
        "daemon_will_exit": daemon_will_exit,
    }


def wake_action(payload: Any = None) -> dict[str, Any]:
    """Ensure ``payload.name`` is running and responsive.

    Behaviour:
      * If the service is tracked and Popen is alive → just probe /health.
      * If the service is registered but not running → start it.
      * If the service is unknown → ``not_registered`` error.

    Returns the post-condition state so callers can render UI.
    """
    name = _require_name(payload)
    services = load_services()
    svc = find_service(services, name)
    if svc is None:
        raise ControlError(f"unknown service {name!r}", code="not_registered")

    proc = _launcher.get_running().get(name)
    if proc is not None and proc.poll() is None:
        # Already running — opportunistically probe /health, but don't
        # raise on a probe failure: the service may still be booting.
        try:
            return {
                "name": name,
                "status": "running",
                "pid": proc.pid,
                "woken": False,
                "health": health_action({"name": name}),
            }
        except ControlError as e:
            return {
                "name": name,
                "status": "running",
                "pid": proc.pid,
                "woken": False,
                "health_error": {"code": e.code, "message": str(e)},
            }

    # Not running → start it.
    started = start_action({"name": name})
    return {**started, "woken": True}


# ─── No-spawn parity surface ─────────────────────────────────────────────────
#
# The ``lifecycle.*`` actions below are the control surface the
# cross-language parity suite exercises. They report (and, for
# ``start_service``, drive) the no-spawn would-have-spawned set WITHOUT
# touching the launcher or service registry — both of which are
# Python-only and so cannot be gated against the Rust port. The Rust
# daemon ships byte-identical handlers (``wylde_lifecycle::control``).
#
# In a normal production daemon no-spawn is off, so ``lifecycle.status``
# and ``lifecycle.list_services`` simply report an empty set and
# ``lifecycle.start_service`` rejects with ``nospawn_required`` — it must
# never be a backdoor to a real spawn. See the no-spawn warning in
# :mod:`Core.Lifecycle.daemon_state`.


def lifecycle_status_action(_payload: Any = None) -> dict[str, Any]:
    """Report the daemon's no-spawn status.

    ``nospawn`` is the mode flag; ``would_have_spawned`` is the sorted list
    of services the daemon short-circuited instead of forking (empty in a
    production daemon). Parity-suite surface — see the section comment.
    """
    from . import daemon_state as _daemon_state

    snapshot = _daemon_state.nospawn_snapshot()
    return {
        "nospawn": _daemon_state.nospawn_enabled(),
        "service_count": len(snapshot),
        "would_have_spawned": snapshot,
    }


def lifecycle_list_services_action(_payload: Any = None) -> dict[str, Any]:
    """Map each would-have-spawned daemon-managed service to its state.

    Parity-suite surface — see the section comment. The map is empty in a
    production daemon (no-spawn off).
    """
    from . import daemon_state as _daemon_state

    snapshot = _daemon_state.nospawn_snapshot()
    services = {name: "would-have-spawned" for name in snapshot}
    return {"services": services, "count": len(services)}


def lifecycle_start_service_action(payload: Any = None) -> dict[str, Any]:
    """Run the no-spawn would-have-spawned short-circuit for ``payload.name``.

    Returns the synthetic success envelope a real spawn mirrors. No-spawn
    only: raises ``nospawn_required`` when no-spawn mode is off, so this can
    never trigger a real child process. Parity-suite surface — see the
    section comment.
    """
    from . import daemon_state as _daemon_state

    if not _daemon_state.nospawn_enabled():
        raise ControlError(
            "lifecycle.start_service is a no-spawn-only parity action",
            code="nospawn_required",
        )
    name = _require_name(payload)
    if not _daemon_state.nospawn_start(name):
        raise ControlError(
            f"unknown daemon-managed service {name!r}", code="unknown_service"
        )
    return {"name": name, "status": "would-have-spawned", "would_have_spawned": True}


# ─── Helpers ────────────────────────────────────────────────────────────────


def _send_graceful_signal(proc: subprocess.Popen) -> None:
    """Send the OS-appropriate graceful-stop signal."""
    if sys.platform == "win32":
        # CTRL_BREAK_EVENT works because launcher spawned with
        # CREATE_NEW_PROCESS_GROUP. CTRL_C_EVENT would also break us.
        try:
            proc.send_signal(signal.CTRL_BREAK_EVENT)
        except (OSError, ValueError):
            proc.terminate()
    else:
        proc.terminate()


def _kill_pid(pid: int) -> int | None:
    """Best-effort taskkill /F. Returns the pid on apparent success."""
    try:
        if sys.platform == "win32":
            subprocess.run(
                ["taskkill", "/F", "/PID", str(pid)],
                check=False,
                capture_output=True,
                timeout=5,
            )
        else:
            os.kill(pid, signal.SIGKILL)
    except (OSError, subprocess.TimeoutExpired) as e:
        logger.warning("control: kill_pid(%d) failed: %s", pid, e)
        return None
    return pid


# ─── Action registration ────────────────────────────────────────────────────


# Map of canonical action name → handler. Daemon registers these with
# ipc.register_action() at boot. Also exposed as ACTIONS for tests that
# want to drive handlers without a pipe.
ACTIONS: dict[str, Any] = {
    "service.list": list_services_action,
    "service.health": health_action,
    "service.start": start_action,
    "service.stop": stop_action,
    "service.wake": wake_action,
    "service.shutdown_all": shutdown_all_action,
    # No-spawn parity surface — see the section comment above. Registered
    # unconditionally; they are inert in a production daemon (no-spawn off).
    "lifecycle.status": lifecycle_status_action,
    "lifecycle.list_services": lifecycle_list_services_action,
    "lifecycle.start_service": lifecycle_start_service_action,
    "lifecycle.shutdown_all": shutdown_all_action,
    # GUI auto-update preferences. Handlers live in updater_prefs.py so this
    # file stays under the 700-line cap; folded in here so they register on
    # the same wylde-lifecycle pipe the Settings panel already calls.
    **_updater_prefs.ACTIONS,
}


def register_with_ipc() -> None:
    """Bind each action to ``Core.shared.ipc`` for pipe dispatch.

    Called once at daemon boot. Re-registering is safe (replaces handler).
    Unused on platforms without pywin32, in which case the daemon never
    gets here because the pipe server short-circuits.
    """
    from Core.shared import ipc

    for name, handler in ACTIONS.items():
        ipc.register_action(name, handler)
    logger.info("control: registered %d actions on wylde-lifecycle", len(ACTIONS))


__all__ = [
    "ACTIONS",
    "ControlError",
    "health_action",
    "lifecycle_list_services_action",
    "lifecycle_start_service_action",
    "lifecycle_status_action",
    "list_services_action",
    "register_with_ipc",
    "shutdown_all_action",
    "start_action",
    "stop_action",
    "wake_action",
]
