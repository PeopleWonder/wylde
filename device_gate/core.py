"""Public service surface — pairing, tokens, tier management.

This is the in-process API the pipe layer wraps. Every state mutation
goes through here; the pipe handlers are thin envelope-translators
over these functions.

Responsibilities:

* Pairing-mode lifecycle. One-shot (auto-OFF on success / cancel /
  expiry). One active code at a time.
* Device record CRUD.
* Token generation / verification / rotation / revocation.
* Pending events queue per device (so Gateway can forward
  ``token_rotated`` / ``revoked`` to the mobile's active connection).

Active sessions are tracked implicitly via ``last_seen`` — the mobile
is "active" if it called :func:`verify` recently. Whether to push an
event is the Gateway's call; here we just maintain the pending queue
and expose :func:`consume_pending_events` for the bridge.
"""

from __future__ import annotations

import logging
import os
import secrets
import string
import threading
import time
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Dict, List, Optional

from device_gate.auth import verify_credentials
from device_gate.store import (
    ALL_TIERS,
    ActionLog,
    Device,
    DeviceStore,
    TIER_DESTRUCTIVE,
    TIER_READ_ONLY,
    TIER_TOOL_USE,
    is_valid_tier,
)

logger = logging.getLogger("wylde.device_gate.core")


# ── Constants ──────────────────────────────────────────────────────────


PAIRING_CODE_TTL_SECONDS = 5 * 60  # 5 minutes per spec
PAIRING_CODE_LENGTH = 6
PAIRING_CODE_ALPHABET = string.digits  # 6 digits is the spec example


def _data_dir() -> Path:
    """Resolve the device_gate data dir.

    Honours ``DEVICE_GATE_DATA_DIR`` first, then falls back to
    ``<service folder>/data``. Tests inject a tmpdir via the env.
    """
    here = Path(__file__).resolve().parent
    return Path(os.getenv("DEVICE_GATE_DATA_DIR") or (here / "data"))


def _devices_path() -> Path:
    return _data_dir() / "devices.json"


def _htpasswd_path() -> Path:
    return Path(os.getenv("DEVICE_GATE_HTPASSWD") or (_data_dir() / "htpasswd"))


def _action_log_path() -> Path:
    return _data_dir() / "action_log.json"


# ── Pairing state ─────────────────────────────────────────────────────


@dataclass
class _PairingState:
    """One pairing attempt at a time. Active iff ``code`` is non-empty
    AND ``expires_at`` is in the future."""

    code: str = ""
    expires_at: float = 0.0
    started_at: float = 0.0

    def active(self, *, now: Optional[float] = None) -> bool:
        ts = float(now if now is not None else time.time())
        return bool(self.code) and ts < self.expires_at

    def reset(self) -> None:
        self.code = ""
        self.expires_at = 0.0
        self.started_at = 0.0

    def to_status(self, *, now: Optional[float] = None) -> Dict[str, Any]:
        if self.active(now=now):
            return {
                "pairing_active": True,
                "code": self.code,
                "expires_at": float(self.expires_at),
            }
        return {"pairing_active": False}


# ── Errors ────────────────────────────────────────────────────────────


class DeviceGateError(Exception):
    """Structured error surfaced through the pipe envelope."""

    def __init__(self, code: str, message: str) -> None:
        super().__init__(message)
        self.code = code
        self.message = message


# ── Service singleton ─────────────────────────────────────────────────


class DeviceGateService:
    """Holds the device store + pairing state + pending-event queues.

    The pipe handlers operate on the module-level singleton via
    :func:`get_service`; tests construct a fresh instance with a
    tmpdir to keep state isolated.
    """

    def __init__(
        self,
        *,
        store: Optional[DeviceStore] = None,
        htpasswd_path: Optional[Path] = None,
        action_log: Optional[ActionLog] = None,
        clock: Optional[Any] = None,
    ) -> None:
        self.store = store or DeviceStore(_devices_path())
        self.htpasswd_path = Path(htpasswd_path) if htpasswd_path else _htpasswd_path()
        # Rolling per-device audit trail for the GUI's "recent activity"
        # strip. Lives in device_gate's own data dir; tests inject a tmp.
        self.action_log = action_log or ActionLog(_action_log_path())
        self._lock = threading.RLock()
        self._pairing = _PairingState()
        self._pending_events: Dict[str, List[Dict[str, Any]]] = {}
        # Injectable clock for deterministic tests; default = time.time.
        self._clock = clock or time.time

    # ── Pairing flow ──────────────────────────────────────────────────

    def start_pairing(self) -> Dict[str, Any]:
        """Open a pairing window. Replaces any existing pending code
        — only one pairing attempt is active at a time. Returns the
        new code + expiry the GUI should display."""
        with self._lock:
            now = float(self._clock())
            self._pairing.code = _mint_code()
            self._pairing.started_at = now
            self._pairing.expires_at = now + PAIRING_CODE_TTL_SECONDS
            logger.info(
                "device_gate: pairing started (expires in %ds)",
                PAIRING_CODE_TTL_SECONDS,
            )
            return {
                "code": self._pairing.code,
                "expires_at": self._pairing.expires_at,
            }

    def cancel_pairing(self) -> Dict[str, Any]:
        with self._lock:
            was_active = self._pairing.active(now=self._clock())
            self._pairing.reset()
            if was_active:
                logger.info("device_gate: pairing cancelled")
            return {"ok": True, "cancelled": was_active}

    def get_pairing_status(self) -> Dict[str, Any]:
        with self._lock:
            now = self._clock()
            # Lazy-expire: if the timer ran out, clear so subsequent
            # complete_pairing calls see the current state cleanly.
            if self._pairing.code and not self._pairing.active(now=now):
                self._pairing.reset()
            return self._pairing.to_status(now=now)

    def complete_pairing(
        self,
        *,
        code: str,
        username: str,
        password: str,
        device_metadata: Optional[Dict[str, Any]] = None,
    ) -> Dict[str, Any]:
        """Finish pairing. Returns ``{device_id, token, tier}`` on
        success; raises :class:`DeviceGateError` on any failure.

        Order of checks: pairing-active first, then code match, then
        credentials. The credential check still runs on a code miss
        so timing doesn't reveal whether the code was right when the
        password was wrong.
        """
        device_metadata = device_metadata or {}
        with self._lock:
            now = self._clock()
            if not self._pairing.active(now=now):
                # Burn the credential check anyway so timing is constant.
                verify_credentials(self.htpasswd_path, username, password)
                raise DeviceGateError(
                    "pairing_inactive",
                    "no pairing window is open — start one from the desktop GUI first",
                )
            code_ok = isinstance(code, str) and secrets.compare_digest(
                code,
                self._pairing.code,
            )
            creds_ok = verify_credentials(self.htpasswd_path, username, password)
            if not code_ok:
                raise DeviceGateError(
                    "code_mismatch",
                    "pairing code is wrong or expired",
                )
            if not creds_ok:
                raise DeviceGateError(
                    "credential_mismatch",
                    "username or password is incorrect",
                )

            # All checks passed — mint the device record.
            device_id = _mint_device_id()
            token = _mint_token()
            name = _device_name_from_metadata(device_metadata) or device_id[:8]
            device = Device(
                device_id=device_id,
                name=name,
                token=token,
                tier=TIER_READ_ONLY,
                paired_at=now,
                last_seen=now,
                metadata=dict(device_metadata),
            )
            self.store.add(device)
            self._pairing.reset()
            self.action_log.record(device_id, "paired", status="ok")
            logger.info(
                "device_gate: paired %s (%s) tier=%s",
                device_id,
                name,
                device.tier,
            )
            return {
                "device_id": device_id,
                "token": token,
                "tier": device.tier,
            }

    # ── Token verification ────────────────────────────────────────────

    def verify(self, token: str) -> Dict[str, Any]:
        """Look up a device by token. Touches ``last_seen`` so the GUI
        sees the device as "active" while the mobile is using it.
        Raises :class:`DeviceGateError` on miss."""
        if not isinstance(token, str) or not token:
            raise DeviceGateError("invalid_token", "token is required")
        device = self.store.find_by_token(token)
        if device is None:
            raise DeviceGateError("invalid_token", "token does not match any device")
        with self._lock:
            self.store.touch(device.device_id, when=self._clock())
        return {"device_id": device.device_id, "tier": device.tier}

    # ── Tier management ───────────────────────────────────────────────

    def set_tier(self, device_id: str, tier: str) -> Dict[str, Any]:
        if not is_valid_tier(tier):
            raise DeviceGateError(
                "bad_request",
                f"tier must be one of {list(ALL_TIERS)!r}",
            )
        with self._lock:
            updated = self.store.update(device_id, tier=tier)
            if updated is None:
                raise DeviceGateError("not_found", f"device {device_id!r} not found")
            logger.info("device_gate: %s tier → %s", device_id, tier)
            self._enqueue_event(device_id, "tier_changed", {"tier": tier})
            self.action_log.record(device_id, f"tier → {tier}", status="ok")
        return {"device_id": device_id, "tier": tier}

    # ── Token rotation ────────────────────────────────────────────────

    def rotate_token(self, device_id: str) -> Dict[str, Any]:
        """Mint a new token; old one is invalidated immediately. If
        the device has been seen recently we queue a ``token_rotated``
        event so the Gateway can forward the new token to the active
        connection — the mobile updates its stored token and the
        session keeps going without a re-pair."""
        with self._lock:
            existing = self.store.get(device_id)
            if existing is None:
                raise DeviceGateError("not_found", f"device {device_id!r} not found")
            new_token = _mint_token()
            self.store.update(device_id, token=new_token)
            logger.info("device_gate: rotated token for %s", device_id)
            self._enqueue_event(
                device_id,
                "token_rotated",
                {"new_token": new_token},
            )
            self.action_log.record(device_id, "token rotated", status="ok")
        return {"device_id": device_id, "new_token": new_token}

    # ── Revocation ────────────────────────────────────────────────────

    def revoke(self, device_id: str) -> Dict[str, Any]:
        with self._lock:
            existing = self.store.get(device_id)
            if existing is None:
                raise DeviceGateError("not_found", f"device {device_id!r} not found")
            removed = self.store.remove(device_id)
            if not removed:
                raise DeviceGateError("not_found", f"device {device_id!r} not found")
            # Queue the revoked event BEFORE the device is gone — but
            # since the queue is keyed by device_id, the Gateway can
            # pick this up on the next consume call even after removal.
            self._enqueue_event(device_id, "revoked", {})
            # Audit trail survives revocation — the ActionLog is keyed by
            # device_id in its own file, so the row's gone but the history
            # of "this device existed and was revoked at T" is preserved.
            self.action_log.record(device_id, "revoked", status="ok")
            logger.info("device_gate: revoked %s", device_id)
        return {"device_id": device_id}

    # ── Event queue (for Gateway forwarding) ─────────────────────────

    def _enqueue_event(self, device_id: str, kind: str, data: Dict[str, Any]) -> None:
        ev = {
            "type": kind,
            "device_id": device_id,
            "at": float(self._clock()),
            **data,
        }
        # Caller already holds the lock.
        self._pending_events.setdefault(device_id, []).append(ev)

    def consume_pending_events(self, device_id: str) -> List[Dict[str, Any]]:
        """Drain queued events for one device. The Gateway calls this
        after each :meth:`verify` and forwards anything pending to the
        mobile's active connection. Returning the events removes them
        from the queue — events are at-most-once per consume call."""
        with self._lock:
            events = self._pending_events.pop(device_id, [])
            return list(events)

    def has_pending_events(self, device_id: str) -> bool:
        with self._lock:
            return bool(self._pending_events.get(device_id))

    # ── Listing ──────────────────────────────────────────────────────

    def list_devices(self, *, active_threshold_s: float = 60.0) -> List[Dict[str, Any]]:
        """GUI device list. ``is_active`` is True iff ``last_seen`` is
        within ``active_threshold_s`` of the current clock — the mobile
        polls verify() at least once per minute when connected, so
        anything in the last minute is "currently online" by reasonable
        heuristic. The actual definition can tighten later."""
        now = float(self._clock())
        out: List[Dict[str, Any]] = []
        for d in self.store.list():
            entry = d.to_dict(include_token=False)
            entry["is_active"] = bool(
                d.last_seen and (now - d.last_seen) <= active_threshold_s
            )
            out.append(entry)
        return out

    # ── Action log ─────────────────────────────────────────────────────

    def recent_actions(
        self, device_id: str, *, limit: int = 20
    ) -> List[Dict[str, Any]]:
        """Return the most-recent GUI-driven actions for ``device_id``,
        newest-first, each ``{action, timestamp, status}``. Unknown
        device → empty list. Backs the Devices panel's per-row "recent
        activity" strip."""
        return self.action_log.recent(device_id, limit=limit)


# ── Module-level singleton ────────────────────────────────────────────


_service_lock = threading.Lock()
_service: Optional[DeviceGateService] = None


def get_service() -> DeviceGateService:
    global _service
    with _service_lock:
        if _service is None:
            _service = DeviceGateService()
        return _service


def install_service(svc: DeviceGateService) -> None:
    """Test seam — replace the module-level singleton. Pair with
    :func:`reset_service` in tearDown so other tests start clean."""
    global _service
    with _service_lock:
        _service = svc


def reset_service() -> None:
    global _service
    with _service_lock:
        _service = None


# ── Helpers ───────────────────────────────────────────────────────────


def _mint_code() -> str:
    """Six-digit pairing code. ``secrets.choice`` keeps it
    cryptographically random — predictable codes would let an
    attacker brute-force during the 5-minute window."""
    return "".join(
        secrets.choice(PAIRING_CODE_ALPHABET) for _ in range(PAIRING_CODE_LENGTH)
    )


def _mint_token() -> str:
    """UUID4 hex — opaque, 32 chars, plenty of entropy."""
    return uuid.uuid4().hex


def _mint_device_id() -> str:
    """Device id — short timestamped + random suffix so the GUI list
    sorts by pairing time without leaking the full pairing timestamp
    to mobile clients (the suffix breaks ties)."""
    return f"dev_{int(time.time())}_{secrets.token_hex(3)}"


def _device_name_from_metadata(meta: Dict[str, Any]) -> str:
    name = meta.get("name") or meta.get("device_name") or meta.get("hostname")
    if isinstance(name, str) and name.strip():
        return name.strip()[:64]
    return ""


__all__ = [
    "DeviceGateError",
    "DeviceGateService",
    "PAIRING_CODE_TTL_SECONDS",
    "PAIRING_CODE_LENGTH",
    "TIER_READ_ONLY",
    "TIER_TOOL_USE",
    "TIER_DESTRUCTIVE",
    "ALL_TIERS",
    "get_service",
    "install_service",
    "reset_service",
]
