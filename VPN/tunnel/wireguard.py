"""WireGuard tunnel state — wg0 (outbound VPN) + wg1 (WyldeLink inbound).

Lifecycle helpers wrap the system ``wg`` and ``wg-quick`` binaries; the
process must run as root on Linux. The Windows path is a stub — the
helpers return errors when the binaries are absent so the management
API can still serve health checks for monitoring purposes.

Lifecycle:
    write wg0.conf
    wg-quick up wg0
    mark active        ←  /run/wylde-vpn-active for dns_stub

Reverse sequence on disable.
"""

from __future__ import annotations

import logging
import os
import subprocess
import threading
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Tuple

import config

logger = logging.getLogger(__name__)
_lock = threading.Lock()

_vpn: dict[str, Any] = {
    "enabled": False,
    "connected": False,
    "connected_at": None,
    "endpoint": None,
    "tunnel_ip": None,
    "public_key": None,
    "error": None,
}

_link: dict[str, Any] = {
    "enabled": False,
    "listen_port": config.LINK_LISTEN_PORT,
    "public_key": None,
}


# ── Public API ────────────────────────────────────────────────────────────────


def get_vpn_status() -> dict:
    with _lock:
        st = dict(_vpn)
    st["interface_up"] = _iface_exists(config.WG_IFACE_OUTBOUND)
    if st["interface_up"]:
        st["rx_bytes"], st["tx_bytes"] = _wg_transfer(config.WG_IFACE_OUTBOUND)
    return st


def get_link_status() -> dict:
    with _lock:
        st = dict(_link)
    st["interface_up"] = _iface_exists(config.WG_IFACE_INBOUND)
    return st


def enable_vpn(
    endpoint: str | None = None,
    peer_pubkey: str | None = None,
    private_key: str | None = None,
    tunnel_addr: str | None = None,
    dns: str | None = None,
    allowed_ips: str | None = None,
) -> dict:
    """Write wg0 config and bring up the tunnel via wg-quick."""
    with _lock:
        if _vpn["connected"]:
            return {"status": "already_connected", "endpoint": _vpn["endpoint"]}

        ep = endpoint or config.VPN_ENDPOINT
        pk = peer_pubkey or config.VPN_PEER_PUBKEY
        priv = private_key or config.VPN_PRIVATE_KEY

        if not ep or not pk:
            raise ValueError("endpoint and peer_pubkey are required")

        if not priv:
            priv = _wg_genkey()

        pub = _wg_pubkey(priv)

        _write_wg0(
            priv,
            pk,
            ep,
            tunnel_addr or config.VPN_TUNNEL_ADDR,
            dns or config.VPN_DNS,
            allowed_ips or config.VPN_ALLOWED_IPS,
        )
        _wg_quick_up(config.WG_IFACE_OUTBOUND)
        _mark_active()

        _vpn.update(
            {
                "enabled": True,
                "connected": True,
                "connected_at": datetime.now(timezone.utc).isoformat(),
                "endpoint": ep,
                "tunnel_ip": (tunnel_addr or config.VPN_TUNNEL_ADDR).split("/")[0],
                "public_key": pub,
                "error": None,
            }
        )
        return {"status": "connected", "endpoint": ep, "public_key": pub}


def disable_vpn() -> dict:
    """Bring down wg0."""
    with _lock:
        if not _vpn["enabled"] and not _iface_exists(config.WG_IFACE_OUTBOUND):
            return {"status": "not_connected"}

        _wg_quick_down(config.WG_IFACE_OUTBOUND)
        _unmark_active()

        _vpn.update(
            {
                "enabled": False,
                "connected": False,
                "connected_at": None,
                "endpoint": None,
                "tunnel_ip": None,
                "error": None,
            }
        )
        return {"status": "disconnected"}


def generate_keypair() -> Tuple[str, str]:
    """Return ``(private_key, public_key)`` suitable for VPN_PRIVATE_KEY env."""
    priv = _wg_genkey()
    return priv, _wg_pubkey(priv)


# ── WyldeLink (wg1) peer management ──────────────────────────────────────────


def add_wg1_peer(public_key: str, tunnel_ip: str, keepalive: int = 25) -> None:
    """Add or update a peer on the wg1 interface via ``wg set``."""
    subprocess.run(
        [
            "wg",
            "set",
            config.WG_IFACE_INBOUND,
            "peer",
            public_key,
            "allowed-ips",
            f"{tunnel_ip}/32",
            "persistent-keepalive",
            str(keepalive),
        ],
        check=True,
    )
    logger.info("wg1 peer added: %s → %s", public_key[:16], tunnel_ip)


def remove_wg1_peer(public_key: str) -> None:
    subprocess.run(
        ["wg", "set", config.WG_IFACE_INBOUND, "peer", public_key, "--remove"],
        check=False,
    )
    logger.info("wg1 peer removed: %s", public_key[:16])


def get_wg1_public_key() -> str:
    """Return wg1 server public key, or empty string if interface is not up."""
    try:
        r = subprocess.run(
            ["wg", "show", config.WG_IFACE_INBOUND, "public-key"],
            capture_output=True,
            text=True,
        )
        return r.stdout.strip()
    except Exception:
        return ""


def get_wg1_latest_handshakes() -> dict:
    """Return ``{ pubkey: epoch_seconds }`` from ``wg show wg1 latest-handshakes``."""
    try:
        r = subprocess.run(
            ["wg", "show", config.WG_IFACE_INBOUND, "latest-handshakes"],
            capture_output=True,
            text=True,
        )
        result = {}
        for line in r.stdout.strip().splitlines():
            parts = line.split()
            if len(parts) >= 2:
                result[parts[0]] = int(parts[1])
        return result
    except Exception:
        return {}


def link_iface_up() -> bool:
    return _iface_exists(config.WG_IFACE_INBOUND)


def set_wg1_isolation() -> None:
    """Apply iptables FORWARD rules so wg1 peers can only reach Wylde services."""
    cmds = [
        [
            "iptables",
            "-A",
            "FORWARD",
            "-i",
            config.WG_IFACE_INBOUND,
            "-m",
            "conntrack",
            "--ctstate",
            "ESTABLISHED,RELATED",
            "-j",
            "ACCEPT",
        ],
        *[
            [
                "iptables",
                "-A",
                "FORWARD",
                "-i",
                config.WG_IFACE_INBOUND,
                "-d",
                net,
                "-j",
                "ACCEPT",
            ]
            for net in config.BYPASS_RANGES
        ],
        ["iptables", "-A", "FORWARD", "-i", config.WG_IFACE_INBOUND, "-j", "DROP"],
        [
            "iptables",
            "-t",
            "nat",
            "-A",
            "POSTROUTING",
            "-s",
            config.LINK_PEER_SUBNET,
            "-o",
            "eth0",
            "-j",
            "MASQUERADE",
        ],
    ]
    for cmd in cmds:
        subprocess.run(cmd, check=False)
    logger.info("wg1 isolation rules applied (peers restricted to Wylde services).")


# ── WireGuard helpers (private) ───────────────────────────────────────────────


def _wg_genkey() -> str:
    r = subprocess.run(["wg", "genkey"], capture_output=True, text=True, check=True)
    return r.stdout.strip()


def _wg_pubkey(private_key: str) -> str:
    r = subprocess.run(
        ["wg", "pubkey"],
        input=private_key,
        capture_output=True,
        text=True,
        check=True,
    )
    return r.stdout.strip()


def _write_wg0(
    priv: str,
    peer_pub: str,
    endpoint: str,
    tunnel_addr: str,
    dns: str,
    allowed_ips: str,
) -> None:
    Path(config.WG_CONFIG_DIR).mkdir(parents=True, exist_ok=True)
    body = (
        "[Interface]\n"
        f"PrivateKey = {priv}\n"
        f"Address = {tunnel_addr}\n"
        f"DNS = {dns}\n"
        f"PostUp   = iptables -A OUTPUT -o {config.WG_IFACE_OUTBOUND} -j ACCEPT\n"
        f"PreDown  = iptables -D OUTPUT -o {config.WG_IFACE_OUTBOUND} -j ACCEPT\n"
        "\n"
        "[Peer]\n"
        f"PublicKey = {peer_pub}\n"
        f"AllowedIPs = {allowed_ips}\n"
        f"Endpoint = {endpoint}\n"
        "PersistentKeepalive = 25\n"
    )
    p = Path(config.WG_CONFIG_OUTBOUND)
    p.write_text(body)
    p.chmod(0o600)
    logger.info("wg0 config written → %s", p)


def _wg_quick_up(iface: str) -> None:
    env = {**os.environ, "WG_QUICK_USERSPACE_IMPLEMENTATION": "boringtun"}
    r = subprocess.run(
        ["wg-quick", "up", iface], capture_output=True, text=True, env=env
    )
    if r.returncode != 0:
        raise RuntimeError(f"wg-quick up {iface}: {r.stderr.strip()}")
    logger.info("wg-quick up %s: OK", iface)


def _wg_quick_down(iface: str) -> None:
    env = {**os.environ, "WG_QUICK_USERSPACE_IMPLEMENTATION": "boringtun"}
    r = subprocess.run(
        ["wg-quick", "down", iface], capture_output=True, text=True, env=env
    )
    if r.returncode != 0:
        logger.warning("wg-quick down %s: %s", iface, r.stderr.strip())
    else:
        logger.info("wg-quick down %s: OK", iface)


# ── State marker (IPC with dns_stub) ─────────────────────────────────────────


def _mark_active() -> None:
    Path(config.VPN_ACTIVE_FILE).touch()


def _unmark_active() -> None:
    try:
        Path(config.VPN_ACTIVE_FILE).unlink(missing_ok=True)
    except Exception:
        pass


# ── Interface inspection ──────────────────────────────────────────────────────


def _iface_exists(iface: str) -> bool:
    try:
        r = subprocess.run(
            ["ip", "link", "show", iface], capture_output=True, check=False
        )
        return r.returncode == 0
    except Exception:
        return False


def _wg_transfer(iface: str) -> Tuple[int, int]:
    """Return ``(rx_bytes, tx_bytes)`` from ``wg show <iface> transfer``."""
    try:
        r = subprocess.run(
            ["wg", "show", iface, "transfer"], capture_output=True, text=True
        )
        if r.returncode == 0:
            parts = r.stdout.strip().split()
            if len(parts) >= 3:
                return int(parts[1]), int(parts[2])
    except Exception:
        pass
    return 0, 0
