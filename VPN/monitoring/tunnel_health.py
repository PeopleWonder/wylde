"""Tunnel health monitor — heartbeat, latency, reconnect detection.

Polls `wg show <iface> latest-handshakes` every N seconds, computes peer
freshness, and emits push notifications when the tunnel state degrades or
recovers. Designed to run as a daemon thread alongside the gateway.
"""

import logging
import threading
import time
from datetime import datetime, timezone
from typing import Callable, Dict, Optional

from tunnel import wireguard

logger = logging.getLogger(__name__)


class TunnelHealth:
    def __init__(
        self,
        *,
        interval_s: int = 15,
        stale_after_s: int = 180,
        on_state_change: Optional[Callable[[str, dict], None]] = None,
    ):
        self._interval = interval_s
        self._stale_after = stale_after_s
        self._on_change = on_state_change
        self._thread: Optional[threading.Thread] = None
        self._stop = threading.Event()
        self._state: Dict[
            str, str
        ] = {}  # peer_pubkey -> 'online' | 'stale' | 'offline'

    def start(self) -> None:
        if self._thread and self._thread.is_alive():
            return
        self._stop.clear()
        self._thread = threading.Thread(
            target=self._loop, name="tunnel-health", daemon=True
        )
        self._thread.start()

    def stop(self) -> None:
        self._stop.set()

    def snapshot(self) -> dict:
        return {
            "peers": dict(self._state),
            "timestamp": datetime.now(timezone.utc).isoformat(),
        }

    def _loop(self) -> None:
        while not self._stop.is_set():
            try:
                self._tick()
            except Exception as exc:  # noqa: BLE001
                logger.debug(
                    "tunnel-health tick failed (likely non-Linux host): %s", exc
                )
            self._stop.wait(self._interval)

    def _tick(self) -> None:
        try:
            handshakes = wireguard.get_wg1_latest_handshakes()
        except Exception:  # noqa: BLE001
            return

        now = time.time()
        for pub, ts in handshakes.items():
            age = now - ts if ts else float("inf")
            if age < 30:
                state = "online"
            elif age < self._stale_after:
                state = "stale"
            else:
                state = "offline"
            previous = self._state.get(pub)
            self._state[pub] = state
            if previous and previous != state and self._on_change is not None:
                try:
                    self._on_change(
                        pub,
                        {"previous": previous, "current": state, "last_handshake": ts},
                    )
                except Exception as exc:  # noqa: BLE001
                    logger.warning("tunnel-health on_change failed: %s", exc)
