"""Direct tests for vram_broker (broker-side).

Exercises the Flask test client against the same app install() wires up at
runtime, so the tests cover both the policy (`_try_grant`, reaper, Ollama
reflection) and the HTTP contract the `Core/shared/vram_broker.py` client
relies on.

Run with:
    cd "Core/resource_monitor"
    python -m pytest test_vram_broker.py -v
"""

from __future__ import annotations

import sys
import time
from pathlib import Path
from typing import Any
from unittest.mock import patch

import pytest
from flask import Flask

HERE = Path(__file__).parent
sys.path.insert(0, str(HERE))

import vram_broker_service as vram_broker  # noqa: E402


GB = 1024 * 1024 * 1024


@pytest.fixture
def app() -> Flask:
    """Fresh broker + fresh Flask app per test. Seeds a 16 GB GPU so
    headroom math is deterministic; individual tests may reset this."""
    vram_broker._reset_for_tests()
    a = Flask(__name__)
    vram_broker.install(a, gpu_available=False)  # skip pynvml init in tests
    vram_broker._registry.set_gpu(total=16 * GB, used=0, name="TestGPU")
    return a


def _post(app: Flask, path: str, body: dict[str, Any]) -> tuple[int, Any]:
    with app.test_client() as c:
        resp = c.post(path, json=body)
    return resp.status_code, resp.get_json()


def _get(app: Flask, path: str) -> tuple[int, Any]:
    with app.test_client() as c:
        resp = c.get(path)
    return resp.status_code, resp.get_json()


def test_health_returns_ok(app: Flask) -> None:
    # The lifecycle `service.health` action probes every service with a
    # GET /health; the broker shipped only /vram/* routes, so that GET
    # 404'd and painted an up broker red on the dashboard. Pin the
    # restored route + its {ok, service} shape.
    status, body = _get(app, "/health")
    assert status == 200
    assert body["ok"] is True
    assert body["service"] == "wylde-vram-broker"


def test_reserve_grants_when_fits(app: Flask) -> None:
    status, body = _post(
        app,
        "/vram/reserve",
        {
            "service": "wylde-caption",
            "model": "florence-2",
            "bytes": 4 * GB,
            "priority": 40,
            "ttl": 60,
        },
    )
    assert status == 200
    assert body["service"] == "wylde-caption"
    assert body["bytes"] == 4 * GB
    assert body["priority"] == 40
    assert body["lease_id"]
    assert not body["synthetic"]


def test_reserve_rejects_when_total_too_small(app: Flask) -> None:
    # Requesting more than total (minus safety margin) — 20 GB on a 16 GB GPU
    status, body = _post(
        app,
        "/vram/reserve",
        {
            "service": "wylde-trainer",
            "model": "big-llm",
            "bytes": 20 * GB,
            "priority": 20,
        },
    )
    assert status == 409
    assert body["code"] == "would_exceed_total"


def test_reserve_rejects_when_no_headroom_no_preempt(app: Flask) -> None:
    # Fill 13 GB with an inference-priority lease
    _post(
        app,
        "/vram/reserve",
        {
            "service": "ollama",
            "model": "gemma3-27b",
            "bytes": 13 * GB,
            "priority": 100,
        },
    )
    # Now trainer asks for 4 GB, only ~3 GB - margin free; no preempt.
    status, body = _post(
        app,
        "/vram/reserve",
        {
            "service": "wylde-trainer",
            "model": "lora",
            "bytes": 4 * GB,
            "priority": 20,
        },
    )
    assert status == 409
    assert body["code"] == "insufficient_vram"
    assert body["details"]["requested_bytes"] == 4 * GB
    # Ollama must show up as a blocker even though trainer is lower priority
    blockers = body["details"]["blockers"]
    assert any(b["service"] == "ollama" for b in blockers)


def test_preemption_evicts_lower_priority(app: Flask) -> None:
    # Caption (priority 40) is holding 10 GB
    status, body = _post(
        app,
        "/vram/reserve",
        {
            "service": "wylde-caption",
            "model": "qwen-vl",
            "bytes": 10 * GB,
            "priority": 40,
        },
    )
    assert status == 200
    caption_lease = body["lease_id"]

    # Background "evict handler" releases the lease as soon as it sees a
    # signal. The broker's _signal_evict goes through ipc.send which we
    # stub; to simulate a well-behaved service, release from a timer so
    # the broker observes the lease disappear inside _EVICT_TIMEOUT_S.
    import threading

    def _auto_release() -> None:
        time.sleep(0.2)
        vram_broker._registry.remove(caption_lease)

    threading.Thread(target=_auto_release, daemon=True).start()

    # After the broker→broker/ package split, _try_grant calls _signal_evict via
    # broker.policy's own module namespace, so patching the shim's binding has
    # no effect — we must patch the policy module's binding directly.
    import broker.policy as _broker_policy

    with patch.object(_broker_policy, "_signal_evict"):
        status, body = _post(
            app,
            "/vram/reserve",
            {
                "service": "wylde-voice",
                "model": "whisper-large",
                "bytes": 8 * GB,
                "priority": 80,
                "preempt": True,
            },
        )
    assert status == 200, body
    assert body["service"] == "wylde-voice"


def test_preemption_refused_when_blockers_are_higher_priority(app: Flask) -> None:
    # Ollama (priority 100) holds 12 GB. Trainer (20) asks for 4 GB with
    # preempt=True — there's nothing lower than trainer to evict, so the
    # broker must refuse, not hang, not evict the higher-priority lease.
    _post(
        app,
        "/vram/reserve",
        {
            "service": "ollama",
            "model": "big",
            "bytes": 12 * GB,
            "priority": 100,
        },
    )
    status, body = _post(
        app,
        "/vram/reserve",
        {
            "service": "wylde-trainer",
            "model": "lora",
            "bytes": 4 * GB,
            "priority": 20,
            "preempt": True,
        },
    )
    assert status == 409
    assert body["code"] == "insufficient_vram"


def test_release_frees_bytes(app: Flask) -> None:
    _, grant = _post(
        app,
        "/vram/reserve",
        {
            "service": "wylde-rag",
            "model": "reranker",
            "bytes": 2 * GB,
            "priority": 60,
        },
    )
    status, body = _post(app, "/vram/release", {"lease_id": grant["lease_id"]})
    assert status == 200
    assert body["known"] is True
    assert body["freed_bytes"] == 2 * GB
    # Re-release is a no-op
    status, body = _post(app, "/vram/release", {"lease_id": grant["lease_id"]})
    assert body["known"] is False


def test_heartbeat_extends_ttl(app: Flask) -> None:
    _, grant = _post(
        app,
        "/vram/reserve",
        {
            "service": "wylde-caption",
            "model": "x",
            "bytes": GB,
            "priority": 40,
            "ttl": 5,
        },
    )
    first_exp = grant["expires_at"]
    time.sleep(0.05)
    status, body = _post(
        app, "/vram/heartbeat", {"lease_id": grant["lease_id"], "ttl": 60}
    )
    assert status == 200
    assert body["expires_at"] > first_exp


def test_heartbeat_on_unknown_lease_is_404(app: Flask) -> None:
    status, body = _post(app, "/vram/heartbeat", {"lease_id": "nope"})
    assert status == 404
    assert body["code"] == "not_found"


def test_reap_expired(app: Flask) -> None:
    # Manually insert a lease already past its deadline, skip HTTP so we
    # can forge expires_at. This matches what the reaper sees when a
    # service crashes before release().
    lease = vram_broker.Lease(
        lease_id="old",
        service="wylde-caption",
        model="x",
        bytes=GB,
        priority=40,
        granted_at=time.time() - 100,
        expires_at=time.time() - 1,
        heartbeat_at=time.time() - 100,
        pid=0,
        synthetic=False,
    )
    vram_broker._registry.add(lease)
    removed = vram_broker._registry.reap_expired()
    assert len(removed) == 1
    assert removed[0].lease_id == "old"
    assert vram_broker._registry.get("old") is None


def test_synthetic_ollama_leases_not_reaped(app: Flask) -> None:
    # Synthetic leases are rebuilt each poll; the reaper must leave them
    # alone even if their expires_at is in the past.
    lease = vram_broker.Lease(
        lease_id="ollama:x",
        service="ollama",
        model="gemma",
        bytes=5 * GB,
        priority=100,
        granted_at=0,
        expires_at=0,
        heartbeat_at=0,
        pid=0,
        synthetic=True,
    )
    vram_broker._registry.add(lease)
    removed = vram_broker._registry.reap_expired()
    assert removed == []
    assert vram_broker._registry.get("ollama:x") is not None


def test_nonce_dedupe(app: Flask) -> None:
    nonce = "abc123"
    _, grant1 = _post(
        app,
        "/vram/reserve",
        {
            "service": "wylde-caption",
            "model": "x",
            "bytes": GB,
            "priority": 40,
            "client_nonce": nonce,
        },
    )
    _, grant2 = _post(
        app,
        "/vram/reserve",
        {
            "service": "wylde-caption",
            "model": "x",
            "bytes": GB,
            "priority": 40,
            "client_nonce": nonce,
        },
    )
    # Same nonce → same lease_id → broker accounts for 1 GB, not 2.
    assert grant1["lease_id"] == grant2["lease_id"]
    assert vram_broker._registry.reserved_total() == GB


def test_state_shape(app: Flask) -> None:
    _post(
        app,
        "/vram/reserve",
        {
            "service": "wylde-caption",
            "model": "x",
            "bytes": 2 * GB,
            "priority": 40,
        },
    )
    status, body = _get(app, "/vram/state")
    assert status == 200
    assert body["gpu"]["total_bytes"] == 16 * GB
    assert body["gpu"]["reserved_bytes"] == 2 * GB
    assert body["gpu"]["free_for_grant"] <= (16 - 2) * GB
    assert any(lease["service"] == "wylde-caption" for lease in body["leases"])
    # by_service is sorted high-priority first
    priorities = [row["priority"] for row in body["by_service"]]
    assert priorities == sorted(priorities, reverse=True)


def test_ollama_reflection_produces_synthetic_leases(app: Flask) -> None:
    fake_ps = {
        "models": [
            {
                "name": "gemma3:27b",
                "size_vram": 10 * GB,
                "expires_at": "2099-01-01T00:00:00Z",
            },
            {"name": "qwen:7b", "size_vram": 4 * GB},
        ],
    }

    class _FakeResp:
        def __init__(self, payload: dict[str, Any]) -> None:
            import json

            self._raw = json.dumps(payload).encode("utf-8")

        def read(self) -> bytes:
            return self._raw

        def __enter__(self) -> _FakeResp:
            return self

        def __exit__(self, *a: Any) -> None:
            return None

    with patch(
        "broker.workers.urllib.request.urlopen", return_value=_FakeResp(fake_ps)
    ):
        leases = vram_broker._poll_ollama()
    assert len(leases) == 2
    assert all(lease.synthetic for lease in leases)
    assert all(lease.service == "ollama" for lease in leases)
    # Replace_synthetic wires them into the registry
    vram_broker._registry.replace_synthetic("ollama", leases)
    assert vram_broker._registry.reserved_total() == 14 * GB
