"""Tool-call wrappers for VPN lifecycle (status / enable / disable / keygen).

The LLM agent loop calls these via the harness tool registry; each
function takes a single ``params`` dict and returns a JSON-serialisable
dict. Behaviour mirrors the management API routes — they're the same
operations exposed under a different surface.
"""

from __future__ import annotations

from . import wireguard


def run_vpn_status(_params: dict) -> dict:
    return wireguard.get_vpn_status()


def run_vpn_enable(params: dict) -> dict:
    return wireguard.enable_vpn(
        endpoint=params.get("endpoint"),
        peer_pubkey=params.get("peer_pubkey"),
        private_key=params.get("private_key"),
        tunnel_addr=params.get("tunnel_addr"),
        dns=params.get("dns"),
        allowed_ips=params.get("allowed_ips"),
    )


def run_vpn_disable(_params: dict) -> dict:
    return wireguard.disable_vpn()


def run_vpn_keygen(_params: dict) -> dict:
    priv, pub = wireguard.generate_keypair()
    return {"private_key": priv, "public_key": pub}


__all__ = [
    "run_vpn_disable",
    "run_vpn_enable",
    "run_vpn_keygen",
    "run_vpn_status",
]
