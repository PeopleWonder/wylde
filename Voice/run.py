"""Voice service entry point — hosts ``\\\\.\\pipe\\wylde-voice``.

The pipe surface (the ten ``voice.*`` action handlers) lives in
:mod:`Voice.pipe`; this module is the thin process wrapper the
Lifecycle daemon launches via ``py -3 -m Voice.run``.  It exposes two
facades over the same start/stop machinery: ``start_voice`` /
``stop_voice`` for embedding hosts that bring the pipe up in-process
(unit tests, a future in-process daemon mode), and a ``__main__``
that runs the service as a long-lived subprocess with signal handling
— which is the path the daemon takes.

Voice does not host STT/TTS engines itself; the harness owns those.
This entry point only wires up the orchestration loop and the pipe.

Service owns its manifest: write_manifest at startup, start_heartbeat
to keep status.heartbeat fresh, mark_stopped from the signal handler.
The Lifecycle daemon no longer writes Voice's manifest — it just
spawns the subprocess and supervises it.

File layout — the orchestrator (``_serve_forever``) is defined first
so the canonical startup-sequence rule (Core/harness/dev/wylde_check
rule 18) can match its calls in source order; the helper functions
that wrap the pipe-startup call come after.
"""

from __future__ import annotations

import logging
import signal
import sys
import threading
from types import FrameType

from Core.shared.manifest import (
    mark_serve_loop_entered,
    mark_stopped,
    start_heartbeat,
    write_manifest,
)

from . import pipe as _pipe

logger = logging.getLogger("wylde.voice.run")

SERVICE_NAME = "wylde-voice"


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
        category="voice",
        description=(
            "Voice service. Wake-word, STT, TTS, and per-turn orchestration "
            "loop via \\\\.\\pipe\\wylde-voice."
        ),
        contributes={
            "dashboard": {"label": "Voice", "icon": "mic", "color": "purple"},
        },
        entry_point="python:Voice.run",
    )
    start_heartbeat(SERVICE_NAME)
    _install_signal_handlers()
    if not start_voice():
        logger.error("voice: pipe failed to start (msgpack/pywin32 missing?)")
        mark_stopped(SERVICE_NAME)
        return 1
    # Attest the serve_loop phase explicitly. ``ipc.serve_forever_background``
    # also calls this from inside ``Voice.pipe.start``, but the attestation
    # there can race with manifest cache rehydration on slow disk. Calling
    # it here too is idempotent (the helper de-dupes adjacent phases) and
    # ensures wylde_check rule 18 sees the full four-phase sequence.
    mark_serve_loop_entered(SERVICE_NAME)
    logger.info("voice: serving \\\\.\\pipe\\%s", _pipe.SERVICE_NAME)
    try:
        while not _shutdown_event.is_set():
            _shutdown_event.wait(timeout=1.0)
    except KeyboardInterrupt:
        pass
    mark_stopped(SERVICE_NAME)
    logger.info("voice: shutdown complete")
    return 0


def _install_signal_handlers() -> None:
    def _handler(signum: int, _frame: FrameType | None) -> None:
        logger.info("voice: signal %s, shutting down", signum)
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


def start_voice() -> bool:
    """Start the voice pipe in a background thread (idempotent).

    Returns True if the pipe is now serving (or was already serving).
    Returns False if dependencies are missing — pywin32/msgpack absent,
    or the host is non-Windows. Embedding hosts that want a tighter
    handle should call :func:`Voice.pipe.start` directly.
    """
    global _started
    with _started_lock:
        if _started:
            return True
        ok = _pipe.start()
        _started = ok
        return ok


def stop_voice() -> None:
    """Trigger graceful shutdown of the entry-point loop. The pipe
    server itself doesn't expose a shutdown hook today; it drains
    when the daemon process exits."""
    _shutdown_event.set()
    _pipe.stop()


if __name__ == "__main__":  # pragma: no cover
    sys.exit(_serve_forever())


__all__ = ["start_voice", "stop_voice"]
