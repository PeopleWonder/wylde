"""resource_monitor service entry point — hosts the VRAM broker pipe.

Spawns ``\\\\.\\pipe\\wylde-vram-broker`` (the fourth Core constituent
alongside lifecycle / harness / memgraph) on a minimal Flask app and
blocks until the host signals shutdown.  The pipe surface lives in the
``broker`` subpackage; this module is the thin process wrapper the
Lifecycle daemon launches.

At startup it puts the Wylde root on ``sys.path`` so the broker can
import ``Core.shared.*`` qualifiedly, writes its service manifest,
starts a heartbeat, then calls ``install()`` to register the
``/vram/*`` routes, open the named pipe alias, and start the reaper /
Ollama-poll / manifest-state worker threads.  The main thread blocks
on SIGINT / SIGTERM / SIGBREAK; on signal it calls ``mark_stopped``
and then ``stop()`` so the background threads can drain.

Daemon-launched in production; safe to run standalone for dev with
``py -3 Core/resource_monitor/run.py`` from the Wylde root, or via
``-m Core.resource_monitor.run`` now that the folder is dotted-import-
addressable.

Service owns its manifest.  Registry filters ``vram-broker`` out of the
peer-services list because it is a Core constituent pipe, but the
manifest file is still written here so the dashboard's per-broker
diagnostics surface (state file path, lease endpoints) stays populated.
"""

from __future__ import annotations

import logging
import os
import signal
import sys
import threading
from pathlib import Path
from types import FrameType

_HERE = Path(__file__).resolve().parent
_WYLDE_ROOT = _HERE.parent.parent

# Put the Wylde root on sys.path so ``from Core.shared import ...``
# resolves in this process.  We deliberately do *not* add ``_HERE``
# itself — the Docker-era pattern of side-loading the service folder
# onto sys.path was the contamination source the Phase-9 VPN smoke
# cleanup tore out.
if str(_WYLDE_ROOT) not in sys.path:
    sys.path.insert(0, str(_WYLDE_ROOT))


SERVICE_NAME = "vram-broker"


def main() -> int:
    from Core.shared.logging_setup import configure_logging
    from Core.shared.manifest import (
        mark_stopped,
        start_heartbeat,
        write_manifest,
    )

    configure_logging()
    log = logging.getLogger("wylde.vram_broker.run")

    # WYLDE_ROOT — used by the broker for data/state and data/manifests
    # paths.  Set explicitly so the default doesn't depend on cwd.
    os.environ.setdefault("WYLDE_ROOT", str(_WYLDE_ROOT))

    port = int(os.getenv("WYLDE_VRAM_BROKER_PORT", "9101"))
    write_manifest(
        service_name=SERVICE_NAME,
        port=port,
        category="core",
        description=(
            "GPU VRAM lease broker — priority-based admission control across "
            "all services"
        ),
        contributes={
            "vram_broker": {
                "state_path": "/vram/state",
                "leases_path": "/vram/leases",
                "reserve_path": "/vram/reserve",
                "release_path": "/vram/release",
                "evict_path": "/vram/evict",
                "state_file": "data/state/vram-broker.json",
            },
        },
        entry_point="python:Core.resource_monitor.run",
    )
    start_heartbeat(SERVICE_NAME)

    from flask import Flask

    from Core.resource_monitor.broker.service import install, stop

    app = Flask("wylde-vram-broker")
    install(app, gpu_available=True)
    log.info("vram_broker: pipe alias up at \\\\.\\pipe\\wylde-vram-broker")

    stop_event = threading.Event()

    def _handler(_signum: int, _frame: FrameType | None) -> None:
        log.info("vram_broker: signal received, shutting down")
        mark_stopped(SERVICE_NAME)
        stop_event.set()

    for sig in (signal.SIGINT, signal.SIGTERM):
        try:
            signal.signal(sig, _handler)
        except (ValueError, OSError):
            pass
    if sys.platform == "win32":
        try:
            signal.signal(signal.SIGBREAK, _handler)
        except (ValueError, OSError, AttributeError):
            pass

    stop_event.wait()
    try:
        stop()
    except Exception:  # noqa: BLE001
        log.exception("vram_broker: stop() raised")
    mark_stopped(SERVICE_NAME)
    return 0


if __name__ == "__main__":
    sys.exit(main())
