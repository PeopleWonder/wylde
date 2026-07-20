"""Tests for architectural rules (dead_service_refs,
service_owns_its_state) — mirrors prod-side wylde_check/rules/_arch.py.

Rules 1 (no_internal_http), 2 (manifest_paths), 5 (import_paths) and 22
(memory_layer_boundaries) were retired 2026-07-20; their tests were
removed with them.
"""

from __future__ import annotations

from typing import Any

from .conftest import _write


# ── Rule 6: dead service references ───────────────────────────────────


def test_dead_service_refs_flags_known_dead_name(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Core" / "harness" / "mod.py",
        "SVC = 'wylde-orchestrator'  # dead reference\n",
    )
    findings = wc.check_dead_service_refs()
    assert len(findings) >= 1
    assert any(f.rule == "dead_service_refs" for f in findings)
    assert any("wylde-orchestrator" in f.message for f in findings)


def test_dead_service_refs_clean(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(root / "Core" / "harness" / "mod.py", "SVC = 'wylde-harness'  # live name\n")
    assert wc.check_dead_service_refs() == []


def test_dead_service_refs_skips_legacy_dirs(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(root / "_legacy" / "foo.py", "x = 'wylde-orchestrator'\n")
    assert wc.check_dead_service_refs() == []


def test_dead_service_refs_fires_on_rust(isolated_tree: Any) -> None:
    """Rule 6 walks .rs files too — Rust crates citing dead names get
    flagged just like Python."""
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "wylde-foo" / "src" / "main.rs",
        'const TARGET: &str = "wylde-orchestrator";\n',
    )
    findings = wc.check_dead_service_refs()
    assert len(findings) == 1
    assert findings[0].rule == "dead_service_refs"
    assert findings[0].file == "rust/crates/wylde-foo/src/main.rs"
    assert "wylde-orchestrator" in findings[0].message


def test_dead_service_refs_honours_rust_marker(isolated_tree: Any) -> None:
    """The inline marker suppresses the Rust line just like the Python
    one (the rule is host-language agnostic for suppression)."""
    wc, root = isolated_tree
    _write(
        root / "rust" / "crates" / "wylde-foo" / "src" / "main.rs",
        'const TARGET: &str = "wylde-orchestrator";  // wylde-check: dead-ref-ok\n',
    )
    assert wc.check_dead_service_refs() == []


# ── Rule 25: service owns its state ─────────────────────────────────


def test_service_owns_its_state_flags_cross_service(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Gateway" / "auth" / "evil.py",
        'PATH = "device_gate/data/approved.json"\n',
    )
    findings = wc.check_service_owns_its_state()
    assert len(findings) == 1
    f = findings[0]
    assert f.rule == "service_owns_its_state"
    assert f.severity == "error"
    assert f.file == "Gateway/auth/evil.py"
    assert "device_gate" in f.message


def test_service_owns_its_state_allows_self_access(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "device_gate" / "store.py",
        'PATH = "device_gate/data/approved.json"\n',
    )
    assert wc.check_service_owns_its_state() == []


def test_service_owns_its_state_exempts_lifecycle_daemon(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Core" / "Lifecycle" / "daemon.py",
        'GATE_PATH = "device_gate/data/approved.json"\n'
        'VOICE_DATA = "Voice/data/state.json"\n',
    )
    # Lifecycle daemon legitimately knows about every service's state.
    assert wc.check_service_owns_its_state() == []
