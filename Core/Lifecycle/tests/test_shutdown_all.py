"""Unit test for the unified shutdown_all_action.

Both surfaces (the lifecycle pipe action AND the daemon's signal
handler) go through the same teardown function. This test patches
out the OS-level subprocess machinery and asserts:

* All three special-cased Popen handles get a stop call.
* The memory scheduler stops too.
* The action's response payload lists every component that was
  alive at the moment of the call.
* Nothing in the response leaks subprocess objects (only names).

We don't actually spawn anything — module-level globals get monkey-
patched with a recording fake Popen that simulates "process is alive
until terminate() is called, then exits with code 0."
"""

from __future__ import annotations

import sys
from pathlib import Path
from typing import Any, Generator, Optional

import pytest

_HERE = Path(__file__).resolve()
_VAULT_ROOT = _HERE.parents[3]
if str(_VAULT_ROOT) not in sys.path:
    sys.path.insert(0, str(_VAULT_ROOT))


class _FakePopen:
    """Behaves like a live ``subprocess.Popen``. Goes 'dead' after
    ``terminate()`` so the daemon's wait() loop returns immediately."""

    def __init__(self, pid: int) -> None:
        self.pid = pid
        self._alive = True
        self.terminate_called = False
        self.kill_called = False
        self.wait_called = 0
        self.signals: list[int] = []

    def poll(self) -> Optional[int]:
        return None if self._alive else 0

    def terminate(self) -> None:
        self.terminate_called = True
        self._alive = False

    def send_signal(self, sig: int) -> None:
        self.signals.append(sig)
        # CTRL_BREAK behaves like terminate() in our fake.
        self._alive = False

    def kill(self) -> None:
        self.kill_called = True
        self._alive = False

    def wait(self, timeout: Optional[float] = None) -> int:
        self.wait_called += 1
        return 0


class _FakeScheduler:
    def __init__(self) -> None:
        self.stop_called = False

    def start(self) -> bool:  # pragma: no cover - unused in this test
        return True

    def stop(self) -> None:
        self.stop_called = True


@pytest.fixture
def daemon_module(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> Generator[dict[str, Any], None, None]:
    """Import + patch the daemon module so the special-cased Popen
    globals carry fakes and the scheduler is a recording stub."""
    from Core.Lifecycle import control, daemon_state

    voice_proc = _FakePopen(pid=11111)
    device_gate_proc = _FakePopen(pid=22222)
    memgraph_proc = _FakePopen(pid=33333)
    scheduler = _FakeScheduler()

    monkeypatch.setattr(daemon_state, "_voice_proc", voice_proc)
    monkeypatch.setattr(daemon_state, "_device_gate_proc", device_gate_proc)
    monkeypatch.setattr(daemon_state, "_memgraph_proc", memgraph_proc)
    monkeypatch.setattr(daemon_state, "_memory_scheduler", scheduler)

    # daemon_state's _stop_* functions call the global Popen via
    # send_signal then wait(timeout=15). Our fake's wait() returns
    # immediately so this works without further patching.

    # Point the manifest-orphan reaper at an empty dir so it doesn't
    # walk the real data/manifests/ during the test — that walk would
    # otherwise read PIDs from a developer's running daemon and (worse)
    # try to terminate them.
    empty_manifests = tmp_path / "no-manifests"
    empty_manifests.mkdir()
    monkeypatch.setattr(daemon_state, "_MANIFEST_DIR", empty_manifests)

    # control.shutdown_all_action also calls launcher.shutdown_all
    # which iterates launcher.get_running(). Replace with a no-op
    # so we test the daemon-managed half in isolation. Note: shutdown
    # is imported in control.shutdown_all_action via `from . import
    # shutdown`; patch it on the Lifecycle package so both paths see
    # the no-op.
    from Core.Lifecycle import shutdown as _shutdown

    monkeypatch.setattr(_shutdown, "shutdown_all", lambda: None)
    monkeypatch.setattr(control._launcher, "get_running", lambda: {})

    yield {
        "daemon_state": daemon_state,
        "control": control,
        "voice_proc": voice_proc,
        "device_gate_proc": device_gate_proc,
        "memgraph_proc": memgraph_proc,
        "scheduler": scheduler,
    }


def test_stop_all_daemon_managed_calls_each_stop(daemon_module: dict[str, Any]) -> None:
    daemon_state = daemon_module["daemon_state"]
    summary = daemon_state.stop_all_daemon_managed()

    # All three subprocess handles got a graceful signal.
    assert daemon_module["voice_proc"]._alive is False
    assert daemon_module["device_gate_proc"]._alive is False
    assert daemon_module["memgraph_proc"]._alive is False

    # Scheduler thread was stopped.
    assert daemon_module["scheduler"].stop_called is True

    # Summary lists every component that was alive at call time.
    stopped = set(summary["stopped"])
    assert stopped == {
        "memory_scheduler",
        "wylde-voice",
        "wylde-device-gate",
        "wylde-memgraph",
    }, f"unexpected stopped set: {summary['stopped']!r}"
    assert summary["count"] == 4
    assert summary["failed"] == []


def test_stop_all_daemon_managed_skips_already_dead(
    daemon_module: dict[str, Any], monkeypatch: pytest.MonkeyPatch
) -> None:
    """If a Popen handle is None or its child already exited, the
    function shouldn't fall over and shouldn't list it in 'stopped'."""
    daemon_state = daemon_module["daemon_state"]
    # Wipe Voice's handle and pre-kill device-gate's child.
    monkeypatch.setattr(daemon_state, "_voice_proc", None)
    daemon_module["device_gate_proc"]._alive = False  # already exited

    summary = daemon_state.stop_all_daemon_managed()

    stopped = set(summary["stopped"])
    assert "wylde-voice" not in stopped
    assert "wylde-device-gate" not in stopped
    assert "wylde-memgraph" in stopped
    assert "memory_scheduler" in stopped
    assert summary["count"] == 2


def test_shutdown_all_action_calls_both_paths(daemon_module: dict[str, Any]) -> None:
    """The pipe action goes through launcher.shutdown_all AND
    daemon.stop_all_daemon_managed. Response payload lists everything
    stopped across both paths."""
    control = daemon_module["control"]
    response = control.shutdown_all_action()

    assert "stopped" in response
    assert "launcher_stopped" in response
    assert "daemon_managed_stopped" in response
    # Launcher half — empty in this test (we patched get_running -> {}).
    assert response["launcher_stopped"] == []
    # Daemon-managed half — the four components our fixture set up.
    assert set(response["daemon_managed_stopped"]) == {
        "memory_scheduler",
        "wylde-voice",
        "wylde-device-gate",
        "wylde-memgraph",
    }
    # Top-level "stopped" merges both halves.
    assert set(response["stopped"]) >= set(response["daemon_managed_stopped"])
    assert response["count"] == len(response["stopped"])
    assert response["daemon_managed_failed"] == []
    # No daemon stop_event registered in this fixture, so the action
    # reports daemon_will_exit=False — the real daemon registers one
    # at boot, in which case this would be True.
    assert response["daemon_will_exit"] is False


def test_request_daemon_exit_sets_registered_event(
    daemon_module: dict[str, Any],
) -> None:
    """When the daemon has registered its stop event, the action's
    deferred-exit thread flips it within the configured delay."""
    import threading

    daemon_state = daemon_module["daemon_state"]
    event = threading.Event()
    daemon_state.register_stop_event(event)
    try:
        scheduled = daemon_state.request_daemon_exit(after_seconds=0.05)
        assert scheduled is True
        assert event.wait(timeout=2.0), "deferred exit thread didn't set the event"
    finally:
        # Don't leave a stray event registered for downstream tests.
        daemon_state.register_stop_event(threading.Event())


def test_shutdown_all_action_propagates_failures(
    daemon_module: dict[str, Any], monkeypatch: pytest.MonkeyPatch
) -> None:
    """A subprocess whose stop function raises should land in
    'failed', not break the rest of the teardown."""
    daemon_state = daemon_module["daemon_state"]

    def _broken_stop() -> None:
        raise RuntimeError("simulated failure")

    monkeypatch.setattr(daemon_state, "_stop_voice", _broken_stop)

    response = daemon_module["control"].shutdown_all_action()
    failed = response["daemon_managed_failed"]
    assert any(f["name"] == "wylde-voice" for f in failed), (
        f"expected voice in failed list, got {failed!r}"
    )
    # Other stops still ran.
    assert "wylde-memgraph" in response["daemon_managed_stopped"]
    assert "memory_scheduler" in response["daemon_managed_stopped"]
