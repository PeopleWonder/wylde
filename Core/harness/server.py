"""Harness service entry point.

Two ways to run the harness:

1. **In-process** — ``Core/Lifecycle/daemon.py`` calls
   :func:`Core.harness.server.start` at boot, which starts the pipe in
   a daemon thread alongside the lifecycle pipe. Default for the
   integrated stack.

2. **Standalone** — ``py -3 -m Core.harness.server`` runs the pipe in
   the foreground. Useful for development and debugging when you want
   the harness logs in their own console.

Either way the surface is the same: ``\\\\.\\pipe\\wylde-harness``
serving the five ``chat.*`` actions defined in :mod:`Core.harness.pipe`.
"""

from __future__ import annotations

import logging
import signal
import sys
import threading
from typing import Any

from . import pipe as _pipe

logger = logging.getLogger("wylde.harness.server")


def start() -> bool:
    """Start the harness pipe. Idempotent.

    Returns True if the pipe is serving. Wraps :func:`Core.harness.pipe.start`
    so callers don't import the pipe module directly.
    """
    return _pipe.start()


def serve_forever() -> int:
    """Foreground entry. Starts the pipe and blocks until SIGINT/SIGTERM.

    Used by ``py -3 -m Core.harness.server``.
    """
    from Core.shared.logging_setup import configure_logging

    configure_logging()
    if not start():
        logger.error("harness pipe: failed to start")
        return 1

    stop_event = threading.Event()

    def _handle(_signum: int, _frame: Any) -> None:  # noqa: ARG001
        stop_event.set()

    for sig in (signal.SIGINT, signal.SIGTERM):
        try:
            signal.signal(sig, _handle)
        except (ValueError, OSError):
            pass

    logger.info("harness pipe: ready (Ctrl-C to stop)")
    while not stop_event.is_set():
        stop_event.wait(timeout=1.0)

    _pipe.stop()
    logger.info("harness pipe: exit")
    return 0


def main() -> int:
    return serve_forever()


if __name__ == "__main__":
    sys.exit(main())


__all__ = ["main", "serve_forever", "start"]
