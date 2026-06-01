"""Unit tests for the Lifecycle daemon's orphan-detection sweep.

Post manifest-ownership refactor, services own their manifests. The
daemon's job is to catch manifests claiming ``state: alive`` whose
pid is no longer running — those are services that died without
calling ``mark_stopped`` (kill -9, segfault, OOM). The sweep flips
those manifests to ``state: dead-orphan``.

These tests use a tmp manifest dir + a fake ``_pid_alive`` so the
test never depends on actual OS process state.
"""

from __future__ import annotations

import json
import sys
import time
from pathlib import Path
from typing import Generator

import pytest

_HERE = Path(__file__).resolve()
_VAULT_ROOT = _HERE.parents[3]
if str(_VAULT_ROOT) not in sys.path:
    sys.path.insert(0, str(_VAULT_ROOT))

from Core.Lifecycle import daemon_state  # noqa: E402
from Core.shared import manifest as _service_manifest  # noqa: E402


@pytest.fixture
def tmp_manifests(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> Generator[Path, None, None]:
    """Redirect both module-level manifest paths to a tmp dir."""
    monkeypatch.setattr(daemon_state, "_MANIFEST_DIR", tmp_path)
    monkeypatch.setattr(_service_manifest, "_MANIFEST_DIR", tmp_path)
    yield tmp_path


def _write_runtime_manifest(path: Path, service: str, pid: int, state: str) -> None:
    (path / f"{service}.json").write_text(
        json.dumps(
            {
                "service": service,
                "status": {
                    "pid": pid,
                    "state": state,
                    "heartbeat": "2026-01-01T00:00:00Z",
                },
            }
        ),
        encoding="utf-8",
    )


class TestSweepOrphans:
    def test_flips_dead_alive_manifest_to_orphan(
        self, tmp_manifests: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        _write_runtime_manifest(tmp_manifests, "ghost", pid=999999, state="alive")
        # Force _pid_alive to say "no".
        monkeypatch.setattr(daemon_state, "_pid_alive", lambda pid: False)

        result = daemon_state.sweep_orphans()

        assert "ghost" in result["orphans"]
        data = json.loads((tmp_manifests / "ghost.json").read_text(encoding="utf-8"))
        assert data["status"]["state"] == "dead-orphan"
        assert data["status"]["last_seen"].endswith("Z")

    def test_leaves_alive_pid_alone(
        self, tmp_manifests: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        _write_runtime_manifest(tmp_manifests, "running", pid=1, state="alive")
        monkeypatch.setattr(daemon_state, "_pid_alive", lambda pid: True)

        result = daemon_state.sweep_orphans()

        assert "running" not in result["orphans"]
        data = json.loads((tmp_manifests / "running.json").read_text(encoding="utf-8"))
        assert data["status"]["state"] == "alive"

    def test_skips_stopped_manifests(
        self, tmp_manifests: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # A service that already wrote 'stopped' is terminal — sweep
        # must not regress its state back to dead-orphan.
        _write_runtime_manifest(tmp_manifests, "graceful", pid=999999, state="stopped")
        monkeypatch.setattr(daemon_state, "_pid_alive", lambda pid: False)

        result = daemon_state.sweep_orphans()

        assert "graceful" not in result["orphans"]
        data = json.loads((tmp_manifests / "graceful.json").read_text(encoding="utf-8"))
        assert data["status"]["state"] == "stopped"

    def test_skips_already_dead_orphan_manifests(
        self, tmp_manifests: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # Re-sweeping a dead-orphan should not re-mark it (last_seen
        # would otherwise tick forward every sweep and lie about when
        # the process actually disappeared).
        _write_runtime_manifest(
            tmp_manifests, "ghosted", pid=999999, state="dead-orphan"
        )
        monkeypatch.setattr(daemon_state, "_pid_alive", lambda pid: False)

        result = daemon_state.sweep_orphans()

        assert "ghosted" not in result["orphans"]

    def test_treats_missing_state_field_as_alive(
        self, tmp_manifests: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # Legacy manifests written before the state-field migration
        # should still be sweepable when their pid is gone.
        path = tmp_manifests / "legacy.json"
        path.write_text(
            json.dumps(
                {
                    "service": "legacy",
                    "status": {"pid": 999999, "heartbeat": "2026-01-01T00:00:00Z"},
                }
            ),
            encoding="utf-8",
        )
        monkeypatch.setattr(daemon_state, "_pid_alive", lambda pid: False)

        result = daemon_state.sweep_orphans()

        assert "legacy" in result["orphans"]


class TestFailedToLaunchDetection:
    def test_spawn_record_past_grace_with_no_manifest_warns(
        self,
        tmp_manifests: Path,
        monkeypatch: pytest.MonkeyPatch,
        caplog: pytest.LogCaptureFixture,
    ) -> None:
        # Pretend the daemon spawned wylde-fakeservice ages ago.
        monkeypatch.setattr(daemon_state, "_SPAWN_GRACE_SECONDS", 0.01)
        daemon_state._record_spawn("wylde-fakeservice", pid=999999)
        time.sleep(0.05)
        monkeypatch.setattr(daemon_state, "_pid_alive", lambda pid: False)

        result = daemon_state.sweep_orphans()

        assert "wylde-fakeservice" in result["failed_to_launch"]
        daemon_state._forget_spawn("wylde-fakeservice")

    def test_spawn_record_within_grace_window_is_quiet(
        self, tmp_manifests: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # Fresh spawn record (within grace) is not yet a failure.
        monkeypatch.setattr(daemon_state, "_SPAWN_GRACE_SECONDS", 60.0)
        daemon_state._record_spawn("wylde-young", pid=999999)
        monkeypatch.setattr(daemon_state, "_pid_alive", lambda pid: False)

        result = daemon_state.sweep_orphans()

        assert "wylde-young" not in result["failed_to_launch"]
        daemon_state._forget_spawn("wylde-young")

    def test_forget_spawn_silences_failed_to_launch(
        self, tmp_manifests: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # _stop_voice etc. clear the spawn record so a deliberate stop
        # doesn't fire the failed-to-launch warning.
        monkeypatch.setattr(daemon_state, "_SPAWN_GRACE_SECONDS", 0.01)
        daemon_state._record_spawn("wylde-stopper", pid=999999)
        time.sleep(0.05)
        daemon_state._forget_spawn("wylde-stopper")
        monkeypatch.setattr(daemon_state, "_pid_alive", lambda pid: False)

        result = daemon_state.sweep_orphans()

        assert "wylde-stopper" not in result["failed_to_launch"]
