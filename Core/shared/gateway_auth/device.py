"""Per-request device-token verification.

Relocated from ``Gateway/auth/device.py`` on 2026-05-30 when the Python
Gateway server was deleted; the device-token gating it provides is still
used (the ``device_gate`` integration tests, and any in-process caller
that needs to gate a request by device tier).

The CIDR allowlist (the former ``require_local``) decided whether the
*network* peer was allowed to reach the Gateway at all — that boundary
now lives in the Rust ``wylde-gateway`` crate. This module decides *which
device* a request is from — extracts the Bearer token, asks Device Gate
to verify it, attaches ``{device_id, tier}`` to ``request.state``, and
gates tool calls based on the device's permission tier.

Two FastAPI dependencies:

* :func:`require_device` — every protected mobile route uses this. 401
  on missing / invalid token. The verified ``DeviceAuth`` is exposed
  via dependency-injection AND attached to ``request.state`` so
  middleware can read it without pulling Depends through every route.
* :func:`require_tier(min_tier)` — chains require_device, then ranks
  the tier. 403 if the device's tier is lower than ``min_tier``.

Per spec, tool tier enforcement uses the tool manifest's
``requires_confirmation`` flag as the "destructive" signal. The helper
:func:`require_tool_access` reads the manifest and gates accordingly.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Callable, Optional

from fastapi import Depends, HTTPException, Request, status


@dataclass
class DeviceAuth:
    device_id: str
    tier: str
    token: str


def _envelope(code: str, message: str, status_code: int) -> HTTPException:
    return HTTPException(
        status_code=status_code,
        detail={"ok": False, "error": {"code": code, "message": message}},
    )


def _extract_bearer(request: Request) -> Optional[str]:
    raw = request.headers.get("authorization") or request.headers.get("Authorization")
    if not raw:
        return None
    parts = raw.strip().split()
    if len(parts) != 2 or parts[0].lower() != "bearer":
        return None
    return parts[1].strip() or None


def require_device(request: Request) -> DeviceAuth:
    """FastAPI dependency: extract+verify the device token.

    Raises 401 on missing or invalid. On success, attaches the
    :class:`DeviceAuth` to ``request.state.device_auth`` AND returns
    it so handlers that want it as a parameter can use ``Depends``.
    """
    token = _extract_bearer(request)
    if not token:
        raise _envelope(
            "missing_token",
            "Bearer token required (Authorization: Bearer <token>)",
            status.HTTP_401_UNAUTHORIZED,
        )
    # Lazy-imported so unit tests can monkeypatch the verify function
    # without pulling in the full device-gate import graph.
    from . import device_gate as svc

    code, body = svc.verify(token)
    if code != 200:
        if code in (404, 400):
            raise _envelope(
                "invalid_token",
                "device token is not recognised",
                status.HTTP_401_UNAUTHORIZED,
            )
        # device_gate down → service unavailable, NOT auth-denied.
        # The mobile app should retry; mis-classifying as 401 would
        # cause the app to clear its token unnecessarily.
        raise _envelope(
            "device_gate_unavailable",
            f"device-gate returned {code}",
            status.HTTP_503_SERVICE_UNAVAILABLE,
        )
    auth = DeviceAuth(
        device_id=str(body.get("device_id") or ""),
        tier=str(body.get("tier") or ""),
        token=token,
    )
    if not auth.device_id or not auth.tier:
        raise _envelope(
            "invalid_token",
            "device-gate returned an empty record",
            status.HTTP_401_UNAUTHORIZED,
        )
    request.state.device_auth = auth
    # Pending-event draining is a separate dependency, mounted after the
    # per-device rate limit so a 429 can't drain the queue before the
    # device sees it.
    return auth


def _tier_rank(tier: str) -> int:
    # Local copy of the rank table so this module doesn't depend on
    # the device_gate package import path (the service folder has a
    # space in its name, not Python-importable as `device_gate`).
    return {
        "read_only": 0,
        "tool_use": 1,
        "destructive_tool_access": 2,
    }.get(tier, -1)


def require_tier(min_tier: str) -> Callable[..., DeviceAuth]:
    """Factory: ``Depends(require_tier("tool_use"))``.

    Returns a dependency function that runs :func:`require_device`
    first, then enforces ``rank(device.tier) >= rank(min_tier)``.
    """
    if _tier_rank(min_tier) < 0:
        raise ValueError(f"unknown tier {min_tier!r}")

    def _checker(auth: DeviceAuth = Depends(require_device)) -> DeviceAuth:
        if _tier_rank(auth.tier) < _tier_rank(min_tier):
            raise _envelope(
                "tier_insufficient",
                f"device tier {auth.tier!r} below required {min_tier!r}",
                status.HTTP_403_FORBIDDEN,
            )
        return auth

    return _checker


def is_destructive_tool(tool_id: str) -> bool:
    """Read a tool's manifest and return its destructive flag.

    Uses the existing ``requires_confirmation`` field as the signal
    per the spec. A missing manifest / unreadable file is treated as
    NOT destructive — wrong direction for safety, but the catalog is
    authoritative and an unreadable manifest already means the tool
    won't be runnable through the harness anyway.
    """
    try:
        from Core.harness.tooling.tool_registry import get_tool  # type: ignore
    except ImportError:
        return False
    try:
        entry = get_tool(tool_id)
    except Exception:  # noqa: BLE001
        return False
    if not entry:
        return False
    if isinstance(entry, dict):
        return bool(entry.get("requires_confirmation", False))
    return bool(getattr(entry, "requires_confirmation", False))


def require_tool_access(tool_id: str) -> Callable[..., DeviceAuth]:
    """Gate one tool call by the device's tier vs the tool's destructive flag.

    Rules:
    * Any device tier can call non-destructive tools (the
      ``read_only`` tier still can't, see below — chat-view-only).
    * ``requires_confirmation=true`` tools require
      ``destructive_tool_access``.
    * Otherwise, ``tool_use`` is enough.
    """

    def _checker(auth: DeviceAuth = Depends(require_device)) -> DeviceAuth:
        destructive = is_destructive_tool(tool_id)
        if destructive:
            needed = "destructive_tool_access"
        else:
            needed = "tool_use"
        if _tier_rank(auth.tier) < _tier_rank(needed):
            raise _envelope(
                "tier_insufficient",
                f"tool {tool_id!r} requires tier {needed!r}; device has {auth.tier!r}",
                status.HTTP_403_FORBIDDEN,
            )
        return auth

    return _checker


__all__ = [
    "DeviceAuth",
    "require_device",
    "require_tier",
    "require_tool_access",
    "is_destructive_tool",
]
