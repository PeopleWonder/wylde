"""Persistent peer registry — JSON file-backed, thread-safe.

Peers are stored at LINK_DATA_DIR/peers.json.  All mutations go through
this module so the on-disk state always reflects in-memory state.
"""

import json
import threading
from datetime import datetime, timezone
from pathlib import Path

import config

_lock = threading.Lock()
_path = Path(config.LINK_DATA_DIR) / "peers.json"


def _load() -> dict:
    try:
        if _path.exists():
            data: dict = json.loads(_path.read_text())
            return data
    except Exception:
        pass
    return {}


def _save(peers: dict) -> None:
    _path.parent.mkdir(parents=True, exist_ok=True)
    tmp = _path.with_suffix(".tmp")
    tmp.write_text(json.dumps(peers, indent=2))
    tmp.replace(_path)


# ── Public API ────────────────────────────────────────────────────────────────


def list_peers() -> list:
    with _lock:
        return list(_load().values())


def get_peer(public_key: str) -> dict | None:
    with _lock:
        return _load().get(public_key)


def upsert_peer(public_key: str, peer: dict) -> None:
    with _lock:
        peers = _load()
        peers[public_key] = peer
        _save(peers)


def remove_peer(public_key: str) -> bool:
    with _lock:
        peers = _load()
        if public_key not in peers:
            return False
        del peers[public_key]
        _save(peers)
        return True


def touch_peer(public_key: str) -> None:
    """Update last_seen timestamp for a peer."""
    with _lock:
        peers = _load()
        if public_key in peers:
            peers[public_key]["last_seen"] = datetime.now(timezone.utc).isoformat()
            _save(peers)


def next_tunnel_ip() -> str | None:
    """Return the lowest available IP in the LINK_PEER_SUBNET range."""
    with _lock:
        peers = _load()
        used = {p["tunnel_ip"] for p in peers.values()}
        return next(
            (
                f"192.0.2.{i}"
                for i in range(2, 254)
                if f"192.0.2.{i}" not in used
            ),
            None,
        )
