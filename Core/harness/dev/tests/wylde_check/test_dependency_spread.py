"""Tests for the dependency-spread ratchet (rule 62, #290 dependency isolation).

Each test rebinds WYLDE_ROOT to a tmp tree of synthetic Cargo.toml manifests
and monkeypatches the rule's own copy of the config constants, so the tiers
(contained / baselined / new) are exercised in isolation from the real tree.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

import pytest

from .conftest import _write


def _rule_module() -> Any:
    try:
        from Wylde.Core.harness.dev.wylde_check.rules import _dependency_spread as m
    except ImportError:
        from Core.harness.dev.wylde_check.rules import _dependency_spread as m
    return m


def _crate(root: Path, name: str, *deps: str) -> None:
    """Write a minimal Cargo.toml for `name` with a [dependencies] table."""
    body = f'[package]\nname = "{name}"\n\n[dependencies]\n' + "".join(
        f'{line}\n' for line in deps
    )
    _write(root / "rust" / "crates" / name / "Cargo.toml", body)


def _configure(
    monkeypatch: pytest.MonkeyPatch,
    module: Any,
    *,
    contained: dict | None = None,
    baseline: dict | None = None,
    new_max: int = 2,
) -> None:
    monkeypatch.setattr(module, "DEPENDENCY_CONTAINED", contained or {})
    monkeypatch.setattr(module, "DEPENDENCY_SPREAD_BASELINE", baseline or {})
    monkeypatch.setattr(module, "DEPENDENCY_SPREAD_NEW_MAX", new_max)


def test_green_within_baseline_and_contained(
    isolated_tree: Any, monkeypatch: pytest.MonkeyPatch
) -> None:
    _wc, root = isolated_tree
    m = _rule_module()
    _configure(
        monkeypatch,
        m,
        contained={"rand": "wylde-shared"},
        baseline={"serde": 2},
    )
    _crate(root, "wylde-a", 'serde = "1"')
    _crate(root, "wylde-b", 'serde = "1"')  # serde spread == baseline 2 → ok
    _crate(root, "wylde-shared", 'rand = "0.8"')  # contained owner → ok
    assert m.check_dependency_spread_ratchet() == []


def test_baselined_dep_trips_when_spread_grows(
    isolated_tree: Any, monkeypatch: pytest.MonkeyPatch
) -> None:
    _wc, root = isolated_tree
    m = _rule_module()
    _configure(monkeypatch, m, baseline={"serde": 1})
    _crate(root, "wylde-a", 'serde = "1"')
    _crate(root, "wylde-b", 'serde = "1"')  # spread 2 > baseline 1
    findings = m.check_dependency_spread_ratchet()
    assert len(findings) == 1
    assert "serde" in findings[0].message
    assert "baseline 1" in findings[0].message


def test_new_dep_trips_threshold(
    isolated_tree: Any, monkeypatch: pytest.MonkeyPatch
) -> None:
    _wc, root = isolated_tree
    m = _rule_module()
    _configure(monkeypatch, m, new_max=2)  # empty baseline + contained
    for c in ("wylde-a", "wylde-b", "wylde-c"):
        _crate(root, c, 'newdep = "1"')  # spread 3 > new_max 2
    findings = m.check_dependency_spread_ratchet()
    assert len(findings) == 1
    assert "newdep" in findings[0].message
    assert "threshold 2" in findings[0].message


def test_new_dep_at_threshold_is_ok(
    isolated_tree: Any, monkeypatch: pytest.MonkeyPatch
) -> None:
    _wc, root = isolated_tree
    m = _rule_module()
    _configure(monkeypatch, m, new_max=2)
    _crate(root, "wylde-a", 'newdep = "1"')
    _crate(root, "wylde-b", 'newdep = "1"')  # spread 2 == new_max → ok
    assert m.check_dependency_spread_ratchet() == []


def test_contained_dep_bypass_is_flagged(
    isolated_tree: Any, monkeypatch: pytest.MonkeyPatch
) -> None:
    _wc, root = isolated_tree
    m = _rule_module()
    _configure(monkeypatch, m, contained={"rand": "wylde-shared"})
    _crate(root, "wylde-shared", 'rand = "0.8"')  # owner → ok
    _crate(root, "wylde-other", 'rand = "0.8"')  # direct dep bypasses adapter
    findings = m.check_dependency_spread_ratchet()
    assert len(findings) == 1
    assert "wylde-other" in findings[0].message
    assert "contained dependency" in findings[0].message


def test_internal_and_path_deps_do_not_count(
    isolated_tree: Any, monkeypatch: pytest.MonkeyPatch
) -> None:
    _wc, root = isolated_tree
    m = _rule_module()
    _configure(monkeypatch, m, new_max=1)
    # A wylde-* dep and an explicit path dep must NOT count toward spread —
    # neither has an upstream crates.io bump to worry about. With new_max=1,
    # if either counted, `libfoo` would look like it spans 2 crates and trip.
    _crate(root, "wylde-a", 'wylde-shared = { path = "../wylde-shared" }', 'libfoo = "1"')
    _crate(
        root,
        "wylde-b",
        'localdep = { path = "../localdep" }',
        'libfoo = "1"',
    )
    # libfoo spans exactly 2 crates == would trip at new_max 1... so assert it
    # DOES trip for libfoo (real external) but NOT for wylde-shared/localdep.
    findings = m.check_dependency_spread_ratchet()
    msgs = " ".join(f.message for f in findings)
    assert "libfoo" in msgs
    assert "wylde-shared" not in msgs
    assert "localdep" not in msgs
