"""Unit tests for the Lifecycle daemon's no-spawn (parity) mode.

No-spawn mode is what makes ``rust/tests/parity/tests/lifecycle.rs``
possible: it brings the daemon's control surface up WITHOUT forking the
tier=core child set. These tests pin the pieces the parity suite leans
on:

* the ``--no-spawn`` / ``WYLDE_LIFECYCLE_NOSPAWN`` flag detection,
* the ``_NoSpawnProc`` Popen stand-in,
* the ``_start_<service>`` short-circuit (records a would-have-spawned
  handle, forks nothing),
* the ``lifecycle.*`` control actions the parity suite gates, and
* the ``WYLDE_LIFECYCLE_PIPE_NAME`` isolated-pipe override.

Nothing here spawns a real process — the no-spawn-records test even
swaps ``subprocess.Popen`` for a fail-fast to prove it.
"""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path
from typing import Any

import pytest

_HERE = Path(__file__).resolve()
_VAULT_ROOT = _HERE.parents[3]
if str(_VAULT_ROOT) not in sys.path:
    sys.path.insert(0, str(_VAULT_ROOT))


# The six daemon-managed subprocess slots no-spawn mode short-circuits.
_PROC_SLOTS = (
    "_memgraph_proc",
    "_voice_proc",
    "_device_gate_proc",
    "_vram_broker_proc",
    "_extension_bridge_proc",
    "_gateway_proc",
)


@pytest.fixture
def lifecycle(monkeypatch: pytest.MonkeyPatch) -> dict[str, Any]:
    """``daemon_state`` + ``control`` with the six no-spawn proc slots
    reset to ``None`` and no-spawn pinned off, so each test starts from a
    clean would-have-spawned set. Tests opt into no-spawn by patching
    ``_nospawn`` True."""
    from Core.Lifecycle import control, daemon_state

    for slot in _PROC_SLOTS:
        monkeypatch.setattr(daemon_state, slot, None)
    monkeypatch.setattr(daemon_state, "_nospawn", False)
    return {"daemon_state": daemon_state, "control": control}


# ── Flag detection ─────────────────────────────────────────────────────


def test_detect_nospawn_env(monkeypatch: pytest.MonkeyPatch) -> None:
    from Core.Lifecycle import daemon_state

    monkeypatch.delenv("WYLDE_LIFECYCLE_NOSPAWN", raising=False)
    assert daemon_state.detect_nospawn(["prog"]) is False

    for truthy in ("1", "true", "yes", "on", "ON"):
        monkeypatch.setenv("WYLDE_LIFECYCLE_NOSPAWN", truthy)
        assert daemon_state.detect_nospawn(["prog"]) is True

    monkeypatch.setenv("WYLDE_LIFECYCLE_NOSPAWN", "0")
    assert daemon_state.detect_nospawn(["prog"]) is False


def test_detect_nospawn_cli(monkeypatch: pytest.MonkeyPatch) -> None:
    from Core.Lifecycle import daemon_state

    monkeypatch.delenv("WYLDE_LIFECYCLE_NOSPAWN", raising=False)
    assert daemon_state.detect_nospawn(["prog", "--no-spawn"]) is True
    assert daemon_state.detect_nospawn(["prog"]) is False


# ── _NoSpawnProc stand-in ──────────────────────────────────────────────


def test_nospawn_proc_quacks_like_popen(lifecycle: dict[str, Any]) -> None:
    daemon_state = lifecycle["daemon_state"]
    proc = daemon_state._NoSpawnProc("wylde-voice")

    # Synthetic pid 0 — never a real Windows process id.
    assert proc.pid == 0
    assert proc.service == "wylde-voice"
    # Alive until a stop signal arrives.
    assert proc.poll() is None
    proc.terminate()
    assert proc.poll() == 0
    # wait() is a no-op returning the synthetic exit code.
    assert proc.wait(timeout=1) == 0


# ── _start_<service> short-circuit ─────────────────────────────────────


def test_start_service_under_nospawn_forks_nothing(
    lifecycle: dict[str, Any], monkeypatch: pytest.MonkeyPatch
) -> None:
    """Under no-spawn, _start_voice records a _NoSpawnProc and never
    reaches subprocess.Popen — proven by swapping Popen for a fail-fast."""
    daemon_state = lifecycle["daemon_state"]
    monkeypatch.setattr(daemon_state, "_nospawn", True)

    def _boom(*_a: Any, **_k: Any) -> Any:
        raise AssertionError("no-spawn mode must never call subprocess.Popen")

    monkeypatch.setattr(subprocess, "Popen", _boom)

    daemon_state._start_voice()

    assert isinstance(daemon_state._voice_proc, daemon_state._NoSpawnProc)
    assert daemon_state._voice_proc.pid == 0


def test_nospawn_snapshot_reflects_recorded(
    lifecycle: dict[str, Any], monkeypatch: pytest.MonkeyPatch
) -> None:
    daemon_state = lifecycle["daemon_state"]
    monkeypatch.setattr(daemon_state, "_nospawn", True)

    assert daemon_state.nospawn_snapshot() == []
    daemon_state._start_voice()
    daemon_state._start_gateway()
    # Sorted, regardless of start order.
    assert daemon_state.nospawn_snapshot() == ["wylde-gateway", "wylde-voice"]


def test_nospawn_start_known_and_unknown(
    lifecycle: dict[str, Any], monkeypatch: pytest.MonkeyPatch
) -> None:
    daemon_state = lifecycle["daemon_state"]
    monkeypatch.setattr(daemon_state, "_nospawn", True)

    assert daemon_state.nospawn_start("wylde-memgraph") is True
    assert isinstance(daemon_state._memgraph_proc, daemon_state._NoSpawnProc)
    assert daemon_state.nospawn_start("wylde-bogus") is False


def test_nospawn_start_requires_nospawn(lifecycle: dict[str, Any]) -> None:
    """nospawn_start must never run a starter outside no-spawn mode — it
    would fork a real child."""
    daemon_state = lifecycle["daemon_state"]  # _nospawn is False here
    with pytest.raises(RuntimeError):
        daemon_state.nospawn_start("wylde-voice")


# ── lifecycle.* control actions ────────────────────────────────────────


def test_lifecycle_status_action(
    lifecycle: dict[str, Any], monkeypatch: pytest.MonkeyPatch
) -> None:
    daemon_state, control = lifecycle["daemon_state"], lifecycle["control"]
    monkeypatch.setattr(daemon_state, "_nospawn", True)
    daemon_state._start_voice()
    daemon_state._start_memgraph()

    assert control.lifecycle_status_action() == {
        "nospawn": True,
        "service_count": 2,
        "would_have_spawned": ["wylde-memgraph", "wylde-voice"],
    }


def test_lifecycle_list_services_action(
    lifecycle: dict[str, Any], monkeypatch: pytest.MonkeyPatch
) -> None:
    daemon_state, control = lifecycle["daemon_state"], lifecycle["control"]
    monkeypatch.setattr(daemon_state, "_nospawn", True)
    daemon_state._start_gateway()

    assert control.lifecycle_list_services_action() == {
        "services": {"wylde-gateway": "would-have-spawned"},
        "count": 1,
    }


def test_lifecycle_start_service_action(
    lifecycle: dict[str, Any], monkeypatch: pytest.MonkeyPatch
) -> None:
    daemon_state, control = lifecycle["daemon_state"], lifecycle["control"]
    monkeypatch.setattr(daemon_state, "_nospawn", True)

    resp = control.lifecycle_start_service_action({"name": "wylde-voice"})

    assert resp == {
        "name": "wylde-voice",
        "status": "would-have-spawned",
        "would_have_spawned": True,
    }
    # The short-circuit actually recorded the entry.
    assert isinstance(daemon_state._voice_proc, daemon_state._NoSpawnProc)


def test_lifecycle_start_service_requires_nospawn(lifecycle: dict[str, Any]) -> None:
    """Outside no-spawn the action rejects — never a backdoor to a real
    spawn."""
    control = lifecycle["control"]  # _nospawn is False here
    with pytest.raises(control.ControlError) as exc:
        control.lifecycle_start_service_action({"name": "wylde-voice"})
    assert exc.value.code == "nospawn_required"


def test_lifecycle_start_service_rejects_unknown(
    lifecycle: dict[str, Any], monkeypatch: pytest.MonkeyPatch
) -> None:
    daemon_state, control = lifecycle["daemon_state"], lifecycle["control"]
    monkeypatch.setattr(daemon_state, "_nospawn", True)
    with pytest.raises(control.ControlError) as exc:
        control.lifecycle_start_service_action({"name": "wylde-bogus"})
    assert exc.value.code == "unknown_service"


# ── WYLDE_LIFECYCLE_PIPE_NAME isolated-pipe override ───────────────────


def test_resolve_pipe_service_name(monkeypatch: pytest.MonkeyPatch) -> None:
    from Core.Lifecycle.daemon import _resolve_pipe_service_name

    monkeypatch.delenv("WYLDE_LIFECYCLE_PIPE_NAME", raising=False)
    assert _resolve_pipe_service_name() == "wylde-lifecycle"

    monkeypatch.setenv("WYLDE_LIFECYCLE_PIPE_NAME", "wylde-lifecycle-parity-py")
    assert _resolve_pipe_service_name() == "wylde-lifecycle-parity-py"

    # Blank / whitespace-only falls back to the canonical name.
    monkeypatch.setenv("WYLDE_LIFECYCLE_PIPE_NAME", "   ")
    assert _resolve_pipe_service_name() == "wylde-lifecycle"
