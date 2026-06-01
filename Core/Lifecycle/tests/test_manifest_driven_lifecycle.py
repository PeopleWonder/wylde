"""Unit tests for the slice-11 manifest-driven launcher/shutdown.

Covers the two pieces the cutover added so launcher + shutdown are
declaratively manifest-driven rather than walking a hardcoded list:

* ``shutdown._shutdown_sequence`` — reverse-launch (reverse-topo) order
  by default, overridable per-service via a manifest ``shutdown_order``.
* ``launcher._wait_for_health`` / ``_health_probe_ok`` — the readiness
  gate the launcher consults between spawns (dormant unless a manifest
  declares a ``health_check``).

All hermetic: the manifest layer is monkeypatched so nothing touches the
real ``data/manifests`` tree or a live process table.
"""

from __future__ import annotations

import sys
from pathlib import Path

import pytest

_HERE = Path(__file__).resolve()
_VAULT_ROOT = _HERE.parents[3]
if str(_VAULT_ROOT) not in sys.path:
    sys.path.insert(0, str(_VAULT_ROOT))


# ── shutdown ordering ──────────────────────────────────────────────────


def _patch_orders(monkeypatch: pytest.MonkeyPatch, orders: dict[str, object]) -> None:
    """Make ``shutdown``'s manifest loader return a synthetic manifest
    (with the given ``shutdown_order``, if any) per service name."""
    from Core.Lifecycle import shutdown as shutdown_mod

    def _fake_load(folder: Path) -> dict | None:
        name = folder.name
        if name not in orders:
            return None
        order = orders[name]
        return {} if order is None else {"shutdown_order": order}

    monkeypatch.setattr(shutdown_mod.manifest_mod, "load_manifest", _fake_load)


def test_shutdown_defaults_to_reverse_launch_order(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """With no manifest declaring a slot, shutdown is the exact reverse
    of the launch (topological) order — dependents stop first."""
    from Core.Lifecycle import shutdown as shutdown_mod

    # No shutdown_order on anyone → all fall to DEFAULT_SHUTDOWN_ORDER.
    _patch_orders(monkeypatch, {"a": None, "b": None, "c": None})
    launch_order = ["a", "b", "c"]  # a launched first (a dep of b dep of c)
    assert shutdown_mod._shutdown_sequence(launch_order) == ["c", "b", "a"]


def test_manifest_shutdown_order_overrides_default(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A declared ``shutdown_order`` wins; lower stops earlier."""
    from Core.Lifecycle import shutdown as shutdown_mod

    # gateway must drain before device_gate even though it launched first.
    _patch_orders(
        monkeypatch,
        {"device_gate": 80, "gateway": 20, "voice": 30},
    )
    launch_order = ["device_gate", "gateway", "voice"]
    assert shutdown_mod._shutdown_sequence(launch_order) == [
        "gateway",
        "voice",
        "device_gate",
    ]


def test_shutdown_order_stable_within_a_slot(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Services sharing a slot keep the reverse-launch relationship
    (the sort is stable over the reversed launch order)."""
    from Core.Lifecycle import shutdown as shutdown_mod

    _patch_orders(monkeypatch, {"x": 50, "y": 50, "z": 10})
    # launch order x, y, z → reversed z, y, x → stable-sort by order:
    # z(10) first, then y, x (both 50, reverse-launch preserved).
    assert shutdown_mod._shutdown_sequence(["x", "y", "z"]) == ["z", "y", "x"]


# ── launcher health gate ───────────────────────────────────────────────


def test_wait_for_health_is_noop_without_probe(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """No manifest ``health_check`` → the gate returns immediately
    (the live bring-up path, unchanged by the cutover)."""
    from Core.Lifecycle import launcher

    monkeypatch.setattr(
        launcher.manifest_mod, "load_manifest", lambda folder: {"health_check": None}
    )
    assert launcher._wait_for_health({"name": "gateway"}, timeout=0.01) is True


def test_wait_for_health_times_out_when_probe_never_passes(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """A declared probe that never passes times out and returns False
    (it logs + continues — it must not abort the whole bring-up)."""
    from Core.Lifecycle import launcher

    monkeypatch.setattr(
        launcher.manifest_mod,
        "load_manifest",
        lambda folder: {"health_check": "pipe:wylde-never"},
    )
    monkeypatch.setattr(launcher, "_health_probe_ok", lambda probe: False)
    assert launcher._wait_for_health({"name": "ghost"}, timeout=0.05) is False


def test_health_probe_pipe_shape(monkeypatch: pytest.MonkeyPatch) -> None:
    """``pipe:`` probes resolve to the canonical ``\\.\\pipe\\wylde-*``
    path and report readiness off its existence."""
    from Core.Lifecycle import launcher

    seen: list[str] = []

    def _fake_exists(path: str) -> bool:
        seen.append(path)
        return path.endswith("wylde-gateway")

    monkeypatch.setattr(launcher.os.path, "exists", _fake_exists)
    assert launcher._health_probe_ok("pipe:wylde-gateway") is True
    assert launcher._health_probe_ok("pipe:gateway") is True  # prefix optional
    assert launcher._health_probe_ok("pipe:wylde-absent") is False
    assert all(p.startswith(r"\\.\pipe\wylde-") for p in seen)


def test_health_probe_unknown_shape_never_blocks() -> None:
    """An unrecognised probe shape is treated as ready — the gate never
    wedges on a manifest typo."""
    from Core.Lifecycle import launcher

    assert launcher._health_probe_ok("something-weird") is True
