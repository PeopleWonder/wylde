"""Daemon-managed service start/stop pairs.

The three impl-dispatched services — device_gate, vram_broker, gateway —
live here: each has a Rust port, so ``_start_X`` picks Python vs Rust via
``WYLDE_<SERVICE>_IMPL`` (the strangler-fig switch). Each ``_start_X``
boots the service as a subprocess and records the spawn so
orphan-detection knows about it; each ``_stop_X`` sends the
OS-appropriate graceful signal, waits, and force-kills on timeout.

The four plain (no impl switch) services — Memgraph, Voice,
extension_bridge, memory_scheduler — were split into :mod:`._services_basic`
to keep this file under the file-size cap; they are re-imported below so
``daemon_state`` still imports the whole start/stop set from ``._services``.

Process handles (``_voice_proc`` etc.) live on the package's
``__init__.py`` namespace so monkeypatches on the package flow through
to the read-sites here. Mutations go through ``_ds`` — the canonical
``daemon_state`` module — so writes in this submodule are visible to
:func:`Core.Lifecycle.daemon_state.stop_all_daemon_managed`.

The strangler-fig dispatch helpers (``_impl_for`` / ``_rust_binary_path``
/ ``_spawn_rust_service``) — how ``WYLDE_<SERVICE>_IMPL=rust`` picks a
sibling Rust binary — live in :mod:`._strangler` and are re-imported
below so the ``_start_*_rust`` branches here can call them.
"""

from __future__ import annotations

import os
import signal
import subprocess
import sys
from pathlib import Path

from .. import daemon_state as _ds
from .._common import WYLDE_ROOT, logger as _lc_logger
from ._strangler import _impl_for, _rust_binary_path, _spawn_rust_service

# Plain (no impl-switch) services. Re-exported so callers and
# ``daemon_state`` keep importing the full start/stop set from ``._services``.
from ._services_basic import (  # noqa: F401
    _start_extension_bridge,
    _start_memgraph,
    _start_memory_scheduler,
    _start_voice,
    _stop_extension_bridge,
    _stop_memgraph,
    _stop_memory_scheduler,
    _stop_voice,
)

# Trainer pair (Phase 3 — Caption pipe surface + Python worker). Split
# out so this file stays under the 700-line cap; re-exported so callers
# keep importing the full start/stop set from ``._services``.
from ._services_trainer import (  # noqa: F401
    _start_wylde_trainer,
    _start_wylde_trainer_worker,
    _stop_wylde_trainer,
    _stop_wylde_trainer_worker,
)

# Harness pair (Phase 5 — consolidated chat-turn / tooling / memory
# driver port). Split out for the same reason as the trainer pair;
# re-exported so callers keep importing the full start/stop set from
# ``._services``. the Wylde user's 2026-05-24 architectural call consolidated
# the prior ``wylde-harness-turn`` standalone crate back into a unified
# ``wylde-harness`` crate with submodules — this is the post-rename
# start/stop pair.
from ._services_harness import (  # noqa: F401
    _start_wylde_harness,
    _stop_wylde_harness,
)


# ── device_gate ───────────────────────────────────────────────────────


def _start_device_gate_python() -> None:
    """Boot the Python device_gate (``python -m device_gate.run``)."""
    cmd = [sys.executable, "-m", "device_gate.run"]
    env = os.environ.copy()
    env.setdefault("WYLDE_SERVICE_NAME", "wylde-device-gate")
    namespace_root = str(WYLDE_ROOT.parent)
    existing = env.get("PYTHONPATH", "")
    env["PYTHONPATH"] = (
        namespace_root + os.pathsep + existing if existing else namespace_root
    )

    creation_flags = 0
    if sys.platform == "win32":
        creation_flags = subprocess.CREATE_NEW_PROCESS_GROUP

    try:
        _ds._device_gate_proc = subprocess.Popen(
            cmd,
            cwd=str(WYLDE_ROOT),
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.STDOUT,
            stdin=subprocess.DEVNULL,
            creationflags=creation_flags,
        )
        # device_gate/run.py owns its manifest + heartbeat. Daemon only
        # records the spawn intent for orphan-detection bookkeeping.
        _ds._record_spawn("wylde-device-gate", _ds._device_gate_proc.pid, impl="python")
        _lc_logger.info(
            "daemon: spawned device_gate impl=python pid=%d",
            _ds._device_gate_proc.pid,
        )
    except Exception:  # noqa: BLE001
        _lc_logger.exception("daemon: device_gate spawn failed")
        _ds._device_gate_proc = None


def _start_device_gate_rust(rust_bin: Path) -> None:
    """Boot the Rust device_gate binary at ``rust_bin``."""
    proc = _spawn_rust_service(service="wylde-device-gate", rust_bin=rust_bin)
    if proc is None:
        _ds._device_gate_proc = None
        return
    _ds._device_gate_proc = proc
    _ds._record_spawn("wylde-device-gate", proc.pid, impl="rust")
    _lc_logger.info(
        "daemon: spawned device_gate impl=rust binary=%s pid=%d", rust_bin, proc.pid
    )


def _start_device_gate() -> None:
    """Boot device_gate as a subprocess of the Lifecycle daemon.

    Dispatches to either the Python module or a Rust binary depending
    on ``WYLDE_WYLDE_DEVICE_GATE_IMPL`` (default ``python``). Falls back
    to Python with a warning if the Rust binary is missing.

    NO-SPAWN MODE (test/parity only — see the no-spawn warning in
    :mod:`Core.Lifecycle.daemon_state`): records a "would-have-spawned"
    handle and forks nothing.
    """
    if _ds.nospawn_enabled():
        _ds._device_gate_proc = _ds._NoSpawnProc(
            "wylde-device-gate", impl=_impl_for("wylde-device-gate")
        )
        _lc_logger.info(
            "device_gate: NO-SPAWN — would-have-spawned recorded; no child forked"
        )
        return
    if _ds._device_gate_proc is not None and _ds._device_gate_proc.poll() is None:
        return

    if _impl_for("wylde-device-gate") == "rust":
        rust_bin = _rust_binary_path("wylde-device-gate")
        if rust_bin is None:
            _lc_logger.warning(
                "device_gate: WYLDE_WYLDE_DEVICE_GATE_IMPL=rust but no binary "
                "found; falling back to python"
            )
            _start_device_gate_python()
            return
        _start_device_gate_rust(rust_bin)
        return
    _start_device_gate_python()


def _stop_device_gate() -> None:
    """Take device_gate down. Service owns its manifest, so the daemon
    only forgets the spawn record — the manifest's terminal ``stopped``
    state is written by device_gate's own SIGTERM handler."""
    _ds._forget_spawn("wylde-device-gate")
    proc = _ds._device_gate_proc
    _ds._device_gate_proc = None
    if proc is None:
        return
    if proc.poll() is not None:
        return

    _lc_logger.info("device_gate: stopping (pid=%d)", proc.pid)
    try:
        if sys.platform == "win32":
            proc.send_signal(signal.CTRL_BREAK_EVENT)
        else:
            proc.terminate()
    except (OSError, ValueError):
        proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        _lc_logger.warning("device_gate: didn't exit within 10s — killing")
        proc.kill()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            pass


# ── VRAM broker ───────────────────────────────────────────────────────


def _start_vram_broker_python() -> None:
    """Boot the Python VRAM broker (``python -m Core.resource_monitor.run``).

    Hosts ``\\\\.\\pipe\\wylde-vram-broker`` — the fourth constituent of
    Core (alongside lifecycle / harness / memgraph). The broker also
    writes its own ``data/manifests/vram-broker.json``, which the
    registry filters out of the dashboard's peer list because the pipe
    is claimed by Core's ``constituent_pipes``. No daemon-side manifest
    write here — the broker owns its own lifecycle file.
    """
    cmd = [sys.executable, "-m", "Core.resource_monitor.run"]
    env = os.environ.copy()
    env.setdefault("WYLDE_SERVICE_NAME", "wylde-vram-broker")
    env.setdefault("WYLDE_ROOT", str(WYLDE_ROOT))
    namespace_root = str(WYLDE_ROOT.parent)
    existing = env.get("PYTHONPATH", "")
    env["PYTHONPATH"] = (
        namespace_root + os.pathsep + existing if existing else namespace_root
    )

    creation_flags = 0
    if sys.platform == "win32":
        creation_flags = subprocess.CREATE_NEW_PROCESS_GROUP

    try:
        _ds._vram_broker_proc = subprocess.Popen(
            cmd,
            cwd=str(WYLDE_ROOT),
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.STDOUT,
            stdin=subprocess.DEVNULL,
            creationflags=creation_flags,
        )
        # No daemon-side manifest write. resource_monitor/run.py writes
        # its own data/manifests/vram-broker.json; registry filters it
        # as a Core constituent so it doesn't surface as a peer service.
        _ds._record_spawn("vram-broker", _ds._vram_broker_proc.pid, impl="python")
        _lc_logger.info(
            "daemon: spawned vram_broker impl=python pid=%d",
            _ds._vram_broker_proc.pid,
        )
    except Exception:  # noqa: BLE001
        _lc_logger.exception("daemon: vram_broker spawn failed")
        _ds._vram_broker_proc = None


def _start_vram_broker_rust(rust_bin: Path) -> None:
    """Boot the Rust VRAM broker binary at ``rust_bin``."""
    proc = _spawn_rust_service(service="wylde-vram-broker", rust_bin=rust_bin)
    if proc is None:
        _ds._vram_broker_proc = None
        return
    _ds._vram_broker_proc = proc
    _ds._record_spawn("vram-broker", proc.pid, impl="rust")
    _lc_logger.info(
        "daemon: spawned vram_broker impl=rust binary=%s pid=%d", rust_bin, proc.pid
    )


def _start_vram_broker() -> None:
    """Boot the VRAM broker as a subprocess of the Lifecycle daemon.

    Dispatches to either the Python module or a Rust binary depending
    on ``WYLDE_WYLDE_VRAM_BROKER_IMPL`` (default ``rust``). Falls back
    to Python with a warning if the Rust binary is missing.

    The default is ``rust`` because only the Rust broker implements the
    Phase-0.5 estimator and DRAM spillover; the Python broker rejects a
    reserve with no ``bytes`` ("bytes must be positive") and cannot admit
    a model larger than VRAM (no spillover) — which is the common case for
    a quantised 27B-class model on a 16 GB card. Python stays as a rollback
    path via ``WYLDE_WYLDE_VRAM_BROKER_IMPL=python``.

    NO-SPAWN MODE (test/parity only — see the no-spawn warning in
    :mod:`Core.Lifecycle.daemon_state`): records a "would-have-spawned"
    handle and forks nothing.
    """
    if _ds.nospawn_enabled():
        _ds._vram_broker_proc = _ds._NoSpawnProc(
            "wylde-vram-broker", impl=_impl_for("wylde-vram-broker", default="rust")
        )
        _lc_logger.info(
            "vram_broker: NO-SPAWN — would-have-spawned recorded; no child forked"
        )
        return
    if _ds._vram_broker_proc is not None and _ds._vram_broker_proc.poll() is None:
        return  # already running

    if _impl_for("wylde-vram-broker", default="rust") == "rust":
        rust_bin = _rust_binary_path("wylde-vram-broker")
        if rust_bin is None:
            _lc_logger.warning(
                "vram_broker: WYLDE_WYLDE_VRAM_BROKER_IMPL=rust but no binary "
                "found; falling back to python"
            )
            _start_vram_broker_python()
            return
        _start_vram_broker_rust(rust_bin)
        return
    _start_vram_broker_python()


def _stop_vram_broker() -> None:
    """Take the VRAM broker subprocess down cleanly."""
    _ds._forget_spawn("vram-broker")
    proc = _ds._vram_broker_proc
    _ds._vram_broker_proc = None
    if proc is None:
        return
    if proc.poll() is not None:
        return  # already exited

    _lc_logger.info("vram_broker: stopping (pid=%d)", proc.pid)
    try:
        if sys.platform == "win32":
            proc.send_signal(signal.CTRL_BREAK_EVENT)
        else:
            proc.terminate()
    except (OSError, ValueError):
        proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        _lc_logger.warning("vram_broker: didn't exit within 10s — killing")
        proc.kill()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            pass


# ── Gateway ───────────────────────────────────────────────────────────


def _start_gateway_python() -> None:
    """Boot the Python Gateway (``python -m Gateway.run``).

    Gateway is a peer service (top-level ``Wylde/Gateway/`` folder with
    its own manifest, tier=core) — same pattern as Voice / device_gate.
    It hosts the unified HTTP ingress/egress on 127.0.0.1:8005 (per
    :class:`Gateway.settings.GatewaySettings`) and is where the browser-
    extension routes (``/extensions/<name>/<endpoint>``) terminate.
    """
    cmd = [sys.executable, "-m", "Gateway.run"]
    env = os.environ.copy()
    env.setdefault("WYLDE_SERVICE_NAME", "wylde-gateway")
    namespace_root = str(WYLDE_ROOT.parent)
    existing = env.get("PYTHONPATH", "")
    env["PYTHONPATH"] = (
        namespace_root + os.pathsep + existing if existing else namespace_root
    )

    creation_flags = 0
    if sys.platform == "win32":
        creation_flags = subprocess.CREATE_NEW_PROCESS_GROUP

    try:
        _ds._gateway_proc = subprocess.Popen(
            cmd,
            cwd=str(WYLDE_ROOT),
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.STDOUT,
            stdin=subprocess.DEVNULL,
            creationflags=creation_flags,
        )
        # Gateway/run.py owns its manifest + heartbeat. Daemon only
        # records the spawn intent for orphan-detection bookkeeping.
        _ds._record_spawn("wylde-gateway", _ds._gateway_proc.pid, impl="python")
        _lc_logger.info(
            "daemon: spawned gateway impl=python pid=%d", _ds._gateway_proc.pid
        )
    except Exception:  # noqa: BLE001
        _lc_logger.exception("daemon: gateway spawn failed")
        _ds._gateway_proc = None


def _start_gateway_rust(rust_bin: Path) -> None:
    """Boot the Rust Gateway binary at ``rust_bin``."""
    proc = _spawn_rust_service(service="wylde-gateway", rust_bin=rust_bin)
    if proc is None:
        _ds._gateway_proc = None
        return
    _ds._gateway_proc = proc
    _ds._record_spawn("wylde-gateway", proc.pid, impl="rust")
    _lc_logger.info(
        "daemon: spawned gateway impl=rust binary=%s pid=%d", rust_bin, proc.pid
    )


def _start_gateway() -> None:
    """Boot the Gateway as a subprocess of the Lifecycle daemon.

    Dispatches to either the Python module or a Rust binary depending
    on ``WYLDE_WYLDE_GATEWAY_IMPL`` (default ``rust`` since 2026-05-30 —
    the Rust ``wylde-gateway`` is the canonical ingress/egress server and
    is a superset of the Python routes). Falls back to Python with a
    warning if the Rust binary is missing; set the env var to ``python``
    to force the (rollback-only) Python ``Gateway/`` module.

    Depends on the harness pipe and device_gate already being up — the
    daemon orders the spawns accordingly in
    :func:`Core.Lifecycle.daemon.serve_forever`.

    NO-SPAWN MODE (test/parity only — see the no-spawn warning in
    :mod:`Core.Lifecycle.daemon_state`): records a "would-have-spawned"
    handle and forks nothing.
    """
    if _ds.nospawn_enabled():
        _ds._gateway_proc = _ds._NoSpawnProc(
            "wylde-gateway", impl=_impl_for("wylde-gateway", default="rust")
        )
        _lc_logger.info(
            "gateway: NO-SPAWN — would-have-spawned recorded; no child forked"
        )
        return
    if _ds._gateway_proc is not None and _ds._gateway_proc.poll() is None:
        return  # already running

    if _impl_for("wylde-gateway", default="rust") == "rust":
        rust_bin = _rust_binary_path("wylde-gateway")
        if rust_bin is None:
            _lc_logger.warning(
                "gateway: WYLDE_WYLDE_GATEWAY_IMPL=rust but no binary found; "
                "falling back to python"
            )
            _start_gateway_python()
            return
        _start_gateway_rust(rust_bin)
        return
    _start_gateway_python()


def _stop_gateway() -> None:
    """Take the Gateway subprocess down cleanly. Service owns its
    manifest — daemon only forgets the spawn record."""
    _ds._forget_spawn("wylde-gateway")
    proc = _ds._gateway_proc
    _ds._gateway_proc = None
    if proc is None:
        return
    if proc.poll() is not None:
        return

    _lc_logger.info("gateway: stopping (pid=%d)", proc.pid)
    try:
        if sys.platform == "win32":
            proc.send_signal(signal.CTRL_BREAK_EVENT)
        else:
            proc.terminate()
    except (OSError, ValueError):
        proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        _lc_logger.warning("gateway: didn't exit within 10s — killing")
        proc.kill()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            pass


# ── wylde-ollama (greenfield Rust — no Python predecessor) ────────────


def _start_wylde_ollama() -> None:
    """Boot wylde-ollama as a subprocess of the Lifecycle daemon.

    Greenfield Rust — there is no Python implementation. The
    strangler-fig env var is read for shape consistency with the other
    services but a value of ``python`` only logs a warning; the Rust
    binary is the only valid impl.

    NO-SPAWN MODE (test/parity only — see the no-spawn warning in
    :mod:`Core.Lifecycle.daemon_state`): records a "would-have-spawned"
    :class:`~Core.Lifecycle.daemon_state._NoSpawnProc` handle and forks
    nothing.
    """
    if _ds.nospawn_enabled():
        _ds._ollama_proc = _ds._NoSpawnProc("wylde-ollama", impl="rust")
        _lc_logger.info(
            "ollama: NO-SPAWN — would-have-spawned recorded; no child forked"
        )
        return
    if _ds._ollama_proc is not None and _ds._ollama_proc.poll() is None:
        return  # already running

    if _impl_for("wylde-ollama") == "python":
        _lc_logger.warning(
            "ollama: WYLDE_WYLDE_OLLAMA_IMPL=python but wylde-ollama is greenfield "
            "Rust (no Python predecessor); proceeding with rust binary"
        )

    rust_bin = _rust_binary_path("wylde-ollama")
    if rust_bin is None:
        _lc_logger.error(
            "ollama: rust binary not found (checked WYLDE_WYLDE_OLLAMA_BIN, "
            "rust/bin/wylde-ollama.exe, rust/target/release/wylde-ollama.exe, "
            "rust/target/debug/wylde-ollama.exe) — build with `cargo build "
            "--release -p wylde-ollama` first"
        )
        return

    proc = _spawn_rust_service(service="wylde-ollama", rust_bin=rust_bin)
    if proc is None:
        _ds._ollama_proc = None
        return
    _ds._ollama_proc = proc
    _ds._record_spawn("wylde-ollama", proc.pid, impl="rust")
    _lc_logger.info(
        "daemon: spawned ollama impl=rust binary=%s pid=%d", rust_bin, proc.pid
    )


def _stop_wylde_ollama() -> None:
    """Take the wylde-ollama subprocess down cleanly."""
    _ds._forget_spawn("wylde-ollama")
    proc = _ds._ollama_proc
    _ds._ollama_proc = None
    if proc is None:
        return
    if proc.poll() is not None:
        return  # already exited

    _lc_logger.info("ollama: stopping (pid=%d)", proc.pid)
    try:
        if sys.platform == "win32":
            proc.send_signal(signal.CTRL_BREAK_EVENT)
        else:
            proc.terminate()
    except (OSError, ValueError):
        proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        _lc_logger.warning("ollama: didn't exit within 10s — killing")
        proc.kill()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            pass


def _start_wylde_vpn_python() -> None:
    """Boot the Python WyldeLink VPN service (``python VPN/run.py``).

    The Python service ships the Flask control plane on 127.0.0.1:8020
    plus the existing tunnel/NAT/discovery side-cars.
    """
    cmd = [sys.executable, str(WYLDE_ROOT / "VPN" / "run.py")]
    env = os.environ.copy()
    env.setdefault("WYLDE_SERVICE_NAME", "wylde-vpn")
    env.setdefault("WYLDE_ROOT", str(WYLDE_ROOT))
    namespace_root = str(WYLDE_ROOT.parent)
    existing = env.get("PYTHONPATH", "")
    env["PYTHONPATH"] = (
        namespace_root + os.pathsep + existing if existing else namespace_root
    )

    creation_flags = 0
    if sys.platform == "win32":
        creation_flags = subprocess.CREATE_NEW_PROCESS_GROUP

    try:
        _ds._vpn_proc = subprocess.Popen(
            cmd,
            cwd=str(WYLDE_ROOT),
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.STDOUT,
            stdin=subprocess.DEVNULL,
            creationflags=creation_flags,
        )
        _ds._record_spawn("wylde-vpn", _ds._vpn_proc.pid, impl="python")
        _lc_logger.info(
            "daemon: spawned wylde-vpn impl=python pid=%d", _ds._vpn_proc.pid
        )
    except Exception:  # noqa: BLE001
        _lc_logger.exception("daemon: wylde-vpn spawn failed")
        _ds._vpn_proc = None


def _start_wylde_vpn_rust(rust_bin: Path) -> None:
    """Boot the Rust wylde-vpn binary at ``rust_bin``."""
    proc = _spawn_rust_service(service="wylde-vpn", rust_bin=rust_bin)
    if proc is None:
        _ds._vpn_proc = None
        return
    _ds._vpn_proc = proc
    _ds._record_spawn("wylde-vpn", proc.pid, impl="rust")
    _lc_logger.info(
        "daemon: spawned wylde-vpn impl=rust binary=%s pid=%d", rust_bin, proc.pid
    )


def _start_wylde_vpn() -> None:
    """Boot wylde-vpn as a subprocess of the Lifecycle daemon.

    Dispatches to either the Python module or a Rust binary depending
    on ``WYLDE_WYLDE_VPN_IMPL``. Phase 2.E (2026-05-24) flipped the
    default from ``python`` to ``rust`` after Gateway's link routes
    were cut over to the action-style pipe surface; the Python
    ``VPN/run.py`` Flask service stays on disk for one release cycle
    as the rollback path. Set ``WYLDE_WYLDE_VPN_IMPL=python`` to
    revert. Falls back to Python with a warning if the Rust binary is
    missing.

    NO-SPAWN MODE (test/parity only — see the no-spawn warning in
    :mod:`Core.Lifecycle.daemon_state`): records a "would-have-spawned"
    handle and forks nothing.
    """
    if _ds.nospawn_enabled():
        _ds._vpn_proc = _ds._NoSpawnProc(
            "wylde-vpn", impl=_impl_for("wylde-vpn", default="rust")
        )
        _lc_logger.info(
            "wylde-vpn: NO-SPAWN — would-have-spawned recorded; no child forked"
        )
        return
    if _ds._vpn_proc is not None and _ds._vpn_proc.poll() is None:
        return  # already running

    if _impl_for("wylde-vpn", default="rust") == "rust":
        rust_bin = _rust_binary_path("wylde-vpn")
        if rust_bin is None:
            _lc_logger.warning(
                "wylde-vpn: rust impl requested (default after Phase 2.E) but no "
                "binary found; falling back to python — build with "
                "`cargo build --release -p wylde-vpn` to engage rust"
            )
            _start_wylde_vpn_python()
            return
        _start_wylde_vpn_rust(rust_bin)
        return
    _start_wylde_vpn_python()


def _stop_wylde_vpn() -> None:
    """Take the wylde-vpn subprocess down cleanly."""
    _ds._forget_spawn("wylde-vpn")
    proc = _ds._vpn_proc
    _ds._vpn_proc = None
    if proc is None:
        return
    if proc.poll() is not None:
        return  # already exited

    _lc_logger.info("wylde-vpn: stopping (pid=%d)", proc.pid)
    try:
        if sys.platform == "win32":
            proc.send_signal(signal.CTRL_BREAK_EVENT)
        else:
            proc.terminate()
    except (OSError, ValueError):
        proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        _lc_logger.warning("wylde-vpn: didn't exit within 10s — killing")
        proc.kill()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            pass


__all__ = [
    "_start_memgraph",
    "_stop_memgraph",
    "_start_voice",
    "_stop_voice",
    "_start_device_gate",
    "_stop_device_gate",
    "_start_vram_broker",
    "_stop_vram_broker",
    "_start_extension_bridge",
    "_stop_extension_bridge",
    "_start_gateway",
    "_stop_gateway",
    "_start_wylde_ollama",
    "_stop_wylde_ollama",
    "_start_wylde_vpn",
    "_stop_wylde_vpn",
    "_start_wylde_trainer",
    "_stop_wylde_trainer",
    "_start_wylde_trainer_worker",
    "_stop_wylde_trainer_worker",
    "_start_wylde_harness",
    "_stop_wylde_harness",
    "_start_memory_scheduler",
    "_stop_memory_scheduler",
]
