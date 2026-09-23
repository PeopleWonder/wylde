"""Tests for the boot/shutdown rules (44-45), mirrors prod-side
wylde_check/rules/_lifecycle.py. Added at the slice-11 cutover; rules
44/45 repointed at the live Rust single source of truth (the
``DAEMON_MANAGED`` table) for issue #101 — the old rules targeted the
deleted ``Core/Lifecycle/launcher.py`` / ``shutdown.py`` and passed green
over the missing files (a dead gate).

Rules 46 (every_service_has_manifest) and 47 (service_manifest_schema)
were retired 2026-07-20; their tests were removed with them.
"""

from __future__ import annotations

from typing import Any

import pytest

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


# #115 — each of these ESCAPED rule 44 before the regex fix. The first is the
# exact CORE_SERVICES literal #101 deleted from control.rs: re-pasting it back
# passed the gate clean. The prefix alternation missed any qualifier
# (CORE_/DAEMON_/WYLDE_), and the `: [` type-annotation requirement missed the
# idiomatic slice form `: &[&str] = &[` (the `[` is preceded by `&`).
_PREVIOUSLY_ESCAPING_ROSTERS = [
    ("core_services_array", 'const CORE_SERVICES: [&str; 2] = ["wylde-gateway", "wylde-voice"];\n'),
    ("core_services_slice", 'pub const CORE_SERVICES: &[&str] = &["wylde-gateway"];\n'),
    ("daemon_services_slice", 'static DAEMON_SERVICES: &[&str] = &["wylde-gateway"];\n'),
    ("bare_services_slice", 'const SERVICES: &[&str] = &["wylde-gateway"];\n'),
]


@pytest.mark.parametrize(
    "literal",
    [lit for _, lit in _PREVIOUSLY_ESCAPING_ROSTERS],
    ids=[label for label, _ in _PREVIOUSLY_ESCAPING_ROSTERS],
)
def test_boot_flags_prefixed_and_slice_service_rosters(isolated_tree: Any, literal: str) -> None:
    """A qualifier-prefixed name or a slice-form declaration is still a
    hand-kept roster and must fire rule 44. Testing only the two forms that
    already matched (bare/`ALL_` array) reproduces the blind spot #115 exists
    to close, so these assert the previously-escaping cases."""
    wc, root = isolated_tree
    _write_single_source(root)
    _write(root / "rust/crates/wylde-lifecycle/src/roster.rs", literal)
    findings = wc.check_launcher_enumerates_services_from_manifests()
    assert any(
        "hardcoded service roster in the Rust boot path" in f.message for f in findings
    ), f"rule 44 did not flag: {literal!r}"


def test_boot_does_not_flag_non_roster_service_constants(isolated_tree: Any) -> None:
    """The widened regex must not over-match: a scalar const whose name merely
    starts with SERVICE (e.g. a timeout) is not a roster and must stay clean."""
    wc, root = isolated_tree
    _write_single_source(root)
    _write(
        root / "rust/crates/wylde-lifecycle/src/roster.rs",
        "const SERVICE_TIMEOUT_MS: u64 = 5_000;\nconst MAX_SERVICES: usize = 12;\n",
    )
    assert wc.check_launcher_enumerates_services_from_manifests() == []


def test_boot_does_not_flag_typed_policy_table(isolated_tree: Any) -> None:
    """The #101 anti-pattern is a hand-kept roster of service-NAME strings
    (`&[&str]`). A TYPED struct table (e.g. the strangler-fig impl-selection
    table `&[StranglerService]`) is a different structure — boot still derives
    from DAEMON_MANAGED — and must not be flagged, exactly as `DAEMON_MANAGED:
    &[DaemonService]` is not."""
    wc, root = isolated_tree
    _write_single_source(root)
    _write(
        root / "rust/crates/wylde-lifecycle/src/state/services.rs",
        "const STRANGLER_SERVICES: &[StranglerService] = &[];\n",
    )
    assert wc.check_launcher_enumerates_services_from_manifests() == []


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
