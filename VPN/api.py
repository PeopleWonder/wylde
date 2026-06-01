"""Wylde VPN — management API (Flask).

Surface trimmed in Phase 9 — what stays here is the **VPN-internal**
control plane: tunnel lifecycle, link pairing, restart. Everything else
that used to live alongside this file moved to the unified Gateway:

* The whole ``/api/link/mobile/*`` mobile-proxy surface — superseded by
  the FastAPI routes under ``Wylde/Gateway/routes/``. The peer-key auth
  decorator collapsed into Gateway's ``require_local`` once principle
  #16 (single auth boundary at the WyldeLink VPN tunnel) made the
  per-route credential redundant.
* The ``/api/link/push/*`` external surface — Gateway's
  ``routes/push.py`` calls into ``peers.push`` over IPC instead.
* The Consul registration block in :func:`main` — Wylde uses
  manifest-based discovery now.

What's left is pipe + HTTP-callable from the rest of Wylde:
``/health``, ``/api/vpn/{status,enable,disable,keygen}``,
``/api/link/{status,pair,register,stun,peers,connect,qr/<token>,
config[GET|PATCH]}``, ``/api/restart``.
"""

from __future__ import annotations

import logging
import os
import sys
import threading
import time
from datetime import datetime, timezone
from pathlib import Path

from typing import Any

import yaml
from flask import Flask, jsonify, request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import config  # noqa: E402
from peers import pairing, rehydrate  # noqa: E402
from tunnel import wireguard as models  # noqa: E402

CONFIG_PATH = Path(__file__).resolve().parent / "config.yaml"

try:
    from Core.shared.logging_setup import configure_logging
except ImportError:
    configure_logging = None  # type: ignore[assignment]
if configure_logging is not None:
    configure_logging(service="wylde-vpn")
logger = logging.getLogger(__name__)
app = Flask(__name__)


# ── Health ────────────────────────────────────────────────────────────────────


@app.route("/health", methods=["GET"])
def health() -> Any:
    st = models.get_vpn_status()
    return jsonify(
        {
            "status": "healthy",
            "service": "wylde-vpn",
            "vpn_connected": st.get("connected", False),
            "interface_up": st.get("interface_up", False),
            "link_up": models.link_iface_up(),
            "timestamp": datetime.now(timezone.utc).isoformat(),
        }
    )


# ── VPN control ───────────────────────────────────────────────────────────────


@app.route("/api/vpn/status", methods=["GET"])
def vpn_status() -> Any:
    return jsonify(models.get_vpn_status())


@app.route("/api/vpn/enable", methods=["POST"])
def vpn_enable() -> Any:
    data = request.get_json() or {}
    try:
        result = models.enable_vpn(
            endpoint=data.get("endpoint"),
            peer_pubkey=data.get("peer_pubkey"),
            private_key=data.get("private_key"),
            tunnel_addr=data.get("tunnel_addr"),
            dns=data.get("dns"),
            allowed_ips=data.get("allowed_ips"),
        )
        return jsonify(result)
    except ValueError as exc:
        return jsonify({"error": str(exc)}), 400
    except Exception as exc:
        logger.error("VPN enable failed: %s", exc)
        return jsonify({"error": str(exc)}), 500


@app.route("/api/vpn/disable", methods=["POST"])
def vpn_disable() -> Any:
    try:
        return jsonify(models.disable_vpn())
    except Exception as exc:
        logger.error("VPN disable failed: %s", exc)
        return jsonify({"error": str(exc)}), 500


@app.route("/api/vpn/keygen", methods=["POST"])
def vpn_keygen() -> Any:
    """Generate a WireGuard key pair. Store private key in VPN_PRIVATE_KEY env."""
    priv, pub = models.generate_keypair()
    return jsonify({"private_key": priv, "public_key": pub})


# ── WyldeLink control plane ───────────────────────────────────────────────────


@app.route("/api/link/status", methods=["GET"])
def link_status() -> Any:
    return jsonify(models.get_link_status())


@app.route("/api/link/pair", methods=["POST"])
def link_pair() -> Any:
    remote_ip = request.headers.get("X-Forwarded-For", request.remote_addr or "")
    body, status = pairing.handle_action("pair", request.get_json() or {}, remote_ip)
    return jsonify(body), status


@app.route("/api/link/register", methods=["POST"])
def link_register() -> Any:
    body, status = pairing.handle_action("register", request.get_json() or {})
    return jsonify(body), status


@app.route("/api/link/stun", methods=["GET"])
def link_stun() -> Any:
    body, status = pairing.handle_action("stun", {})
    return jsonify(body), status


@app.route("/api/link/peers", methods=["GET"])
def link_peers() -> Any:
    body, status = pairing.handle_action("peers", {})
    return jsonify(body), status


@app.route("/api/link/peers/remove", methods=["POST"])
def link_remove_peer() -> Any:
    body, status = pairing.handle_action("remove", request.get_json() or {})
    return jsonify(body), status


@app.route("/api/link/connect", methods=["POST"])
def link_connect() -> Any:
    body, status = pairing.handle_action("connect", request.get_json() or {})
    return jsonify(body), status


@app.route("/api/link/qr/<token>", methods=["GET"])
def link_qr(token: str) -> Any:
    """Return the QR code SVG for a pending pairing token."""
    payload, content_type, status = pairing.render_qr_for_token(token)
    if content_type == "application/json":
        return jsonify(payload), status
    from flask import make_response

    resp = make_response(payload)
    resp.headers["Content-Type"] = content_type
    return resp, status


# ── Config plane (GUI-editable settings) ──────────────────────────────────────

_LINK_PATCHABLE = {
    "enabled": bool,
    "public_host": str,
    "listen_port": int,
    "tunnel_addr": str,
    "peer_subnet": str,
    "token_ttl": int,
    "pair_rate_max": int,
    "pair_rate_win": int,
}
_LINK_RELAY_PATCHABLE = {
    "host": str,
    "port": int,
    "user": str,
    "password": str,
    "realm": str,
    "lifetime": int,
}


def _read_yaml_config() -> dict:
    if not CONFIG_PATH.exists():
        return {}
    try:
        with open(CONFIG_PATH, "r", encoding="utf-8") as f:
            return yaml.safe_load(f) or {}
    except (OSError, yaml.YAMLError) as exc:
        logger.error("Failed to read %s: %s", CONFIG_PATH, exc)
        return {}


def _write_yaml_config(cfg: dict) -> None:
    tmp = CONFIG_PATH.with_suffix(".yaml.tmp")
    with open(tmp, "w", encoding="utf-8") as f:
        yaml.safe_dump(cfg, f, sort_keys=False, default_flow_style=False)
    os.replace(tmp, CONFIG_PATH)


def _coerce(value: Any, kind: type) -> Any:
    if kind is bool:
        if isinstance(value, bool):
            return value
        if isinstance(value, str):
            return value.strip().lower() in ("true", "1", "yes", "on")
        return bool(value)
    if kind is int:
        return int(value)
    return str(value) if value is not None else ""


def _link_view(cfg: dict) -> dict:
    link = cfg.get("link", {}) or {}
    relay = link.get("relay", {}) or {}
    return {
        "enabled": bool(link.get("enabled", False)),
        "public_host": link.get("public_host", "") or "",
        "listen_port": int(link.get("listen_port", 51821)),
        "tunnel_addr": link.get("tunnel_addr", "192.0.2.1/24"),
        "peer_subnet": link.get("peer_subnet", "192.0.2.0/24"),
        "token_ttl": int(link.get("token_ttl", 300)),
        "pair_rate_max": int(link.get("pair_rate_max", 5)),
        "pair_rate_win": int(link.get("pair_rate_win", 60)),
        "relay": {
            "host": relay.get("host", "") or "",
            "port": int(relay.get("port", 3478)),
            "user": relay.get("user", "wylde"),
            "password": relay.get("password", "") or "",
            "realm": relay.get("realm", "wylde.local"),
            "lifetime": int(relay.get("lifetime", 600)),
        },
    }


@app.route("/api/link/config", methods=["GET"])
def link_get_config() -> Any:
    cfg = _read_yaml_config()
    view = _link_view(cfg)
    view["runtime"] = {
        "enabled": config.LINK_ENABLED,
        "listen_port": config.LINK_LISTEN_PORT,
        "public_host": config.LINK_PUBLIC_HOST,
    }
    view["restart_required"] = (
        view["enabled"] != view["runtime"]["enabled"]
        or view["listen_port"] != view["runtime"]["listen_port"]
        or view["public_host"] != view["runtime"]["public_host"]
    )
    return jsonify(view)


@app.route("/api/link/config", methods=["PATCH"])
def link_patch_config() -> Any:
    body = request.get_json(silent=True) or {}
    if not isinstance(body, dict):
        return jsonify({"error": "request body must be an object"}), 400

    cfg = _read_yaml_config()
    link = cfg.setdefault("link", {})
    relay = link.setdefault("relay", {})

    invalid = []
    for key, value in body.items():
        if key == "relay" and isinstance(value, dict):
            for rk, rv in value.items():
                kind = _LINK_RELAY_PATCHABLE.get(rk)
                if kind is None:
                    invalid.append(f"relay.{rk}")
                    continue
                try:
                    relay[rk] = _coerce(rv, kind)
                except (TypeError, ValueError):
                    invalid.append(f"relay.{rk}")
            continue
        kind = _LINK_PATCHABLE.get(key)
        if kind is None:
            invalid.append(key)
            continue
        try:
            link[key] = _coerce(value, kind)
        except (TypeError, ValueError):
            invalid.append(key)

    if invalid:
        return jsonify(
            {"error": f"invalid or unknown fields: {', '.join(invalid)}"}
        ), 400

    try:
        _write_yaml_config(cfg)
    except OSError as exc:
        logger.error("Failed to write %s: %s", CONFIG_PATH, exc)
        return jsonify({"error": f"config write failed: {exc}"}), 500

    view = _link_view(cfg)
    view["runtime"] = {
        "enabled": config.LINK_ENABLED,
        "listen_port": config.LINK_LISTEN_PORT,
        "public_host": config.LINK_PUBLIC_HOST,
    }
    view["restart_required"] = (
        view["enabled"] != view["runtime"]["enabled"]
        or view["listen_port"] != view["runtime"]["listen_port"]
        or view["public_host"] != view["runtime"]["public_host"]
    )
    return jsonify(view)


@app.route("/api/restart", methods=["POST"])
def restart() -> Any:
    """Exit the process so the launching wrapper can respawn it with the
    freshly-written config. Deferred ~250ms so the response can flush."""
    delay_ms = 250

    def _suicide() -> None:
        time.sleep(delay_ms / 1000.0)
        os._exit(0)

    threading.Thread(target=_suicide, name="restart-exit", daemon=True).start()
    return jsonify({"status": "restarting", "delay_ms": delay_ms})


# ── Error handlers ────────────────────────────────────────────────────────────


@app.errorhandler(404)
def not_found(e: Any) -> Any:
    return jsonify({"error": "not found"}), 404


@app.errorhandler(500)
def internal_error(e: Any) -> Any:
    logger.error("Internal error: %s", e)
    return jsonify({"error": "internal server error"}), 500


# ── Discovery side-cars ───────────────────────────────────────────────────────


def _start_discovery_if_enabled() -> None:
    """Start mDNS advertisement and (optionally) the DDNS updater + endpoint poller."""
    cfg = _read_yaml_config()
    discovery = cfg.get("discovery") or {}

    mdns_cfg = discovery.get("mdns", {}) or {}
    if mdns_cfg.get("enabled", False):
        try:
            from discovery.mdns import MdnsAdvertiser

            adv = MdnsAdvertiser(
                hostname=os.environ.get("COMPUTERNAME", "wylde-desktop").lower(),
                port=config.LINK_LISTEN_PORT,
                service_name=(
                    mdns_cfg.get("service_name", "_wylde-link._udp") + ".local."
                ),
                instance_name=mdns_cfg.get("instance_name", "Wylde Desktop"),
            )
            adv.start()
        except Exception as exc:  # noqa: BLE001
            logger.warning("mdns: failed to start: %s", exc)

    nat_cfg = cfg.get("nat", {}) or {}
    interval_s = int(nat_cfg.get("stun_probe_interval_s", 300))
    if interval_s > 0:
        try:
            from nat.endpoint_updater import EndpointUpdater

            updater = EndpointUpdater(
                stun_servers=config.LINK_STUN_SERVERS,
                interval_s=interval_s,
                on_change=_endpoint_change_callback,
            )
            updater.start()
        except Exception as exc:  # noqa: BLE001
            logger.warning("endpoint-updater: failed to start: %s", exc)


def _endpoint_change_callback(previous: str, current: str) -> None:
    """Notify every paired peer that the home endpoint moved."""
    try:
        from peers import push as push_store

        push_store.broadcast(
            "WyldeLink endpoint changed",
            f"New endpoint: {current}",
            {"type": "endpoint_change", "previous": previous, "new_endpoint": current},
        )
    except Exception as exc:  # noqa: BLE001
        logger.warning("endpoint-change broadcast failed: %s", exc)


# ── Main ──────────────────────────────────────────────────────────────────────


def main() -> None:
    logger.info("=" * 70)
    logger.info("Wylde VPN — management API")
    logger.info("  Port : %d", config.PORT)
    logger.info("  VPN  : %s", "enabled" if config.VPN_ENABLED else "disabled (opt-in)")
    logger.info("  Link : %s", "enabled" if config.LINK_ENABLED else "disabled")
    logger.info("=" * 70)

    # Apply wg1 peer isolation rules if the interface is already up.
    # Skipped on non-Linux hosts where wg-quick/iptables are unavailable.
    try:
        if config.LINK_ENABLED and models.link_iface_up():
            models.set_wg1_isolation()
            rehydrate.rehydrate_peers()
    except Exception as exc:  # noqa: BLE001
        logger.warning(
            "Skipping wg1 isolation/rehydrate (likely non-Linux host): %s", exc
        )

    # Boot the periodic last-seen refresh so the dashboard reflects live activity.
    rehydrate.start_last_seen_worker()

    # Discovery side-cars (mDNS, endpoint poller). The Gateway runs as its own
    # service now, so we do NOT start it here — the launcher spawns Gateway
    # directly off its own manifest.
    _start_discovery_if_enabled()

    try:
        from Core.shared import ipc

        ipc.serve("wylde-vpn", app, port=config.PORT, host="127.0.0.1", register=False)
    except ImportError:
        try:
            import ipc  # type: ignore

            ipc.serve(
                "wylde-vpn", app, port=config.PORT, host="127.0.0.1", register=False
            )
        except ImportError:
            app.run(host="127.0.0.1", port=config.PORT, debug=False, threaded=True)


if __name__ == "__main__":
    main()
