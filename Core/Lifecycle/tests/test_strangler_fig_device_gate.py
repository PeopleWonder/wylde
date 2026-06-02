"""Strangler-fig dispatch tests for ``_start_device_gate``.

Split out of ``test_strangler_fig.py`` (which sits just under the
700-line file cap) when the 2026-06-02 device_gate cutover added a
dispatch class: ``_start_device_gate`` flipped its default from
``python`` to ``rust``. The Rust ``wylde-device-gate`` is byte-parity
with the Python verifier and is now the canonical impl; Python stays as
the rollback path via ``WYLDE_WYLDE_DEVICE_GATE_IMPL=python``.

The missing-binary fallback (``rust`` requested, no binary → warn + fall
back to ``python``) is already pinned by
``TestStartDispatchFallback.test_device_gate_falls_back_when_rust_missing``
in ``test_strangler_fig.py``; the two cases here lock the new default +
the explicit-python rollback, mirroring ``TestStartGatewayDispatch``.

Real ``subprocess.Popen`` is patched out everywhere so no actual process
ever spawns from this test module.
"""

from __future__ import annotations

import sys
from pathlib import Path
from typing import Any, Generator

import pytest

_HERE = Path(__file__).resolve()
_VAULT_ROOT = _HERE.parents[3]
if str(_VAULT_ROOT) not in sys.path:
    sys.path.insert(0, str(_VAULT_ROOT))

from Core.Lifecycle import daemon_state  # noqa: E402
from Core.Lifecycle.daemon_state import _services  # noqa: E402
from Core.Lifecycle.daemon_state import _strangler  # noqa: E402


class _FakePopen:
    """Recording stand-in for subprocess.Popen."""

    def __init__(self, *args: Any, **kwargs: Any) -> None:
        self.args = args
        self.kwargs = kwargs
        self.pid = 4242
        self._alive = True

    def poll(self) -> Any:
        return None if self._alive else 0


@pytest.fixture
def isolated_device_gate_handle(
    monkeypatch: pytest.MonkeyPatch,
) -> Generator[None, None, None]:
    """Reset the device_gate handle + spawn records and pin no-spawn off."""
    monkeypatch.setattr(daemon_state, "_device_gate_proc", None)
    monkeypatch.setattr(daemon_state, "_nospawn", False)
    daemon_state._spawn_records.clear()
    yield
    daemon_state._spawn_records.clear()


class TestStartDeviceGateDispatch:
    def test_device_gate_default_is_rust_and_spawns_rust_binary(
        self,
        monkeypatch: pytest.MonkeyPatch,
        tmp_path: Path,
        isolated_device_gate_handle: None,
    ) -> None:
        """Unset env → default ``rust``; a present binary takes the rust
        branch via ``_spawn_rust_service`` and NEVER calls
        ``subprocess.Popen`` (the python ``-m device_gate.run`` fallback)."""
        monkeypatch.delenv("WYLDE_WYLDE_DEVICE_GATE_IMPL", raising=False)
        monkeypatch.setattr(_strangler, "WYLDE_ROOT", tmp_path)
        suffix = ".exe" if sys.platform == "win32" else ""
        debug = tmp_path / "rust" / "target" / "debug"
        debug.mkdir(parents=True)
        rust_bin = debug / f"wylde-device-gate{suffix}"
        rust_bin.write_text("fake", encoding="utf-8")

        called: dict[str, Any] = {}

        def _fake_spawn(*, service: str, rust_bin: Path) -> _FakePopen:
            called["service"] = service
            called["rust_bin"] = rust_bin
            return _FakePopen()

        monkeypatch.setattr(_services, "_spawn_rust_service", _fake_spawn)

        def _boom(*_a: Any, **_k: Any) -> Any:
            raise AssertionError(
                "rust branch must not fall through to python -m device_gate.run"
            )

        monkeypatch.setattr(_services.subprocess, "Popen", _boom)

        _services._start_device_gate()

        assert called["service"] == "wylde-device-gate"
        assert called["rust_bin"] == rust_bin
        rec = daemon_state._spawn_records.get("wylde-device-gate")
        assert rec is not None
        assert rec.impl == "rust"

    def test_device_gate_python_override_spawns_run_module(
        self,
        monkeypatch: pytest.MonkeyPatch,
        tmp_path: Path,
        isolated_device_gate_handle: None,
    ) -> None:
        """``WYLDE_WYLDE_DEVICE_GATE_IMPL=python`` (rollback) → the python
        branch runs ``[sys.executable, '-m', 'device_gate.run']`` via
        ``subprocess.Popen`` and the rust helper is NEVER consulted."""
        monkeypatch.setenv("WYLDE_WYLDE_DEVICE_GATE_IMPL", "python")
        # Even with a rust binary present, python override must win.
        monkeypatch.setattr(_strangler, "WYLDE_ROOT", tmp_path)
        suffix = ".exe" if sys.platform == "win32" else ""
        debug = tmp_path / "rust" / "target" / "debug"
        debug.mkdir(parents=True)
        (debug / f"wylde-device-gate{suffix}").write_text("fake", encoding="utf-8")

        def _no_rust(*_a: Any, **_k: Any) -> Any:
            raise AssertionError(
                "python override must not call the rust spawn helper"
            )

        monkeypatch.setattr(_services, "_spawn_rust_service", _no_rust)

        captured: dict[str, Any] = {}

        def _capture_popen(cmd: Any, *args: Any, **kwargs: Any) -> _FakePopen:
            captured["cmd"] = cmd
            return _FakePopen()

        monkeypatch.setattr(_services.subprocess, "Popen", _capture_popen)

        _services._start_device_gate()

        assert captured["cmd"] == [sys.executable, "-m", "device_gate.run"]
        rec = daemon_state._spawn_records.get("wylde-device-gate")
        assert rec is not None
        assert rec.impl == "python"
