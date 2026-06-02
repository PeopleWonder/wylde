"""Unit tests for the W3.1-W3.3 strangler-fig dispatch.

Covers:

* :func:`_impl_for` — env-var read, default, and unparseable fallback.
* :func:`_rust_binary_path` — env override, dev-target resolution, and
  the no-match → ``None`` case.
* The ``_start_<service>`` dispatch. For the Rust-only cohort
  (device_gate, vram_broker, gateway — collapsed 2026-06-02 when their
  Python packages were deleted) a missing Rust binary leaves the service
  down with NO Python fallback; for the two-impl services (voice) a
  missing binary still falls back to ``python -m <module>``.

The ``_impl_for`` / ``_rust_binary_path`` / ``_spawn_rust_service``
helpers live in :mod:`daemon_state._strangler` (re-imported by
``_services``); the ``_start_<service>`` pairs live in ``_services``.

Real ``subprocess.Popen`` is patched out everywhere so no actual
process ever spawns from this test module.
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
from Core.Lifecycle.daemon_state import _services_basic  # noqa: E402
from Core.Lifecycle.daemon_state import _strangler  # noqa: E402


# ── _impl_for ─────────────────────────────────────────────────────────


class TestImplFor:
    def test_defaults_python_when_unset(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.delenv("WYLDE_WYLDE_VRAM_BROKER_IMPL", raising=False)
        assert _services._impl_for("wylde-vram-broker") == "python"

    def test_reads_rust(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv("WYLDE_WYLDE_GATEWAY_IMPL", "rust")
        assert _services._impl_for("wylde-gateway") == "rust"

    def test_case_insensitive(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv("WYLDE_WYLDE_DEVICE_GATE_IMPL", "RUST")
        assert _services._impl_for("wylde-device-gate") == "rust"

    def test_invalid_falls_back_with_warning(
        self,
        monkeypatch: pytest.MonkeyPatch,
        caplog: pytest.LogCaptureFixture,
    ) -> None:
        monkeypatch.setenv("WYLDE_WYLDE_GATEWAY_IMPL", "garbage")
        with caplog.at_level("WARNING", logger="wylde.lifecycle"):
            result = _services._impl_for("wylde-gateway")
        assert result == "python"
        assert any("garbage" in r.message for r in caplog.records), (
            f"expected garbage warning, got {[r.message for r in caplog.records]}"
        )

    def test_env_var_name_construction(self, monkeypatch: pytest.MonkeyPatch) -> None:
        """``wylde-vram-broker`` → ``WYLDE_WYLDE_VRAM_BROKER_IMPL``."""
        # If the function reads anything else, the env we DON'T set
        # still defaults to python — so we set the expected name and
        # confirm it's the one observed.
        monkeypatch.setenv("WYLDE_WYLDE_VRAM_BROKER_IMPL", "rust")
        assert _services._impl_for("wylde-vram-broker") == "rust"

    def test_default_rust_when_unset(self, monkeypatch: pytest.MonkeyPatch) -> None:
        """Per-service default flip — Phase 2.E ships VPN with default=rust."""
        monkeypatch.delenv("WYLDE_WYLDE_VPN_IMPL", raising=False)
        assert _services._impl_for("wylde-vpn", default="rust") == "rust"

    def test_default_rust_honours_explicit_python_override(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """The rollback path: even with default=rust, the operator's
        explicit ``python`` override wins."""
        monkeypatch.setenv("WYLDE_WYLDE_VPN_IMPL", "python")
        assert _services._impl_for("wylde-vpn", default="rust") == "python"

    def test_default_rust_invalid_value_falls_back_to_default(
        self,
        monkeypatch: pytest.MonkeyPatch,
        caplog: pytest.LogCaptureFixture,
    ) -> None:
        """Unrecognised value with default=rust → rust + warning."""
        monkeypatch.setenv("WYLDE_WYLDE_VPN_IMPL", "garbage")
        with caplog.at_level("WARNING", logger="wylde.lifecycle"):
            result = _services._impl_for("wylde-vpn", default="rust")
        assert result == "rust"
        assert any("garbage" in r.message for r in caplog.records)


# ── _rust_binary_path ─────────────────────────────────────────────────


@pytest.fixture
def fake_wylde_root(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> Generator[Path, None, None]:
    """Point the resolver at a temp ``rust/`` tree instead of the real one."""
    monkeypatch.setattr(_strangler, "WYLDE_ROOT", tmp_path)
    yield tmp_path


class TestRustBinaryPath:
    def _bin_name(self, stripped: str) -> str:
        return (
            f"wylde-{stripped}.exe" if sys.platform == "win32" else f"wylde-{stripped}"
        )

    def test_returns_none_on_no_match(
        self, fake_wylde_root: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.delenv("WYLDE_WYLDE_GATEWAY_BIN", raising=False)
        assert _services._rust_binary_path("wylde-gateway") is None

    def test_resolves_dev_release_target(
        self, fake_wylde_root: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.delenv("WYLDE_WYLDE_VRAM_BROKER_BIN", raising=False)
        target = fake_wylde_root / "rust" / "target" / "release"
        target.mkdir(parents=True)
        binary = target / self._bin_name("vram-broker")
        binary.write_text("not a real binary", encoding="utf-8")

        resolved = _services._rust_binary_path("wylde-vram-broker")
        assert resolved == binary

    def test_resolves_dev_debug_target(
        self, fake_wylde_root: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.delenv("WYLDE_WYLDE_GATEWAY_BIN", raising=False)
        target = fake_wylde_root / "rust" / "target" / "debug"
        target.mkdir(parents=True)
        binary = target / self._bin_name("gateway")
        binary.write_text("not a real binary", encoding="utf-8")

        resolved = _services._rust_binary_path("wylde-gateway")
        assert resolved == binary

    def test_bin_dir_preferred_over_target(
        self, fake_wylde_root: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Bundled install path beats both cargo targets."""
        monkeypatch.delenv("WYLDE_WYLDE_DEVICE_GATE_BIN", raising=False)
        installed = fake_wylde_root / "rust" / "bin"
        installed.mkdir(parents=True)
        bundled = installed / self._bin_name("device-gate")
        bundled.write_text("install", encoding="utf-8")

        debug = fake_wylde_root / "rust" / "target" / "debug"
        debug.mkdir(parents=True)
        (debug / self._bin_name("device-gate")).write_text("dev", encoding="utf-8")

        resolved = _services._rust_binary_path("wylde-device-gate")
        assert resolved == bundled

    def test_env_override(
        self,
        fake_wylde_root: Path,
        tmp_path: Path,
        monkeypatch: pytest.MonkeyPatch,
    ) -> None:
        override = tmp_path / "custom-broker.exe"
        override.write_text("custom", encoding="utf-8")
        monkeypatch.setenv("WYLDE_WYLDE_VRAM_BROKER_BIN", str(override))

        # Put a sibling debug binary so we can prove the override wins.
        debug = fake_wylde_root / "rust" / "target" / "debug"
        debug.mkdir(parents=True)
        (debug / self._bin_name("vram-broker")).write_text("dev", encoding="utf-8")

        resolved = _services._rust_binary_path("wylde-vram-broker")
        assert resolved == override

    def test_env_override_missing_file_returns_none(
        self, fake_wylde_root: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """An override pointing at a non-existent path returns None — the
        operator named a binary that isn't there; the dispatcher falls
        back to Python rather than silently using a dev-target binary
        the operator never asked for."""
        monkeypatch.setenv("WYLDE_WYLDE_GATEWAY_BIN", str(fake_wylde_root / "nope.exe"))
        # Put a debug binary that would otherwise resolve.
        debug = fake_wylde_root / "rust" / "target" / "debug"
        debug.mkdir(parents=True)
        (debug / self._bin_name("gateway")).write_text("dev", encoding="utf-8")

        assert _services._rust_binary_path("wylde-gateway") is None


# ── _start_<service> dispatch + fallback ──────────────────────────────


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
def isolated_handles(monkeypatch: pytest.MonkeyPatch) -> Generator[None, None, None]:
    """Reset the daemon's process handles + spawn records for each test."""
    monkeypatch.setattr(daemon_state, "_vram_broker_proc", None)
    monkeypatch.setattr(daemon_state, "_device_gate_proc", None)
    monkeypatch.setattr(daemon_state, "_gateway_proc", None)
    daemon_state._spawn_records.clear()
    yield
    daemon_state._spawn_records.clear()


class TestStartDispatchNoFallback:
    """Rust-only cohort (device_gate, vram_broker, gateway): the Python
    packages were deleted 2026-06-02, so a missing Rust binary leaves the
    service DOWN — there is no Python fallback and ``subprocess.Popen`` is
    never reached."""

    def test_device_gate_no_spawn_when_rust_missing(
        self,
        monkeypatch: pytest.MonkeyPatch,
        tmp_path: Path,
        caplog: pytest.LogCaptureFixture,
        isolated_handles: None,
    ) -> None:
        monkeypatch.setattr(_strangler, "WYLDE_ROOT", tmp_path)
        monkeypatch.delenv("WYLDE_WYLDE_DEVICE_GATE_BIN", raising=False)

        def _boom(*_a: Any, **_k: Any) -> Any:
            raise AssertionError("rust-only device_gate must not spawn python")

        monkeypatch.setattr(_services.subprocess, "Popen", _boom)

        with caplog.at_level("WARNING", logger="wylde.lifecycle"):
            _services._start_device_gate()

        assert any(
            "no rust binary" in r.message and "device_gate" in r.message
            for r in caplog.records
        )
        assert daemon_state._device_gate_proc is None
        assert daemon_state._spawn_records.get("wylde-device-gate") is None

    def test_gateway_no_spawn_when_rust_missing(
        self,
        monkeypatch: pytest.MonkeyPatch,
        tmp_path: Path,
        caplog: pytest.LogCaptureFixture,
        isolated_handles: None,
    ) -> None:
        monkeypatch.setattr(_strangler, "WYLDE_ROOT", tmp_path)
        monkeypatch.delenv("WYLDE_WYLDE_GATEWAY_BIN", raising=False)

        def _boom(*_a: Any, **_k: Any) -> Any:
            raise AssertionError("rust-only gateway must not spawn python")

        monkeypatch.setattr(_services.subprocess, "Popen", _boom)

        with caplog.at_level("WARNING", logger="wylde.lifecycle"):
            _services._start_gateway()

        assert any(
            "no rust binary" in r.message and "gateway" in r.message
            for r in caplog.records
        )
        assert daemon_state._gateway_proc is None
        assert daemon_state._spawn_records.get("wylde-gateway") is None

    def test_vram_broker_no_spawn_when_rust_missing(
        self,
        monkeypatch: pytest.MonkeyPatch,
        tmp_path: Path,
        caplog: pytest.LogCaptureFixture,
        isolated_handles: None,
    ) -> None:
        monkeypatch.setattr(_strangler, "WYLDE_ROOT", tmp_path)
        monkeypatch.delenv("WYLDE_WYLDE_VRAM_BROKER_BIN", raising=False)

        def _boom(*_a: Any, **_k: Any) -> Any:
            raise AssertionError("rust-only vram_broker must not spawn python")

        monkeypatch.setattr(_services.subprocess, "Popen", _boom)

        with caplog.at_level("WARNING", logger="wylde.lifecycle"):
            _services._start_vram_broker()

        assert any(
            "no rust binary" in r.message and "vram_broker" in r.message
            for r in caplog.records
        )
        assert daemon_state._vram_broker_proc is None
        assert daemon_state._spawn_records.get("vram-broker") is None

    def test_rust_branch_records_impl_rust(
        self,
        monkeypatch: pytest.MonkeyPatch,
        tmp_path: Path,
        isolated_handles: None,
    ) -> None:
        """When the binary IS present, the rust branch runs and records
        ``impl=rust``."""
        monkeypatch.setattr(_strangler, "WYLDE_ROOT", tmp_path)
        monkeypatch.setenv("WYLDE_WYLDE_GATEWAY_IMPL", "rust")
        suffix = ".exe" if sys.platform == "win32" else ""
        debug = tmp_path / "rust" / "target" / "debug"
        debug.mkdir(parents=True)
        (debug / f"wylde-gateway{suffix}").write_text("fake", encoding="utf-8")
        monkeypatch.setattr(_services.subprocess, "Popen", _FakePopen)

        _services._start_gateway()

        rec = daemon_state._spawn_records.get("wylde-gateway")
        assert rec is not None
        assert rec.impl == "rust"


# ── _start_gateway impl dispatch (Rust-only since 2026-06-02) ─────────
#
# The Rust ``wylde-gateway`` server is a superset of the Python routes
# and is the canonical ingress/egress. The Python ``Gateway`` package was
# deleted 2026-06-02, collapsing this to Rust-only — the python-override
# and missing-binary-fallback cases no longer exist (the no-spawn-when-
# binary-missing contract is pinned in ``TestStartDispatchNoFallback``).
# What remains is the happy path: default ``rust`` spawns the binary and
# never touches ``subprocess.Popen``.


@pytest.fixture
def isolated_gateway_handle(
    monkeypatch: pytest.MonkeyPatch,
) -> Generator[None, None, None]:
    """Reset the gateway handle + spawn records and pin no-spawn off."""
    monkeypatch.setattr(daemon_state, "_gateway_proc", None)
    monkeypatch.setattr(daemon_state, "_nospawn", False)
    daemon_state._spawn_records.clear()
    yield
    daemon_state._spawn_records.clear()


class TestStartGatewayDispatch:
    def test_gateway_default_is_rust_and_spawns_rust_binary(
        self,
        monkeypatch: pytest.MonkeyPatch,
        tmp_path: Path,
        isolated_gateway_handle: None,
    ) -> None:
        """Unset env → default ``rust``; a present binary takes the rust
        branch via ``_spawn_rust_service`` and NEVER calls
        ``subprocess.Popen`` (the python ``-m Gateway.run`` fallback)."""
        monkeypatch.delenv("WYLDE_WYLDE_GATEWAY_IMPL", raising=False)
        monkeypatch.setattr(_strangler, "WYLDE_ROOT", tmp_path)
        suffix = ".exe" if sys.platform == "win32" else ""
        debug = tmp_path / "rust" / "target" / "debug"
        debug.mkdir(parents=True)
        rust_bin = debug / f"wylde-gateway{suffix}"
        rust_bin.write_text("fake", encoding="utf-8")

        called: dict[str, Any] = {}

        def _fake_spawn(*, service: str, rust_bin: Path) -> _FakePopen:
            called["service"] = service
            called["rust_bin"] = rust_bin
            return _FakePopen()

        monkeypatch.setattr(_services, "_spawn_rust_service", _fake_spawn)

        def _boom(*_a: Any, **_k: Any) -> Any:
            raise AssertionError(
                "rust branch must not fall through to python -m Gateway.run"
            )

        monkeypatch.setattr(_services.subprocess, "Popen", _boom)

        _services._start_gateway()

        assert called["service"] == "wylde-gateway"
        assert called["rust_bin"] == rust_bin
        rec = daemon_state._spawn_records.get("wylde-gateway")
        assert rec is not None
        assert rec.impl == "rust"


# ── _start_vram_broker impl dispatch (Rust-only since 2026-06-02) ──────
# Only the Rust broker has the Phase-0.5 estimator + DRAM spillover; the
# Python ``Core/resource_monitor`` package was deleted 2026-06-02. What
# remains is the happy path — default ``rust`` spawns the binary. The
# no-spawn-when-binary-missing contract is in TestStartDispatchNoFallback.


@pytest.fixture
def isolated_vram_broker_handle(
    monkeypatch: pytest.MonkeyPatch,
) -> Generator[None, None, None]:
    """Reset the broker handle + spawn records and pin no-spawn off."""
    monkeypatch.setattr(daemon_state, "_vram_broker_proc", None)
    monkeypatch.setattr(daemon_state, "_nospawn", False)
    daemon_state._spawn_records.clear()
    yield
    daemon_state._spawn_records.clear()


class TestStartVramBrokerDispatch:
    def test_vram_broker_default_is_rust_and_spawns_rust_binary(
        self,
        monkeypatch: pytest.MonkeyPatch,
        tmp_path: Path,
        isolated_vram_broker_handle: None,
    ) -> None:
        """Unset env → default ``rust``; a present binary takes the rust
        branch and NEVER calls the python fallback."""
        monkeypatch.delenv("WYLDE_WYLDE_VRAM_BROKER_IMPL", raising=False)
        monkeypatch.setattr(_strangler, "WYLDE_ROOT", tmp_path)
        suffix = ".exe" if sys.platform == "win32" else ""
        debug = tmp_path / "rust" / "target" / "debug"
        debug.mkdir(parents=True)
        rust_bin = debug / f"wylde-vram-broker{suffix}"
        rust_bin.write_text("fake", encoding="utf-8")

        called: dict[str, Any] = {}

        def _fake_spawn(*, service: str, rust_bin: Path) -> _FakePopen:
            called["service"] = service
            called["rust_bin"] = rust_bin
            return _FakePopen()

        monkeypatch.setattr(_services, "_spawn_rust_service", _fake_spawn)

        def _boom(*_a: Any, **_k: Any) -> Any:
            raise AssertionError(
                "rust branch must not fall through to the python broker"
            )

        monkeypatch.setattr(_services.subprocess, "Popen", _boom)

        _services._start_vram_broker()

        assert called["service"] == "wylde-vram-broker"
        assert called["rust_bin"] == rust_bin
        rec = daemon_state._spawn_records.get("vram-broker")
        assert rec is not None
        assert rec.impl == "rust"


# ── _start_voice impl dispatch (Phase 11.E — Python daemon parity) ─────
#
# The live lifecycle daemon is the PYTHON one; until 2026-05-30 its
# ``_start_voice`` hard-coded ``python -m Voice.run`` and never honoured
# the Phase-11.E ``WYLDE_WYLDE_VOICE_IMPL`` flip, so the rust voice
# binary was never launched despite the default being ``rust``. These
# tests pin the two-impl dispatch for BOTH selector values.


@pytest.fixture
def isolated_voice_handle(
    monkeypatch: pytest.MonkeyPatch,
) -> Generator[None, None, None]:
    """Reset the voice handle + spawn records and pin no-spawn off."""
    monkeypatch.setattr(daemon_state, "_voice_proc", None)
    monkeypatch.setattr(daemon_state, "_nospawn", False)
    daemon_state._spawn_records.clear()
    yield
    daemon_state._spawn_records.clear()


class TestStartVoiceDispatch:
    def test_voice_default_is_rust_and_spawns_rust_binary(
        self,
        monkeypatch: pytest.MonkeyPatch,
        tmp_path: Path,
        isolated_voice_handle: None,
    ) -> None:
        """Unset env → default ``rust``; a present binary takes the rust
        branch via ``_spawn_rust_service`` and NEVER calls
        ``subprocess.Popen`` (the python fallback)."""
        monkeypatch.delenv("WYLDE_WYLDE_VOICE_IMPL", raising=False)
        # Point the resolver at a temp tree with a real (placeholder) binary.
        monkeypatch.setattr(_strangler, "WYLDE_ROOT", tmp_path)
        suffix = ".exe" if sys.platform == "win32" else ""
        debug = tmp_path / "rust" / "target" / "debug"
        debug.mkdir(parents=True)
        rust_bin = debug / f"wylde-voice{suffix}"
        rust_bin.write_text("fake", encoding="utf-8")

        called: dict[str, Any] = {}

        def _fake_spawn(*, service: str, rust_bin: Path) -> _FakePopen:
            called["service"] = service
            called["rust_bin"] = rust_bin
            return _FakePopen()

        monkeypatch.setattr(_services_basic, "_spawn_rust_service", _fake_spawn)

        def _boom(*_a: Any, **_k: Any) -> Any:
            raise AssertionError(
                "rust branch must not fall through to python -m Voice.run"
            )

        monkeypatch.setattr(_services_basic.subprocess, "Popen", _boom)

        _services._start_voice()

        # Rust spawn helper was called with the resolved binary.
        assert called["service"] == "wylde-voice"
        assert called["rust_bin"] == rust_bin
        rec = daemon_state._spawn_records.get("wylde-voice")
        assert rec is not None
        assert rec.impl == "rust"

    def test_voice_python_override_spawns_voice_run_module(
        self,
        monkeypatch: pytest.MonkeyPatch,
        tmp_path: Path,
        isolated_voice_handle: None,
    ) -> None:
        """``WYLDE_WYLDE_VOICE_IMPL=python`` (rollback) → the python
        branch runs ``[sys.executable, '-m', 'Voice.run']`` via
        ``subprocess.Popen`` and the rust helper is NEVER consulted."""
        monkeypatch.setenv("WYLDE_WYLDE_VOICE_IMPL", "python")
        # Even with a rust binary present, python override must win.
        monkeypatch.setattr(_strangler, "WYLDE_ROOT", tmp_path)
        suffix = ".exe" if sys.platform == "win32" else ""
        debug = tmp_path / "rust" / "target" / "debug"
        debug.mkdir(parents=True)
        (debug / f"wylde-voice{suffix}").write_text("fake", encoding="utf-8")

        def _no_rust(*_a: Any, **_k: Any) -> Any:
            raise AssertionError(
                "python override must not call the rust spawn helper"
            )

        monkeypatch.setattr(_services_basic, "_spawn_rust_service", _no_rust)

        captured: dict[str, Any] = {}

        def _capture_popen(cmd: Any, *args: Any, **kwargs: Any) -> _FakePopen:
            captured["cmd"] = cmd
            return _FakePopen()

        monkeypatch.setattr(_services_basic.subprocess, "Popen", _capture_popen)

        _services._start_voice()

        assert captured["cmd"] == [sys.executable, "-m", "Voice.run"]
        rec = daemon_state._spawn_records.get("wylde-voice")
        assert rec is not None
        assert rec.impl == "python"

    def test_voice_rust_default_missing_binary_falls_back_to_python(
        self,
        monkeypatch: pytest.MonkeyPatch,
        tmp_path: Path,
        caplog: pytest.LogCaptureFixture,
        isolated_voice_handle: None,
    ) -> None:
        """Default ``rust`` but no binary on disk → warn + fall back to
        ``python -m Voice.run``."""
        monkeypatch.delenv("WYLDE_WYLDE_VOICE_IMPL", raising=False)
        monkeypatch.setattr(_strangler, "WYLDE_ROOT", tmp_path)
        monkeypatch.delenv("WYLDE_WYLDE_VOICE_BIN", raising=False)

        captured: dict[str, Any] = {}

        def _capture_popen(cmd: Any, *args: Any, **kwargs: Any) -> _FakePopen:
            captured["cmd"] = cmd
            return _FakePopen()

        monkeypatch.setattr(_services_basic.subprocess, "Popen", _capture_popen)

        with caplog.at_level("WARNING", logger="wylde.lifecycle"):
            _services._start_voice()

        assert any(
            "no binary found" in r.message and "voice" in r.message
            for r in caplog.records
        ), f"expected fallback warning, got {[r.message for r in caplog.records]}"
        assert captured["cmd"] == [sys.executable, "-m", "Voice.run"]
        rec = daemon_state._spawn_records.get("wylde-voice")
        assert rec is not None
        assert rec.impl == "python"
