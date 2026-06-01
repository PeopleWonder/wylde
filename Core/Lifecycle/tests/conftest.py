"""Package-scoped safety net for ``Core/Lifecycle/tests/``.

Background — 2026-05-25 regression
==================================

``test_reap_manifest_orphans.py::test_reap_kills_live_alive_pid`` was
once written with a synthetic-pid that happened to equal the real
``wylde-gateway.exe`` pid recorded in ``data/manifests/wylde-gateway.json``.
The fixture intent was to stub ``_pid_alive`` + ``_force_kill_pid``, but
any drift in how the patches resolved (e.g. a stale module instance
under a different import root, or a sibling test's monkeypatch leaking)
would have let the real ``_force_kill_pid`` run against the real
wylde-gateway. The bug bit during a Phase 11.B verification gate: the
reaper walked the production manifest dir and killed pid 19652.

The surgical fix replaced that test with a real-subprocess version.
This conftest is the structural belt-and-braces around it: every test
in this package is *automatically* sandboxed, and any code path that
tries to force-kill a pid that wasn't registered as test-owned fails
loudly.

What this enforces
==================

1. ``sandboxed_manifest_dir`` (autouse) — for every test in this
   package, ``daemon_state._MANIFEST_DIR`` and
   ``Core.shared.manifest._MANIFEST_DIR`` are pre-bound to a per-test
   tmp directory. A test that *forgets* to patch them no longer
   touches the real ``data/manifests/`` by default — the patch is
   already in place.

2. ``_force_kill_pid`` watchdog — module-level wrap of the kill
   helper. Test-owned pids (those registered via
   :func:`register_test_owned_pid`, including any pid the
   ``test_reap_kills_live_alive_pid`` real-subprocess test spawns)
   are passed through. Any other pid raises ``RuntimeError`` so a
   stray manifest entry pointing at a real wylde process can never
   be silently killed during a test run.

Both mechanisms together give "the Wylde user's stack survives any pytest
invocation in this directory" as a structural invariant — not a
convention any individual test has to remember.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any, Callable, Generator, Set

import pytest


# ── Test-owned pid registry ──────────────────────────────────────────
#
# A central place tests register pids they themselves spawned. The
# ``_force_kill_pid`` watchdog (see below) refuses any pid not in this
# set, so a stray manifest entry pointing at a real wylde service can
# never end up force-killed during a test run.

_test_owned_pids: Set[int] = set()


def register_test_owned_pid(pid: int) -> None:
    """Mark ``pid`` as belonging to this test — safe for the reaper to kill."""
    _test_owned_pids.add(int(pid))


def _release_test_owned_pid(pid: int) -> None:
    _test_owned_pids.discard(int(pid))


# ── Autouse sandbox ─────────────────────────────────────────────────


@pytest.fixture(autouse=True)
def sandboxed_manifest_dir(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> Generator[Path, None, None]:
    """Rebind ``_MANIFEST_DIR`` to a per-test tmp dir for every test.

    Two modules carry an independent ``_MANIFEST_DIR`` constant
    (``Core.Lifecycle.daemon_state`` and ``Core.shared.manifest``); in
    production they resolve to the same path, but under test both have
    to be rebound or a write through ``mark_orphan_dead`` lands in the
    wrong directory.

    Made autouse so a test that forgets to patch ``_MANIFEST_DIR``
    *cannot* accidentally read the real ``data/manifests/``. Individual
    tests are free to monkeypatch the constant further (e.g. to a
    non-existent path for the no-manifest-dir test) — the autouse
    binding only ensures the *default* is sandboxed.
    """
    from Core.Lifecycle import daemon_state
    from Core.shared import manifest as _service_manifest

    manifest_dir = tmp_path / "manifests"
    manifest_dir.mkdir()
    monkeypatch.setattr(daemon_state, "_MANIFEST_DIR", manifest_dir)
    monkeypatch.setattr(_service_manifest, "_MANIFEST_DIR", manifest_dir)
    yield manifest_dir


# ── Watchdog wrap of _force_kill_pid ─────────────────────────────────


@pytest.fixture(autouse=True)
def _force_kill_pid_watchdog(
    monkeypatch: pytest.MonkeyPatch,
) -> Generator[None, None, None]:
    """Refuse to force-kill a pid the test didn't explicitly register.

    Wraps :func:`Core.Lifecycle.daemon_state._orphan_sweep._force_kill_pid`
    so any pid not in :data:`_test_owned_pids` raises ``RuntimeError``
    immediately instead of running ``psutil.terminate`` / ``taskkill``.

    Tests that intentionally exercise the real kill path call
    :func:`register_test_owned_pid` with the pid they spawned; every
    other pid (a stray manifest pointing at a real wylde service, for
    instance) trips the guard with a message that names this file as
    the place to look.

    The guard runs alongside the autouse manifest sandbox, so even a
    test that forgets to call ``register_test_owned_pid`` AND forgets
    to monkeypatch the manifest dir is still safe: the sandbox keeps
    the reaper from seeing real pids, and if anything bypasses the
    sandbox the watchdog catches it.
    """
    from Core.Lifecycle.daemon_state import _orphan_sweep

    real_force_kill_pid: Callable[..., bool] = _orphan_sweep._force_kill_pid

    def _guarded_force_kill_pid(pid: int, **kwargs: Any) -> bool:
        if int(pid) not in _test_owned_pids:
            raise RuntimeError(
                f"_force_kill_pid({pid}) refused by Core/Lifecycle/tests/conftest.py: "
                f"pid is not in the test-owned registry. If your test intentionally "
                f"spawns a subprocess to verify the kill path, call "
                f"`from Core.Lifecycle.tests.conftest import register_test_owned_pid` "
                f"and register the pid before invoking the reaper. If you didn't mean "
                f"to call the real kill path, monkeypatch _force_kill_pid in your "
                f"fixture (see reaper_env in test_reap_manifest_orphans.py for the "
                f"pattern)."
            )
        return real_force_kill_pid(pid, **kwargs)

    monkeypatch.setattr(_orphan_sweep, "_force_kill_pid", _guarded_force_kill_pid)
    yield


# ── Auto-register pids spawned by tests via subprocess.Popen ─────────


@pytest.fixture(autouse=True)
def _auto_register_popen_pids(
    monkeypatch: pytest.MonkeyPatch,
) -> Generator[None, None, None]:
    """Register every subprocess a test spawns as test-owned.

    The watchdog above refuses unowned pids; tests that spawn a real
    child for the reaper to kill would otherwise have to remember to
    call :func:`register_test_owned_pid` explicitly. This fixture
    wraps ``subprocess.Popen`` for the duration of the test so every
    spawn automatically lands in the test-owned set.

    Wrapping is done by patching the ``__init__`` of ``subprocess.Popen``
    so callers that pass through positional or keyword args (the
    standard ``Popen([cmd])`` shape) all flow through one chokepoint.
    The pid is collected post-init.
    """
    import subprocess

    real_init = subprocess.Popen.__init__
    spawned: Set[int] = set()

    def _tracking_init(self: subprocess.Popen, *args: Any, **kwargs: Any) -> None:
        real_init(self, *args, **kwargs)
        try:
            pid = int(self.pid)
        except (AttributeError, TypeError, ValueError):
            return
        if pid > 0:
            register_test_owned_pid(pid)
            spawned.add(pid)

    monkeypatch.setattr(subprocess.Popen, "__init__", _tracking_init)
    try:
        yield
    finally:
        for pid in spawned:
            _release_test_owned_pid(pid)
