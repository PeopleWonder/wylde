"""Daemon-managed service start/stop pairs.

Memgraph, Voice, extension_bridge, memory_scheduler — four
daemon-managed components. Memgraph and the memory scheduler have NO
strangler-fig impl switch (Memgraph supervises a Neo4j JVM and stays
Python-only; the scheduler is an in-process Python thread with no Rust
equivalent). Voice and extension_bridge ARE impl-dispatched via
``WYLDE_<SERVICE>_IMPL`` — Voice defaults to ``rust`` after the Phase
11.E cutover (2026-05-27), extension_bridge defaults to ``python``
pending its dogfood window. Each ``_start_X`` boots the service as a
subprocess (or in-process thread for the scheduler) and records the
spawn so orphan-detection knows about it; each ``_stop_X`` sends the
OS-appropriate graceful signal, waits, and force-kills on timeout.

Split out of :mod:`._services` so that file stays under the file-size
cap — the same move that pulled the strangler-fig helpers into
:mod:`._strangler`. The three other impl-dispatched services
(device_gate, vram_broker, gateway) stay in :mod:`._services`, which
re-exports the four pairs below so ``daemon_state`` keeps importing the
whole set from one place.

Process handles (``_voice_proc`` etc.) live on the package's
``__init__.py`` namespace so monkeypatches on the package flow through
to the read-sites here. Mutations go through ``_ds`` — the canonical
``daemon_state`` module — so writes in this submodule are visible to
:func:`Core.Lifecycle.daemon_state.stop_all_daemon_managed`.
"""

from __future__ import annotations

import os
import signal
import subprocess
import sys

from .. import daemon_state as _ds
from .._common import WYLDE_ROOT, logger as _lc_logger
from ._strangler import _impl_for, _rust_binary_path, _spawn_rust_service


# ── Memgraph ──────────────────────────────────────────────────────────


def _start_memgraph() -> None:
    """Boot Memgraph as a subprocess of the Lifecycle daemon.

    See the Phase 2c comment in :func:`Core.Lifecycle.daemon.serve_forever`
    for why this is a subprocess rather than an in-process thread.
    Tracked in module-level ``_memgraph_proc`` so :func:`_stop_memgraph`
    can take it down.

    NO-SPAWN MODE (test/parity only — see the no-spawn warning in
    :mod:`Core.Lifecycle.daemon_state`): records a "would-have-spawned"
    :class:`~Core.Lifecycle.daemon_state._NoSpawnProc` handle and forks
    nothing.
    """
    if _ds.nospawn_enabled():
        _ds._memgraph_proc = _ds._NoSpawnProc("wylde-memgraph")
        _lc_logger.info(
            "memgraph: NO-SPAWN — would-have-spawned recorded; no child forked"
        )
        return
    if _ds._memgraph_proc is not None and _ds._memgraph_proc.poll() is None:
        return  # already running

    cmd = [sys.executable, "-m", "Core.Memgraph.run"]
    env = os.environ.copy()
    env.setdefault("GRAPH_BOLT_PORT", "7687")
    env.setdefault("WYLDE_SERVICE_NAME", "wylde-memgraph")
    namespace_root = str(WYLDE_ROOT.parent)
    existing = env.get("PYTHONPATH", "")
    env["PYTHONPATH"] = (
        namespace_root + os.pathsep + existing if existing else namespace_root
    )

    creation_flags = 0
    if sys.platform == "win32":
        creation_flags = subprocess.CREATE_NEW_PROCESS_GROUP

    try:
        _ds._memgraph_proc = subprocess.Popen(
            cmd,
            cwd=str(WYLDE_ROOT),
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.STDOUT,
            stdin=subprocess.DEVNULL,
            creationflags=creation_flags,
        )
        _lc_logger.info(
            "memgraph: spawned (pid=%d) — Neo4j boot may take up to 120s",
            _ds._memgraph_proc.pid,
        )
        # No daemon-side manifest write — Memgraph's run.py owns its
        # own data/manifests/wylde-memgraph.json. Registry still filters
        # that file out of the peer-services list (Memgraph is a Core
        # constituent pipe), but the manifest itself is the service's.
        _ds._record_spawn("wylde-memgraph", _ds._memgraph_proc.pid)
    except Exception:  # noqa: BLE001
        _lc_logger.exception("daemon: memgraph spawn failed")
        _ds._memgraph_proc = None


def _stop_memgraph() -> None:
    """Take the Memgraph subprocess (and its Neo4j child) down cleanly."""
    _ds._forget_spawn("wylde-memgraph")
    proc = _ds._memgraph_proc
    _ds._memgraph_proc = None
    if proc is None:
        return
    if proc.poll() is not None:
        return  # already exited

    _lc_logger.info("memgraph: stopping (pid=%d)", proc.pid)
    try:
        if sys.platform == "win32":
            proc.send_signal(signal.CTRL_BREAK_EVENT)
        else:
            proc.terminate()
    except (OSError, ValueError):
        proc.terminate()
    try:
        proc.wait(timeout=15)
    except subprocess.TimeoutExpired:
        _lc_logger.warning("memgraph: didn't exit within 15s — killing")
        proc.kill()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            pass


# ── Voice ─────────────────────────────────────────────────────────────


def _start_voice() -> None:
    """Boot Voice as a subprocess of the Lifecycle daemon.

    Phase 11 strangler-fig: the historical Python impl (``Voice.run``,
    sounddevice + faster-whisper + kokoro pipeline) and the Rust port
    (``wylde-voice``, cpal + ort + openWakeWord) coexist. Impl is
    selected via ``WYLDE_WYLDE_VOICE_IMPL=python|rust``. Default is
    ``rust`` — Slice 11.E+ (2026-05-27) flipped it after the eight
    GUI-facing actions (``voice.toggle`` / ``voice.set_mode`` / friends)
    were ported. Both impls bind the SAME ``\\.\\pipe\\wylde-voice`` and
    accept the SAME action shape, so only one runs at a time and GUI
    routing is unchanged. ``WYLDE_WYLDE_VOICE_IMPL=python`` is the
    rollback path during the strangler-fig soak; the Python ``Voice/``
    tree stays on disk for that window. This mirrors the Rust daemon's
    ``wylde-lifecycle::services::start_voice`` (``default = Rust`` via
    ``impl_for_with_default``) so behaviour is consistent regardless of
    which daemon (``WYLDE_LIFECYCLE_IMPL``) is running.

    NO-SPAWN MODE (test/parity only — see the no-spawn warning in
    :mod:`Core.Lifecycle.daemon_state`): records a "would-have-spawned"
    handle and forks nothing.
    """
    if _ds.nospawn_enabled():
        _ds._voice_proc = _ds._NoSpawnProc(
            "wylde-voice", impl=_impl_for("wylde-voice", default="rust")
        )
        _lc_logger.info(
            "voice: NO-SPAWN — would-have-spawned recorded; no child forked"
        )
        return
    if _ds._voice_proc is not None and _ds._voice_proc.poll() is None:
        return  # already running

    impl_choice = _impl_for("wylde-voice", default="rust")
    rust_bin = _rust_binary_path("wylde-voice") if impl_choice == "rust" else None

    if impl_choice == "rust" and rust_bin is not None:
        proc = _spawn_rust_service(
            service="wylde-voice",
            rust_bin=rust_bin,
        )
        if proc is not None:
            _ds._voice_proc = proc
            _lc_logger.info("voice: spawned impl=rust pid=%d", proc.pid)
            _ds._record_spawn("wylde-voice", proc.pid, impl="rust")
            return
        _lc_logger.warning("voice: rust spawn failed; falling back to python")
    elif impl_choice == "rust":
        _lc_logger.warning(
            "voice: default impl=rust but no binary found; falling back to "
            "python (rollback path) — build with "
            "`cargo build --release -p wylde-voice` to engage rust"
        )

    # Python branch (rollback during soak; also covers the rust-fallback
    # case when no binary is on disk).
    cmd = [sys.executable, "-m", "Voice.run"]
    env = os.environ.copy()
    env.setdefault("WYLDE_SERVICE_NAME", "wylde-voice")
    namespace_root = str(WYLDE_ROOT.parent)
    existing = env.get("PYTHONPATH", "")
    env["PYTHONPATH"] = (
        namespace_root + os.pathsep + existing if existing else namespace_root
    )

    creation_flags = 0
    if sys.platform == "win32":
        creation_flags = subprocess.CREATE_NEW_PROCESS_GROUP

    try:
        _ds._voice_proc = subprocess.Popen(
            cmd,
            cwd=str(WYLDE_ROOT),
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.STDOUT,
            stdin=subprocess.DEVNULL,
            creationflags=creation_flags,
        )
        _lc_logger.info("voice: spawned impl=python pid=%d", _ds._voice_proc.pid)
        # Manifest writes + heartbeat are owned by the service itself
        # (Voice/run.py). The daemon only records the spawn intent so
        # orphan-detection can distinguish "we spawned this and it
        # vanished" from "we never tried to spawn this".
        _ds._record_spawn("wylde-voice", _ds._voice_proc.pid)
    except Exception:  # noqa: BLE001
        _lc_logger.exception("daemon: voice spawn failed")
        _ds._voice_proc = None


def _stop_voice() -> None:
    """Take the Voice subprocess down cleanly.

    The service owns its manifest, so the daemon does NOT delete it
    here — Voice's own signal handler calls ``mark_stopped`` which
    flips ``status.state`` to ``"stopped"``. Leaving the file in place
    preserves a forensic record of the last clean stop time.
    """
    _ds._forget_spawn("wylde-voice")
    proc = _ds._voice_proc
    _ds._voice_proc = None
    if proc is None:
        return
    if proc.poll() is not None:
        return

    _lc_logger.info("voice: stopping (pid=%d)", proc.pid)
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
        _lc_logger.warning("voice: didn't exit within 10s — killing")
        proc.kill()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            pass


# ── Extension bridge ──────────────────────────────────────────────────


def _start_extension_bridge() -> None:
    """Boot the extension bridge as a subprocess of the Lifecycle daemon.

    Phase 4 strangler-fig: the historical Python impl
    (``Extensions.extension_bridge.run``, in-process importlib
    dispatcher) and the Rust port (``wylde-extension-bridge``,
    MCP-server host) coexist. Impl is selected via
    ``WYLDE_WYLDE_EXTENSION_BRIDGE_IMPL=python|rust``. Default is
    ``python`` — see master plan §11 Q-E1 for the dogfood-before-flip
    rationale (extensions are user-facing, the contract change is
    large).

    NO-SPAWN MODE (test/parity only — see the no-spawn warning in
    :mod:`Core.Lifecycle.daemon_state`): records a "would-have-spawned"
    handle and forks nothing.
    """
    if _ds.nospawn_enabled():
        _ds._extension_bridge_proc = _ds._NoSpawnProc("wylde-extension-bridge")
        _lc_logger.info(
            "extension_bridge: NO-SPAWN — would-have-spawned recorded; no child forked"
        )
        return
    if (
        _ds._extension_bridge_proc is not None
        and _ds._extension_bridge_proc.poll() is None
    ):
        return  # already running

    impl_choice = _impl_for("wylde-extension-bridge")
    rust_bin = (
        _rust_binary_path("wylde-extension-bridge") if impl_choice == "rust" else None
    )

    if impl_choice == "rust" and rust_bin is not None:
        proc = _spawn_rust_service(
            service="wylde-extension-bridge",
            rust_bin=rust_bin,
        )
        if proc is not None:
            _ds._extension_bridge_proc = proc
            _lc_logger.info("extension_bridge: spawned impl=rust pid=%d", proc.pid)
            _ds._record_spawn("wylde-extension-bridge", proc.pid, impl="rust")
            return
        _lc_logger.warning(
            "extension_bridge: rust spawn failed; falling back to python"
        )
    elif impl_choice == "rust":
        _lc_logger.warning(
            "extension_bridge: WYLDE_WYLDE_EXTENSION_BRIDGE_IMPL=rust but no "
            "binary found; falling back to python"
        )

    # Python branch (default; also covers the rust-fallback case).
    cmd = [sys.executable, "-m", "Extensions.extension_bridge.run"]
    env = os.environ.copy()
    env.setdefault("WYLDE_SERVICE_NAME", "wylde-extension-bridge")
    namespace_root = str(WYLDE_ROOT.parent)
    existing = env.get("PYTHONPATH", "")
    env["PYTHONPATH"] = (
        namespace_root + os.pathsep + existing if existing else namespace_root
    )

    creation_flags = 0
    if sys.platform == "win32":
        creation_flags = subprocess.CREATE_NEW_PROCESS_GROUP

    try:
        _ds._extension_bridge_proc = subprocess.Popen(
            cmd,
            cwd=str(WYLDE_ROOT),
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.STDOUT,
            stdin=subprocess.DEVNULL,
            creationflags=creation_flags,
        )
        _lc_logger.info(
            "extension_bridge: spawned impl=python pid=%d",
            _ds._extension_bridge_proc.pid,
        )
        # The service owns its manifest + heartbeat (run.py). The daemon
        # only records the spawn intent for orphan-detection bookkeeping.
        _ds._record_spawn("wylde-extension-bridge", _ds._extension_bridge_proc.pid)
    except Exception:  # noqa: BLE001
        _lc_logger.exception("daemon: extension_bridge spawn failed")
        _ds._extension_bridge_proc = None


def _stop_extension_bridge() -> None:
    """Take the extension bridge subprocess down cleanly. Service owns
    its manifest — daemon only forgets the spawn record."""
    _ds._forget_spawn("wylde-extension-bridge")
    proc = _ds._extension_bridge_proc
    _ds._extension_bridge_proc = None
    if proc is None:
        return
    if proc.poll() is not None:
        return

    _lc_logger.info("extension_bridge: stopping (pid=%d)", proc.pid)
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
        _lc_logger.warning("extension_bridge: didn't exit within 10s — killing")
        proc.kill()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            pass


# ── Memory scheduler ──────────────────────────────────────────────────


def _start_memory_scheduler() -> None:
    """Boot the reflection + curation scheduler.

    Pulls a chat_fn from the harness's scheduler factory; if no router
    can be constructed the scheduler stays parked in skipped-only mode.

    NO-SPAWN MODE (test/parity only — see the no-spawn warning in
    :mod:`Core.Lifecycle.daemon_state`): skipped entirely. The scheduler
    is an in-process thread, not a subprocess, and the Rust daemon has no
    equivalent — leaving it unstarted keeps the "would-have-spawned" set
    identical on both sides (subprocesses only).
    """
    if _ds.nospawn_enabled():
        _lc_logger.info("memory scheduler: NO-SPAWN — skipped")
        return
    if _ds._memory_scheduler is not None:
        return
    try:
        from Core.harness.memory.scheduler import (
            MemoryScheduler,
            default_chat_fn,
        )
    except Exception:  # noqa: BLE001
        _lc_logger.exception("daemon: memory scheduler module not importable")
        return
    chat_fn = None
    try:
        chat_fn = default_chat_fn()
    except Exception:  # noqa: BLE001
        _lc_logger.exception("daemon: scheduler chat_fn factory raised")
    try:
        _ds._memory_scheduler = MemoryScheduler(chat_fn=chat_fn)
        if not _ds._memory_scheduler.start():
            _lc_logger.info("memory scheduler: not started (no chat_fn)")
            _ds._memory_scheduler = None
            return
    except Exception:  # noqa: BLE001
        _lc_logger.exception("daemon: memory scheduler start raised")
        _ds._memory_scheduler = None
        return
    # In-process; runs under the daemon's PID and is rolled up under Core
    # in the dashboard. No separate manifest — Core's constituent_pipes
    # don't include the scheduler (it has no pipe), and the daemon's
    # liveness already implies the scheduler's heartbeat.


def _stop_memory_scheduler() -> None:
    if _ds._memory_scheduler is None:
        return
    try:
        _ds._memory_scheduler.stop()
    except Exception:  # noqa: BLE001
        _lc_logger.exception("daemon: stop_memory_scheduler raised")
    _ds._memory_scheduler = None


__all__ = [
    "_start_memgraph",
    "_stop_memgraph",
    "_start_voice",
    "_stop_voice",
    "_start_extension_bridge",
    "_stop_extension_bridge",
    "_start_memory_scheduler",
    "_stop_memory_scheduler",
]
