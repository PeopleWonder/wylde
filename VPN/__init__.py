"""WyldeLink VPN — peer-to-peer WireGuard tunnel + NAT traversal.

The VPN is the single auth boundary for the Wylde mesh (Design Principle
#16). Peers authenticate at WireGuard handshake; once tunneled in they
appear to the Gateway as ordinary local callers in the CGNAT range.

Layout
------
* ``tunnel/``      — WireGuard wg0/wg1 lifecycle, DNS stub
* ``nat/``         — STUN classification, TURN relay, hole punching,
                     periodic external-endpoint probe
* ``discovery/``   — mDNS advertisement (LAN), DDNS update (WAN)
* ``peers/``       — peer registry, pairing tokens / QR, rehydrate, push
* ``monitoring/``  — wg1 handshake polling for online/stale/offline
* ``api.py``       — Flask management API on 127.0.0.1:8020 (control plane)

What lives in Gateway (not here)
--------------------------------
Per Phase 9 audit, every ``/api/{chat,models,services,workflows,...}``
mobile-bridge route was relocated under :mod:`Wylde.Gateway.routes`.
This package exposes only the VPN's own control plane (``/api/vpn/*``,
``/api/link/*``, ``/api/restart``); Gateway is the front door for
everything mobile actually does once tunneled.
"""
