"""Peer pairing — tokens, QR, registration, connect.

Split out of the legacy ``tools/wylde_link.py``. This module owns the
mobile-app pairing handshake:

* Issue short-lived pairing tokens (``LINK_TOKEN_TTL`` seconds).
* Render a QR-encodable URI (``wylde://link/pair?token=...``).
* Accept registration calls that exchange a pairing token for a peer
  record + tunnel IP allocation.
* Push the peer onto wg1 via the WireGuard control surface.
* Compose connection-params blocks (server pubkey, endpoint, port) the
  mobile client needs to bring its tunnel up.

Rate limiting on pair-token issuance lives here too — pairing is the
only path where an unauthenticated remote IP can drive state changes,
so the bucket sits next to the issuance logic rather than out in
middleware.

The :data:`_WYLDE_SERVICES` literal that used to live next to this
code (a hardcoded list of service ports) is gone — the Gateway's
``/api/services`` route reads ``data/manifests/*.json`` and supersedes
it.
"""

from __future__ import annotations

import io
import logging
import secrets
import threading
import time
from collections import defaultdict
from datetime import datetime, timezone
from typing import Tuple

import config

from . import store as peer_store
from nat import stun as stun_client
from nat import turn as turn_client
from tunnel import wireguard as models

try:
    import qrcode
    import qrcode.image.svg

    _QR_OK = True
except ImportError:
    _QR_OK = False

logger = logging.getLogger(__name__)

# In-memory token table: { token: { label, created_at, expires_at, ip, uri } }
_tokens: dict = {}
_tokens_lock = threading.Lock()

# Per-IP pairing rate limiter: { ip: [timestamp, ...] }
_rate_buckets: dict = defaultdict(list)
_rate_lock = threading.Lock()

# STUN endpoint cache so we don't hammer STUN on every request.
_stun_cache: dict = {}
_STUN_CACHE_TTL = 120


# ── Dispatch surface (used by api.py and tool-call wrappers) ──────────────────


def handle_action(action: str, data: dict, remote_ip: str = "") -> Tuple[dict, int]:
    """Dispatch a link-control action by name. Returns ``(body, status)``."""
    dispatch = {
        "register": lambda: register_peer(data),
        "pair": lambda: generate_pair_token(data, remote_ip),
        "stun": lambda: (stun_info(), 200),
        "peers": lambda: ({"peers": list_peers()}, 200),
        "remove": lambda: remove_peer_action(data),
        "connect": lambda: connect(data),
    }
    fn = dispatch.get(action)
    if fn is None:
        return {"error": f"unknown action: {action}"}, 400
    return fn()


def render_qr_for_token(token: str) -> Tuple[bytes | dict, str, int]:
    """Return ``(payload, content_type, status)`` for the QR endpoint."""
    with _tokens_lock:
        entry = _tokens.get(token)
    if not entry or time.time() > entry["expires_at"]:
        return ({"error": "token not found or expired"}, "application/json", 404)
    if not _QR_OK:
        return (
            {"error": "qrcode library unavailable", "uri": entry["uri"]},
            "application/json",
            503,
        )
    svg = _make_qr_svg(entry["uri"])
    if svg is None:
        return (
            {"error": "QR generation failed", "uri": entry["uri"]},
            "application/json",
            500,
        )
    return (svg.encode(), "image/svg+xml", 200)


# ── Tool-call shapes (LLM agent loop) ─────────────────────────────────────────


def run_link_status(_params: dict) -> dict:
    peers = peer_store.list_peers()
    return {
        "enabled": config.LINK_ENABLED,
        "interface_up": models.link_iface_up(),
        "listen_port": config.LINK_LISTEN_PORT,
        "server_pubkey": models.get_wg1_public_key(),
        "peer_count": len(peers),
        "phase": 2,
    }


def run_link_pair(params: dict) -> dict:
    result, _ = generate_pair_token(params, "")
    return result


def run_link_register(params: dict) -> dict:
    result, _ = register_peer(params)
    return result


def run_link_peers(_params: dict) -> dict:
    return {"peers": list_peers()}


def run_link_remove(params: dict) -> dict:
    result, _ = remove_peer_action(params)
    return result


# ── Peer registration ─────────────────────────────────────────────────────────


def register_peer(data: dict) -> Tuple[dict, int]:
    pub_key = (data.get("public_key") or "").strip()
    label = data.get("label", "")
    token = (data.get("token") or "").strip()
    allowed = data.get("allowed_services", [])

    if not pub_key:
        return {"error": "public_key required"}, 400
    if not token:
        return {"error": "pairing token required"}, 400

    with _tokens_lock:
        entry = _tokens.get(token)
        if not entry:
            return {"error": "invalid or expired pairing token"}, 400
        if time.time() > entry["expires_at"]:
            del _tokens[token]
            return {"error": "pairing token expired"}, 400
        del _tokens[token]

    existing = peer_store.get_peer(pub_key)
    if existing:
        _push_to_wg1(pub_key, existing["tunnel_ip"])
        return {
            "status": "already_registered",
            "peer": existing,
            **connection_params(),
        }, 200

    tunnel_ip = peer_store.next_tunnel_ip()
    if not tunnel_ip:
        return {"error": "peer address space exhausted"}, 503

    peer = {
        "public_key": pub_key,
        "label": label or pub_key[:16],
        "tunnel_ip": tunnel_ip,
        "registered_at": datetime.now(timezone.utc).isoformat(),
        "last_seen": None,
        "allowed_services": allowed,
    }
    peer_store.upsert_peer(pub_key, peer)
    _push_to_wg1(pub_key, tunnel_ip)
    logger.info("Peer registered: %s → %s", peer["label"], tunnel_ip)

    return {"status": "ok", "peer": peer, **connection_params()}, 201


def _push_to_wg1(pub_key: str, tunnel_ip: str) -> None:
    if not models.link_iface_up():
        logger.warning(
            "wg1 not up — peer %s queued (will activate on next connect)", pub_key[:16]
        )
        return
    try:
        models.add_wg1_peer(pub_key, tunnel_ip)
    except Exception as exc:
        logger.error("wg1 peer push failed: %s", exc)


# ── Pairing token ─────────────────────────────────────────────────────────────


def generate_pair_token(data: dict, remote_ip: str) -> Tuple[dict, int]:
    if remote_ip and not _rate_ok(remote_ip):
        return {"error": "too many pairing attempts, try again shortly"}, 429

    label = data.get("label", "unnamed")
    token = secrets.token_urlsafe(20)
    endpoint = _cached_endpoint()
    server_pubkey = models.get_wg1_public_key()

    uri = (
        f"wylde://link/pair"
        f"?token={token}"
        f"&endpoint={endpoint}"
        f"&server_pubkey={server_pubkey}"
        f"&version=2"
    )

    entry = {
        "label": label,
        "created_at": datetime.now(timezone.utc).isoformat(),
        "expires_at": time.time() + config.LINK_TOKEN_TTL,
        "ip": remote_ip,
        "uri": uri,
    }

    with _tokens_lock:
        expired = [t for t, e in _tokens.items() if time.time() > e["expires_at"]]
        for t in expired:
            del _tokens[t]
        _tokens[token] = entry

    logger.info('Pair token issued for "%s" (TTL %ds)', label, config.LINK_TOKEN_TTL)
    return {
        "token": token,
        "label": label,
        "uri": uri,
        "qr_svg": _make_qr_svg(uri),
        "expires_in": config.LINK_TOKEN_TTL,
        "endpoint": endpoint,
        "server_pubkey": server_pubkey,
        "stun_servers": config.LINK_STUN_SERVERS,
    }, 200


# ── STUN / NAT traversal ──────────────────────────────────────────────────────


def stun_info() -> dict:
    discovered = stun_client.discover_endpoint(
        config.LINK_STUN_SERVERS,
        local_port=config.LINK_LISTEN_PORT,
    )
    classification = stun_client.classify(
        config.LINK_STUN_SERVERS,
        local_port=config.LINK_LISTEN_PORT,
    )

    relay = None
    if config.LINK_RELAY_HOST:
        relay = {
            "host": config.LINK_RELAY_HOST,
            "port": config.LINK_RELAY_PORT,
            "username": config.LINK_RELAY_USER,
            "password": config.LINK_RELAY_PASS,
            "protocol": "TURN/UDP",
            "realm": config.LINK_RELAY_REALM,
        }
        allocation = turn_client.allocate(
            host=config.LINK_RELAY_HOST,
            port=config.LINK_RELAY_PORT,
            username=config.LINK_RELAY_USER,
            password=config.LINK_RELAY_PASS,
            realm=config.LINK_RELAY_REALM,
            lifetime=config.LINK_RELAY_LIFETIME,
        )
        relay["allocation"] = allocation
        relay["reachable"] = allocation is not None

    recommend = classification.get("recommend", "hole-punch")
    if recommend == "relay" and not (relay and relay.get("reachable")):
        recommend = "relay-unavailable"

    return {
        "stun_servers": config.LINK_STUN_SERVERS,
        "link_port": config.LINK_LISTEN_PORT,
        "discovered": discovered,
        "public_endpoint": _fmt_endpoint(discovered),
        "relay": relay,
        "nat_type": classification.get("nat_type", _nat_hint()),
        "nat_detail": _nat_hint(),
        "nat_mappings": classification.get("mappings", []),
        "recommend": recommend,
    }


def _cached_endpoint() -> str:
    if config.LINK_PUBLIC_HOST:
        return f"{config.LINK_PUBLIC_HOST}:{config.LINK_LISTEN_PORT}"
    now = time.time()
    if _stun_cache.get("expires", 0) > now:
        return str(_stun_cache.get("endpoint", f"<host>:{config.LINK_LISTEN_PORT}"))
    result = stun_client.discover_endpoint(config.LINK_STUN_SERVERS)
    ep = _fmt_endpoint(result)
    _stun_cache["endpoint"] = ep
    _stun_cache["expires"] = now + _STUN_CACHE_TTL
    return ep


def _fmt_endpoint(result: dict | None) -> str:
    if result:
        return f"{result['ip']}:{result['port']}"
    return f"<host>:{config.LINK_LISTEN_PORT}"


def _nat_hint() -> str:
    if config.LINK_PUBLIC_HOST:
        return "static-override"
    return "wsl2-double-nat"


# ── Peer list, removal, connect ───────────────────────────────────────────────


def list_peers() -> list:
    peers = peer_store.list_peers()
    now = time.time()
    hshakes = models.get_wg1_latest_handshakes()
    for p in peers:
        epoch = hshakes.get(p["public_key"], 0)
        if epoch:
            p["last_handshake"] = datetime.fromtimestamp(
                epoch, tz=timezone.utc
            ).isoformat()
            p["online"] = (now - epoch) < 180
        else:
            p["last_handshake"] = None
            p["online"] = False
    return peers


def remove_peer_action(data: dict) -> Tuple[dict, int]:
    pub_key = (data.get("public_key") or "").strip()
    if not pub_key:
        return {"error": "public_key required"}, 400
    if not peer_store.get_peer(pub_key):
        return {"error": "peer not found"}, 404
    try:
        models.remove_wg1_peer(pub_key)
    except Exception as exc:
        logger.warning("wg1 remove error: %s", exc)
    peer_store.remove_peer(pub_key)
    logger.info("Peer removed: %s", pub_key[:16])
    return {"status": "removed", "public_key": pub_key}, 200


def connect(data: dict) -> Tuple[dict, int]:
    """Mobile app calls this to confirm activation and receive WG config."""
    pub_key = (data.get("public_key") or "").strip()
    if not pub_key:
        return {"error": "public_key required"}, 400
    peer = peer_store.get_peer(pub_key)
    if not peer:
        return {"error": "peer not registered — call /api/link/register first"}, 404

    _push_to_wg1(pub_key, peer["tunnel_ip"])
    params = connection_params()

    wg_cfg = (
        "[Interface]\n"
        f"Address = {peer['tunnel_ip']}/32\n"
        f"DNS = {config.LINK_TUNNEL_ADDR.split('/')[0]}\n"
        "\n"
        "[Peer]\n"
        f"PublicKey = {params['server_pubkey']}\n"
        f"AllowedIPs = {config.LINK_PEER_SUBNET}\n"
        f"Endpoint = {params['endpoint']}\n"
        "PersistentKeepalive = 25\n"
    )

    return {"status": "connected", "peer": peer, "wg_config": wg_cfg, **params}, 200


# ── Connection params helper ──────────────────────────────────────────────────


def connection_params() -> dict:
    return {
        "server_pubkey": models.get_wg1_public_key(),
        "server_addr": config.LINK_TUNNEL_ADDR.split("/")[0],
        "endpoint": _cached_endpoint(),
        "listen_port": config.LINK_LISTEN_PORT,
        "peer_subnet": config.LINK_PEER_SUBNET,
    }


# ── QR + rate limiting helpers ────────────────────────────────────────────────


def _make_qr_svg(uri: str) -> str | None:
    if not _QR_OK:
        return None
    try:
        img = qrcode.make(uri, image_factory=qrcode.image.svg.SvgPathImage)
        buf = io.BytesIO()
        img.save(buf)
        return buf.getvalue().decode("utf-8")
    except Exception as exc:
        logger.warning("QR generation failed: %s", exc)
        return None


def _rate_ok(ip: str) -> bool:
    now = time.time()
    window = config.LINK_PAIR_RATE_WIN
    limit = config.LINK_PAIR_RATE_MAX
    with _rate_lock:
        bucket = _rate_buckets[ip]
        bucket[:] = [t for t in bucket if now - t < window]
        if len(bucket) >= limit:
            return False
        bucket.append(now)
    return True


__all__ = [
    "connect",
    "connection_params",
    "generate_pair_token",
    "handle_action",
    "list_peers",
    "register_peer",
    "remove_peer_action",
    "render_qr_for_token",
    "run_link_pair",
    "run_link_peers",
    "run_link_register",
    "run_link_remove",
    "run_link_status",
    "stun_info",
]
