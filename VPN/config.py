"""wylde-vpn — environment config and constants.

Read at import time and never mutated. The launcher applies values from
``config.yaml`` into the process environment before any of these get
read, so editing the YAML is the supported configuration channel.
"""

import os

PORT = int(os.getenv("PORT", "8020"))

# ── Outbound VPN (wg0) ────────────────────────────────────────────────────────
# All opt-in, nothing connects unless VPN_ENABLED=true is explicitly set.
VPN_ENABLED = os.getenv("VPN_ENABLED", "false").lower() == "true"
VPN_ENDPOINT = os.getenv("VPN_ENDPOINT", "")  # host:port of VPN server
VPN_PEER_PUBKEY = os.getenv("VPN_PEER_PUBKEY", "")  # VPN server's WireGuard public key
VPN_PRIVATE_KEY = os.getenv(
    "VPN_PRIVATE_KEY", ""
)  # our private key (generated if blank)
VPN_TUNNEL_ADDR = os.getenv("VPN_TUNNEL_ADDR", "10.8.0.2/24")
VPN_DNS = os.getenv("VPN_DNS", "1.1.1.1")
VPN_ALLOWED_IPS = os.getenv("VPN_ALLOWED_IPS", "0.0.0.0/0, ::/0")

# ── WyldeLink inbound (wg1) ───────────────────────────────────────────────────
# Remote-access layer; also opt-in.
LINK_ENABLED = os.getenv("LINK_ENABLED", "false").lower() == "true"
LINK_PRIVATE_KEY = os.getenv("LINK_PRIVATE_KEY", "")
LINK_TUNNEL_ADDR = os.getenv("LINK_TUNNEL_ADDR", "192.0.2.1/24")
LINK_LISTEN_PORT = int(os.getenv("LINK_LISTEN_PORT", "51821"))
LINK_STUN_SERVERS = os.getenv(
    "LINK_STUN_SERVERS", "stun.l.google.com:19302,stun1.l.google.com:19302"
).split(",")

# ── WireGuard paths ───────────────────────────────────────────────────────────
WG_CONFIG_DIR = "/etc/wireguard"
WG_CONFIG_OUTBOUND = "/etc/wireguard/wg0.conf"
WG_CONFIG_INBOUND = "/etc/wireguard/wg1.conf"
WG_IFACE_OUTBOUND = "wg0"
WG_IFACE_INBOUND = "wg1"

# ── wg1 isolation bypass ranges (Docker/LAN, never routed through VPN) ──────
# Used by tunnel.wireguard.set_wg1_isolation to keep these reachable from
# WyldeLink peers without being forced through the wg0 outbound tunnel.
BYPASS_RANGES = [
    "10.0.0.0/8",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "127.0.0.0/8",
]

# ── Runtime state file (read by dns_stub) ─────────────────────────────────────
VPN_ACTIVE_FILE = "/run/wylde-vpn-active"

# ── DNS stub ──────────────────────────────────────────────────────────────────
DNS_STUB_HOST = os.getenv("DNS_STUB_HOST", "0.0.0.0")
DNS_STUB_PORT = int(os.getenv("DNS_STUB_PORT", "5300"))

# ── WyldeLink Phase 2 ─────────────────────────────────────────────────────────
LINK_DATA_DIR = os.getenv("LINK_DATA_DIR", "/data/wylde-link")
LINK_TOKEN_TTL = int(os.getenv("LINK_TOKEN_TTL", "300"))
LINK_PAIR_RATE_MAX = int(os.getenv("LINK_PAIR_RATE_MAX", "5"))
LINK_PAIR_RATE_WIN = int(os.getenv("LINK_PAIR_RATE_WIN", "60"))
LINK_PEER_SUBNET = os.getenv("LINK_PEER_SUBNET", "192.0.2.0/24")
LINK_RELAY_HOST = os.getenv("LINK_RELAY_HOST", "")
LINK_RELAY_PORT = int(os.getenv("LINK_RELAY_PORT", "3478"))
LINK_RELAY_USER = os.getenv("LINK_RELAY_USER", "wylde")
LINK_RELAY_PASS = os.getenv("LINK_RELAY_PASS", "")
LINK_RELAY_REALM = os.getenv("LINK_RELAY_REALM", "wylde.local")
LINK_RELAY_LIFETIME = int(os.getenv("LINK_RELAY_LIFETIME", "600"))
# Override the public-facing endpoint advertised to mobile clients.
# On WSL2 set this to Windows-host-external-IP:51821.
LINK_PUBLIC_HOST = os.getenv("LINK_PUBLIC_HOST", "")
