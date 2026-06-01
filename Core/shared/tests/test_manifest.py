"""Tests for core/shared/manifest.py.

Manifests are the contract between services and the GUI: the GUI reads
these files to render service cards, so their shape, atomic-write
behavior, and started_at preservation matter.
"""

from __future__ import annotations

import json
import time
from pathlib import Path
from typing import Any, Generator

import pytest

from Core.shared import manifest


@pytest.fixture(autouse=True)
def _point_manifest_dir_at_tmp(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> Generator[Path, None, None]:
    """Redirect manifest writes to a per-test tmp dir."""
    monkeypatch.setattr(manifest, "_MANIFEST_DIR", tmp_path)
    yield tmp_path


def _read(tmp_path: Path, service: str) -> Any:
    return json.loads((tmp_path / f"{service}.json").read_text(encoding="utf-8"))


class TestWriteManifest:
    def test_basic_shape(self, _point_manifest_dir_at_tmp: Path) -> None:
        manifest.write_manifest(
            service_name="demo",
            port=1234,
            category="ai",
            description="a demo service",
        )
        data = _read(_point_manifest_dir_at_tmp, "demo")
        assert data["service"] == "demo"
        assert data["port"] == 1234
        assert data["category"] == "ai"
        assert data["description"] == "a demo service"
        assert data["pipe"] == r"\\.\pipe\wylde-demo"
        assert data["version"] == "1.0.0"
        assert data["contributes"] == {}
        assert "status" in data
        assert data["status"]["pid"] > 0
        assert data["status"]["started_at"].endswith("Z")
        assert data["status"]["heartbeat"].endswith("Z")

    def test_contributes_passthrough(self, _point_manifest_dir_at_tmp: Path) -> None:
        contributes = {
            "tools": [{"name": "foo", "description": "f"}],
            "views": [{"id": "bar", "title": "Bar"}],
            "settings": [{"key": "x", "type": "bool"}],
        }
        manifest.write_manifest("demo", 1, "x", "y", contributes=contributes)
        data = _read(_point_manifest_dir_at_tmp, "demo")
        assert data["contributes"] == contributes

    def test_started_at_preserved_across_writes(
        self, _point_manifest_dir_at_tmp: Path
    ) -> None:
        manifest.write_manifest("demo", 1, "x", "y")
        first = _read(_point_manifest_dir_at_tmp, "demo")
        started_first = first["status"]["started_at"]

        # Small delay to ensure `_now_iso()` would produce a different value
        time.sleep(1.01)
        manifest.write_manifest("demo", 1, "x", "y-v2")
        second = _read(_point_manifest_dir_at_tmp, "demo")
        assert second["status"]["started_at"] == started_first
        # heartbeat still updates
        assert second["status"]["heartbeat"] >= started_first
        # description change landed
        assert second["description"] == "y-v2"

    def test_overwrite_on_multiple_calls(
        self, _point_manifest_dir_at_tmp: Path
    ) -> None:
        manifest.write_manifest("demo", 100, "a", "first")
        manifest.write_manifest("demo", 200, "b", "second")
        data = _read(_point_manifest_dir_at_tmp, "demo")
        assert data["port"] == 200
        assert data["category"] == "b"
        assert data["description"] == "second"

    def test_atomic_write_no_tmp_leftover(
        self, _point_manifest_dir_at_tmp: Path
    ) -> None:
        manifest.write_manifest("demo", 1, "x", "y")
        # After a successful atomic replace, the .tmp should not exist.
        assert not (_point_manifest_dir_at_tmp / "demo.tmp").exists()
        assert (_point_manifest_dir_at_tmp / "demo.json").exists()

    def test_creates_parent_directory(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # Even if the parent doesn't exist yet, the writer must create it.
        nested = tmp_path / "nested" / "manifests"
        monkeypatch.setattr(manifest, "_MANIFEST_DIR", nested)
        manifest.write_manifest("demo", 1, "x", "y")
        assert (nested / "demo.json").exists()

    def test_json_is_pretty(self, _point_manifest_dir_at_tmp: Path) -> None:
        # Humans read these files during debugging; indent=2 is part of
        # the contract.
        manifest.write_manifest("demo", 1, "x", "y")
        raw = (_point_manifest_dir_at_tmp / "demo.json").read_text("utf-8")
        assert "\n  " in raw


class TestHeartbeat:
    def test_start_and_stop(self, _point_manifest_dir_at_tmp: Path) -> None:
        manifest.write_manifest("demo", 1, "x", "y")
        manifest.start_heartbeat("demo", interval=0.05)
        # Give the thread one tick
        time.sleep(0.15)
        manifest.stop_heartbeat("demo")
        # stop is idempotent
        manifest.stop_heartbeat("demo")

    def test_stop_on_unknown_is_noop(self) -> None:
        # Must not raise even when no heartbeat was started.
        manifest.stop_heartbeat("never-started")


class TestTimestampFormat:
    def test_now_iso_is_utc_suffixed(self) -> None:
        ts = manifest._now_iso()
        assert ts.endswith("Z")
        # Parse-able as ISO 8601
        import datetime

        datetime.datetime.strptime(ts, "%Y-%m-%dT%H:%M:%SZ")


class TestStateField:
    """status.state starts as 'alive' and flips on mark_stopped /
    mark_orphan_dead. This is the contract the orphan-detection sweep
    in Core/Lifecycle/daemon_state.py relies on."""

    def test_initial_state_is_alive(self, _point_manifest_dir_at_tmp: Path) -> None:
        manifest.write_manifest("demo", 1, "x", "y")
        data = _read(_point_manifest_dir_at_tmp, "demo")
        assert data["status"]["state"] == "alive"

    def test_mark_stopped_flips_state(self, _point_manifest_dir_at_tmp: Path) -> None:
        manifest.write_manifest("demo", 1, "x", "y")
        manifest.mark_stopped("demo")
        data = _read(_point_manifest_dir_at_tmp, "demo")
        assert data["status"]["state"] == "stopped"
        assert data["status"]["stop_time"].endswith("Z")

    def test_mark_stopped_with_no_cache_reads_from_disk(
        self, _point_manifest_dir_at_tmp: Path
    ) -> None:
        # Service B's process never went through write_manifest in this
        # interpreter — mark_stopped must still flip the disk file.
        # Simulate by writing the file directly.
        path = _point_manifest_dir_at_tmp / "ghost.json"
        path.write_text(
            json.dumps(
                {
                    "service": "ghost",
                    "status": {"pid": 999, "state": "alive", "heartbeat": "z"},
                }
            )
        )
        manifest.mark_stopped("ghost")
        data = json.loads(path.read_text(encoding="utf-8"))
        assert data["status"]["state"] == "stopped"

    def test_mark_stopped_is_idempotent(self, _point_manifest_dir_at_tmp: Path) -> None:
        # Missing manifest is a no-op, not an error.
        manifest.mark_stopped("never-existed")
        # Double-mark on existing manifest is also fine.
        manifest.write_manifest("demo", 1, "x", "y")
        manifest.mark_stopped("demo")
        manifest.mark_stopped("demo")
        data = _read(_point_manifest_dir_at_tmp, "demo")
        assert data["status"]["state"] == "stopped"

    def test_mark_orphan_dead_flips_state(
        self, _point_manifest_dir_at_tmp: Path
    ) -> None:
        manifest.write_manifest("demo", 1, "x", "y")
        manifest.mark_orphan_dead("demo")
        data = _read(_point_manifest_dir_at_tmp, "demo")
        assert data["status"]["state"] == "dead-orphan"
        assert data["status"]["last_seen"].endswith("Z")

    def test_mark_orphan_dead_preserves_heartbeat(
        self, _point_manifest_dir_at_tmp: Path
    ) -> None:
        # Forensic timeline matters: the heartbeat at time-of-death is
        # exactly the data point a postmortem needs.
        manifest.write_manifest("demo", 1, "x", "y")
        before = _read(_point_manifest_dir_at_tmp, "demo")["status"]["heartbeat"]
        manifest.mark_orphan_dead("demo")
        after = _read(_point_manifest_dir_at_tmp, "demo")["status"]["heartbeat"]
        assert before == after

    def test_mark_orphan_dead_on_missing_manifest_is_noop(
        self, _point_manifest_dir_at_tmp: Path
    ) -> None:
        # Missing file → silent no-op, never raises.
        manifest.mark_orphan_dead("never-existed")
        assert not (_point_manifest_dir_at_tmp / "never-existed.json").exists()
