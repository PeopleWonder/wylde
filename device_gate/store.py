"""JSON-backed device record store.

One file (``devices.json``) holds every paired device. Records carry
the issued token, the device's tier, last-seen timestamp, and the
metadata the mobile app supplied at pairing time. The file is written
atomically (temp + rename) so a crash mid-write doesn't corrupt the
store.

Surface mirrors what the public API in :mod:`core` needs — the
package never exposes raw ``Device`` objects to GUI / Gateway callers;
both flows go through ``core.list_devices`` etc. which return dicts.
"""

from __future__ import annotations

import json
import os
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Dict, List, Optional

from Core.shared.secure_file import harden_perms


# ── Tiers ──────────────────────────────────────────────────────────────


TIER_READ_ONLY = "read_only"
TIER_TOOL_USE = "tool_use"
TIER_DESTRUCTIVE = "destructive_tool_access"

ALL_TIERS = (TIER_READ_ONLY, TIER_TOOL_USE, TIER_DESTRUCTIVE)

# Each tier strictly contains the previous one. Numeric ranks make
# "is tier X >= tier Y" comparisons explicit at the call site.
TIER_RANK: Dict[str, int] = {
    TIER_READ_ONLY: 0,
    TIER_TOOL_USE: 1,
    TIER_DESTRUCTIVE: 2,
}


def is_valid_tier(tier: Any) -> bool:
    return isinstance(tier, str) and tier in ALL_TIERS


def tier_rank(tier: str) -> int:
    return TIER_RANK.get(tier, -1)


# ── Device record ──────────────────────────────────────────────────────


@dataclass
class Device:
    device_id: str
    name: str
    token: str
    tier: str = TIER_READ_ONLY
    paired_at: float = 0.0
    last_seen: float = 0.0
    metadata: Dict[str, Any] = field(default_factory=dict)

    def to_dict(self, *, include_token: bool = False) -> Dict[str, Any]:
        out: Dict[str, Any] = {
            "device_id": self.device_id,
            "name": self.name,
            "tier": self.tier,
            "paired_at": float(self.paired_at),
            "last_seen": float(self.last_seen),
            "metadata": dict(self.metadata),
        }
        if include_token:
            out["token"] = self.token
        return out

    @classmethod
    def from_dict(cls, d: Dict[str, Any]) -> "Device":
        return cls(
            device_id=str(d.get("device_id", "")),
            name=str(d.get("name", "")),
            token=str(d.get("token", "")),
            tier=str(d.get("tier", TIER_READ_ONLY) or TIER_READ_ONLY),
            paired_at=float(d.get("paired_at", 0.0) or 0.0),
            last_seen=float(d.get("last_seen", 0.0) or 0.0),
            metadata=dict(d.get("metadata", {}) or {}),
        )


# ── Store (JSON-backed) ────────────────────────────────────────────────


class DeviceStore:
    """Thread-safe device-record store. One JSON file on disk."""

    def __init__(self, path: Path) -> None:
        self.path = Path(path)
        self._lock = threading.RLock()

    def _load(self) -> List[Device]:
        if not self.path.exists():
            return []
        try:
            raw = json.loads(self.path.read_text(encoding="utf-8"))
        except Exception:  # noqa: BLE001
            return []
        items = raw.get("devices") if isinstance(raw, dict) else raw
        if not isinstance(items, list):
            return []
        return [Device.from_dict(it) for it in items if isinstance(it, dict)]

    def _save(self, devices: List[Device]) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        payload = {"devices": [d.to_dict(include_token=True) for d in devices]}
        tmp = self.path.with_suffix(".json.tmp")
        tmp.write_text(json.dumps(payload, indent=2), encoding="utf-8")
        os.replace(tmp, self.path)
        # Live device bearer tokens — restrict to owner-only access.
        harden_perms(self.path)

    # ── Read-side ─────────────────────────────────────────────────────

    def list(self) -> List[Device]:
        with self._lock:
            return self._load()

    def get(self, device_id: str) -> Optional[Device]:
        with self._lock:
            for d in self._load():
                if d.device_id == device_id:
                    return d
        return None

    def find_by_token(self, token: str) -> Optional[Device]:
        if not isinstance(token, str) or not token:
            return None
        with self._lock:
            for d in self._load():
                if d.token == token:
                    return d
        return None

    # ── Write-side ────────────────────────────────────────────────────

    def add(self, device: Device) -> Device:
        with self._lock:
            devices = self._load()
            # Reject if device_id collision; caller should mint a fresh id.
            if any(d.device_id == device.device_id for d in devices):
                raise ValueError(f"device_id {device.device_id!r} already exists")
            devices.append(device)
            self._save(devices)
            return device

    def update(self, device_id: str, **fields: Any) -> Optional[Device]:
        with self._lock:
            devices = self._load()
            target = next((d for d in devices if d.device_id == device_id), None)
            if target is None:
                return None
            for k, v in fields.items():
                if hasattr(target, k):
                    setattr(target, k, v)
            self._save(devices)
            return target

    def remove(self, device_id: str) -> bool:
        with self._lock:
            devices = self._load()
            before = len(devices)
            devices = [d for d in devices if d.device_id != device_id]
            if len(devices) == before:
                return False
            self._save(devices)
            return True

    def touch(self, device_id: str, when: Optional[float] = None) -> None:
        """Update ``last_seen`` for a device. Idempotent; missing
        device_id is a no-op so a stale token check doesn't crash."""
        ts = float(when if when is not None else time.time())
        with self._lock:
            devices = self._load()
            changed = False
            for d in devices:
                if d.device_id == device_id:
                    d.last_seen = ts
                    changed = True
                    break
            if changed:
                self._save(devices)


# ── Action log (per-device rolling history) ────────────────────────────


# How many action entries we retain per device. The GUI only renders the
# most-recent handful; the cap keeps the JSON file bounded even for a
# device that's been rotated / re-tiered many times.
ACTION_LOG_CAP = 50


class ActionLog:
    """JSON-backed rolling log of GUI-driven mutations, keyed by device.

    Separate file from ``devices.json`` so the audit trail survives a
    device being revoked (the device row is gone, but the operator may
    still want to see "this device was revoked at T"). Same atomic
    temp+rename write discipline as :class:`DeviceStore`; the entries
    carry no secrets, so no ``harden_perms`` call here.

    Entry shape: ``{action, timestamp, status}`` where ``timestamp`` is
    ISO-8601 UTC. Stored oldest-first on disk; :meth:`recent` returns
    newest-first to match the GUI's display order.
    """

    def __init__(self, path: Path) -> None:
        self.path = Path(path)
        self._lock = threading.RLock()

    def _load(self) -> Dict[str, List[Dict[str, Any]]]:
        if not self.path.exists():
            return {}
        try:
            raw = json.loads(self.path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return {}
        if not isinstance(raw, dict):
            return {}
        out: Dict[str, List[Dict[str, Any]]] = {}
        for did, entries in raw.items():
            if isinstance(did, str) and isinstance(entries, list):
                out[did] = [e for e in entries if isinstance(e, dict)]
        return out

    def _save(self, data: Dict[str, List[Dict[str, Any]]]) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        tmp = self.path.with_suffix(".json.tmp")
        tmp.write_text(json.dumps(data, indent=2), encoding="utf-8")
        os.replace(tmp, self.path)

    def record(
        self,
        device_id: str,
        action: str,
        *,
        status: str = "ok",
        timestamp: Optional[str] = None,
    ) -> Dict[str, Any]:
        """Append one ``{action, timestamp, status}`` entry for a device.

        ``timestamp`` defaults to ``now()`` in ISO-8601 UTC. Oldest
        entries are dropped once the per-device list exceeds
        :data:`ACTION_LOG_CAP`. Returns the entry written so callers can
        assert on it in tests."""
        ts = timestamp or _utc_now_iso()
        entry = {"action": action, "timestamp": ts, "status": status}
        with self._lock:
            data = self._load()
            entries = data.setdefault(device_id, [])
            entries.append(entry)
            # Trim from the front — oldest-first on disk.
            if len(entries) > ACTION_LOG_CAP:
                del entries[: len(entries) - ACTION_LOG_CAP]
            self._save(data)
        return entry

    def recent(self, device_id: str, *, limit: int = 20) -> List[Dict[str, Any]]:
        """Return up to ``limit`` entries for ``device_id``, newest-first.

        Unknown device → empty list. ``limit`` is clamped to ``>= 0``."""
        limit = max(0, int(limit))
        with self._lock:
            entries = list(self._load().get(device_id, []))
        # Disk is oldest-first; reverse for newest-first, then cap.
        return list(reversed(entries))[:limit]


def _utc_now_iso() -> str:
    """Current UTC time as a second-resolution ISO-8601 string with a
    trailing ``Z`` (e.g. ``2026-05-30T12:34:56Z``) — matches the format
    the GUI's relative-time parser expects."""
    from datetime import datetime, timezone

    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


__all__ = [
    "TIER_READ_ONLY",
    "TIER_TOOL_USE",
    "TIER_DESTRUCTIVE",
    "ALL_TIERS",
    "TIER_RANK",
    "is_valid_tier",
    "tier_rank",
    "Device",
    "DeviceStore",
    "ACTION_LOG_CAP",
    "ActionLog",
]
