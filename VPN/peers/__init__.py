"""Peer registry, pairing flow, push delivery.

Submodules:

* :mod:`store`     — JSON-backed peer registry + tunnel-IP allocator.
* :mod:`pairing`   — pairing tokens, QR, registration, connect, the
  ``_connection_params`` helper used by the management API.
* :mod:`rehydrate` — boot-time re-push of every stored peer to wg1.
* :mod:`push`      — peer-keyed notification subscription + delivery.

The Gateway calls into ``store`` and ``push`` over IPC rather than via
in-process import, so these modules stay process-local to the VPN.
"""
