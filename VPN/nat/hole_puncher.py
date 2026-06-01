"""UDP hole punching coordinator.

For NAT types where direct connection fails but symmetric isn't required,
both peers fire empty UDP datagrams at each other's STUN-mapped endpoint.
The outbound packets create NAT state on each side, after which the
WireGuard handshake can flow.

The coordination signal goes through the QR-pairing exchange — the mobile
client sends its own STUN-mapped endpoint when it calls /api/link/register,
and the desktop replies with its endpoint. From that point on, both sides
have the address pair they need to start punching.
"""

import logging
import socket
import time

logger = logging.getLogger(__name__)


def punch(
    remote_endpoint: str, local_port: int, *, attempts: int = 8, interval: float = 0.25
) -> bool:
    """Send a burst of empty UDP datagrams to coax NAT mappings open.

    Returns True if all datagrams were sent successfully (this does NOT
    confirm that the remote peer received them — that's WireGuard's job).
    """
    host, _, port_s = remote_endpoint.partition(":")
    if not port_s.isdigit():
        return False
    port = int(port_s)

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
        sock.bind(("0.0.0.0", local_port))
    except OSError as exc:
        logger.warning("hole_puncher: cannot bind local port %d: %s", local_port, exc)
        sock.close()
        return False

    sent = 0
    for _ in range(attempts):
        try:
            sock.sendto(b"\x00", (host, port))
            sent += 1
        except OSError as exc:
            logger.debug("hole_puncher: send failed (%s)", exc)
        time.sleep(interval)
    sock.close()
    logger.info(
        "hole_puncher: sent %d/%d datagrams to %s", sent, attempts, remote_endpoint
    )
    return sent == attempts
