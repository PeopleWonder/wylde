"""device_gate service entry point — hosts ``\\\\.\\pipe\\wylde-device-gate``.

The pipe surface (the nine ``device_gate.*`` action handlers covering
pairing, tier management, token rotation, revocation) lives in
:mod:`device_gate.pipe`; this module is the process wrapper.  The
Lifecycle daemon launches it by direct script path —
``py -3 device_gate/run.py`` — so both that invocation and the
qualified ``-m device_gate.run`` form land on the same code path.

Two facades over the same start/stop machinery: ``start_device_gate``
/ ``stop_device_gate`` for embedding hosts (tests, a future
in-process daemon mode) and a ``__main__`` long-lived loop with
signal handling.  Startup also nudges ``sys.path`` so both the
service-local ``pipe`` import and the qualified ``from Core.shared
import ipc`` import resolve regardless of how the caller invoked us.

Service owns its manifest: write_manifest at startup, start_heartbeat
to keep status.heartbeat fresh, mark_stopped from the signal handler.
The Lifecycle daemon no longer writes device_gate's manifest — it just
spawns the subprocess and supervises it.

File layout — the orchestrator (``_serve_forever``) is defined first
so the canonical startup-sequence rule (Core/harness/dev/wylde_check
rule 18) matches its calls in source order; the helper functions that
wrap the pipe-startup call come after.
"""

from __future__ import annotations

import logging
import signal
import sys
import threading
from pathlib import Path
from types import FrameType
from typing import Optional

# When run as a script, Python doesn't add the script's dir to
# sys.path automatically — explicit insert makes ``from core import``
# resolve from the same folder this file lives in.
_HERE = Path(__file__).resolve().parent
if str(_HERE) not in sys.path:
    sys.path.insert(0, str(_HERE))
# Also drop the vault root on sys.path so ``from Core.shared import
# ipc`` (the import the pipe layer uses) resolves regardless of how
# the caller launched us. The Lifecycle daemon sets PYTHONPATH for
# the spawned subprocess, but the manual ``py -3 "device_gate/run.py"``
# invocation in smoke scripts skips that step — this fallback keeps
# both paths working.
_VAULT_ROOT = _HERE.parent
if str(_VAULT_ROOT) not in sys.path:
    sys.path.insert(0, str(_VAULT_ROOT))

from device_gate import pipe as _pipe  # noqa: E402

from Core.shared.manifest import (  # noqa: E402
    mark_stopped,
    start_heartbeat,
    write_manifest,
)

logger = logging.getLogger("wylde.device_gate.run")

SERVICE_NAME = "wylde-device-gate"


_shutdown_event = threading.Event()
_started = False
_started_lock = threading.Lock()


def _serve_forever() -> int:
    try:
        from Core.shared.logging_setup import configure_logging
    except ImportError:
        # device_gate is launched as a bare script — when Core.shared
        # isn't on sys.path the wider system already has root logging
        # configured (the daemon does it) so we fall back to a no-op.
        pass
    else:
        configure_logging(service=SERVICE_NAME)
    write_manifest(
        service_name=SERVICE_NAME,
        port=0,
        category="auth",
        description=(
            "Per-device pairing + permission tiers. Issues tokens that "
            "Gateway verifies on every external request."
        ),
        contributes={
            "dashboard": {
                "label": "device_gate",
                "icon": "shield",
                "color": "yellow",
            },
        },
        entry_point="python:device_gate.run",
    )
    start_heartbeat(SERVICE_NAME)
    _install_signal_handlers()
    if not start_device_gate():
        logger.error("device_gate: pipe failed to start (msgpack/pywin32 missing?)")
        mark_stopped(SERVICE_NAME)
        return 1
    logger.info("device_gate: serving \\\\.\\pipe\\%s", _pipe.SERVICE_NAME)
    try:
        while not _shutdown_event.is_set():
            _shutdown_event.wait(timeout=1.0)
    except KeyboardInterrupt:
        pass
    mark_stopped(SERVICE_NAME)
    logger.info("device_gate: shutdown complete")
    return 0


def _install_signal_handlers() -> None:
    def _handler(signum: int, _frame: Optional[FrameType]) -> None:
        logger.info("device_gate: signal %s, shutting down", signum)
        mark_stopped(SERVICE_NAME)
        _shutdown_event.set()

    for sig_name in ("SIGINT", "SIGTERM"):
        sig = getattr(signal, sig_name, None)
        if sig is None:
            continue
        try:
            signal.signal(sig, _handler)
        except (ValueError, OSError):
            pass


def start_device_gate() -> bool:
    """Start the device_gate pipe in a background thread (idempotent)."""
    global _started
    with _started_lock:
        if _started:
            return True
        ok = _pipe.start()
        _started = ok
        return ok


def stop_device_gate() -> None:
    """Trigger graceful shutdown of the entry-point loop."""
    _shutdown_event.set()
    _pipe.stop()


if __name__ == "__main__":  # pragma: no cover
    sys.exit(_serve_forever())


__all__ = ["start_device_gate", "stop_device_gate"]
