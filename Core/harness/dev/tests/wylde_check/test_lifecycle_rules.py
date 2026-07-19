"""Tests for the boot/shutdown/service-manifest rules (44-47),
mirrors prod-side wylde_check/rules/_lifecycle.py. Added at the slice-11
cutover; rules 44/45 repointed at the live Rust single source of truth
(the ``DAEMON_MANAGED`` table) for issue #101 — the old rules targeted the
deleted ``Core/Lifecycle/launcher.py`` / ``shutdown.py`` and passed green
over the missing files (a dead gate).
"""

from __future__ import annotations

import json
from typing import Any

from .conftest import _write

_DAEMON_MANAGED = "rust/crates/wylde-lifecycle/src/daemon_managed.rs"
_BOOT = "rust/crates/wylde-lifecycle/src/daemon.rs"
_SHUTDOWN = "rust/crates/wylde-lifecycle/src/state/mod.rs"
_GPUI_SHUTDOWN = "Core/GUI/Shell/src/shutdown.rs"


def _write_single_source(root: Any) -> None:
    """Write a minimal, structurally-clean single source: the
    ``DAEMON_MANAGED`` table plus boot + shutdown derived from it."""
    _write(root / _DAEMON_MANAGED, "pub const DAEMON_MANAGED: &[DaemonService] = &[];\n")
    _write(root / _BOOT, "for svc in crate::daemon_managed::boot_sequence() {}\n")
    _write(
        root / _SHUTDOWN,
        "for svc in crate::daemon_managed::shutdown_sequence() {}\n",
    )


# ── Rule 44: boot is derived from the single DAEMON_MANAGED table ──────


def test_boot_clean_when_table_driven(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write_single_source(root)
    assert wc.check_launcher_enumerates_services_from_manifests() == []


def test_boot_flags_missing_daemon_managed_table(isolated_tree: Any) -> None:
    """The single-source file is gone (or never declared the table): the
    rule must FIRE, not silently pass — the exact rot issue #101 fixed."""
    wc, root = isolated_tree
    _write(root / _BOOT, "for svc in crate::daemon_managed::boot_sequence() {}\n")
    # No daemon_managed.rs at all.
    findings = wc.check_launcher_enumerates_services_from_manifests()
    assert any(
        f.rule == "launcher_enumerates_services_from_manifests"
        and "DAEMON_MANAGED table is missing" in f.message
        for f in findings
    )


def test_boot_flags_boot_not_derived_from_table(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(root / _DAEMON_MANAGED, "pub const DAEMON_MANAGED: &[DaemonService] = &[];\n")
    # daemon.rs that spawns by hand instead of iterating boot_sequence().
    _write(root / _BOOT, "services::start_gateway().await;\n")
    findings = wc.check_launcher_enumerates_services_from_manifests()
    assert any("boot is no longer derived" in f.message for f in findings)


def test_boot_flags_rust_const_services_array(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write_single_source(root)
    _write(
        root / "rust/crates/wylde-lifecycle/src/roster.rs",
        'const SERVICES: [&str; 2] = ["wylde-gateway", "wylde-voice"];\n',
    )
    findings = wc.check_launcher_enumerates_services_from_manifests()
    assert any("hardcoded service roster in the Rust boot path" in f.message for f in findings)


# ── Rule 45: shutdown is derived from the same DAEMON_MANAGED table ────


def test_shutdown_clean_when_table_driven(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write_single_source(root)
    _write(root / _GPUI_SHUTDOWN, 'lifecycle_action("lifecycle.shutdown_all", Null)\n')
    assert wc.check_shutdown_enumerates_services_from_manifests() == []


def test_shutdown_flags_not_derived_from_table(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    # state/mod.rs that drains a hand-kept array instead of shutdown_sequence().
    _write(root / _SHUTDOWN, "let steps: [(&str, bool); 12] = [];\n")
    _write(root / _GPUI_SHUTDOWN, 'lifecycle_action("lifecycle.shutdown_all", Null)\n')
    findings = wc.check_shutdown_enumerates_services_from_manifests()
    assert any("shutdown is no longer derived" in f.message for f in findings)


def test_shutdown_flags_gpui_not_delegating(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / _SHUTDOWN,
        "for svc in crate::daemon_managed::shutdown_sequence() {}\n",
    )
    # gpui shutdown.rs that enumerates on its own instead of delegating.
    _write(root / _GPUI_SHUTDOWN, 'taskkill(["wylde-gateway.exe"]);\n')
    findings = wc.check_shutdown_enumerates_services_from_manifests()
    assert any("delegate" in f.message.lower() for f in findings)
    assert any(f.file == _GPUI_SHUTDOWN for f in findings)


def test_shutdown_flags_gpui_delegate_file_missing(isolated_tree: Any) -> None:
    """Hardened: a deleted gpui delegate must FIRE, not silently pass."""
    wc, root = isolated_tree
    _write(
        root / _SHUTDOWN,
        "for svc in crate::daemon_managed::shutdown_sequence() {}\n",
    )
    # No gpui shutdown.rs at all.
    findings = wc.check_shutdown_enumerates_services_from_manifests()
    assert any(f.file == _GPUI_SHUTDOWN for f in findings)


# ── Rule 46: every backend service has a manifest ─────────────────────


def test_every_service_forward_flags_runpy_without_manifest(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(root / "MyService" / "run.py", "# entry point\n")
    findings = wc.check_every_service_has_manifest()
    assert len(findings) == 1
    assert findings[0].rule == "every_service_has_manifest"
    assert "MyService" in findings[0].message


def test_every_service_forward_clean_with_manifest(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(root / "MyService" / "run.py", "# entry point\n")
    _write(root / "MyService" / "manifest.json", json.dumps({"name": "MyService"}))
    assert wc.check_every_service_has_manifest() == []


def test_every_service_reverse_flags_manifest_in_runtime_dir(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(root / "logs" / "manifest.json", json.dumps({"name": "logs"}))
    findings = wc.check_every_service_has_manifest()
    assert len(findings) == 1
    assert "runtime/archive" in findings[0].message


def test_every_service_reverse_exempts_core(isolated_tree: Any) -> None:
    """Core holds a legitimate infra rollup manifest — never flagged."""
    wc, root = isolated_tree
    _write(root / "Core" / "manifest.json", json.dumps({"name": "Core"}))
    assert wc.check_every_service_has_manifest() == []


# ── Rule 47: service manifest schema ──────────────────────────────────


def _svc_manifest(root: Any, name: str, body: dict) -> None:
    _write(root / name / "manifest.json", json.dumps(body))


def test_schema_clean_with_required_keys(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _svc_manifest(
        root,
        "Gateway",
        {"name": "Gateway", "entry_point": "py -3 -m Gateway.run", "shutdown_order": 20},
    )
    assert wc.check_service_manifest_schema() == []


def test_schema_allows_null_entry_point(isolated_tree: Any) -> None:
    """entry_point may be null — a library / in-process / pipe-only service."""
    wc, root = isolated_tree
    _svc_manifest(
        root, "N8N", {"name": "N8N", "entry_point": None, "shutdown_order": 40}
    )
    assert wc.check_service_manifest_schema() == []


def test_schema_flags_missing_shutdown_order(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _svc_manifest(root, "Voice", {"name": "Voice", "entry_point": "py -3 -m Voice.run"})
    findings = wc.check_service_manifest_schema()
    assert any("shutdown_order" in f.message and "missing" in f.message for f in findings)


def test_schema_flags_wrong_shutdown_order_type(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _svc_manifest(
        root,
        "Voice",
        {"name": "Voice", "entry_point": "x", "shutdown_order": "soon"},
    )
    findings = wc.check_service_manifest_schema()
    assert any("shutdown_order" in f.message and "integer" in f.message for f in findings)


def test_schema_flags_empty_name(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _svc_manifest(root, "VPN", {"name": "", "entry_point": "x", "shutdown_order": 70})
    findings = wc.check_service_manifest_schema()
    assert any("name" in f.message for f in findings)


def test_schema_flags_bad_health_check_type(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _svc_manifest(
        root,
        "Gateway",
        {
            "name": "Gateway",
            "entry_point": "x",
            "shutdown_order": 20,
            "health_check": 123,
        },
    )
    findings = wc.check_service_manifest_schema()
    assert any("health_check" in f.message for f in findings)
