#!/usr/bin/env python3
"""Memgraph service entry point — supervises the bundled Neo4j JVM.

Memgraph is Wylde's relational-data layer, backed by a bundled Neo4j
JVM child.  This module is the process wrapper the Lifecycle daemon
spawns via ``py -3 -m Core.Memgraph.run``: it brings up the
``vendor/neo4j`` server as a managed ``subprocess.Popen`` so Neo4j
stays grouped under the wylde-memgraph process in Task Manager (and
dies when this process exits), waits for it to accept Bolt
connections on ``127.0.0.1:7687``, then blocks in a minimal supervisor
loop (:func:`_serve_until_shutdown`) until a shutdown signal arrives.

The legacy named-pipe surface (the old ``graph_service`` Flask app) was
retired in the 2026-05-26 direct-Bolt cutover — the harness now reads and
writes the graph over Bolt (neo4rs) directly, so nothing consumes the pipe
anymore. This process exists solely to own the Neo4j JVM lifecycle.

On shutdown the wrapper gracefully terminates the Neo4j child.
Daemon-launched in production; safe to run standalone for development
with the same module path from the Wylde root.

Service owns its manifest.  Memgraph is a Core constituent pipe — the
registry filters it out of the peer-services list (Core's rollup
manifest covers it), but the per-service manifest is still written
here so the orphan-detection sweep in the Lifecycle daemon can
observe Memgraph's pid + heartbeat alongside everything else.
"""

from __future__ import annotations

import atexit
import logging
import os
import signal
import socket
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import Any, Optional

HERE = Path(__file__).parent.resolve()
sys.path.insert(0, str(HERE))

LOGS_DIR = HERE / "logs"
LOGS_DIR.mkdir(parents=True, exist_ok=True)

try:
    from Core.shared.logging_setup import configure_logging
except ImportError:
    # Memgraph runs as a subprocess that bootstraps its own sys.path; if
    # Core.shared isn't on the path yet, define a no-op so the rest of
    # the module continues to import without configuring root logging.
    def configure_logging(service: str | None = None, **_: object) -> None:  # type: ignore[misc]
        return None


try:
    from Core.shared.manifest import (
        mark_stopped,
        start_heartbeat,
        write_manifest,
    )
except ImportError:
    # Same fallback rationale as configure_logging — when Core.shared isn't
    # importable yet, treat the manifest writes as no-ops so the rest of
    # Memgraph startup continues.
    def write_manifest(*args: Any, **kwargs: Any) -> None:  # type: ignore[misc]
        return None

    def start_heartbeat(*args: Any, **kwargs: Any) -> None:  # type: ignore[misc]
        return None

    def mark_stopped(*args: Any, **kwargs: Any) -> None:  # type: ignore[misc]
        return None


SERVICE_NAME = "wylde-memgraph"

configure_logging(service=SERVICE_NAME)

logger = logging.getLogger(__name__)

_BOLT_HOST = "127.0.0.1"
_BOLT_PORT = int(os.getenv("GRAPH_BOLT_PORT", "7687"))
_READY_WAIT_S = int(os.getenv("GRAPH_READY_WAIT_S", "120"))

_NEO4J_DIR = HERE / "vendor" / "neo4j"
_JDK_DIR = HERE / "vendor" / "jdk"
_NEO4J_BAT = _NEO4J_DIR / "bin" / "neo4j.bat"

_CREATE_NO_WINDOW = 0x08000000

_neo4j_proc: Optional[subprocess.Popen] = None

# Flipped by the signal handler to release the supervisor loop in
# :func:`_serve_until_shutdown` so ``main`` can run its teardown and exit.
_shutdown_event = threading.Event()


def _bolt_ready() -> bool:
    try:
        s = socket.create_connection((_BOLT_HOST, _BOLT_PORT), timeout=1.0)
        s.close()
        return True
    except OSError:
        return False


def _wait_for_neo4j(timeout: float = _READY_WAIT_S) -> bool:
    deadline = time.monotonic() + timeout
    interval = 1.0
    while time.monotonic() < deadline:
        if _bolt_ready():
            return True
        time.sleep(interval)
        interval = min(interval * 1.5, 5.0)
    return False


def _spawn_neo4j() -> None:
    global _neo4j_proc
    if _bolt_ready():
        logger.info(
            "Neo4j already up on bolt://%s:%d (external instance), skipping spawn",
            _BOLT_HOST,
            _BOLT_PORT,
        )
        return
    if not _NEO4J_BAT.exists():
        logger.error(
            "Neo4j launcher not found at %s — vendor download incomplete?", _NEO4J_BAT
        )
        return

    env = os.environ.copy()
    env["JAVA_HOME"] = str(_JDK_DIR)
    env["NEO4J_HOME"] = str(_NEO4J_DIR)
    env["NEO4J_CONF"] = str(_NEO4J_DIR / "conf")
    env["PATH"] = str(_JDK_DIR / "bin") + os.pathsep + env.get("PATH", "")

    log_path = LOGS_DIR / "neo4j.log"
    log_fh = open(log_path, "ab", buffering=0)

    logger.info("spawning Neo4j (logs → %s)", log_path)
    _neo4j_proc = subprocess.Popen(
        ["cmd", "/c", str(_NEO4J_BAT), "console"],
        cwd=str(_NEO4J_DIR),
        env=env,
        stdout=log_fh,
        stderr=subprocess.STDOUT,
        stdin=subprocess.DEVNULL,
        creationflags=_CREATE_NO_WINDOW,
    )
    logger.info("Neo4j started pid=%d", _neo4j_proc.pid)
    atexit.register(_stop_neo4j)


def _stop_neo4j() -> None:
    """Stop the managed Neo4j child. Tries `neo4j.bat stop` first so the
    JVM flushes the WAL and releases data-folder file locks; falls back to
    a tree-kill so we never leave a runaway java.exe behind."""
    global _neo4j_proc
    if _neo4j_proc is None:
        return
    if _neo4j_proc.poll() is not None:
        _neo4j_proc = None
        return
    logger.info("stopping Neo4j (pid=%d)", _neo4j_proc.pid)

    # Graceful: ask Neo4j to stop itself. neo4j.bat stop pings the running
    # instance over its admin protocol, waits for a clean shutdown, releases
    # store locks. Skipped if the launcher is missing (vendor not bootstrapped).
    if _NEO4J_BAT.exists():
        env = os.environ.copy()
        env["JAVA_HOME"] = str(_JDK_DIR)
        env["NEO4J_HOME"] = str(_NEO4J_DIR)
        env["NEO4J_CONF"] = str(_NEO4J_DIR / "conf")
        env["PATH"] = str(_JDK_DIR / "bin") + os.pathsep + env.get("PATH", "")
        try:
            subprocess.run(
                ["cmd", "/c", str(_NEO4J_BAT), "stop"],
                cwd=str(_NEO4J_DIR),
                env=env,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                stdin=subprocess.DEVNULL,
                creationflags=_CREATE_NO_WINDOW,
                timeout=20,
            )
        except (OSError, subprocess.TimeoutExpired) as exc:
            logger.warning("neo4j.bat stop failed: %s", exc)

    # Belt-and-braces tree-kill in case the graceful stop did not bring the
    # whole tree down (or the launcher was missing). taskkill /T descends to
    # the java.exe child of neo4j.bat.
    if _neo4j_proc.poll() is None:
        try:
            subprocess.run(
                ["taskkill", "/PID", str(_neo4j_proc.pid), "/T", "/F"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                creationflags=_CREATE_NO_WINDOW,
                timeout=10,
            )
        except Exception as exc:
            logger.warning("taskkill failed: %s", exc)
    try:
        _neo4j_proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        logger.warning("Neo4j did not exit cleanly within 5s")
    _neo4j_proc = None


def _on_signal(signum: int, _frame: Any) -> None:
    # SIGTERM/SIGINT/SIGBREAK arrive on launcher stop or Ctrl+C (the Lifecycle
    # daemon takes this process down with CTRL_BREAK_EVENT). Releasing the
    # supervisor loop lets ``main`` run its teardown (mark_stopped + Neo4j
    # stop) on the main thread — keeping all the JVM-stop work out of the
    # signal handler, where a 20s ``neo4j.bat stop`` would be running under
    # async-signal constraints. The atexit hook is a belt-and-braces backstop
    # for hard exits that skip the loop.
    logger.info("signal %s received; releasing supervisor loop", signum)
    _shutdown_event.set()


def _serve_until_shutdown() -> None:
    """Block until a shutdown signal arrives, keeping the Neo4j JVM child
    supervised under this process.

    Replaces the former ``graph_service.main()`` IPC serve loop. That loop
    served the legacy ``\\\\.\\pipe\\wylde-memgraph`` Flask surface, which was
    retired in the 2026-05-26 direct-Bolt cutover (the harness talks to Neo4j
    over Bolt now). With the pipe surface gone, the only thing the loop still
    needs to do is keep this process alive so the JVM child it spawned stays
    grouped under it and is torn down with it — that is all this does.

    The short ``wait`` timeout keeps the main thread returning to the
    interpreter regularly so a pending signal handler (which sets the event)
    is observed promptly.
    """
    while not _shutdown_event.wait(timeout=1.0):
        pass


def _install_signal_handlers() -> None:
    for sig_name in ("SIGTERM", "SIGINT", "SIGBREAK"):
        sig = getattr(signal, sig_name, None)
        if sig is None:
            continue
        try:
            signal.signal(sig, _on_signal)
        except (ValueError, OSError):
            # Not main thread, or signal unsupported on this platform.
            pass


def main() -> None:
    os.environ.setdefault("GRAPH_BOLT_URL", f"bolt://{_BOLT_HOST}:{_BOLT_PORT}")
    os.environ.setdefault("WYLDE_SERVICE_NAME", SERVICE_NAME)

    write_manifest(
        service_name=SERVICE_NAME,
        port=_BOLT_PORT,
        category="core",
        description=(
            "Graph data layer (bundled Neo4j via Bolt). Constituent pipe "
            "of Core — the registry filters this entry out of the peer "
            "services list because Core's rollup manifest covers it."
        ),
        contributes={
            "dashboard": {"label": "Memgraph", "icon": "database", "color": "blue"},
        },
        entry_point="python:Core.Memgraph.run",
    )
    start_heartbeat(SERVICE_NAME)

    _install_signal_handlers()
    _spawn_neo4j()

    if _bolt_ready():
        logger.info("Neo4j up on bolt://%s:%d", _BOLT_HOST, _BOLT_PORT)
    else:
        logger.info(
            "waiting up to %ds for Neo4j on bolt://%s:%d ...",
            _READY_WAIT_S,
            _BOLT_HOST,
            _BOLT_PORT,
        )
        if _wait_for_neo4j():
            logger.info("Neo4j ready")
        else:
            logger.warning(
                "Neo4j did not come up within %ds — supervisor will stay up "
                "and the harness retries its Bolt connection lazily on first "
                "request",
                _READY_WAIT_S,
            )

    try:
        _serve_until_shutdown()
    finally:
        mark_stopped(SERVICE_NAME)
        _stop_neo4j()


if __name__ == "__main__":
    main()
