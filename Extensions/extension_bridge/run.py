r"""extension_bridge entry point — hosts ``\\.\pipe\wylde-extension-bridge``.

The pipe surface (the ``extensions.*`` action handlers) lives in
:mod:`extension_bridge.pipe`; this module is the thin process wrapper
the Lifecycle daemon launches via ``py -3 -m Extensions.extension_bridge.run``.

The extension bridge itself — loader / registry / dispatcher — is plain
in-process code that Wylde-Core and the Python Gateway import directly.
This service exists so callers with NO in-process Python (chiefly the
Rust Gateway port) can still reach the bridge's dispatch entry point
over the named-pipe transport. The handler logic is unchanged; the pipe
is a thin wrapper around :func:`extension_bridge.dispatch_external`.

Two facades over the same start/stop machinery: ``start_extension_bridge``
/ ``stop_extension_bridge`` for embedding hosts that bring the pipe up
in-process (unit tests, a future in-process daemon mode), and a
``__main__`` that runs the service as a long-lived subprocess with
signal handling — the path the daemon takes.

Service owns its manifest: write_manifest at startup, start_heartbeat
to keep status.heartbeat fresh, mark_stopped from the signal handler.
The Lifecycle daemon only spawns + supervises the subprocess.

File layout — the orchestrator (``_serve_forever``) is defined first so
the canonical startup-sequence rule (Core/harness/dev/wylde_check rule
18) matches its calls in source order; the helper that wraps the
pipe-startup call comes after.
"""

from __future__ import annotations

import logging
import signal
import sys
import threading
from types import FrameType
from typing import Optional

from Core.shared.manifest import (
    mark_stopped,
    start_heartbeat,
    write_manifest,
)

from . import pipe as _pipe

logger = logging.getLogger("wylde.extensions.bridge.run")

SERVICE_NAME = "wylde-extension-bridge"


_shutdown_event = threading.Event()
_started = False
_started_lock = threading.Lock()


def _serve_forever() -> int:
    try:
        from Core.shared.logging_setup import configure_logging
    except ImportError:
        pass
    else:
        configure_logging(service=SERVICE_NAME)
    write_manifest(
        service_name=SERVICE_NAME,
        port=0,
        category="extensions",
        description=(
            "Extension bridge pipe. External entry point to the in-process "
            "extension dispatcher for callers with no in-process Python "
            "(the Rust Gateway), via \\\\.\\pipe\\wylde-extension-bridge."
        ),
        contributes={
            "dashboard": {
                "label": "Extension Bridge",
                "icon": "puzzle",
                "color": "green",
            },
        },
        entry_point="python:Extensions.extension_bridge.run",
    )
    start_heartbeat(SERVICE_NAME)
    _install_signal_handlers()
    if not start_extension_bridge():
        logger.error(
            "extension_bridge: pipe failed to start (msgpack/pywin32 missing?)"
        )
        mark_stopped(SERVICE_NAME)
        return 1
    logger.info("extension_bridge: serving \\\\.\\pipe\\%s", _pipe.SERVICE_NAME)
    try:
        while not _shutdown_event.is_set():
            _shutdown_event.wait(timeout=1.0)
    except KeyboardInterrupt:
        pass
    mark_stopped(SERVICE_NAME)
    logger.info("extension_bridge: shutdown complete")
    return 0


def _install_signal_handlers() -> None:
    def _handler(signum: int, _frame: Optional[FrameType]) -> None:
        logger.info("extension_bridge: signal %s, shutting down", signum)
        mark_stopped(SERVICE_NAME)
        _shutdown_event.set()

    for sig_name in ("SIGINT", "SIGTERM"):
        sig = getattr(signal, sig_name, None)
        if sig is None:
            continue
        try:
            signal.signal(sig, _handler)
        except (ValueError, OSError):
            # Off-main-thread or unsupported on this platform.
            pass


def start_extension_bridge() -> bool:
    """Start the extension-bridge pipe in a background thread (idempotent).

    Returns True if the pipe is now serving (or was already serving).
    Returns False if dependencies are missing — pywin32/msgpack absent,
    or the host is non-Windows."""
    global _started
    with _started_lock:
        if _started:
            return True
        ok = _pipe.start()
        _started = ok
        return ok


def stop_extension_bridge() -> None:
    """Trigger graceful shutdown of the entry-point loop. The pipe
    server itself drains when the daemon process exits."""
    _shutdown_event.set()
    _pipe.stop()


if __name__ == "__main__":  # pragma: no cover
    sys.exit(_serve_forever())


__all__ = ["start_extension_bridge", "stop_extension_bridge"]
