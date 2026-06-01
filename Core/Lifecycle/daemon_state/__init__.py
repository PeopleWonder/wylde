"""Daemon-managed subprocess + scheduler handles.

Lives in its own module (not in :mod:`Core.Lifecycle.daemon`) so the
state survives the ``__main__`` vs ``Core.Lifecycle.daemon`` module-
identity split that ``python -m Core.Lifecycle.daemon`` introduces.
When the daemon is spawned via ``-m``, Python loads daemon.py twice
in effect: once as ``__main__`` (where ``serve_forever`` runs and
populates the module-level Popen handles) and once as
``Core.Lifecycle.daemon`` (the package-rooted import path other
modules use). Code that depends on those globals — most notably
:func:`Core.Lifecycle.control.shutdown_all_action` — needs a single
shared module to reach them, regardless of which import root it
came in through. Keeping them here, only ever imported (never
``-m``-launched), gives every caller the same instance.

Public surface:

* The seven globals (``_memgraph_proc``, ``_voice_proc``,
  ``_device_gate_proc``, ``_vram_broker_proc``,
  ``_extension_bridge_proc``, ``_gateway_proc``, ``_memory_scheduler``)
  — set by the ``_start_*`` functions when the daemon boots.
* The ``_start_*`` / ``_stop_*`` pairs for each managed component.
* :func:`stop_all_daemon_managed` — the single teardown the
  signal handler AND the ``service.shutdown_all`` pipe action both
  go through.

Module layout
~~~~~~~~~~~~~

The original 1043-line ``daemon_state.py`` was split into a package so
each concern can be read in isolation:

* This file (`__init__.py`) — canonical module-level state, helpers,
  stop-event API, and the unified ``stop_all_daemon_managed`` teardown.
* :mod:`._manifest` — manifest writers + Core's runtime manifest.
* :mod:`._orphan_sweep` — orphan-detection sweep loop.
* :mod:`._services` — the three impl-dispatched service start/stop
  pairs (device_gate, vram_broker, gateway); also re-exports the four
  plain pairs from :mod:`._services_basic` so the whole set imports
  from one place.
* :mod:`._services_basic` — the four plain (no impl switch) service
  start/stop pairs (Memgraph, Voice, extension_bridge,
  memory_scheduler).

The state globals (process handles, spawn records, manifest dir) all
live here in the package namespace because:

  1. The signal-path teardown reads them directly.
  2. ``test_shutdown_all.py`` monkeypatches them via
     ``daemon_state._voice_proc = fake``; that only updates the
     package's namespace, so submodules that mutate or read these
     fields do so via the ``_ds.X`` indirection back to this module.
  3. Same for ``test_orphan_sweep.py``'s ``_MANIFEST_DIR`` /
     ``_pid_alive`` / ``_SPAWN_GRACE_SECONDS`` patches.
"""

from __future__ import annotations

import datetime
import json
import os
import subprocess
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Dict, Optional, Union

from .._common import WYLDE_ROOT, logger as _lc_logger


# ── No-spawn (parity / test) mode ─────────────────────────────────────
#
# No-spawn mode is a TEST-AND-PARITY-ONLY switch. When enabled (env
# ``WYLDE_LIFECYCLE_NOSPAWN=1`` or the ``--no-spawn`` CLI flag) the daemon
# brings up its full control surface — the ``\\.\pipe\wylde-lifecycle``
# pipe, every registered action — but the ``_start_<service>`` functions
# DO NOT fork child processes. Instead each records a "would-have-spawned"
# entry so the ``lifecycle.*`` parity actions (and ``service.shutdown_all``)
# report what the daemon *would* have done.
#
# A no-spawn daemon also leaves the host-wide ``data/manifests/core.json``
# untouched — it is neither written at boot nor deleted at shutdown — so a
# parity run never clobbers a production daemon's manifest. Combined with
# the ``WYLDE_LIFECYCLE_PIPE_NAME`` isolated-pipe override, a parity run is
# safe to perform while the real Wylde stack is up.
#
# ⚠️  THIS MUST NEVER BE ENABLED IN PRODUCTION. ⚠️
# A no-spawn daemon supervises nothing — Memgraph, Voice, the VRAM broker,
# the gateway and device_gate never start. It exists solely so the
# cross-language parity suite (``rust/tests/parity/tests/lifecycle.rs``)
# can exercise the control + manifest surfaces without booting Wylde's
# entire tier=core stack.

_nospawn: bool = False


def set_nospawn(enabled: bool) -> None:
    """Enable / disable no-spawn mode. Set once at daemon boot.

    TEST/PARITY ONLY — see the module-level no-spawn warning. Production
    daemons never call this; the flag defaults to ``False``.
    """
    global _nospawn
    _nospawn = bool(enabled)


def nospawn_enabled() -> bool:
    """True when no-spawn mode is active (``_start_<service>`` short-circuits).

    TEST/PARITY ONLY — see the module-level no-spawn warning.
    """
    return _nospawn


def detect_nospawn(argv: Optional[list[str]] = None) -> bool:
    """Resolve no-spawn from the ``--no-spawn`` CLI flag or env var.

    True if ``--no-spawn`` appears anywhere in ``argv`` (defaults to
    ``sys.argv``) or ``WYLDE_LIFECYCLE_NOSPAWN`` is set truthy. TEST/PARITY
    ONLY — see the module-level no-spawn warning.
    """
    import sys

    args = sys.argv if argv is None else argv
    if "--no-spawn" in args:
        return True
    return os.environ.get("WYLDE_LIFECYCLE_NOSPAWN", "").strip().lower() in (
        "1",
        "true",
        "yes",
        "on",
    )


class _NoSpawnProc:
    """Stand-in for :class:`subprocess.Popen` used ONLY in no-spawn mode.

    NO operating-system process is created. The object quacks like a live
    ``Popen`` just enough for the existing ``_stop_<service>`` /
    :func:`_is_proc_running` / :func:`stop_all_daemon_managed` machinery to
    treat a "would-have-spawned" service exactly like a real child — so the
    no-spawn short-circuit needs zero changes to the teardown code.

    ⚠️  TEST/PARITY ONLY. Never constructed when the daemon runs for real —
    see the module-level no-spawn warning.
    """

    def __init__(self, service: str, impl: str = "python") -> None:
        self.service = service
        self.impl = impl
        # Synthetic pid: 0 is never a real Windows process id, so any code
        # that pid-probes a no-spawn handle correctly sees "not running".
        self.pid = 0
        self._alive = True

    def poll(self) -> Optional[int]:
        """``None`` while "would-have-spawned", ``0`` once "stopped"."""
        return None if self._alive else 0

    def send_signal(self, _sig: int) -> None:
        self._alive = False

    def terminate(self) -> None:
        self._alive = False

    def kill(self) -> None:
        self._alive = False

    def wait(self, timeout: Optional[float] = None) -> int:
        return 0


# A daemon-managed handle is either a real subprocess.Popen or, in
# no-spawn mode, a _NoSpawnProc stand-in.
_ProcHandle = Union[subprocess.Popen, _NoSpawnProc]


# ── Daemon-managed state ──────────────────────────────────────────────


_memgraph_proc: Optional[_ProcHandle] = None
_voice_proc: Optional[_ProcHandle] = None
_device_gate_proc: Optional[_ProcHandle] = None
_vram_broker_proc: Optional[_ProcHandle] = None
_extension_bridge_proc: Optional[_ProcHandle] = None
_gateway_proc: Optional[_ProcHandle] = None
_ollama_proc: Optional[_ProcHandle] = None
# Phase 2 — wylde-vpn. Phase 2.E (2026-05-24) flipped the strangler-fig
# default to ``rust`` after Gateway's link routes were cut over to the
# action-style pipe surface. Set ``WYLDE_WYLDE_VPN_IMPL=python`` to roll
# back to ``VPN/run.py`` (still on disk for one release cycle).
_vpn_proc: Optional[_ProcHandle] = None
# Phase 3 — wylde-trainer (Caption sub-service). Default
# ``WYLDE_WYLDE_TRAINER_IMPL=python`` means in-process (no daemon-managed
# subprocess); set to ``rust`` to spawn the Rust binary fronting the
# captioner over ``\\.\pipe\wylde-trainer``.
_trainer_proc: Optional[_ProcHandle] = None
# Phase 3 — wylde-trainer-worker (Python inference engine). Only spawned
# when WYLDE_WYLDE_TRAINER_IMPL=rust; hosts \\.\pipe\wylde-trainer-worker
# and is where Florence-2 weights actually load. The Rust wylde-trainer
# forwards inference requests to this pipe.
_trainer_worker_proc: Optional[_ProcHandle] = None
# Phase 5 — wylde-harness (consolidated chat-turn / tooling / memory
# driver). Slice 5.D (2026-05-25) flipped the strangler-fig default
# from ``python`` to ``rust``: the lifecycle daemon now spawns
# ``wylde-harness.exe`` (Rust) and the Python harness pipe forwards
# ``chat.run_turn`` to ``\\.\pipe\wylde-harness`` by default. Set
# ``WYLDE_WYLDE_HARNESS_IMPL=python`` to revert to the in-process
# Python driver inside ``Core/harness/turn/`` during the rollback
# window.
_harness_proc: Optional[_ProcHandle] = None
_memory_scheduler: Optional[Any] = None

# The daemon's main loop blocks on this event. ``daemon.serve_forever``
# creates and registers it at boot via :func:`register_stop_event`;
# :func:`request_daemon_exit` flips it from a worker thread (e.g. the
# ``service.shutdown_all`` action handler) so the main loop unwinds and
# the daemon process exits — taking the lifecycle + harness pipes with
# it. Without this, the action could stop everything the daemon spawned
# but leave the daemon itself alive serving its two in-process pipes.
_daemon_stop_event: Optional[threading.Event] = None


def register_stop_event(event: threading.Event) -> None:
    """Hand the daemon's main-loop stop event to this module so action
    handlers can request a graceful exit. Idempotent — re-registering
    just replaces the reference."""
    global _daemon_stop_event
    _daemon_stop_event = event


def request_daemon_exit(*, after_seconds: float = 0.5) -> bool:
    """Ask the daemon to exit cleanly after a brief delay.

    The delay matters: the action that triggers this is mid-response.
    If we set the stop event synchronously, the daemon's main thread
    can reach ``stop.is_set()`` and start tearing the pipe server down
    before the worker thread has flushed the action's reply frame to
    the caller. Half a second is more than enough for an msgpack
    envelope to make it across a named pipe on the local box.

    Returns True if a stop event is registered (i.e., the daemon will
    exit), False if no event was registered (called outside a running
    daemon — e.g., from a unit test).
    """
    if _daemon_stop_event is None:
        return False

    def _wait_and_set() -> None:
        try:
            time.sleep(max(0.0, after_seconds))
        finally:
            ev = _daemon_stop_event
            if ev is not None:
                ev.set()

    threading.Thread(
        target=_wait_and_set,
        name="daemon-deferred-exit",
        daemon=True,
    ).start()
    return True


def _is_proc_running(proc: Optional[_ProcHandle]) -> bool:
    return proc is not None and proc.poll() is None


# ── Manifest paths + atomic write ─────────────────────────────────────
#
# Core's runtime manifest lives at `data/manifests/core.json`. Core is one
# logical service in the dashboard — its internal pipes (wylde-lifecycle,
# wylde-harness, wylde-memgraph) are NOT individually surfaced. Registry
# probes each constituent pipe live; this manifest only carries pid /
# started_at / heartbeat so the dashboard can show uptime and a fresh
# heartbeat indicator.
#
# Daemon-managed top-level services (Voice, device_gate) still publish
# their own runtime manifest at `data/manifests/wylde-<name>.json`, with
# a per-service heartbeat thread. Memgraph's wrapper writes its own too
# (we don't author wylde-memgraph.json from here — registry filters it
# out anyway as a Core constituent).

_MANIFEST_DIR: Path = WYLDE_ROOT / "data" / "manifests"
_heartbeat_stops: Dict[str, threading.Event] = {}

# Stale runtime manifest files from the prior granularity (each Core
# sub-pipe got its own row). Cleared on register_core_manifest() so a
# fresh daemon start doesn't leave the dashboard reporting them as peers.
_DEPRECATED_CORE_SUB_MANIFESTS: tuple[str, ...] = (
    "wylde-lifecycle",
    "wylde-harness",
    "wylde-memgraph",
    "wylde-memory-scheduler",
)


def _now_iso() -> str:
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _manifest_path(name: str) -> Path:
    # Core's runtime manifest lives at the conventional ``core.json``
    # path — the service field inside still reads ``wylde-core`` so
    # registry's name-normalised lookup against ``Core/manifest.json``
    # hits. All other services use the ``<name>.json`` convention.
    if name == "wylde-core":
        return _MANIFEST_DIR / "core.json"
    return _MANIFEST_DIR / f"{name}.json"


def _atomic_write_json(path: Path, data: Dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(f"{path.stem}.{os.getpid()}.tmp")
    try:
        tmp.write_text(json.dumps(data, indent=2), encoding="utf-8")
        os.replace(tmp, path)
    except OSError:
        try:
            tmp.unlink(missing_ok=True)
        except OSError:
            pass
        raise


def _delete_manifest(name: str) -> None:
    path = _manifest_path(name)
    try:
        path.unlink(missing_ok=True)
    except OSError as exc:
        _lc_logger.warning("manifest: delete failed for %s: %s", name, exc)


# ── Spawn records + orphan detection state ────────────────────────────
#
# Post manifest-ownership refactor, services own their data/manifests/
# files (write_manifest at startup, mark_stopped on graceful shutdown).
# The daemon's job is:
#
#   1. Track what it *spawned* — the spawn record stores the service
#      name, the pid it expected, and the spawn time. This is the
#      daemon's source of truth that "I started this thing", separate
#      from whether the service got far enough to write its manifest.
#
#   2. Sweep periodically (see :mod:`._orphan_sweep`).
#
# Spawn records live in-memory only. They reset on every daemon boot,
# which is the right behaviour: a fresh daemon doesn't inherit stale
# spawn expectations from the prior session.


@dataclass
class _SpawnRecord:
    pid: int
    spawn_time: float
    impl: str = "python"
    grace_satisfied: bool = False


_spawn_records: Dict[str, _SpawnRecord] = {}
_spawn_lock = threading.Lock()

# Window after spawn within which the service is expected to publish
# its manifest. Past this, with no manifest visible, the daemon emits
# a failed-to-launch warning. 30s is well past the slowest observed
# service startup (Memgraph waits up to 120s for Neo4j Bolt readiness,
# but its manifest is written BEFORE the Neo4j wait; only services
# that crash before reaching write_manifest hit this window).
_SPAWN_GRACE_SECONDS = 30.0

# Cadence for the orphan-detection sweep. Matches the unified 60s
# heartbeat tick so observed liveness signals are roughly synchronous.
_ORPHAN_SWEEP_INTERVAL = 60.0

_orphan_sweep_stop: Optional[threading.Event] = None


def _record_spawn(name: str, pid: int, impl: str = "python") -> None:
    """Tell orphan-detection that the daemon just spawned ``name``.

    ``impl`` records which implementation language is running ("python"
    or "rust") so dashboards and the orphan-sweep log can distinguish
    the two during the strangler-fig migration.
    """
    with _spawn_lock:
        _spawn_records[name] = _SpawnRecord(
            pid=pid, spawn_time=time.monotonic(), impl=impl
        )


def _forget_spawn(name: str) -> None:
    """Clear the spawn record on graceful stop. Stops orphan-detection
    from flagging a deliberately-stopped service as failed."""
    with _spawn_lock:
        _spawn_records.pop(name, None)


def _pid_alive(pid: int) -> bool:
    """Cheap liveness check for a pid the daemon spawned.

    Uses psutil when available (handles zombie / non-existent / access
    denied cases cleanly) with an os.kill(pid, 0) fallback. Returns
    False on any error path so a missing pid never falsely keeps a
    manifest in the alive bucket.
    """
    if pid <= 0:
        return False
    try:
        import psutil

        return bool(psutil.pid_exists(pid))
    except ImportError:
        try:
            os.kill(pid, 0)
            return True
        except OSError:
            return False


# ── Submodule re-exports ──────────────────────────────────────────────
#
# These imports come AFTER the helpers above so the submodules can
# safely ``from . import _manifest_path`` etc. at module-top. The
# submodules' write-sites for ``_voice_proc`` etc. route through
# ``from .. import daemon_state as _ds`` and mutate ``_ds.X``, which
# lands the new value back in this module's namespace.

from ._manifest import (  # noqa: E402
    register_core_manifest,
    unregister_core_manifest,
)
from ._orphan_sweep import (  # noqa: E402
    reap_manifest_orphans,
    start_orphan_sweep,
    stop_orphan_sweep,
    sweep_orphans,
)
from ._services import (  # noqa: E402
    _start_device_gate,
    _start_extension_bridge,
    _start_gateway,
    _start_memgraph,
    _start_memory_scheduler,
    _start_vram_broker,
    _start_voice,
    _start_wylde_harness,
    _start_wylde_ollama,
    _start_wylde_trainer,
    _start_wylde_trainer_worker,
    _start_wylde_vpn,
    _stop_device_gate,
    _stop_extension_bridge,
    _stop_gateway,
    _stop_memgraph,
    _stop_memory_scheduler,
    _stop_vram_broker,
    _stop_voice,
    _stop_wylde_harness,
    _stop_wylde_ollama,
    _stop_wylde_trainer,
    _stop_wylde_trainer_worker,
    _stop_wylde_vpn,
)


# ── No-spawn introspection (parity / test only) ───────────────────────


def nospawn_snapshot() -> list[str]:
    """Sorted names of services currently recorded as would-have-spawned.

    Walks the six daemon-managed process slots and returns the service name
    of every one holding a :class:`_NoSpawnProc` stand-in. TEST/PARITY ONLY
    — see the module-level no-spawn warning; in a production daemon the
    slots hold real ``Popen`` handles and this returns an empty list.

    The Rust daemon exposes the byte-identical
    :func:`wylde_lifecycle::state::nospawn_snapshot`; the cross-language
    parity suite diffs the two.
    """
    out: list[str] = []
    for proc in (
        _memgraph_proc,
        _voice_proc,
        _device_gate_proc,
        _vram_broker_proc,
        _extension_bridge_proc,
        _gateway_proc,
        _ollama_proc,
        _vpn_proc,
        _trainer_proc,
        _trainer_worker_proc,
        _harness_proc,
    ):
        if isinstance(proc, _NoSpawnProc):
            out.append(proc.service)
    return sorted(out)


def nospawn_start(name: str) -> bool:
    """Run the no-spawn would-have-spawned short-circuit for ``name``.

    Returns ``True`` if ``name`` is a known daemon-managed service (its
    ``_start_<service>`` ran and recorded a :class:`_NoSpawnProc`),
    ``False`` for an unknown name.

    TEST/PARITY ONLY: raises :class:`RuntimeError` when no-spawn mode is not
    active, so it can never trigger a real spawn — see the module-level
    no-spawn warning.
    """
    if not _nospawn:
        raise RuntimeError("nospawn_start requires no-spawn mode")
    starters = {
        "wylde-memgraph": _start_memgraph,
        "wylde-voice": _start_voice,
        "wylde-device-gate": _start_device_gate,
        "wylde-vram-broker": _start_vram_broker,
        "wylde-extension-bridge": _start_extension_bridge,
        "wylde-gateway": _start_gateway,
        "wylde-ollama": _start_wylde_ollama,
        "wylde-vpn": _start_wylde_vpn,
        "wylde-trainer": _start_wylde_trainer,
        "wylde-trainer-worker": _start_wylde_trainer_worker,
        "wylde-harness": _start_wylde_harness,
    }
    starter = starters.get(name)
    if starter is None:
        return False
    starter()
    return True


# ── Unified teardown ──────────────────────────────────────────────────


def stop_all_daemon_managed() -> Dict[str, Any]:
    """Tear down every long-lived child the daemon spawned outside the
    launcher's tracked-services set.

    Captures the running set first (so the response payload is honest
    about what was alive), runs each stop in scheduler → voice →
    device-gate → memgraph order (Memgraph last so anything still
    holding a Bolt driver releases first), swallows individual stop
    failures (each gets logged), and returns a structured summary.

    Both the SIGINT/SIGTERM handler AND
    :func:`Core.Lifecycle.control.shutdown_all_action` go through here,
    so external invocation and Ctrl-C tear down the same way.
    """
    stopped: list[str] = []
    failed: list[Dict[str, Any]] = []

    def _try(name: str, alive: bool, fn: Any) -> None:
        if not alive:
            return
        try:
            fn()
            stopped.append(name)
        except Exception as exc:  # noqa: BLE001
            _lc_logger.exception("daemon: stop %s raised", name)
            failed.append({"name": name, "error": f"{type(exc).__name__}: {exc}"})

    # Halt orphan-detection BEFORE stopping services — otherwise an
    # in-flight sweep could flag a service mid-teardown as a "dead
    # orphan" and rewrite its manifest to dead-orphan after the
    # service already wrote stopped.
    try:
        stop_orphan_sweep()
    except Exception:  # noqa: BLE001
        _lc_logger.exception("daemon: stop_orphan_sweep raised")

    _try("memory_scheduler", _memory_scheduler is not None, _stop_memory_scheduler)
    # Gateway first — it's the outward-facing surface, taking it down
    # before its dependents (Voice + device_gate) reduces the blast
    # radius if a teardown hangs.
    _try("wylde-gateway", _is_proc_running(_gateway_proc), _stop_gateway)
    # Extension bridge next — Gateway dispatched extension calls into
    # it, so it has no users left once Gateway is down.
    _try(
        "wylde-extension-bridge",
        _is_proc_running(_extension_bridge_proc),
        _stop_extension_bridge,
    )
    # wylde-harness (Phase 5) — consolidated chat-turn driver. Stops
    # AFTER Gateway/extension-bridge (callers are gone) but BEFORE
    # wylde-ollama (its main downstream) so any final LLM call lease is
    # released before Ollama tears its broker connection down.
    _try(
        "wylde-harness",
        _is_proc_running(_harness_proc),
        _stop_wylde_harness,
    )
    _try("wylde-voice", _is_proc_running(_voice_proc), _stop_voice)
    _try("wylde-device-gate", _is_proc_running(_device_gate_proc), _stop_device_gate)
    # wylde-ollama BEFORE the broker so any in-flight VRAM leases get
    # released cleanly; broker shutdown then has nothing to reap.
    _try("wylde-ollama", _is_proc_running(_ollama_proc), _stop_wylde_ollama)
    # wylde-trainer alongside ollama — also a VRAM consumer (Florence-2
    # loads ~1.5 GB) so it should release before the broker. In python
    # (in-process) mode this is a no-op stop. Trainer stops BEFORE its
    # worker so trainer's last in-flight inference can complete cleanly.
    _try("wylde-trainer", _is_proc_running(_trainer_proc), _stop_wylde_trainer)
    _try(
        "wylde-trainer-worker",
        _is_proc_running(_trainer_worker_proc),
        _stop_wylde_trainer_worker,
    )
    # wylde-vpn — independent service tier (optional). Stopped alongside
    # the others so a daemon-driven shutdown_all drains it too.
    _try("wylde-vpn", _is_proc_running(_vpn_proc), _stop_wylde_vpn)
    _try("wylde-vram-broker", _is_proc_running(_vram_broker_proc), _stop_vram_broker)
    _try("wylde-memgraph", _is_proc_running(_memgraph_proc), _stop_memgraph)

    # Safety net — kill anything in data/manifests/*.json whose pid is
    # still in the process table. The tracked-Popen path above misses
    # orphans from prior crashed daemon sessions: ``_<service>_proc``
    # is ``None`` on a fresh daemon, so the matching ``_try`` call
    # short-circuits and the survivor outlives every restart. Without
    # this reap a wylde-gateway.exe started by an earlier daemon can
    # keep its pipe + port-8005 bind across every later boot.
    #
    # Skipped under no-spawn: the no-spawn daemon writes no real
    # manifests and any alive-pid hit would belong to a real daemon
    # sharing the host.
    reaped: list[Dict[str, Any]] = []
    if not _nospawn:
        try:
            reaped = reap_manifest_orphans()
        except Exception:  # noqa: BLE001
            _lc_logger.exception("daemon: reap_manifest_orphans raised")

    # Core's runtime manifest cleanup runs out-of-band — it's not a
    # subprocess that "stopped", just a JSON file we remove so the next
    # service.list doesn't surface a Core entry with a stale heartbeat.
    # Keeping it out of the `_try` loop means stop_all_daemon_managed's
    # response stays scoped to the actually-stopped subprocesses.
    #
    # Skipped under no-spawn: core.json is host-wide shared state. A
    # no-spawn (parity) daemon never wrote it (see the no-spawn skip in
    # daemon.serve_forever), and deleting it here would remove a *real*
    # daemon's manifest if one is running on the same box.
    if not _nospawn:
        try:
            unregister_core_manifest()
        except Exception:  # noqa: BLE001
            _lc_logger.exception("daemon: unregister_core_manifest raised")

    return {
        "stopped": stopped,
        "failed": failed,
        "count": len(stopped),
        "reaped": reaped,
    }


__all__ = [
    "_is_proc_running",
    "set_nospawn",
    "nospawn_enabled",
    "detect_nospawn",
    "nospawn_snapshot",
    "nospawn_start",
    "_start_memgraph",
    "_stop_memgraph",
    "_start_voice",
    "_stop_voice",
    "_start_gateway",
    "_stop_gateway",
    "_start_device_gate",
    "_stop_device_gate",
    "_start_vram_broker",
    "_stop_vram_broker",
    "_start_extension_bridge",
    "_stop_extension_bridge",
    "_start_wylde_ollama",
    "_stop_wylde_ollama",
    "_start_wylde_vpn",
    "_stop_wylde_vpn",
    "_start_wylde_harness",
    "_stop_wylde_harness",
    "_start_wylde_trainer",
    "_stop_wylde_trainer",
    "_start_wylde_trainer_worker",
    "_stop_wylde_trainer_worker",
    "_start_memory_scheduler",
    "_stop_memory_scheduler",
    "stop_all_daemon_managed",
    "register_stop_event",
    "request_daemon_exit",
    "register_core_manifest",
    "unregister_core_manifest",
    "sweep_orphans",
    "start_orphan_sweep",
    "stop_orphan_sweep",
    "reap_manifest_orphans",
]
