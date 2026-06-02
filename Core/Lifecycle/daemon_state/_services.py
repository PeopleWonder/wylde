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

    Rust-only since 2026-06-02: the Python ``device_gate/`` module was
    deleted once the Rust ``wylde-device-gate`` verifier reached parity
    (it carries its own bcrypt / sha-crypt / inline-APR1 hash verification,
    no interpreter deps). There is no Python fallback — a missing Rust
    binary means device_gate simply doesn't start (the dashboard paints it
    down). ``WYLDE_WYLDE_DEVICE_GATE_IMPL`` no longer has a ``python``
    target.

    NO-SPAWN MODE (test/parity only — see the no-spawn warning in
    :mod:`Core.Lifecycle.daemon_state`): records a "would-have-spawned"
    handle and forks nothing.
    """
    if _ds.nospawn_enabled():
        _ds._device_gate_proc = _ds._NoSpawnProc("wylde-device-gate", impl="rust")
        _lc_logger.info(
            "device_gate: NO-SPAWN — would-have-spawned recorded; no child forked"
        )
        return
    if _ds._device_gate_proc is not None and _ds._device_gate_proc.poll() is None:
        return

    rust_bin = _rust_binary_path("wylde-device-gate")
    if rust_bin is None:
        _lc_logger.warning(
            "device_gate: no rust binary found; device_gate will not start — the "
            "Python device_gate module was removed, so build with "
            "`cargo build --release -p wylde-device-gate`"
        )
        _ds._device_gate_proc = None
        return
    _start_device_gate_rust(rust_bin)


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

    Rust-only since 2026-06-02: the Python ``Core/resource_monitor/``
    package was deleted once the Rust ``wylde-vram-broker`` passed a live
    function test. Only the Rust broker implements the Phase-0.5 estimator
    and DRAM spillover (the Python broker rejected a reserve with no
    ``bytes`` and could not admit a model larger than VRAM), so the Rust
    binary is the sole impl. There is no Python fallback — a missing Rust
    binary means the broker simply doesn't start.
    ``WYLDE_WYLDE_VRAM_BROKER_IMPL`` no longer has a ``python`` target.

    NO-SPAWN MODE (test/parity only — see the no-spawn warning in
    :mod:`Core.Lifecycle.daemon_state`): records a "would-have-spawned"
    handle and forks nothing.
    """
    if _ds.nospawn_enabled():
        _ds._vram_broker_proc = _ds._NoSpawnProc("wylde-vram-broker", impl="rust")
        _lc_logger.info(
            "vram_broker: NO-SPAWN — would-have-spawned recorded; no child forked"
        )
        return
    if _ds._vram_broker_proc is not None and _ds._vram_broker_proc.poll() is None:
        return  # already running

    rust_bin = _rust_binary_path("wylde-vram-broker")
    if rust_bin is None:
        _lc_logger.warning(
            "vram_broker: no rust binary found; vram_broker will not start — the "
            "Python Core/resource_monitor package was removed, so build with "
            "`cargo build --release -p wylde-vram-broker`"
        )
        _ds._vram_broker_proc = None
        return
    _start_vram_broker_rust(rust_bin)


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

    Rust-only since 2026-06-02: the Python ``Gateway/`` package was
    deleted once the Rust ``wylde-gateway`` (axum) — a superset of the
    Python routes — became the canonical ingress/egress server. There is
    no Python fallback — a missing Rust binary means the Gateway simply
    doesn't start (the dashboard paints it down).
    ``WYLDE_WYLDE_GATEWAY_IMPL`` no longer has a ``python`` target.

    Depends on the harness pipe and device_gate already being up — the
    daemon orders the spawns accordingly in
    :func:`Core.Lifecycle.daemon.serve_forever`.

    NO-SPAWN MODE (test/parity only — see the no-spawn warning in
    :mod:`Core.Lifecycle.daemon_state`): records a "would-have-spawned"
    handle and forks nothing.
    """
    if _ds.nospawn_enabled():
        _ds._gateway_proc = _ds._NoSpawnProc("wylde-gateway", impl="rust")
        _lc_logger.info(
            "gateway: NO-SPAWN — would-have-spawned recorded; no child forked"
        )
        return
    if _ds._gateway_proc is not None and _ds._gateway_proc.poll() is None:
        return  # already running

    rust_bin = _rust_binary_path("wylde-gateway")
    if rust_bin is None:
        _lc_logger.warning(
            "gateway: no rust binary found; gateway will not start — the Python "
            "Gateway package was removed, so build with "
            "`cargo build --release -p wylde-gateway`"
        )
        _ds._gateway_proc = None
        return
    _start_gateway_rust(rust_bin)


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

    Rust-only since 2026-06-02: the Python ``VPN/run.py`` Flask service
    was deleted once the shared pipe server gained an HTTP route-table
    adapter and ``wylde-vpn`` wired its ``GET /api/link/*`` routes onto
    it (route-table parity). There is no Python fallback — a missing
    Rust binary means VPN simply doesn't start (the dashboard paints it
    down). ``WYLDE_WYLDE_VPN_IMPL`` no longer has a ``python`` target.

    NO-SPAWN MODE (test/parity only — see the no-spawn warning in
    :mod:`Core.Lifecycle.daemon_state`): records a "would-have-spawned"
    handle and forks nothing.
    """
    if _ds.nospawn_enabled():
        _ds._vpn_proc = _ds._NoSpawnProc("wylde-vpn", impl="rust")
        _lc_logger.info(
            "wylde-vpn: NO-SPAWN — would-have-spawned recorded; no child forked"
        )
        return
    if _ds._vpn_proc is not None and _ds._vpn_proc.poll() is None:
        return  # already running

    rust_bin = _rust_binary_path("wylde-vpn")
    if rust_bin is None:
        _lc_logger.warning(
            "wylde-vpn: no rust binary found; VPN will not start — the Python "
            "VPN service was removed, so build with "
            "`cargo build --release -p wylde-vpn`"
        )
        _ds._vpn_proc = None
        return
    _start_wylde_vpn_rust(rust_bin)


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
