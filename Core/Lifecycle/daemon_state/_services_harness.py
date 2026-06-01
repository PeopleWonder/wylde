"""wylde-harness start/stop pair.

Split out of :mod:`._services` to keep that file under the 700-line cap.
The harness pair drives Phase 5 of the Rust migration — the consolidated
chat-turn / tooling / memory driver crate. Slice 5.D (2026-05-25)
flipped the strangler-fig default from ``python`` to ``rust`` after
byte-level parity coverage landed for the salvage parser,
``_call_hash``, and ``_find_balanced_braces`` — the pure functions
whose port fidelity is load-bearing for the dispatch loop
(``rust/tests/parity/tests/harness_turn.rs``, 25 cases, all green).
The Python driver inside ``Core/harness/turn/`` stays on disk for
one release cycle as the rollback path; set
``WYLDE_WYLDE_HARNESS_IMPL=python`` to revert.

Lifecycle topology in ``rust`` mode (the default after slice 5.D):

  * Daemon spawns ``wylde-harness`` (Rust binary) on
    ``\\\\.\\pipe\\wylde-harness``.
  * The Python harness pipe handler (``Core/harness/pipe/_chat.py``)
    reads ``WYLDE_HARNESS_IMPL`` (default ``rust`` post-5.D) and
    forwards ``chat.run_turn`` — plus, once 5.B streaming parity is
    broad, the four streaming actions — to the Rust pipe at the same
    name.

In ``python`` mode the daemon spawns nothing — the existing
in-process driver inside the Python harness service continues to
serve all five chat.* actions as before.

History
-------

Slice 5.A shipped as a standalone ``wylde-harness-turn`` crate /
binary / pipe. the Wylde user's 2026-05-24 architectural call consolidated it
back into a unified ``wylde-harness`` crate with submodules (turn,
tooling, memory). This file is the post-consolidation start/stop pair;
the old ``_services_harness_turn.py`` is removed by the same change.
"""

from __future__ import annotations

import signal
import subprocess
import sys

from .. import daemon_state as _ds
from .._common import logger as _lc_logger
from ._strangler import _impl_for, _rust_binary_path, _spawn_rust_service


def _start_wylde_harness() -> None:
    """Boot wylde-harness as a subprocess of the Lifecycle daemon.

    Slice 5.D (2026-05-25) flipped the strangler-fig default from
    ``python`` to ``rust`` after byte-level parity coverage landed.
    By default this function spawns the Rust ``wylde-harness.exe``
    binary, which exposes the full chat.* surface (chat.run_turn /
    start_turn / cancel / stream_turn / stream_tools) over
    ``\\\\.\\pipe\\wylde-harness``. The Python harness pipe's
    strangler at ``Core/harness/pipe/_chat.py`` forwards the relevant
    actions there.

    ``WYLDE_WYLDE_HARNESS_IMPL=python`` reverts to the in-process
    Python driver inside the existing Python harness service. In that
    mode there is NO daemon-managed subprocess; this function is a
    no-op. Falls back to Python with a warning if the Rust binary is
    missing.

    NO-SPAWN MODE: records a "would-have-spawned" handle and forks
    nothing, regardless of impl.
    """
    if _ds.nospawn_enabled():
        impl_lang = _impl_for("wylde-harness", default="rust")
        _ds._harness_proc = _ds._NoSpawnProc("wylde-harness", impl=impl_lang)
        _lc_logger.info(
            "wylde-harness: NO-SPAWN — would-have-spawned recorded; no child forked"
        )
        return
    if _ds._harness_proc is not None and _ds._harness_proc.poll() is None:
        return  # already running

    if _impl_for("wylde-harness", default="rust") == "python":
        # Python "impl" = in-process driver inside the existing Python
        # wylde-harness service. No separate subprocess to spawn.
        _lc_logger.info(
            "wylde-harness: WYLDE_WYLDE_HARNESS_IMPL=python — chat driver "
            "stays in-process on the Python harness; no daemon-managed subprocess"
        )
        return

    rust_bin = _rust_binary_path("wylde-harness")
    if rust_bin is None:
        _lc_logger.warning(
            "wylde-harness: rust impl requested (default after slice 5.D) "
            "but no binary found (checked WYLDE_WYLDE_HARNESS_BIN, rust/bin/, "
            "rust/target/release/, rust/target/debug/); falling back to Python "
            "in-process driver — build with `cargo build --release -p wylde-harness`"
        )
        return

    proc = _spawn_rust_service(service="wylde-harness", rust_bin=rust_bin)
    if proc is None:
        _ds._harness_proc = None
        return
    _ds._harness_proc = proc
    _ds._record_spawn("wylde-harness", proc.pid, impl="rust")
    _lc_logger.info(
        "daemon: spawned wylde-harness impl=rust binary=%s pid=%d",
        rust_bin,
        proc.pid,
    )


def _stop_wylde_harness() -> None:
    """Take the wylde-harness subprocess down cleanly."""
    _ds._forget_spawn("wylde-harness")
    proc = _ds._harness_proc
    _ds._harness_proc = None
    if proc is None:
        return
    if proc.poll() is not None:
        return  # already exited

    _lc_logger.info("wylde-harness: stopping (pid=%d)", proc.pid)
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
        _lc_logger.warning("wylde-harness: didn't exit within 10s — killing")
        proc.kill()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            pass
