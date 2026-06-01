r"""Long-running Lifecycle daemon.

Runs the boot-time launcher once, then stays alive serving the
``\\.\pipe\wylde-lifecycle`` named pipe so the GUI (and anything else
on the local box) can drive start/stop/wake/list/health calls without
going through the Gateway.

Run standalone::

    py -3 -m Core.Lifecycle.daemon

Boot-only mode (the historical behaviour of ``launcher.main``) stays
available via ``py -3 -m Core.Lifecycle.launcher`` — that path doesn't
expose the pipe and exits after spawning children.

No-spawn mode
~~~~~~~~~~~~~
``WYLDE_LIFECYCLE_NOSPAWN=1`` (or the ``--no-spawn`` CLI flag) brings the
daemon's control + manifest surfaces up WITHOUT forking the tier=core
children — see the no-spawn warning in :mod:`Core.Lifecycle.daemon_state`.
It exists only so the cross-language parity suite can exercise the
control surface. **It must never be enabled in production.**

``WYLDE_LIFECYCLE_PIPE_NAME`` overrides the service name the daemon binds
its pipe / identifies as (default ``wylde-lifecycle`` →
``\\.\pipe\wylde-lifecycle``). The parity suite sets it to
``wylde-lifecycle-parity-py`` so a parity run gets an isolated pipe and
never collides with a production daemon on the canonical pipe. Like
no-spawn, this is **test/parity only** — never set it in production.
"""

from __future__ import annotations

import logging
import os
import signal
import sys
import threading
from typing import Any

from . import control, daemon_state, discovery, launcher, shutdown
from ._common import logger as _lc_logger
from .daemon_state import (
    _start_memgraph,
    _start_voice,
    _start_device_gate,
    _start_extension_bridge,
    _start_gateway,
    _start_vram_broker,
    _start_wylde_harness,
    _start_wylde_ollama,
    _start_wylde_trainer,
    _start_wylde_trainer_worker,
    _start_memory_scheduler,
    register_core_manifest,
    start_orphan_sweep,
    stop_all_daemon_managed,
)


def _setup_logging(level: int = logging.INFO) -> None:
    from Core.shared.logging_setup import configure_logging

    configure_logging(level=level)


def _resolve_pipe_service_name() -> str:
    r"""Resolve the service name the daemon binds its pipe / identifies as.

    Defaults to ``wylde-lifecycle`` — pipe ``\\.\pipe\wylde-lifecycle``.
    ``WYLDE_LIFECYCLE_PIPE_NAME`` overrides it; the cross-language parity
    suite sets ``wylde-lifecycle-parity-py`` so the parity Python and Rust
    daemons run on isolated pipes and never collide with a production
    daemon on the canonical pipe. **Test/parity only — never set this in
    production.**
    """
    override = os.environ.get("WYLDE_LIFECYCLE_PIPE_NAME", "").strip()
    return override or "wylde-lifecycle"


def _build_stub_app() -> Any:
    """Return a minimal Flask app with no routes.

    The pipe server in ``Core.shared.ipc`` requires *some* WSGI app for
    its non-action dispatch path — it falls through to the Flask test
    client when an envelope doesn't carry the ``/__action__`` magic
    method. The lifecycle pipe is action-only; that fallback should
    never fire, but a no-route app is the safe shape if it does.

    Flask is imported lazily so unit tests can exercise control.py
    without pulling in werkzeug.
    """
    from flask import Flask

    app = Flask("wylde-lifecycle")

    @app.route("/health", methods=["GET"])
    def _health() -> Any:  # pragma: no cover - smoke surface
        return {"ok": True, "service": "wylde-lifecycle"}

    return app


def _install_signal_handlers(stop_event: threading.Event) -> None:
    """Translate SIGINT / SIGTERM into a clean exit.

    The pipe accept loop is a daemon thread, so it doesn't block the
    process from terminating once the main thread returns. We also drain
    the launcher's tracked children via ``shutdown.shutdown_all`` so
    services get the same graceful → kill sequence they would on a
    boot-launcher exit.
    """

    def _handler(_signum: int, _frame: Any) -> None:
        _lc_logger.info("daemon: signal received, draining children")
        try:
            shutdown.shutdown_all()
        except Exception:  # noqa: BLE001
            _lc_logger.exception("daemon: shutdown_all raised")
        # The action-driven path (service.shutdown_all) and the signal
        # path now share this teardown function so both routes leave
        # exactly the same surface behind.
        try:
            stop_all_daemon_managed()
        except Exception:  # noqa: BLE001
            _lc_logger.exception("daemon: stop_all_daemon_managed raised")
        stop_event.set()

    for sig in (signal.SIGINT, signal.SIGTERM):
        try:
            signal.signal(sig, _handler)
        except (ValueError, OSError):
            # Non-main thread or unsupported signal — best-effort.
            pass


def serve_forever() -> int:
    """Boot the daemon, register actions, serve the pipe, block.

    Return code matches POSIX conventions (0 = clean exit).

    Honours no-spawn mode (``WYLDE_LIFECYCLE_NOSPAWN`` / ``--no-spawn``):
    when set, discovery, the launcher, the harness pipe and the orphan
    sweep are all skipped and ``_start_<service>`` forks nothing. TEST/
    PARITY ONLY — see the module docstring and :mod:`Core.Lifecycle.daemon_state`.
    """
    _setup_logging()
    _lc_logger.info("daemon: booting Lifecycle controller")

    # Service name the pipe surface binds / identifies as. Normally
    # ``wylde-lifecycle``; the parity suite overrides it via
    # WYLDE_LIFECYCLE_PIPE_NAME so its daemons get isolated pipes.
    service_name = _resolve_pipe_service_name()

    # No-spawn mode (test/parity only). When set, the control + manifest
    # surfaces still come up but every ``_start_<service>`` short-circuits
    # to a "would-have-spawned" record instead of forking a child. See the
    # loud warning in :mod:`Core.Lifecycle.daemon_state`.
    nospawn = daemon_state.detect_nospawn()
    daemon_state.set_nospawn(nospawn)
    if nospawn:
        _lc_logger.warning(
            "daemon: NO-SPAWN MODE ACTIVE — control + manifest surfaces will "
            "come up but NO tier=core children will be forked. This mode is "
            "for testing/parity ONLY and must never run in production."
        )

    # Phase 0 — filesystem-as-registry discovery. Picks up new service
    # folders, drops missing ones, and updates Network/services.yaml.
    # Integration runs in a background thread; the fast path is ~5ms
    # when nothing changed. Skipped under no-spawn: discovery rewrites
    # Network/services.yaml, an on-disk side effect a parity run avoids.
    if not nospawn:
        try:
            discovery.discover()
        except Exception:  # noqa: BLE001
            _lc_logger.exception("daemon: discover() raised — continuing")

    # Phase 1 — spawn enabled services. Failures here don't take down the
    # daemon: a partial bring-up is still useful, and the user can drive
    # the missing ones via the GUI start action. Skipped under no-spawn —
    # the launcher's whole job is forking children.
    if not nospawn:
        try:
            launcher.launch_all()
        except Exception:  # noqa: BLE001
            _lc_logger.exception("daemon: launch_all raised — continuing")

    # Phase 2 — register handlers on the action dispatcher and start the
    # pipe server. Both sides are no-ops on non-Windows / missing pywin32;
    # the daemon still serves the rest of its lifecycle by sitting in the
    # main loop, useful for dev on Linux.
    try:
        control.register_with_ipc()
    except Exception:  # noqa: BLE001
        _lc_logger.exception("daemon: register_with_ipc failed")

    try:
        from Core.shared import ipc

        ipc.serve_forever_background(service_name, _build_stub_app())
    except Exception:  # noqa: BLE001
        _lc_logger.exception(
            "daemon: serve_forever_background failed — running headless"
        )

    # Phase 2b — start the harness pipe (\\.\pipe\wylde-harness). The
    # harness is core infrastructure (chat-turn driver) but doesn't get
    # picked up by service discovery because it lives under Core/. We
    # start it in-process here so the chat surface comes up with the
    # daemon. Failure is non-fatal — pipe-only callers will see
    # `pipe_unavailable`; the rest of Wylde keeps working.
    #
    # Skipped under no-spawn: the harness pipe is part of the tier=core
    # surface a parity run avoids, and the Rust daemon never brings it up
    # either — leaving it off keeps the two daemons symmetric.
    if not nospawn:
        try:
            from Core.harness.server import start as start_harness

            if not start_harness():
                _lc_logger.info("harness pipe: not started (see prior warnings)")
        except Exception:  # noqa: BLE001
            _lc_logger.exception("daemon: harness pipe startup raised")

    # Publish Core's runtime manifest (data/manifests/core.json) — one
    # entry that rolls up lifecycle, harness, memgraph and memory
    # scheduler. Registry probes constituent pipes live; this manifest
    # carries pid / started_at / heartbeat. Also clears any stale
    # per-pipe manifest files from prior daemon versions.
    #
    # Skipped under no-spawn: core.json is host-wide shared state. Writing
    # (and, at shutdown, deleting) it would clobber a production daemon's
    # manifest — so a parity run stays runnable while the real stack is up.
    if not nospawn:
        try:
            register_core_manifest()
        except Exception:  # noqa: BLE001
            _lc_logger.exception("daemon: register_core_manifest raised")

    # ── tier=core start sequence (deliberately explicit) ──────────────
    #
    # Which services EXIST is manifest-driven: discovery walks the
    # filesystem and `launcher.launch_all` (above) spawns the enabled
    # *standard*-tier services purely from their manifests — no hardcoded
    # roster (the `launcher_enumerates_services_from_manifests` rule
    # guards that). The tier=core START ORDER below, by contrast, is an
    # explicit hand-ordered sequence ON PURPOSE: each `_start_<name>` has
    # bespoke bring-up logic that a generic manifest loop can't express —
    # Memgraph forks a Neo4j JVM, the harness can run in-process, several
    # services flip Python⇄Rust on a per-service `WYLDE_*_IMPL` env var,
    # and the ordering encodes hard runtime dependencies (broker before
    # the GPU services that lease VRAM from it; extension_bridge + the
    # device gate before Gateway). Collapsing this into a data-driven
    # loop would trade documented, debuggable startup for a fragile
    # rewrite of soak-sensitive paths (Memgraph + Voice). The manifests
    # still describe these services (for discovery, shutdown ordering,
    # and the GUI); the daemon just owns their bring-up choreography.
    #
    # Phase 2c — start Memgraph as a subprocess.
    #
    # Memgraph doesn't fit the in-process pattern: Core/Memgraph/run.py
    # spawns a Neo4j JVM child via subprocess, calls sys.path.insert at
    # import time (line 32 / line 214), installs its own SIGINT handler,
    # and ends in a blocking graph_service.main() loop. Pulling that
    # into the daemon's process would mix signal handlers, pollute
    # sys.path, and tie the JVM's lifecycle to the daemon.
    #
    # Subprocess instead. `py -3 -m Core.Memgraph.run` boots Neo4j and
    # serves \\.\pipe\wylde-memgraph; we track the Popen so the daemon
    # can take it down with the rest of the stack at shutdown.
    _start_memgraph()

    # Phase 2d — start the memory scheduler. Reflection + curation
    # cycles run on idle so chat-turn latency stays out of the way.
    # Failure to start is non-fatal: the scheduler logs and the rest
    # of Wylde keeps working without it.
    _start_memory_scheduler()

    # Phase 2d.5 — start the VRAM broker. Fourth Core constituent
    # (\\.\pipe\wylde-vram-broker). Boots before Voice / device_gate
    # so that GPU-bound services can request leases as soon as they
    # come up, rather than racing the broker's pipe accept loop.
    _start_vram_broker()

    # Phase 2e — start the Voice service as a subprocess. Voice owns
    # mic/speaker I/O and the orchestration loop; it talks to the
    # harness through \\.\pipe\wylde-harness and exposes its own
    # action surface on \\.\pipe\wylde-voice.
    _start_voice()

    # Phase 2f — start device_gate as a subprocess. device_gate owns
    # device pairing, token issuance, and the three permission tiers;
    # Gateway calls device_gate.verify on every external request.
    _start_device_gate()

    # Phase 2f.5 — start the extension bridge as a subprocess. It hosts
    # \\.\pipe\wylde-extension-bridge — the external surface to the
    # in-process extension dispatcher. Gateway dispatches browser-
    # extension calls (/extensions/<name>/<endpoint>) through it, so it
    # spawns before Gateway.
    _start_extension_bridge()

    # Phase 2f.6 — start wylde-ollama. Greenfield Rust binary at
    # `rust/target/release/wylde-ollama.exe`. Owns \\.\pipe\wylde-ollama
    # and is the single inference surface for the local Ollama daemon.
    # Spawns after the broker (depends on it for VRAM leases) and before
    # Gateway / harness (which call into it for chat/embed).
    _start_wylde_ollama()

    # Phase 2f.7 — start the trainer worker BEFORE the trainer binary so
    # its pipe is bound before the first inference call lands. In python
    # (in-process) mode the worker is skipped; in rust mode the daemon
    # spawns `python Trainer/Caption/rust_worker.py` which exposes
    # \\.\pipe\wylde-trainer-worker — the inference engine that hosts
    # the lazy Florence-2 captioner.
    _start_wylde_trainer_worker()
    # Phase 2f.8 — start wylde-trainer (Caption sub-service pipe surface).
    # Default WYLDE_WYLDE_TRAINER_IMPL=python keeps Caption in-process
    # (no subprocess spawned). Set to `rust` to launch the Rust binary at
    # `rust/target/release/wylde-trainer.exe` fronting Florence-2 over
    # \\.\pipe\wylde-trainer — it forwards inference to
    # \\.\pipe\wylde-trainer-worker.
    _start_wylde_trainer()

    # Phase 2g — start the Gateway as a subprocess. Gateway is the
    # outward-facing FastAPI ingress on 127.0.0.1:8005 — every external
    # HTTP request (browser extensions, mobile devices via VPN) lands
    # here first. It depends on the harness pipe + device_gate already
    # being up, so it spawns last among the daemon-managed services.
    _start_gateway()

    # Phase 5 — wylde-harness (consolidated chat-turn driver). Default
    # impl is ``python`` (in-process driver inside the Python
    # wylde-harness service; this call is a no-op). Flip with
    # ``WYLDE_WYLDE_HARNESS_IMPL=rust`` to dogfood the chat.* surface
    # (5.B streaming included) over ``\\.\pipe\wylde-harness``.
    _start_wylde_harness()

    # Phase 2h — start the orphan-detection sweep. Each service owns
    # its own manifest now; this background tick is the daemon's
    # safety net for processes that crash without calling
    # mark_stopped (e.g. kill -9, segfault, OOM). It walks
    # data/manifests/*.json every 60s and flips any alive-marked
    # manifest with a dead pid to dead-orphan.
    #
    # Skipped under no-spawn: there are no real children to orphan, and
    # the sweep would otherwise rewrite unrelated manifests on the host.
    if not nospawn:
        try:
            start_orphan_sweep()
        except Exception:  # noqa: BLE001
            _lc_logger.exception("daemon: start_orphan_sweep raised — continuing")

    # Phase 3 — block forever. SIGINT / SIGTERM unblock via the
    # handler; the service.shutdown_all action unblocks via
    # daemon_state.request_daemon_exit (which sets the same event
    # after a brief delay so the action's response can flush first).
    stop = threading.Event()
    _install_signal_handlers(stop)
    daemon_state.register_stop_event(stop)
    pipe_suffix = service_name.removeprefix("wylde-")
    _lc_logger.info("daemon: ready (\\\\.\\pipe\\wylde-%s)", pipe_suffix)
    while not stop.is_set():
        stop.wait(timeout=1.0)

    _lc_logger.info("daemon: exit")
    return 0


def main() -> int:
    return serve_forever()


if __name__ == "__main__":
    sys.exit(main())
