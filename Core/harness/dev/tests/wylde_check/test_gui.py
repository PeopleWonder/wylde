"""Tests for the surviving GUI rule — gui_no_backend_bypass (10) —
mirrors prod-side wylde_check/rules/_gui.py.

Rules 7 (inferencebar_purity), 11 (gui_pipe_constants) and 30
(gui_error_reporting) were retired at the slice-11 cutover when the
Svelte/Tauri trees were deleted; rule 8 (gateway_scope) was retired
2026-07-20 when the Python Gateway tree was deleted. Their tests went
with them. Rule 10 was repointed from the Svelte/Tauri source to the
gpui panel + shell Rust source, so its tests now write synthetic `.rs`
files.
"""

from __future__ import annotations

from typing import Any

from .conftest import _write


# ── Rule 10: GUI no backend bypass (repointed to gpui Rust source) ────


def _panel(root: Any, rel: str, text: str) -> None:
    """Write a synthetic gpui panel source file."""
    _write(root / "Core" / "GUI" / "Frontend" / "Panels" / rel, text)


def test_gui_no_backend_bypass_flags_memory_path(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _panel(
        root,
        "Memory/src/memory_panel.rs",
        'let raw = std::fs::read("Core/harness/memory/indexes/long_term.bin");\n',
    )
    findings = wc.check_gui_no_backend_bypass()
    assert len(findings) == 1
    assert findings[0].rule == "gui_no_backend_bypass"
    assert "Core/harness/memory/indexes" in findings[0].message


def test_gui_no_backend_bypass_flags_manifest_path(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _panel(
        root,
        "Settings/src/settings_panel.rs",
        'let m = std::fs::read_to_string("Voice/manifest.json");\n',
    )
    findings = wc.check_gui_no_backend_bypass()
    assert len(findings) == 1
    assert "manifest.json" in findings[0].message


def test_gui_no_backend_bypass_clean_when_going_through_pipe(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _panel(
        root,
        "Memory/src/memory_panel.rs",
        'let rows = wylde_gui_pipe::call("wylde-harness", "POST", "/x", None).await;\n',
    )
    assert wc.check_gui_no_backend_bypass() == []


def test_gui_no_backend_bypass_ignores_comment_mention(isolated_tree: Any) -> None:
    """A `//` comment that mentions a backend path is documentation, not a
    bypass.  Don't flag those — they're how the panel explains itself."""
    wc, root = isolated_tree
    _panel(
        root,
        "Memory/src/memory_panel.rs",
        "// loaded via pipe — backend persists at Core/harness/memory/indexes/\n",
    )
    assert wc.check_gui_no_backend_bypass() == []


def test_gui_no_backend_bypass_skips_the_registry_aggregator(isolated_tree: Any) -> None:
    """The panel-registry aggregator under Core/GUI/Manifest/ reads panel
    manifest.json files by design — it's out of rule 10's scope, so a
    manifest.json literal there must NOT be flagged."""
    wc, root = isolated_tree
    _write(
        root / "Core" / "GUI" / "Manifest" / "Extension_handlers" / "src" / "agg.rs",
        'let m = path.join("manifest.json");\n',
    )
    assert wc.check_gui_no_backend_bypass() == []
