"""Tests for the launcher/shutdown/service-manifest rules (44-47),
mirrors prod-side wylde_check/rules/_lifecycle.py. Added at the slice-11
cutover.
"""

from __future__ import annotations

import json
from typing import Any

from .conftest import _write

_LAUNCHER = "Core/Lifecycle/launcher.py"
_SHUTDOWN = "Core/Lifecycle/shutdown.py"
_GPUI_SHUTDOWN = "Core/GUI/Shell/src/shutdown.rs"


# ── Rule 44: launcher enumerates services from manifests ──────────────


def test_launcher_clean_when_manifest_driven(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / _LAUNCHER,
        "def launch_all():\n"
        "    services = load_services()\n"
        "    mf = load_manifest(folder)\n",
    )
    assert wc.check_launcher_enumerates_services_from_manifests() == []


def test_launcher_flags_missing_manifest_reference(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(root / _LAUNCHER, "def launch_all():\n    return spawn_everything()\n")
    findings = wc.check_launcher_enumerates_services_from_manifests()
    assert len(findings) == 1
    assert findings[0].rule == "launcher_enumerates_services_from_manifests"
    assert "filesystem registry" in findings[0].message


def test_launcher_flags_hardcoded_service_roster(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / _LAUNCHER,
        "services = load_services()\n"
        'SERVICES = ["wylde-gateway", "wylde-voice", "wylde-vpn"]\n',
    )
    findings = wc.check_launcher_enumerates_services_from_manifests()
    assert len(findings) == 1
    assert "hardcoded service roster" in findings[0].message


def test_launcher_ignores_lowercase_services_local(isolated_tree: Any) -> None:
    """`services = load_services()` is the normal idiom — the rule only
    flags UPPERCASE roster constants, not lowercase locals."""
    wc, root = isolated_tree
    _write(root / _LAUNCHER, "services = load_services()\nfor s in services:\n    pass\n")
    assert wc.check_launcher_enumerates_services_from_manifests() == []


def test_launcher_flags_rust_const_services_array(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(root / _LAUNCHER, "load_services()\n")  # keep the python half clean
    _write(
        root / "rust/crates/wylde-lifecycle/src/daemon.rs",
        'const SERVICES: [&str; 2] = ["wylde-gateway", "wylde-voice"];\n',
    )
    findings = wc.check_launcher_enumerates_services_from_manifests()
    assert any("Rust launcher" in f.message for f in findings)


# ── Rule 45: shutdown enumerates services from manifests ──────────────


def test_shutdown_clean_when_manifest_driven(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / _SHUTDOWN,
        "def shutdown_all():\n"
        "    running = launcher.get_running()\n"
        "    order = _shutdown_sequence(list(running))\n",
    )
    _write(root / _GPUI_SHUTDOWN, 'lifecycle_action("lifecycle.shutdown_all", Null)\n')
    assert wc.check_shutdown_enumerates_services_from_manifests() == []


def test_shutdown_flags_missing_enumeration(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(root / _SHUTDOWN, "def shutdown_all():\n    kill_them_all()\n")
    _write(root / _GPUI_SHUTDOWN, 'lifecycle_action("lifecycle.shutdown_all", Null)\n')
    findings = wc.check_shutdown_enumerates_services_from_manifests()
    assert any("no longer enumerates" in f.message for f in findings)


def test_shutdown_flags_gpui_not_delegating(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(root / _SHUTDOWN, "running = launcher.get_running()\n")
    # gpui shutdown.rs that enumerates on its own instead of delegating.
    _write(root / _GPUI_SHUTDOWN, 'taskkill(["wylde-gateway.exe"]);\n')
    findings = wc.check_shutdown_enumerates_services_from_manifests()
    assert any("does not delegate".lower() in f.message.lower() or
               "delegate" in f.message.lower() for f in findings)
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
