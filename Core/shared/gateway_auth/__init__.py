"""Device-token auth helpers — relocated from ``Gateway/auth/``.

When the Python FastAPI Gateway server was deleted on 2026-05-30 (the
Rust ``wylde-gateway`` crate is the live server), the only part of
``Gateway/auth/`` that still had a caller was the per-request
device-token verification — the FastAPI dependencies
:func:`require_device` / :func:`require_tier` / :func:`require_tool_access`
and the :class:`DeviceAuth` record. Those moved here, alongside the thin
``\\.\pipe\wylde-device-gate`` wrapper (:mod:`.device_gate`) the
dependencies call to verify a token.

The deleted bits did **not** move: the CIDR allowlist tier checkers
(``require_local`` / ``is_local_ip``) and the :class:`AuthTier` vocabulary
were the HTTP server's network-boundary auth, which the Rust Gateway now
owns; the ``is_device_approved`` placeholder was never wired in. Only the
device-token surface — used by ``device_gate``'s integration tests and
intended for any in-process caller that needs to gate a request by device
tier — survives.
"""

from __future__ import annotations

from .device import (
    DeviceAuth,
    is_destructive_tool,
    require_device,
    require_tier,
    require_tool_access,
)

__all__ = [
    "DeviceAuth",
    "is_destructive_tool",
    "require_device",
    "require_tier",
    "require_tool_access",
]
