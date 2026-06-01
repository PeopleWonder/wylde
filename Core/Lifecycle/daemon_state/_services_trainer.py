"""Trainer + Trainer-worker start/stop pairs.

Split out of :mod:`._services` to keep that file under the 700-line cap.
The trainer pair drives Phase 3 of the Rust migration — ``wylde-trainer``
is the public pipe surface for the Caption sub-service; ``wylde-trainer-worker``
is the Python inference engine the trainer talks to over its own pipe.

Lifecycle topology in ``rust`` mode (``WYLDE_WYLDE_TRAINER_IMPL=rust``):

  * Daemon spawns ``wylde-trainer-worker`` (Python child running
    ``Trainer/Caption/rust_worker.py``) on ``\\\\.\\pipe\\wylde-trainer-worker``.
  * Daemon spawns ``wylde-trainer`` (Rust binary) on
    ``\\\\.\\pipe\\wylde-trainer``.
  * Action requests on the public pipe forward to the worker pipe via
    ``wylde_shared::ipc::call``.

In ``python`` mode the daemon spawns neither — the existing in-process
``Trainer.Caption.run`` API continues to serve direct callers.

Process spawning lives in the daemon by policy (the
``no_external_process_spawn_rust`` lint rule pins it to ``wylde-lifecycle``);
that's why the worker is a daemon-managed service rather than a child
of ``wylde-trainer``.
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


# ── wylde-trainer-worker (Python inference engine, pipe service) ──────


def _start_wylde_trainer_worker() -> None:
    """Boot the Python inference worker for wylde-trainer.

    Spawned only when ``WYLDE_WYLDE_TRAINER_IMPL=rust`` — in python mode
    Caption stays in-process and the worker isn't needed. The worker
    hosts ``\\\\.\\pipe\\wylde-trainer-worker`` and is the place where
    Florence-2 weights are actually loaded.
    """
    if _ds.nospawn_enabled():
        _ds._trainer_worker_proc = _ds._NoSpawnProc(
            "wylde-trainer-worker", impl="python"
        )
        _lc_logger.info(
            "wylde-trainer-worker: NO-SPAWN — would-have-spawned recorded; "
            "no child forked"
        )
        return
    if _ds._trainer_worker_proc is not None and _ds._trainer_worker_proc.poll() is None:
        return  # already running

    if _impl_for("wylde-trainer") != "rust":
        _lc_logger.info(
            "wylde-trainer-worker: skipped (WYLDE_WYLDE_TRAINER_IMPL=python); "
            "Caption stays in-process"
        )
        return

    cmd = [sys.executable, str(WYLDE_ROOT / "Trainer" / "Caption" / "rust_worker.py")]
    env = os.environ.copy()
    env.setdefault("WYLDE_SERVICE_NAME", "wylde-trainer-worker")
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
        _ds._trainer_worker_proc = subprocess.Popen(
            cmd,
            cwd=str(WYLDE_ROOT),
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.STDOUT,
            stdin=subprocess.DEVNULL,
            creationflags=creation_flags,
        )
        _ds._record_spawn(
            "wylde-trainer-worker",
            _ds._trainer_worker_proc.pid,
            impl="python",
        )
        _lc_logger.info(
            "daemon: spawned wylde-trainer-worker impl=python pid=%d",
            _ds._trainer_worker_proc.pid,
        )
    except Exception:  # noqa: BLE001
        _lc_logger.exception("daemon: wylde-trainer-worker spawn failed")
        _ds._trainer_worker_proc = None


def _stop_wylde_trainer_worker() -> None:
    """Take the trainer worker subprocess down cleanly."""
    _ds._forget_spawn("wylde-trainer-worker")
    proc = _ds._trainer_worker_proc
    _ds._trainer_worker_proc = None
    if proc is None:
        return
    if proc.poll() is not None:
        return  # already exited

    _lc_logger.info("wylde-trainer-worker: stopping (pid=%d)", proc.pid)
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
        _lc_logger.warning("wylde-trainer-worker: didn't exit within 15s — killing")
        proc.kill()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            pass


# ── wylde-trainer (Rust pipe surface) ─────────────────────────────────


def _start_wylde_trainer_python() -> None:
    """Python path: Caption stays in-process.

    No subprocess is spawned. The existing in-process API
    (``Trainer.Caption.run.get_captioner``) and the
    ``Trainer/Caption/tools/`` direct-call tools continue to work
    unchanged. Set ``WYLDE_WYLDE_TRAINER_IMPL=rust`` to get a pipe
    surface managed by the daemon.
    """
    _lc_logger.info(
        "wylde-trainer: in-process mode (WYLDE_WYLDE_TRAINER_IMPL=python); "
        "daemon does not spawn a Caption subprocess. Existing in-process "
        "callers continue to work."
    )


def _start_wylde_trainer_rust(rust_bin: Path) -> None:
    """Boot the Rust wylde-trainer binary at ``rust_bin``."""
    proc = _spawn_rust_service(service="wylde-trainer", rust_bin=rust_bin)
    if proc is None:
        _ds._trainer_proc = None
        return
    _ds._trainer_proc = proc
    _ds._record_spawn("wylde-trainer", proc.pid, impl="rust")
    _lc_logger.info(
        "daemon: spawned wylde-trainer impl=rust binary=%s pid=%d",
        rust_bin,
        proc.pid,
    )


def _start_wylde_trainer() -> None:
    """Boot wylde-trainer as a subprocess of the Lifecycle daemon.

    Dispatches to either the no-op in-process branch or a Rust binary
    depending on ``WYLDE_WYLDE_TRAINER_IMPL`` (default ``python`` =
    in-process; Caption stays where it is). Falls back to in-process
    with a warning if ``rust`` is requested but no binary is found.

    The caller should already have started ``wylde-trainer-worker``
    in rust mode — the Rust binary cold-starts faster than the Python
    worker, so wiring worker-first in :func:`Core.Lifecycle.daemon.serve_forever`
    avoids a race where the trainer's first inference call lands before
    the worker pipe is bound.

    NO-SPAWN MODE (test/parity only — see the no-spawn warning in
    :mod:`Core.Lifecycle.daemon_state`): records a "would-have-spawned"
    handle and forks nothing.
    """
    if _ds.nospawn_enabled():
        _ds._trainer_proc = _ds._NoSpawnProc(
            "wylde-trainer", impl=_impl_for("wylde-trainer")
        )
        _lc_logger.info(
            "wylde-trainer: NO-SPAWN — would-have-spawned recorded; no child forked"
        )
        return
    if _ds._trainer_proc is not None and _ds._trainer_proc.poll() is None:
        return  # already running

    if _impl_for("wylde-trainer") == "rust":
        rust_bin = _rust_binary_path("wylde-trainer")
        if rust_bin is None:
            _lc_logger.warning(
                "wylde-trainer: WYLDE_WYLDE_TRAINER_IMPL=rust but no binary found; "
                "falling back to in-process python (Caption stays in-process)"
            )
            _start_wylde_trainer_python()
            return
        _start_wylde_trainer_rust(rust_bin)
        return
    _start_wylde_trainer_python()


def _stop_wylde_trainer() -> None:
    """Take the wylde-trainer subprocess down cleanly.

    No-op when running on the python (in-process) branch — there's no
    daemon-managed child to stop.
    """
    _ds._forget_spawn("wylde-trainer")
    proc = _ds._trainer_proc
    _ds._trainer_proc = None
    if proc is None:
        return
    if proc.poll() is not None:
        return  # already exited

    _lc_logger.info("wylde-trainer: stopping (pid=%d)", proc.pid)
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
        _lc_logger.warning("wylde-trainer: didn't exit within 10s — killing")
        proc.kill()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            pass


__all__ = [
    "_start_wylde_trainer",
    "_stop_wylde_trainer",
    "_start_wylde_trainer_worker",
    "_stop_wylde_trainer_worker",
]
