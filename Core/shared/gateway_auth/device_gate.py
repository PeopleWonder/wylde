"""Thin wrapper around ``\\\\.\\pipe\\wylde-device-gate``.

Relocated from ``Gateway/services/device_gate.py`` on 2026-05-30 when the
Python Gateway server was deleted. The device-token dependencies in
:mod:`.device` call into here; the wrapper translates each public
function into a pipe action call so callers stay free of pipe-envelope
plumbing.

Every call returns ``(status_code, body)`` so the caller can pass the
tuple straight back to a JSON response.
"""

from __future__ import annotations

import logging
from typing import Any, Dict, Optional, Tuple

logger = logging.getLogger("wylde.gateway.services.device_gate")

SERVICE_NAME = "wylde-device-gate"


def _call_action(
    action: str, payload: Optional[Dict[str, Any]] = None, *, timeout: float = 5.0
) -> Tuple[int, Dict[str, Any]]:
    try:
        from Core.shared import ipc
    except ImportError:
        return 503, {
            "ok": False,
            "error": {
                "code": "transport",
                "message": "ipc module not importable",
            },
        }
    reply = ipc.send_action(SERVICE_NAME, action, payload, timeout=timeout)
    if not getattr(reply, "ok", False):
        err = getattr(reply, "error", None) or {}
        # Map handler-tagged action errors back to canonical HTTP codes.
        msg = str(err.get("message", ""))
        code = _err_code_to_http(err.get("code", "unknown"), msg)
        return code, {
            "ok": False,
            "error": {
                "code": err.get("code", "unknown"),
                "message": msg or "device-gate call failed",
            },
        }
    return 200, getattr(reply, "data", None) or {}


def _err_code_to_http(code: str, message: str) -> int:
    code = (code or "").lower()
    msg = (message or "").lower()
    if code == "not_found" or "not found" in msg:
        return 404
    if code in ("bad_request", "invalid_token", "code_mismatch", "credential_mismatch"):
        return 400
    if code == "pairing_inactive":
        return 409
    if "[invalid_token]" in msg or "[credential_mismatch]" in msg:
        return 400
    if "[not_found]" in msg:
        return 404
    if "[pairing_inactive]" in msg:
        return 409
    return 502


# ── Public surface ────────────────────────────────────────────────────


def list_devices() -> Tuple[int, Dict[str, Any]]:
    return _call_action("device_gate.list_devices")


def start_pairing() -> Tuple[int, Dict[str, Any]]:
    return _call_action("device_gate.start_pairing")


def cancel_pairing() -> Tuple[int, Dict[str, Any]]:
    return _call_action("device_gate.cancel_pairing")


def get_pairing_status() -> Tuple[int, Dict[str, Any]]:
    return _call_action("device_gate.get_pairing_status")


def complete_pairing(
    *,
    code: str,
    username: str,
    password: str,
    device_metadata: Optional[Dict[str, Any]] = None,
) -> Tuple[int, Dict[str, Any]]:
    return _call_action(
        "device_gate.complete_pairing",
        {
            "code": code,
            "username": username,
            "password": password,
            "device_metadata": device_metadata or {},
        },
    )


def verify(token: str) -> Tuple[int, Dict[str, Any]]:
    return _call_action("device_gate.verify", {"token": token})


def set_tier(device_id: str, tier: str) -> Tuple[int, Dict[str, Any]]:
    return _call_action(
        "device_gate.set_tier",
        {
            "device_id": device_id,
            "tier": tier,
        },
    )


def rotate_token(device_id: str) -> Tuple[int, Dict[str, Any]]:
    return _call_action("device_gate.rotate_token", {"device_id": device_id})


def revoke(device_id: str) -> Tuple[int, Dict[str, Any]]:
    return _call_action("device_gate.revoke", {"device_id": device_id})


def consume_pending_events(device_id: str) -> Tuple[int, Dict[str, Any]]:
    return _call_action(
        "device_gate.consume_pending_events",
        {
            "device_id": device_id,
        },
    )


__all__ = [
    "SERVICE_NAME",
    "list_devices",
    "start_pairing",
    "cancel_pairing",
    "get_pairing_status",
    "complete_pairing",
    "verify",
    "set_tier",
    "rotate_token",
    "revoke",
    "consume_pending_events",
]
