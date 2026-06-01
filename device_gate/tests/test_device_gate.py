"""Smoke for the device_gate service surface.

Tests run against the in-process :class:`DeviceGateService` directly —
no pipe, no Gateway. The pipe layer is a thin envelope translator
over these calls; if the core API is right, the wire surface lands
correctly too.

We seed an htpasswd file in the tmpdir so credential checks have a
real password to verify against. The clock is injected so we can
fast-forward expiry.
"""

from __future__ import annotations

from pathlib import Path
from typing import Iterator, Tuple

import pytest

from Core.shared.secure_file import harden_perms
from device_gate import core
from device_gate.store import (
    ACTION_LOG_CAP,
    ActionLog,
    DeviceStore,
    TIER_READ_ONLY,
    TIER_TOOL_USE,
)
from passlib.hash import apr_md5_crypt

Service = Tuple[core.DeviceGateService, "_FakeClock"]


# ── Fixtures ──────────────────────────────────────────────────────────


def _write_htpasswd(path: Path, username: str, password: str) -> None:
    """Write a single-entry htpasswd in apr1 format using passlib —
    the same library device_gate's verify path uses."""
    line = f"{username}:{apr_md5_crypt.hash(password)}\n"
    path.write_text(line, encoding="utf-8")
    # Password hashes on disk — restrict to owner-only access.
    harden_perms(path)


class _FakeClock:
    """Manual clock so expiry tests are deterministic."""

    def __init__(self, start: float = 1_000_000.0) -> None:
        self._t = float(start)

    def __call__(self) -> float:
        return self._t

    def advance(self, seconds: float) -> None:
        self._t += float(seconds)


@pytest.fixture
def service(tmp_path: Path) -> Iterator[Service]:
    htpasswd = tmp_path / "htpasswd"
    _write_htpasswd(htpasswd, "wylde", "letmein")
    devices = tmp_path / "devices.json"
    action_log = ActionLog(tmp_path / "action_log.json")
    clock = _FakeClock()
    svc = core.DeviceGateService(
        store=DeviceStore(devices),
        htpasswd_path=htpasswd,
        action_log=action_log,
        clock=clock,
    )
    core.install_service(svc)
    yield svc, clock
    core.reset_service()


def _pair(svc: core.DeviceGateService) -> str:
    """Pair a device through the happy path and return its device_id —
    a helper for the action-log tests that don't care about the pairing
    mechanics themselves."""
    started = svc.start_pairing()
    return svc.complete_pairing(
        code=started["code"],
        username="wylde",
        password="letmein",
    )["device_id"]


# ── Pairing happy path ───────────────────────────────────────────────


def test_pairing_happy_path(service: Service) -> None:
    svc, _clock = service

    start = svc.start_pairing()
    assert start["code"]
    assert isinstance(start["expires_at"], float)
    assert svc.get_pairing_status()["pairing_active"] is True

    result = svc.complete_pairing(
        code=start["code"],
        username="wylde",
        password="letmein",
        device_metadata={"name": "iPhone-15"},
    )
    assert result["device_id"]
    assert result["token"]
    assert result["tier"] == TIER_READ_ONLY

    # Pairing-mode auto-OFF after success.
    assert svc.get_pairing_status()["pairing_active"] is False

    # The device is in the list.
    devices = svc.list_devices()
    assert len(devices) == 1
    assert devices[0]["device_id"] == result["device_id"]
    assert devices[0]["name"] == "iPhone-15"
    assert devices[0]["tier"] == TIER_READ_ONLY


# ── Pairing failure modes ────────────────────────────────────────────


def test_pairing_wrong_code(service: Service) -> None:
    svc, _clock = service
    svc.start_pairing()
    with pytest.raises(core.DeviceGateError) as exc:
        svc.complete_pairing(
            code="000000",
            username="wylde",
            password="letmein",
        )
    assert exc.value.code == "code_mismatch"


def test_pairing_wrong_credentials(service: Service) -> None:
    svc, _clock = service
    started = svc.start_pairing()
    with pytest.raises(core.DeviceGateError) as exc:
        svc.complete_pairing(
            code=started["code"],
            username="wylde",
            password="WRONG",
        )
    assert exc.value.code == "credential_mismatch"


def test_pairing_expired_code(service: Service) -> None:
    svc, clock = service
    started = svc.start_pairing()
    # Push past the 5-minute expiry.
    clock.advance(core.PAIRING_CODE_TTL_SECONDS + 1)

    with pytest.raises(core.DeviceGateError) as exc:
        svc.complete_pairing(
            code=started["code"],
            username="wylde",
            password="letmein",
        )
    # Once the window is closed, complete_pairing reports it as
    # pairing_inactive (the lazy-expire collapses the state first).
    assert exc.value.code == "pairing_inactive"


def test_pairing_mode_off(service: Service) -> None:
    svc, _clock = service
    # No start_pairing call — pairing-mode is OFF.
    with pytest.raises(core.DeviceGateError) as exc:
        svc.complete_pairing(
            code="123456",
            username="wylde",
            password="letmein",
        )
    assert exc.value.code == "pairing_inactive"


def test_cancel_pairing(service: Service) -> None:
    svc, _clock = service
    svc.start_pairing()
    assert svc.get_pairing_status()["pairing_active"] is True
    out = svc.cancel_pairing()
    assert out["cancelled"] is True
    assert svc.get_pairing_status()["pairing_active"] is False
    # Cancel-when-already-off is a benign no-op.
    out = svc.cancel_pairing()
    assert out["cancelled"] is False


# ── Verify ────────────────────────────────────────────────────────────


def test_verify_returns_tier(service: Service) -> None:
    svc, _clock = service
    started = svc.start_pairing()
    paired = svc.complete_pairing(
        code=started["code"],
        username="wylde",
        password="letmein",
    )
    out = svc.verify(paired["token"])
    assert out["device_id"] == paired["device_id"]
    assert out["tier"] == TIER_READ_ONLY


def test_verify_rejects_invalid_token(service: Service) -> None:
    svc, _clock = service
    with pytest.raises(core.DeviceGateError) as exc:
        svc.verify("not-a-real-token")
    assert exc.value.code == "invalid_token"


def test_verify_updates_last_seen(service: Service) -> None:
    svc, clock = service
    started = svc.start_pairing()
    paired = svc.complete_pairing(
        code=started["code"],
        username="wylde",
        password="letmein",
    )
    clock.advance(60)
    svc.verify(paired["token"])
    devices = svc.list_devices()
    assert devices[0]["last_seen"] >= 1_000_000.0 + 60


# ── Tier change ──────────────────────────────────────────────────────


def test_set_tier_persists(service: Service) -> None:
    svc, _clock = service
    started = svc.start_pairing()
    paired = svc.complete_pairing(
        code=started["code"],
        username="wylde",
        password="letmein",
    )
    out = svc.set_tier(paired["device_id"], TIER_TOOL_USE)
    assert out["tier"] == TIER_TOOL_USE
    # Verify reflects the new tier.
    assert svc.verify(paired["token"])["tier"] == TIER_TOOL_USE


def test_set_tier_rejects_unknown(service: Service) -> None:
    svc, _clock = service
    started = svc.start_pairing()
    paired = svc.complete_pairing(
        code=started["code"],
        username="wylde",
        password="letmein",
    )
    with pytest.raises(core.DeviceGateError) as exc:
        svc.set_tier(paired["device_id"], "superuser")
    assert exc.value.code == "bad_request"


# ── Token rotation ───────────────────────────────────────────────────


def test_rotate_invalidates_old_returns_new(service: Service) -> None:
    svc, _clock = service
    started = svc.start_pairing()
    paired = svc.complete_pairing(
        code=started["code"],
        username="wylde",
        password="letmein",
    )
    rotated = svc.rotate_token(paired["device_id"])
    assert rotated["new_token"] != paired["token"]

    # New token works.
    assert svc.verify(rotated["new_token"])["device_id"] == paired["device_id"]
    # Old token is dead.
    with pytest.raises(core.DeviceGateError):
        svc.verify(paired["token"])


def test_rotate_emits_token_rotated_event(service: Service) -> None:
    svc, _clock = service
    started = svc.start_pairing()
    paired = svc.complete_pairing(
        code=started["code"],
        username="wylde",
        password="letmein",
    )
    # Simulate an active session: a recent verify() touch.
    svc.verify(paired["token"])
    rotated = svc.rotate_token(paired["device_id"])

    events = svc.consume_pending_events(paired["device_id"])
    types = [e["type"] for e in events]
    assert "token_rotated" in types
    rotation_ev = next(e for e in events if e["type"] == "token_rotated")
    assert rotation_ev["new_token"] == rotated["new_token"]
    # Subsequent consume drains the queue (at-most-once).
    assert svc.consume_pending_events(paired["device_id"]) == []


# ── Revocation ───────────────────────────────────────────────────────


def test_revoke_removes_device_and_token(service: Service) -> None:
    svc, _clock = service
    started = svc.start_pairing()
    paired = svc.complete_pairing(
        code=started["code"],
        username="wylde",
        password="letmein",
    )
    svc.revoke(paired["device_id"])
    assert svc.list_devices() == []
    with pytest.raises(core.DeviceGateError):
        svc.verify(paired["token"])


def test_revoke_emits_event(service: Service) -> None:
    svc, _clock = service
    started = svc.start_pairing()
    paired = svc.complete_pairing(
        code=started["code"],
        username="wylde",
        password="letmein",
    )
    svc.revoke(paired["device_id"])
    events = svc.consume_pending_events(paired["device_id"])
    assert any(e["type"] == "revoked" for e in events)


def test_revoke_unknown_device_errors(service: Service) -> None:
    svc, _clock = service
    with pytest.raises(core.DeviceGateError) as exc:
        svc.revoke("dev_nonexistent")
    assert exc.value.code == "not_found"


# ── List + introspection ─────────────────────────────────────────────


def test_list_devices_marks_active_within_threshold(service: Service) -> None:
    svc, clock = service
    started = svc.start_pairing()
    svc.complete_pairing(
        code=started["code"],
        username="wylde",
        password="letmein",
    )
    devices = svc.list_devices()
    assert devices[0]["is_active"] is True

    # Far past the active window → not active anymore.
    clock.advance(120)
    devices = svc.list_devices(active_threshold_s=60)
    assert devices[0]["is_active"] is False


# ── Action log ────────────────────────────────────────────────────────


def test_pairing_records_action(service: Service) -> None:
    svc, _clock = service
    device_id = _pair(svc)
    actions = svc.recent_actions(device_id)
    assert len(actions) == 1
    assert actions[0]["action"] == "paired"
    assert actions[0]["status"] == "ok"
    # ISO-8601 UTC, second resolution, trailing Z.
    assert actions[0]["timestamp"].endswith("Z")


def test_mutators_each_record_an_entry(service: Service) -> None:
    svc, _clock = service
    device_id = _pair(svc)
    svc.set_tier(device_id, TIER_TOOL_USE)
    svc.rotate_token(device_id)
    actions = svc.recent_actions(device_id)
    # paired + tier + rotate, newest-first.
    verbs = [a["action"] for a in actions]
    assert verbs == ["token rotated", f"tier → {TIER_TOOL_USE}", "paired"]
    assert all(a["status"] == "ok" for a in actions)


def test_recent_actions_newest_first_and_honours_limit(service: Service) -> None:
    svc, _clock = service
    device_id = _pair(svc)
    for _ in range(5):
        svc.rotate_token(device_id)
    # 1 paired + 5 rotations = 6 total; limit caps the returned slice.
    limited = svc.recent_actions(device_id, limit=2)
    assert len(limited) == 2
    # Newest-first: the two most recent are rotations.
    assert all(a["action"] == "token rotated" for a in limited)


def test_recent_actions_unknown_device_is_empty(service: Service) -> None:
    svc, _clock = service
    assert svc.recent_actions("dev_nonexistent") == []


def test_revoke_records_and_survives_device_removal(service: Service) -> None:
    svc, _clock = service
    device_id = _pair(svc)
    svc.revoke(device_id)
    # The device row is gone, but the audit trail persists.
    assert svc.list_devices() == []
    actions = svc.recent_actions(device_id)
    verbs = [a["action"] for a in actions]
    assert verbs == ["revoked", "paired"]


def test_action_log_caps_oldest_dropped(tmp_path: Path) -> None:
    log = ActionLog(tmp_path / "action_log.json")
    for i in range(ACTION_LOG_CAP + 10):
        log.record("dev_x", f"act-{i}", status="ok")
    actions = log.recent("dev_x", limit=ACTION_LOG_CAP + 50)
    # Cap enforced — only the most-recent ACTION_LOG_CAP survive.
    assert len(actions) == ACTION_LOG_CAP
    # Newest-first: act-(N+9) at the front, oldest-surviving at the back.
    assert actions[0]["action"] == f"act-{ACTION_LOG_CAP + 9}"
    assert actions[-1]["action"] == "act-10"


def test_recent_actions_bad_payload_rejected() -> None:
    # The pipe handler rejects a non-string device_id / negative limit
    # as bad_request before reaching the service.
    from Core.shared.ipc import IpcError
    from device_gate import pipe

    handler = pipe._wrap_handler(pipe._recent_actions_action)
    with pytest.raises(IpcError) as exc:
        handler({"device_id": 123})
    assert exc.value.code == "bad_request"
    with pytest.raises(IpcError) as exc2:
        handler({"device_id": "dev_x", "limit": -1})
    assert exc2.value.code == "bad_request"
