"""Tests for the shutdown-time live-orphan reaper.

The reaper is the safety net that catches services whose manifest claims
``alive`` with a pid still in the process table but the daemon's
``_<service>_proc`` slot is ``None`` (orphan from a prior crashed daemon
session). Without it, every shutdown is a no-op for those orphans and
they survive every restart.

Two test shapes live here:

* ``test_reap_kills_live_alive_pid`` spawns a real do-nothing subprocess,
  writes a synthetic manifest pointing at *its* pid in a tmp dir, and
  exercises the real ``_force_kill_pid`` path end-to-end. This is the
  only way to prove the real kill code works — stubbing it out only
  proves the orchestration around it.

* The remaining tests use the ``reaper_env`` fixture's stubbed
  ``_pid_alive`` / ``_force_kill_pid`` to exercise the manifest-walking
  decisions (terminal states, state-field-missing, malformed manifests)
  without spawning anything.

Both shapes patch ``_MANIFEST_DIR`` to a tmp dir, AND the
``conftest.py`` autouse guard at this package scope refuses to let any
test in the package read the real ``data/manifests/`` even if a test
forgets to patch — see ``conftest._sandboxed_manifest_dir``.
"""

from __future__ import annotations

import json
import subprocess
import sys
import time
from pathlib import Path
from typing import Any, Generator

import pytest

_HERE = Path(__file__).resolve()
_VAULT_ROOT = _HERE.parents[3]
if str(_VAULT_ROOT) not in sys.path:
    sys.path.insert(0, str(_VAULT_ROOT))


@pytest.fixture
def reaper_env(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    sandboxed_manifest_dir: Path,
) -> Generator[dict[str, Any], None, None]:
    """Point the reaper at a tmp manifest dir and stub the kill path.

    ``sandboxed_manifest_dir`` (autouse conftest fixture) already rebound
    ``daemon_state._MANIFEST_DIR`` to a per-test tmp dir; we reuse it as
    the manifest store so the production path is unreachable by
    construction.
    """
    from Core.Lifecycle import daemon_state
    from Core.Lifecycle.daemon_state import _orphan_sweep

    manifest_dir = sandboxed_manifest_dir

    alive_pids: set[int] = set()
    killed_pids: list[int] = []

    def fake_pid_alive(pid: int) -> bool:
        return pid in alive_pids

    def fake_force_kill_pid(pid: int, *, grace_seconds: float = 5.0) -> bool:
        killed_pids.append(pid)
        alive_pids.discard(pid)
        return True

    monkeypatch.setattr(daemon_state, "_pid_alive", fake_pid_alive)
    monkeypatch.setattr(_orphan_sweep, "_force_kill_pid", fake_force_kill_pid)

    yield {
        "manifest_dir": manifest_dir,
        "alive_pids": alive_pids,
        "killed_pids": killed_pids,
        "daemon_state": daemon_state,
    }


def _write_manifest(
    manifest_dir: Path, name: str, pid: int, state: str = "alive"
) -> None:
    payload = {
        "service": name,
        "version": "1.0.0",
        "status": {
            "pid": pid,
            "state": state,
            "heartbeat": "2026-05-25T01:02:45Z",
        },
    }
    (manifest_dir / f"{name}.json").write_text(
        json.dumps(payload), encoding="utf-8"
    )


def test_reap_kills_live_alive_pid(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    sandboxed_manifest_dir: Path,
) -> None:
    """Real subprocess + real kill path: the reaper terminates a pid it
    finds alive in a manifest.

    The previous incarnation of this test stubbed both ``_pid_alive``
    and ``_force_kill_pid`` and wrote a manifest with ``pid=19652``.
    That pid happened to be the real wylde-gateway's recorded pid; any
    drift in how the patches resolved (e.g. a stale module instance
    under a different import root) would have meant the real
    ``_force_kill_pid`` ran against a real wylde service. We avoid that
    failure mode entirely by spawning a child whose pid we OWN.
    """
    # Spawn a do-nothing child. ``sys.executable`` is the canonical way
    # to find a Python the test runner can launch — under uv it's the
    # in-venv interpreter, under raw ``py -3`` it's whatever 3.x is
    # active. The child sleeps for 120 s; the test's job is to kill it
    # well before that bound, with the test's ``finally`` clause as the
    # belt-and-braces cleanup.
    child = subprocess.Popen(
        [sys.executable, "-c", "import time; time.sleep(120)"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        child_pid = child.pid
        assert child_pid > 0, "spawned child should have a real pid"

        # Synthetic manifest in the sandboxed dir. The reaper walks
        # ``_MANIFEST_DIR``; the autouse conftest already rebound it
        # to ``sandboxed_manifest_dir`` and any pid here belongs to
        # the test (the spawned child).
        _write_manifest(sandboxed_manifest_dir, "wylde-test-victim", pid=child_pid)

        from Core.Lifecycle import daemon_state
        from Core.Lifecycle.daemon_state import reap_manifest_orphans

        # Use the real ``_pid_alive`` and the real ``_force_kill_pid``
        # so this test exercises the kill-orchestration end-to-end. If
        # the conftest guard ever lapsed and the reaper hit the real
        # manifest dir, the helper there would refuse the unowned pid
        # rather than silently kill it.
        assert daemon_state._pid_alive(child_pid), (
            "child should be alive before the reap"
        )

        reaped = reap_manifest_orphans(grace_seconds=2.0)

        assert reaped == [
            {"name": "wylde-test-victim", "pid": child_pid, "killed": True}
        ]
        # Wait briefly for the OS to fully retire the pid (the kill
        # path's post-condition check is best-effort; a short follow-up
        # poll covers any residual race on Windows).
        deadline = time.monotonic() + 3.0
        while time.monotonic() < deadline and daemon_state._pid_alive(child_pid):
            time.sleep(0.05)
        assert not daemon_state._pid_alive(child_pid), (
            "child pid should be gone after the reap"
        )

        # Manifest got flipped to dead-orphan so subsequent reaps skip it.
        on_disk = json.loads(
            (sandboxed_manifest_dir / "wylde-test-victim.json").read_text(
                encoding="utf-8"
            )
        )
        assert on_disk["status"]["state"] == "dead-orphan"
    finally:
        # Belt-and-braces: if anything above raised before the reap,
        # the child is still running. Tear it down so the test never
        # leaks a process.
        if child.poll() is None:
            try:
                child.kill()
                child.wait(timeout=5.0)
            except (OSError, subprocess.TimeoutExpired):
                pass


# Synthetic pids for the stubbed tests below. Chosen to be unmistakably
# fake (well above the realistic Windows pid range observed on this host)
# so a grep for "19652" — the real-world wylde-gateway pid that bit the
# previous incarnation of these tests — turns up nothing here.
_SYNTHETIC_PID_A = 9_000_001
_SYNTHETIC_PID_B = 9_000_002
_SYNTHETIC_PID_C = 9_000_003


def test_reap_skips_dead_pid(reaper_env: dict[str, Any]) -> None:
    """Manifest says alive but pid is gone — the periodic sweep's job, not ours."""
    _write_manifest(
        reaper_env["manifest_dir"], "wylde-test-svc", pid=_SYNTHETIC_PID_A
    )
    # alive_pids is empty — pid is dead.

    from Core.Lifecycle.daemon_state import reap_manifest_orphans

    reaped = reap_manifest_orphans()

    assert reaped == []
    assert reaper_env["killed_pids"] == []


def test_reap_skips_terminal_states(reaper_env: dict[str, Any]) -> None:
    """stopped / dead-orphan / crashed manifests are never reaped, even
    when the pid happens to still be alive — the manifest is the
    authoritative shutdown record."""
    for state in ("stopped", "dead-orphan", "crashed"):
        _write_manifest(
            reaper_env["manifest_dir"],
            f"wylde-test-{state}",
            pid=_SYNTHETIC_PID_B,
            state=state,
        )
        reaper_env["alive_pids"].add(_SYNTHETIC_PID_B)

    from Core.Lifecycle.daemon_state import reap_manifest_orphans

    reaped = reap_manifest_orphans()

    assert reaped == []
    assert reaper_env["killed_pids"] == []


def test_reap_reaps_state_field_missing(reaper_env: dict[str, Any]) -> None:
    """Pre-state-field manifests (no ``status.state``) must still be
    reaped when their pid is alive — older services don't write the
    field but the reaper is the only thing that can stop them."""
    payload = {
        "service": "wylde-test-legacy",
        "version": "0.9.0",
        "status": {"pid": _SYNTHETIC_PID_C, "heartbeat": "2026-05-25T01:02:45Z"},
    }
    (reaper_env["manifest_dir"] / "wylde-test-legacy.json").write_text(
        json.dumps(payload), encoding="utf-8"
    )
    reaper_env["alive_pids"].add(_SYNTHETIC_PID_C)

    from Core.Lifecycle.daemon_state import reap_manifest_orphans

    reaped = reap_manifest_orphans()
    assert len(reaped) == 1
    assert reaped[0]["pid"] == _SYNTHETIC_PID_C
    assert reaper_env["killed_pids"] == [_SYNTHETIC_PID_C]


def test_reap_handles_unreadable_manifest(reaper_env: dict[str, Any]) -> None:
    """A malformed manifest must not stop the reap of the rest."""
    (reaper_env["manifest_dir"] / "garbled.json").write_text(
        "{this is not valid json", encoding="utf-8"
    )
    _write_manifest(
        reaper_env["manifest_dir"], "wylde-test-survivor", pid=_SYNTHETIC_PID_A
    )
    reaper_env["alive_pids"].add(_SYNTHETIC_PID_A)

    from Core.Lifecycle.daemon_state import reap_manifest_orphans

    reaped = reap_manifest_orphans()
    assert reaped == [
        {"name": "wylde-test-survivor", "pid": _SYNTHETIC_PID_A, "killed": True}
    ]


def test_reap_no_manifest_dir(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """When the manifest dir doesn't exist, the reap returns empty
    rather than raising — the daemon may shut down before any service
    ever wrote a manifest.

    Autouse conftest fixture already sandboxed ``_MANIFEST_DIR``; we
    further point it at a *non-existent* path under the same tmp tree.
    """
    from Core.Lifecycle import daemon_state

    nope = tmp_path / "no-manifests"
    monkeypatch.setattr(daemon_state, "_MANIFEST_DIR", nope)

    assert daemon_state.reap_manifest_orphans() == []


def test_stop_all_daemon_managed_invokes_reaper(
    monkeypatch: pytest.MonkeyPatch,
    sandboxed_manifest_dir: Path,
) -> None:
    """The shutdown entry-point must call the reaper as the final step
    — that's the structural fix the wylde_check rule guards."""
    from Core.Lifecycle import daemon_state

    # Wipe all daemon-managed handles so nothing in the tracked-Popen
    # phase runs — the reaper is the only thing that can possibly hit.
    for slot in (
        "_memgraph_proc",
        "_voice_proc",
        "_device_gate_proc",
        "_vram_broker_proc",
        "_extension_bridge_proc",
        "_gateway_proc",
        "_ollama_proc",
        "_vpn_proc",
        "_trainer_proc",
        "_trainer_worker_proc",
        "_harness_proc",
        "_memory_scheduler",
    ):
        monkeypatch.setattr(daemon_state, slot, None)

    _write_manifest(sandboxed_manifest_dir, "wylde-test-victim", pid=_SYNTHETIC_PID_A)

    alive_pids: set[int] = {_SYNTHETIC_PID_A}

    def fake_pid_alive(pid: int) -> bool:
        return pid in alive_pids

    monkeypatch.setattr(daemon_state, "_pid_alive", fake_pid_alive)
    from Core.Lifecycle.daemon_state import _orphan_sweep

    def fake_force_kill_pid(pid: int, *, grace_seconds: float = 5.0) -> bool:
        alive_pids.discard(pid)
        return True

    monkeypatch.setattr(_orphan_sweep, "_force_kill_pid", fake_force_kill_pid)
    # core.json delete is a no-op against tmp_path — neuter it so the
    # test doesn't depend on the manifest writer.
    monkeypatch.setattr(daemon_state, "unregister_core_manifest", lambda: None)
    # stop_orphan_sweep would log if no sweep is registered; harmless,
    # but stubbing keeps the test output clean.
    monkeypatch.setattr(daemon_state, "stop_orphan_sweep", lambda: None)

    summary = daemon_state.stop_all_daemon_managed()

    assert summary["reaped"] == [
        {"name": "wylde-test-victim", "pid": _SYNTHETIC_PID_A, "killed": True}
    ]
    assert _SYNTHETIC_PID_A not in alive_pids
