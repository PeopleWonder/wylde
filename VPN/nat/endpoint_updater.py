"""Periodic STUN probe + endpoint-change notification.

Runs as a daemon thread. When the public endpoint changes (ISP DHCP
rotation, network interface flip, etc.) it:

  1. Updates the DDNS record (if configured)
  2. Queues a push notification to every paired peer with the new endpoint
  3. Logs the change to data/wylde-link/endpoint-history.json
"""

import json
import logging
import threading
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable, List, Optional

import config
from . import stun

logger = logging.getLogger(__name__)


class EndpointUpdater:
    def __init__(
        self,
        stun_servers: List[str],
        *,
        interval_s: int = 300,
        on_change: Optional[Callable[[str, str], None]] = None,
    ):
        self._servers = stun_servers
        self._interval = interval_s
        self._on_change = on_change
        self._thread: Optional[threading.Thread] = None
        self._stop = threading.Event()
        self._current: Optional[str] = None
        self._history_path = Path(config.LINK_DATA_DIR) / "endpoint-history.json"

    def start(self) -> None:
        if self._thread and self._thread.is_alive():
            return
        self._stop.clear()
        self._thread = threading.Thread(
            target=self._loop, name="endpoint-updater", daemon=True
        )
        self._thread.start()

    def stop(self) -> None:
        self._stop.set()

    def current(self) -> Optional[str]:
        return self._current

    def _loop(self) -> None:
        while not self._stop.is_set():
            self._tick()
            self._stop.wait(self._interval)

    def _tick(self) -> None:
        try:
            res = stun.discover_endpoint(self._servers, timeout=3.0)
        except Exception as exc:  # noqa: BLE001
            logger.warning("endpoint-updater probe failed: %s", exc)
            return
        if res is None:
            return
        endpoint = f"{res['ip']}:{res['port']}"
        if endpoint == self._current:
            return
        previous = self._current
        self._current = endpoint
        self._record(previous, endpoint)
        if self._on_change is not None:
            try:
                self._on_change(previous or "", endpoint)
            except Exception as exc:  # noqa: BLE001
                logger.warning("endpoint-updater on_change callback failed: %s", exc)

    def _record(self, previous: Optional[str], current: str) -> None:
        entry = {
            "timestamp": datetime.now(timezone.utc).isoformat(),
            "previous": previous or "",
            "current": current,
        }
        try:
            self._history_path.parent.mkdir(parents=True, exist_ok=True)
            history = []
            if self._history_path.exists():
                history = json.loads(self._history_path.read_text())
            history.append(entry)
            history = history[-100:]
            tmp = self._history_path.with_suffix(".tmp")
            tmp.write_text(json.dumps(history, indent=2))
            tmp.replace(self._history_path)
        except (OSError, json.JSONDecodeError) as exc:
            logger.warning("endpoint-updater history write failed: %s", exc)
