"""STUN client — RFC 5389 binding requests + RFC-5780-style NAT classification.

Consolidates the two pre-Phase-9 implementations (the lite
``stun_client.py`` and the full ``stun_prober.py``) into one canonical
module. The full four-test classification is the default; a cheap
single-shot ``discover_endpoint`` is kept for the discovery path that
just wants a public IP fast.

Public API:

* :func:`discover_endpoint` — one binding request per server, return the
  first success. Used when the caller only needs a public IP/port.
* :func:`classify` — RFC 5780 four-test sequence. Returns NAT type
  (``open``, ``full-cone``, ``restricted-cone``, ``port-restricted``,
  ``symmetric``, ``blocked``) and a recommended traversal strategy
  (``direct``, ``hole-punch``, ``relay``).
* :func:`punch_hole` — UDP empty-datagram burst to coax NAT mappings.
  Used when the recommendation comes back ``hole-punch``.

The hole-punch helper lives here rather than in :mod:`hole_puncher`
because the discovery path frequently does ``classify → punch_hole``
back-to-back; keeping both functions in the same module avoids a
circular import. The richer coordination helper in
:mod:`hole_puncher` (``punch``) is still used for paired peer punches
where both sides know each other's mapped endpoint.
"""

from __future__ import annotations

import logging
import secrets
import socket
import struct
import time
from dataclasses import dataclass
from typing import List, Optional, Tuple

logger = logging.getLogger(__name__)

# RFC 5389 magic cookie & message types
_MAGIC = 0x2112A442
_BIND_REQUEST = 0x0001
_BIND_RESPONSE = 0x0101

# RFC 5389 / 5780 attribute types
_ATTR_MAPPED = 0x0001
_ATTR_XOR_MAPPED = 0x0020
_ATTR_CHANGE_REQUEST = 0x0003
_ATTR_OTHER_ADDRESS = 0x802C


@dataclass
class StunResult:
    server: str
    mapped_ip: str
    mapped_port: int
    other_address: Optional[Tuple[str, int]] = None
    rtt_ms: float = 0.0


# ── Wire helpers ──────────────────────────────────────────────────────────────


def _parse_server(server: str) -> Tuple[str, int]:
    """Split ``host:port`` (default port 3478)."""
    if ":" in server:
        h, p = server.rsplit(":", 1)
        return h, int(p)
    return server, 3478


def _encode_request(transaction_id: bytes, change_flags: int = 0) -> bytes:
    attrs = b""
    if change_flags:
        attrs += struct.pack("!HHI", _ATTR_CHANGE_REQUEST, 4, change_flags)
    header = struct.pack("!HHI12s", _BIND_REQUEST, len(attrs), _MAGIC, transaction_id)
    return header + attrs


def _decode_response(data: bytes, transaction_id: bytes) -> Optional[StunResult]:
    if len(data) < 20:
        return None
    msg_type, msg_len, magic, txid = struct.unpack("!HHI12s", data[:20])
    if msg_type != _BIND_RESPONSE or magic != _MAGIC or txid != transaction_id:
        return None
    body = data[20 : 20 + msg_len]
    pos = 0
    mapped_ip = ""
    mapped_port = 0
    other = None
    while pos + 4 <= len(body):
        attr_type, attr_len = struct.unpack("!HH", body[pos : pos + 4])
        val = body[pos + 4 : pos + 4 + attr_len]
        if attr_type in (_ATTR_MAPPED, _ATTR_XOR_MAPPED):
            if len(val) >= 8:
                family = val[1]
                port = struct.unpack("!H", val[2:4])[0]
                ip_bytes = val[4:8]
                if attr_type == _ATTR_XOR_MAPPED:
                    port ^= (_MAGIC >> 16) & 0xFFFF
                    ip_int = struct.unpack("!I", ip_bytes)[0] ^ _MAGIC
                    ip_bytes = struct.pack("!I", ip_int)
                if family == 0x01:
                    mapped_ip = socket.inet_ntoa(ip_bytes)
                    mapped_port = port
        elif attr_type == _ATTR_OTHER_ADDRESS and len(val) >= 8:
            port = struct.unpack("!H", val[2:4])[0]
            ip = socket.inet_ntoa(val[4:8])
            other = (ip, port)
        pos += 4 + ((attr_len + 3) & ~3)

    if not mapped_ip:
        return None
    return StunResult(
        server="",
        mapped_ip=mapped_ip,
        mapped_port=mapped_port,
        other_address=other,
    )


def _probe(
    server: str,
    *,
    change_flags: int = 0,
    local_port: int = 0,
    timeout: float = 2.0,
) -> Optional[StunResult]:
    host, port = _parse_server(server)
    txid = secrets.token_bytes(12)
    payload = _encode_request(txid, change_flags=change_flags)

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.settimeout(timeout)
    if local_port:
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            sock.bind(("", local_port))
        except OSError:
            sock.bind(("", 0))
    started = time.time()
    try:
        sock.sendto(payload, (host, port))
        data, _ = sock.recvfrom(2048)
    except (socket.timeout, OSError):
        return None
    finally:
        sock.close()

    rtt = round((time.time() - started) * 1000.0, 2)
    res = _decode_response(data, txid)
    if res:
        res.server = server
        res.rtt_ms = rtt
    return res


# ── Public API ───────────────────────────────────────────────────────────────


def discover_endpoint(
    stun_servers: List[str],
    local_port: int = 0,
    timeout: float = 3.0,
) -> Optional[dict]:
    """Cheap mapped-address lookup. Returns the first successful response.

    Output: ``{ip, port, server, rtt_ms}`` or ``None`` if every server
    failed. Suitable for the periodic endpoint-history poll, where we
    only need the public address.
    """
    for server in stun_servers:
        result = _probe(server, local_port=local_port, timeout=timeout)
        if result is not None:
            return {
                "ip": result.mapped_ip,
                "port": result.mapped_port,
                "server": server,
                "rtt_ms": result.rtt_ms,
                "via": server,
                "type": "xor-mapped",
            }
    return None


def classify(
    stun_servers: List[str],
    local_port: int = 0,
    timeout: float = 2.5,
) -> dict:
    """Run the four-test NAT classification (RFC 5780-lite).

    Returns ``{nat_type, mappings, recommend, public_endpoint?}``. The
    pairing flow uses the recommendation to decide whether to advertise
    a direct endpoint, kick off hole punching, or fall back to TURN.
    """
    if not stun_servers:
        return {"nat_type": "unknown", "mappings": [], "recommend": "relay"}

    primary = stun_servers[0]
    secondary = stun_servers[1] if len(stun_servers) > 1 else stun_servers[0]

    # Test I — basic Binding Request.
    test_i = _probe(primary, local_port=local_port, timeout=timeout)
    if test_i is None:
        return {"nat_type": "blocked", "mappings": [], "recommend": "relay"}

    mappings = [
        {
            "server": primary,
            "ip": test_i.mapped_ip,
            "port": test_i.mapped_port,
            "rtt_ms": test_i.rtt_ms,
        }
    ]

    local_ip = _local_ip()
    if local_ip and local_ip == test_i.mapped_ip:
        return {
            "nat_type": "open",
            "mappings": mappings,
            "recommend": "direct",
            "public_endpoint": f"{test_i.mapped_ip}:{test_i.mapped_port}",
        }

    # Test II — request response from a different IP+port (CHANGE-REQUEST 0x06).
    test_ii = _probe(
        primary,
        change_flags=0x06,
        local_port=test_i.mapped_port if local_port else 0,
        timeout=timeout,
    )
    if test_ii is not None:
        return {
            "nat_type": "full-cone",
            "mappings": mappings,
            "recommend": "direct",
            "public_endpoint": f"{test_i.mapped_ip}:{test_i.mapped_port}",
        }

    # Test III — query secondary server, compare mapping.
    test_iii = _probe(secondary, local_port=local_port, timeout=timeout)
    if test_iii is not None:
        mappings.append(
            {
                "server": secondary,
                "ip": test_iii.mapped_ip,
                "port": test_iii.mapped_port,
                "rtt_ms": test_iii.rtt_ms,
            }
        )
        if (test_iii.mapped_ip, test_iii.mapped_port) != (
            test_i.mapped_ip,
            test_i.mapped_port,
        ):
            return {
                "nat_type": "symmetric",
                "mappings": mappings,
                "recommend": "relay",
                "public_endpoint": f"{test_i.mapped_ip}:{test_i.mapped_port}",
            }

    # Test IV — request response from same IP, different port (0x02).
    test_iv = _probe(primary, change_flags=0x02, local_port=local_port, timeout=timeout)
    if test_iv is not None:
        return {
            "nat_type": "restricted-cone",
            "mappings": mappings,
            "recommend": "hole-punch",
            "public_endpoint": f"{test_i.mapped_ip}:{test_i.mapped_port}",
        }
    return {
        "nat_type": "port-restricted",
        "mappings": mappings,
        "recommend": "hole-punch",
        "public_endpoint": f"{test_i.mapped_ip}:{test_i.mapped_port}",
    }


# Backwards-compat alias — the legacy lite client called this name.
classify_nat = classify


def punch_hole(
    peer_endpoint: str,
    local_port: int,
    attempts: int = 5,
    interval: float = 0.2,
) -> None:
    """Send a burst of empty UDP datagrams to coax a NAT mapping open.

    The richer coordinated punch (both peers fire simultaneously after
    exchanging mapped endpoints) lives in :mod:`hole_puncher.punch`;
    this is the one-sided version used inside the discovery path.
    """
    host, port = _parse_server(peer_endpoint)
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        sock.bind(("", local_port))
        for _ in range(attempts):
            try:
                sock.sendto(b"", (host, port))
                time.sleep(interval)
            except Exception:
                pass
    finally:
        sock.close()
    logger.debug("punch_hole sent %d datagrams → %s:%d", attempts, host, port)


def _local_ip() -> Optional[str]:
    """Best-effort lookup of the local egress IP (no traffic actually sent)."""
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        s.connect(("8.8.8.8", 80))
        return str(s.getsockname()[0])
    except OSError:
        return None
    finally:
        s.close()


__all__ = [
    "StunResult",
    "classify",
    "classify_nat",
    "discover_endpoint",
    "punch_hole",
]
