"""device_gate pipe — ``\\\\.\\pipe\\wylde-device-gate``.

Eleven ``device_gate.*`` actions backed by :mod:`core`. The GUI drives
pairing / tier / rotate / revoke and reads the per-device
``recent_actions`` audit strip; the Gateway calls ``device_gate.verify``
and ``device_gate.consume_pending_events`` on every authenticated
request.

Same envelope contract every Wylde service uses: handlers take the
payload dict and return a JSON-serialisable result. Handler errors
raise :class:`DeviceGateError` (or :class:`_ActionError`) which the
wrapper converts to ``{ok: False, error: {code, message}}`` on the
wire.
"""

from __future__ import annotations

import logging
import threading
from types import ModuleType
from typing import Any, Callable, Dict, Optional

from Core.shared.ipc import IpcError
from device_gate.core import DeviceGateError, get_service

ActionHandler = Callable[[Any], Dict[str, Any]]

logger = logging.getLogger("wylde.device_gate.pipe")

SERVICE_NAME = "wylde-device-gate"

_started = False
_started_lock = threading.Lock()


# ── Helpers ────────────────────────────────────────────────────────────


class _ActionError(Exception):
    def __init__(self, code: str, message: str) -> None:
        super().__init__(message)
        self.code = code
        self.message = message


def _payload_dict(payload: Any) -> Dict[str, Any]:
    if payload is None:
        return {}
    if not isinstance(payload, dict):
        raise _ActionError("bad_request", "payload must be a map")
    return payload


# ── Action handlers ────────────────────────────────────────────────────


def _list_devices_action(_payload: Any) -> Dict[str, Any]:
    devices = get_service().list_devices()
    return {"devices": devices, "count": len(devices)}


def _start_pairing_action(_payload: Any) -> Dict[str, Any]:
    return get_service().start_pairing()


def _cancel_pairing_action(_payload: Any) -> Dict[str, Any]:
    return get_service().cancel_pairing()


def _get_pairing_status_action(_payload: Any) -> Dict[str, Any]:
    return get_service().get_pairing_status()


def _complete_pairing_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    code = p.get("code")
    username = p.get("username")
    password = p.get("password")
    if not isinstance(code, str) or not code:
        raise _ActionError("bad_request", "code is required")
    if not isinstance(username, str) or not username:
        raise _ActionError("bad_request", "username is required")
    if not isinstance(password, str) or not password:
        raise _ActionError("bad_request", "password is required")
    metadata = p.get("device_metadata") or {}
    if not isinstance(metadata, dict):
        raise _ActionError("bad_request", "device_metadata must be a map")
    try:
        return get_service().complete_pairing(
            code=code,
            username=username,
            password=password,
            device_metadata=metadata,
        )
    except DeviceGateError as exc:
        raise _ActionError(exc.code, exc.message)


def _verify_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    token = p.get("token")
    if not isinstance(token, str) or not token:
        raise _ActionError("bad_request", "token is required")
    try:
        return get_service().verify(token)
    except DeviceGateError as exc:
        raise _ActionError(exc.code, exc.message)


def _set_tier_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    device_id = p.get("device_id")
    tier = p.get("tier")
    if not isinstance(device_id, str) or not device_id:
        raise _ActionError("bad_request", "device_id is required")
    if not isinstance(tier, str) or not tier:
        raise _ActionError("bad_request", "tier is required")
    try:
        return get_service().set_tier(device_id, tier)
    except DeviceGateError as exc:
        raise _ActionError(exc.code, exc.message)


def _rotate_token_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    device_id = p.get("device_id")
    if not isinstance(device_id, str) or not device_id:
        raise _ActionError("bad_request", "device_id is required")
    try:
        return get_service().rotate_token(device_id)
    except DeviceGateError as exc:
        raise _ActionError(exc.code, exc.message)


def _revoke_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    device_id = p.get("device_id")
    if not isinstance(device_id, str) or not device_id:
        raise _ActionError("bad_request", "device_id is required")
    try:
        return get_service().revoke(device_id)
    except DeviceGateError as exc:
        raise _ActionError(exc.code, exc.message)


def _recent_actions_action(payload: Any) -> Dict[str, Any]:
    """Return the rolling per-device action log, newest-first, for the
    Devices panel's "recent activity" strip. Payload ``{device_id,
    limit?}`` (limit defaults to 20); reply ``{device_id, actions, count}``
    where each action is ``{action, timestamp, status}`` with an ISO-8601
    UTC timestamp. Unknown device returns an empty list."""
    p = _payload_dict(payload)
    device_id = p.get("device_id")
    if not isinstance(device_id, str) or not device_id:
        raise _ActionError("bad_request", "device_id is required")
    raw_limit = p.get("limit", 20)
    if not isinstance(raw_limit, int) or isinstance(raw_limit, bool) or raw_limit < 0:
        raise _ActionError("bad_request", "limit must be a non-negative integer")
    actions = get_service().recent_actions(device_id, limit=raw_limit)
    return {"device_id": device_id, "actions": actions, "count": len(actions)}


def _consume_pending_events_action(payload: Any) -> Dict[str, Any]:
    """Gateway-only action: drain queued events for a device. Not
    listed in the GUI's lib/api wrappers because the GUI doesn't need
    it — only the Gateway forwards events to the mobile connection."""
    p = _payload_dict(payload)
    device_id = p.get("device_id")
    if not isinstance(device_id, str) or not device_id:
        raise _ActionError("bad_request", "device_id is required")
    events = get_service().consume_pending_events(device_id)
    return {"events": events, "count": len(events)}


# ── Wiring ────────────────────────────────────────────────────────────


_ACTIONS: Dict[str, ActionHandler] = {
    "device_gate.list_devices": _list_devices_action,
    "device_gate.start_pairing": _start_pairing_action,
    "device_gate.cancel_pairing": _cancel_pairing_action,
    "device_gate.get_pairing_status": _get_pairing_status_action,
    "device_gate.complete_pairing": _complete_pairing_action,
    "device_gate.verify": _verify_action,
    "device_gate.set_tier": _set_tier_action,
    "device_gate.rotate_token": _rotate_token_action,
    "device_gate.revoke": _revoke_action,
    "device_gate.recent_actions": _recent_actions_action,
    "device_gate.consume_pending_events": _consume_pending_events_action,
}


def _wrap_handler(handler: ActionHandler) -> ActionHandler:
    def _wrapped(payload: Any) -> Dict[str, Any]:
        try:
            return handler(payload)
        except _ActionError as exc:
            # Surface as IpcError so the dispatcher emits a structured
            # envelope with the real code (not_found / bad_request /
            # invalid_token / pairing_inactive), matching the Rust
            # port's reply shape.
            raise IpcError(exc.code, exc.message)

    _wrapped.__name__ = getattr(handler, "__name__", "wrapped")
    return _wrapped


def _ipc_module() -> Optional[ModuleType]:
    try:
        from Core.shared import ipc

        return ipc
    except ImportError as exc:
        logger.warning("device_gate pipe: ipc not importable (%s) — pipe disabled", exc)
        return None


def _register_actions() -> Optional[ModuleType]:
    ipc = _ipc_module()
    if ipc is None:
        return None
    for name, handler in _ACTIONS.items():
        ipc.register_action(name, _wrap_handler(handler))
    logger.info("device_gate pipe: registered %d device_gate.* actions", len(_ACTIONS))
    return ipc


def _build_stub_app() -> Optional[Any]:
    try:
        from flask import Flask
    except ImportError:
        return None
    app = Flask("wylde-device-gate")

    @app.route("/health", methods=["GET"])
    def _health() -> Dict[str, Any]:  # pragma: no cover
        return {"ok": True, "service": SERVICE_NAME}

    return app


def start() -> bool:
    """Start the device-gate pipe in a daemon thread.

    Idempotent. Returns True if the pipe is now serving (or was
    already), False if dependencies are missing (msgpack/pywin32
    absent, non-Windows host).
    """
    global _started
    with _started_lock:
        if _started:
            return True
        ipc = _register_actions()
        if ipc is None:
            return False
        try:
            ipc.serve_forever_background(SERVICE_NAME, _build_stub_app())
        except Exception as exc:  # noqa: BLE001
            logger.warning(
                "device_gate pipe: serve_forever_background failed (%s)", exc
            )
            return False
        _started = True
        logger.info("device_gate pipe: serving \\\\.\\pipe\\%s", SERVICE_NAME)
        return True


def stop() -> None:
    """Reserved for future graceful shutdown (PipeServer doesn't
    expose a stop hook today; the pipe drains on process exit)."""
    return None


__all__ = ["SERVICE_NAME", "start", "stop"]
