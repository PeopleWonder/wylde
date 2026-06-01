"""Boot-time peer rehydration + last-seen background refresh.

Two daemon-thread workers:

* :func:`rehydrate_peers` — once on boot, push every stored peer back
  onto wg1. The interface comes up clean after restart so this is the
  cheapest way to keep mobile clients reconnected.
* :func:`start_last_seen_worker` — periodic ``wg show`` poll that keeps
  ``peer.last_seen`` fresh in the registry. The dashboard reads
  last_seen to render online/offline status.

Split out of the legacy ``tools/wylde_link.py`` so the pairing logic
and the boot/heartbeat threads can be reasoned about separately.
"""

from __future__ import annotations

import logging
import threading
import time
from datetime import datetime, timezone

from . import store as peer_store
from tunnel import wireguard as models

logger = logging.getLogger(__name__)


def rehydrate_peers() -> int:
    """Re-push every stored peer to wg1. Returns count pushed."""
    if not models.link_iface_up():
        logger.info("Peer rehydrate skipped — wg1 not up yet.")
        return 0
    pushed = 0
    for p in peer_store.list_peers():
        try:
            models.add_wg1_peer(p["public_key"], p["tunnel_ip"])
            pushed += 1
        except Exception as exc:
            logger.warning("Rehydrate failed for %s: %s", p["public_key"][:16], exc)
    if pushed:
        logger.info("Rehydrated %d peer(s) onto wg1.", pushed)
    return pushed


def _last_seen_worker_loop(interval_s: int) -> None:
    while True:
        time.sleep(interval_s)
        try:
            for pub, epoch in models.get_wg1_latest_handshakes().items():
                if epoch > 0:
                    p = peer_store.get_peer(pub)
                    if p:
                        p["last_seen"] = datetime.fromtimestamp(
                            epoch, tz=timezone.utc
                        ).isoformat()
                        peer_store.upsert_peer(pub, p)
        except Exception as exc:
            logger.debug("last-seen refresh: %s", exc)


def start_last_seen_worker(*, interval_s: int = 30) -> threading.Thread:
    """Spawn the last-seen-refresh daemon thread. Returns the thread handle."""
    t = threading.Thread(
        target=_last_seen_worker_loop,
        args=(interval_s,),
        daemon=True,
        name="link-last-seen",
    )
    t.start()
    return t


__all__ = ["rehydrate_peers", "start_last_seen_worker"]
