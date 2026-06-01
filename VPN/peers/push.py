"""Push-notification subscription + delivery store.

A peer (mobile app) registers a push endpoint — one of:
  - 'webhook' with a URL the mobile app's backend receives POSTs on, or
  - 'poll'    meaning: the app will pull /api/link/push/pending itself.

Notifications are keyed by peer public key.  Delivery is best-effort:
  1. If the subscription is a webhook, POST the payload and mark delivered.
  2. On webhook failure (timeout, 4xx, 5xx), enqueue for the peer to pull.
  3. Poll-mode peers always enqueue; they drain by calling the pending API.

Backed by a JSON file at LINK_DATA_DIR/push.json:
  {
    "subscriptions": { "<pubkey>": {...} },
    "queued":        { "<pubkey>": [ {id, ts, title, body, data}, ... ] }
  }
"""

import json
import secrets
import threading
import urllib.request
import urllib.error
import logging
from datetime import datetime, timezone
from pathlib import Path

import config

logger = logging.getLogger(__name__)

_lock = threading.Lock()
_path = Path(config.LINK_DATA_DIR) / "push.json"
_QUEUE_CAP = 64  # per peer — oldest evicted beyond this
_WEBHOOK_TIMEOUT = 3.0


# ── Internal ──────────────────────────────────────────────────────────────────


def _load() -> dict:
    try:
        if _path.exists():
            data: dict = json.loads(_path.read_text())
            return data
    except Exception:
        pass
    return {"subscriptions": {}, "queued": {}}


def _save(state: dict) -> None:
    _path.parent.mkdir(parents=True, exist_ok=True)
    tmp = _path.with_suffix(".tmp")
    tmp.write_text(json.dumps(state, indent=2))
    tmp.replace(_path)


# ── Public API ────────────────────────────────────────────────────────────────


def subscribe(public_key: str, kind: str, endpoint: str = "") -> dict:
    """Register / update a push subscription for a peer.

    kind: 'webhook' | 'poll'
    endpoint: the URL when kind='webhook'; ignored when kind='poll'.
    """
    if kind not in ("webhook", "poll"):
        raise ValueError('kind must be "webhook" or "poll"')
    if kind == "webhook" and not endpoint:
        raise ValueError("endpoint URL required for webhook subscriptions")

    sub = {
        "kind": kind,
        "endpoint": endpoint,
        "subscribed_at": datetime.now(timezone.utc).isoformat(),
    }
    with _lock:
        state = _load()
        state["subscriptions"][public_key] = sub
        _save(state)
    return sub


def unsubscribe(public_key: str) -> bool:
    with _lock:
        state = _load()
        if public_key not in state["subscriptions"]:
            return False
        del state["subscriptions"][public_key]
        state["queued"].pop(public_key, None)
        _save(state)
    return True


def list_subscriptions() -> list:
    with _lock:
        state = _load()
        return [
            {"public_key": k, **v, "queued": len(state["queued"].get(k, []))}
            for k, v in state["subscriptions"].items()
        ]


def drain_pending(public_key: str) -> list:
    """Return all queued notifications for a peer and clear the queue."""
    with _lock:
        state = _load()
        items: list = state["queued"].pop(public_key, [])
        _save(state)
    return items


def notify(public_key: str, title: str, body: str, data: dict | None = None) -> dict:
    """Deliver (or enqueue) a notification for a peer.

    Returns { delivered: bool, queued: bool, id: str }.
    """
    payload = {
        "id": secrets.token_hex(8),
        "ts": datetime.now(timezone.utc).isoformat(),
        "title": title,
        "body": body,
        "data": data or {},
    }

    with _lock:
        state = _load()
        sub = state["subscriptions"].get(public_key)

    if sub and sub["kind"] == "webhook":
        if _post_webhook(sub["endpoint"], {"peer": public_key, **payload}):
            return {"delivered": True, "queued": False, "id": payload["id"]}

    # Enqueue for poll-mode peers, or as webhook fallback.
    with _lock:
        state = _load()
        q = state["queued"].setdefault(public_key, [])
        q.append(payload)
        # Trim oldest beyond cap.
        if len(q) > _QUEUE_CAP:
            del q[: len(q) - _QUEUE_CAP]
        _save(state)

    return {"delivered": False, "queued": True, "id": payload["id"]}


def broadcast(title: str, body: str, data: dict | None = None) -> dict:
    """Fire a notification to every subscribed peer.  Returns counts."""
    delivered = queued = 0
    with _lock:
        state = _load()
        peers = list(state["subscriptions"].keys())
    for pub in peers:
        r = notify(pub, title, body, data)
        delivered += int(r["delivered"])
        queued += int(r["queued"])
    return {"delivered": delivered, "queued": queued, "recipients": len(peers)}


# ── Webhook delivery ──────────────────────────────────────────────────────────


def _post_webhook(url: str, payload: dict) -> bool:
    try:
        data = json.dumps(payload).encode()
        req = urllib.request.Request(
            url,
            data=data,
            headers={
                "Content-Type": "application/json",
                "User-Agent": "wylde-link-push/2",
            },
            method="POST",
        )
        with urllib.request.urlopen(req, timeout=_WEBHOOK_TIMEOUT) as r:
            return bool(200 <= r.status < 300)
    except (
        urllib.error.URLError,
        urllib.error.HTTPError,
        TimeoutError,
        OSError,
    ) as exc:
        logger.debug("webhook %s failed: %s", url, exc)
        return False
