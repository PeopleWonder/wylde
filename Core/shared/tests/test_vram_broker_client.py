"""Tests for the client-side VRAM broker library (Core/shared/vram_broker.py).

Broker-side behavior (reserve/preempt/reap) lives in
Core/resource_monitor/test_vram_broker.py; this file only covers what the
client library guarantees: priority-for-service mapping, the synthetic
fallback when WYLDE_VRAM_REQUIRED=0, proper routing of the VramUnavailable
error class, and the input-validation edge cases.
"""

from __future__ import annotations

from typing import Any

import pytest

from Core.shared import ipc as _ipc_mod
from Core.shared import vram_broker as vram

GB = 1024 * 1024 * 1024


def _install_fake_ipc(monkeypatch: pytest.MonkeyPatch, send_return: Any) -> None:
    """Replace ``Core.shared.ipc.send_action`` with a stub returning
    ``send_return`` (a Reply-like object). The client now invokes broker
    operations as actions, so tests intercept :func:`ipc.send_action`
    rather than the legacy :func:`ipc.send` route call."""
    monkeypatch.setattr(_ipc_mod, "send_action", lambda *a, **kw: send_return)


class _FakeReply:
    def __init__(self, ok: bool, data: Any = None, error: Any = None) -> None:
        self.ok = ok
        self.data = data
        self.error = error


# ── Priority.for_service mapping ──────────────────────────────────────
class TestPriorityForService:
    def test_ollama(self) -> None:
        assert vram.Priority.for_service("ollama") == vram.Priority.INFERENCE

    def test_voice_assistant(self) -> None:
        assert vram.Priority.for_service("wylde-voice-assistant") == vram.Priority.VOICE

    def test_rag(self) -> None:
        assert vram.Priority.for_service("wylde-rag") == vram.Priority.RAG

    def test_caption(self) -> None:
        assert vram.Priority.for_service("wylde-caption") == vram.Priority.CAPTION

    def test_trainer(self) -> None:
        assert vram.Priority.for_service("wylde-trainer") == vram.Priority.TRAINER

    def test_unknown_defaults_to_caption(self) -> None:
        # A sensible non-preemptible default: new services don't accidentally
        # get promoted to INFERENCE priority.
        assert vram.Priority.for_service("mystery") == vram.Priority.CAPTION

    def test_empty_or_none_string(self) -> None:
        assert vram.Priority.for_service("") == vram.Priority.CAPTION
        assert vram.Priority.for_service(None) == vram.Priority.CAPTION

    def test_priority_ordering(self) -> None:
        # The policy contract: inference > voice > rag > caption > trainer.
        assert vram.Priority.INFERENCE > vram.Priority.VOICE
        assert vram.Priority.VOICE > vram.Priority.RAG
        assert vram.Priority.RAG > vram.Priority.CAPTION
        assert vram.Priority.CAPTION > vram.Priority.TRAINER


# ── reserve() input validation ────────────────────────────────────────
class TestReserveValidation:
    def test_zero_bytes_rejected(self) -> None:
        with pytest.raises(vram.VramError) as exc_info:
            vram.reserve("wylde-caption", "x", bytes=0)
        assert exc_info.value.code == "invalid_request"

    def test_negative_bytes_rejected(self) -> None:
        with pytest.raises(vram.VramError) as exc_info:
            vram.reserve("wylde-caption", "x", bytes=-1)
        assert exc_info.value.code == "invalid_request"


# ── reserve() happy path with fake broker ─────────────────────────────
class TestReserveHappyPath:
    def test_grant_builds_lease_from_reply(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        reply_data = {
            "lease_id": "lease-xyz",
            "service": "wylde-caption",
            "model": "florence-2",
            "bytes": 4 * GB,
            "priority": 40,
            "granted_at": 1000.0,
            "expires_at": 1060.0,
            "synthetic": False,
        }
        _install_fake_ipc(monkeypatch, _FakeReply(ok=True, data=reply_data))

        lease = vram.reserve("wylde-caption", "florence-2", bytes=4 * GB)
        assert lease.lease_id == "lease-xyz"
        assert lease.bytes == 4 * GB
        assert lease.priority == 40
        assert not lease.synthetic

        # Clean up background heartbeat
        vram.release(lease.lease_id)

    def test_priority_defaulted_from_service(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        seen: dict[str, Any] = {}

        def _send_action(
            service: str, action: str, payload: Any, timeout: float | None = None
        ) -> _FakeReply:
            if action == "vram.reserve":
                seen["priority"] = payload["priority"]
                return _FakeReply(
                    ok=True,
                    data={
                        "lease_id": "l1",
                        "service": service,
                        "model": "m",
                        "bytes": payload["bytes"],
                        "priority": payload["priority"],
                        "granted_at": 0,
                        "expires_at": 1,
                        "synthetic": False,
                    },
                )
            return _FakeReply(ok=True, data={"known": True, "freed_bytes": 0})

        monkeypatch.setattr(_ipc_mod, "send_action", _send_action)

        lease = vram.reserve("wylde-trainer", "lora", bytes=GB)
        assert seen["priority"] == vram.Priority.TRAINER
        vram.release(lease.lease_id)


# ── reserve() error routing ───────────────────────────────────────────
class TestReserveErrorRouting:
    def test_insufficient_vram_raises_vram_unavailable(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        reply = _FakeReply(
            ok=False,
            error={
                "code": "insufficient_vram",
                "message": "need 4 GiB, free 1 GiB",
                "details": {
                    "blockers": [],
                    "free_bytes": GB,
                    "requested_bytes": 4 * GB,
                },
            },
        )
        _install_fake_ipc(monkeypatch, reply)
        with pytest.raises(vram.VramUnavailable) as exc_info:
            vram.reserve("wylde-trainer", "x", bytes=4 * GB)
        assert exc_info.value.code == "insufficient_vram"
        assert isinstance(exc_info.value, vram.VramError)

    def test_would_exceed_total_raises_vram_unavailable(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        reply = _FakeReply(
            ok=False,
            error={
                "code": "would_exceed_total",
                "message": "request > GPU total",
            },
        )
        _install_fake_ipc(monkeypatch, reply)
        with pytest.raises(vram.VramUnavailable):
            vram.reserve("wylde-trainer", "x", bytes=100 * GB)

    def test_broker_unreachable_fails_fast_when_required(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setattr(vram, "_REQUIRED", True)
        reply = _FakeReply(
            ok=False,
            error={
                "code": "transport",
                "message": "pipe not found",
            },
        )
        _install_fake_ipc(monkeypatch, reply)
        with pytest.raises(vram.VramError) as exc_info:
            vram.reserve("wylde-caption", "x", bytes=GB)
        assert exc_info.value.code == "broker_unreachable"

    def test_broker_unreachable_returns_synthetic_when_optional(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setattr(vram, "_REQUIRED", False)
        reply = _FakeReply(
            ok=False,
            error={
                "code": "pipe_connect",
                "message": "broker service is down",
            },
        )
        _install_fake_ipc(monkeypatch, reply)
        lease = vram.reserve("wylde-caption", "x", bytes=GB)
        assert lease.synthetic is True
        assert lease.lease_id.startswith("synthetic:")
        # Synthetic leases don't need release() to touch the broker, but
        # calling it should be safe.
        vram.release(lease.lease_id)

    def test_http_404_treated_as_unreachable(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # Happens when the broker process is up but the broker module didn't load.
        monkeypatch.setattr(vram, "_REQUIRED", False)
        reply = _FakeReply(
            ok=False,
            error={
                "code": "http_404",
                "message": "not found",
            },
        )
        _install_fake_ipc(monkeypatch, reply)
        lease = vram.reserve("wylde-caption", "x", bytes=GB)
        assert lease.synthetic is True


# ── release() ─────────────────────────────────────────────────────────
class TestRelease:
    def test_release_empty_id_is_noop(self) -> None:
        vram.release("")
        vram.release(None)

    def test_release_synthetic_does_not_call_broker(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        called: list[tuple[Any, Any]] = []

        def _send_action(*a: Any, **kw: Any) -> _FakeReply:
            called.append((a, kw))
            return _FakeReply(ok=True, data={})

        monkeypatch.setattr(_ipc_mod, "send_action", _send_action)
        vram.release("synthetic:abc123")
        assert called == []

    def test_release_swallows_broker_errors(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        reply = _FakeReply(ok=False, error={"code": "transport", "message": "x"})
        _install_fake_ipc(monkeypatch, reply)
        # Must not raise — a dying caller shouldn't get an exception from
        # its own cleanup path.
        vram.release("real-lease-id")


# ── reserved() context manager ────────────────────────────────────────
class TestReservedContext:
    def test_releases_on_exit(self, monkeypatch: pytest.MonkeyPatch) -> None:
        released: list[Any] = []

        def _send_action(
            service: str, action: str, payload: Any, timeout: float | None = None
        ) -> _FakeReply:
            if action == "vram.reserve":
                return _FakeReply(
                    ok=True,
                    data={
                        "lease_id": "l1",
                        "service": service,
                        "model": "m",
                        "bytes": payload["bytes"],
                        "priority": payload["priority"],
                        "granted_at": 0,
                        "expires_at": 1,
                        "synthetic": False,
                    },
                )
            if action == "vram.release":
                released.append(payload.get("lease_id"))
                return _FakeReply(
                    ok=True,
                    data={"known": True, "freed_bytes": payload.get("bytes", 0)},
                )
            return _FakeReply(ok=True, data={})

        monkeypatch.setattr(_ipc_mod, "send_action", _send_action)

        with vram.reserved("wylde-caption", "m", bytes=GB) as lease:
            assert lease.lease_id == "l1"

        assert released == ["l1"]

    def test_releases_on_exception(self, monkeypatch: pytest.MonkeyPatch) -> None:
        released: list[Any] = []

        def _send_action(
            service: str, action: str, payload: Any, timeout: float | None = None
        ) -> _FakeReply:
            if action == "vram.reserve":
                return _FakeReply(
                    ok=True,
                    data={
                        "lease_id": "l1",
                        "service": service,
                        "model": "m",
                        "bytes": payload["bytes"],
                        "priority": payload["priority"],
                        "granted_at": 0,
                        "expires_at": 1,
                        "synthetic": False,
                    },
                )
            if action == "vram.release":
                released.append(payload.get("lease_id"))
                return _FakeReply(ok=True, data={})
            return _FakeReply(ok=True, data={})

        monkeypatch.setattr(_ipc_mod, "send_action", _send_action)

        with pytest.raises(RuntimeError), vram.reserved("wylde-caption", "m", bytes=GB):
            raise RuntimeError("kaboom")
        assert released == ["l1"]


# ── get_state() ───────────────────────────────────────────────────────
class TestGetState:
    def test_returns_empty_dict_when_unreachable(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        reply = _FakeReply(ok=False, error={"code": "transport", "message": "x"})
        _install_fake_ipc(monkeypatch, reply)
        # _REQUIRED=True would raise from _call, but get_state catches and
        # returns {} either way.
        monkeypatch.setattr(vram, "_REQUIRED", True)
        state = vram.get_state()
        assert state == {}

    def test_passes_through_broker_state(self, monkeypatch: pytest.MonkeyPatch) -> None:
        payload = {"gpu": {"total_bytes": 16 * GB}, "leases": []}
        _install_fake_ipc(monkeypatch, _FakeReply(ok=True, data=payload))
        assert vram.get_state() == payload


# ── format_blockers() ─────────────────────────────────────────────────
class TestFormatBlockers:
    def test_empty(self) -> None:
        err = vram.VramUnavailable(
            "insufficient_vram",
            "no room",
            {"blockers": [], "free_bytes": 0, "requested_bytes": GB},
        )
        out = vram.format_blockers(err)
        assert "1024 MiB" in out
        assert "free 0 MiB" in out

    def test_lists_each_blocker(self) -> None:
        err = vram.VramUnavailable(
            "insufficient_vram",
            "no room",
            {
                "blockers": [
                    {
                        "service": "ollama",
                        "model": "g",
                        "bytes": 10 * GB,
                        "priority": 100,
                    },
                    {
                        "service": "wylde-caption",
                        "model": "x",
                        "bytes": 2 * GB,
                        "priority": 40,
                    },
                ],
                "free_bytes": GB,
                "requested_bytes": 4 * GB,
            },
        )
        out = vram.format_blockers(err)
        assert "ollama" in out
        assert "wylde-caption" in out
        assert "priority 100" in out
