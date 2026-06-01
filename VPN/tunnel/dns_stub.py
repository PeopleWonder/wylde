#!/usr/bin/env python3
"""Minimal DNS forwarder for VPN leak prevention.

Listens on DNS_STUB_PORT (default 5300). Forwards queries to the VPN's DNS
server only when the tunnel is active (checked via VPN_ACTIVE_FILE); returns
SERVFAIL otherwise so DNS cannot leak over the physical interface.

Usage: started by entrypoint.sh in background before the tunnel comes up.
The wg0 PostUp hook in wg0.conf sets /run/wylde-vpn-active so the stub
starts forwarding once the tunnel is confirmed up.

Note: if you want container DNS to route through the stub, redirect port 53
to DNS_STUB_PORT with an iptables NAT rule in entrypoint.sh:
    iptables -t nat -A OUTPUT -p udp --dport 53 ! -d 127.0.0.11 \\
             -j REDIRECT --to-ports 5300
(127.0.0.11 is Docker's internal resolver; leave it untouched.)
"""

import os
import socket
import threading
import logging

try:
    from Core.shared.logging_setup import configure_logging
except ImportError:
    configure_logging = None  # type: ignore[assignment]
if configure_logging is not None:
    configure_logging(service="wylde-vpn")
logger = logging.getLogger("dns_stub")

LISTEN_HOST = os.getenv("DNS_STUB_HOST", "0.0.0.0")
LISTEN_PORT = int(os.getenv("DNS_STUB_PORT", "5300"))
UPSTREAM_DNS = os.getenv("VPN_DNS", "1.1.1.1")
ACTIVE_FILE = os.getenv("VPN_ACTIVE_FILE", "/run/wylde-vpn-active")
BUF = 4096


def _vpn_active() -> bool:
    return os.path.exists(ACTIVE_FILE)


def _servfail(query: bytes) -> bytes:
    """Construct a SERVFAIL response carrying the same query ID."""
    if len(query) < 12:
        return b""
    # QR=1 AA=0 TC=0 RD=1 RA=0 Z=0 RCODE=2(SERVFAIL)
    flags = b"\x81\x82"
    qdcount = query[4:6]  # mirror question count
    zeroes = b"\x00\x00" * 3  # AN RR NS RR AR RR = 0
    return query[:2] + flags + qdcount + zeroes + query[12:]


def _forward(sock: socket.socket, data: bytes, client: tuple, upstream: str) -> None:
    fwd = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    fwd.settimeout(3.0)
    try:
        fwd.sendto(data, (upstream, 53))
        resp, _ = fwd.recvfrom(BUF)
        sock.sendto(resp, client)
    except Exception as exc:
        logger.debug("Forward error: %s", exc)
        sf = _servfail(data)
        if sf:
            try:
                sock.sendto(sf, client)
            except Exception:
                pass
    finally:
        fwd.close()


def run() -> None:
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.bind((LISTEN_HOST, LISTEN_PORT))
    logger.info(
        "DNS stub %s:%d → %s:53 (active-file: %s)",
        LISTEN_HOST,
        LISTEN_PORT,
        UPSTREAM_DNS,
        ACTIVE_FILE,
    )

    while True:
        try:
            data, addr = sock.recvfrom(BUF)
        except Exception:
            continue

        if _vpn_active():
            threading.Thread(
                target=_forward,
                args=(sock, data, addr, UPSTREAM_DNS),
                daemon=True,
            ).start()
        else:
            sf = _servfail(data)
            if sf:
                try:
                    sock.sendto(sf, addr)
                except Exception:
                    pass


if __name__ == "__main__":
    run()
