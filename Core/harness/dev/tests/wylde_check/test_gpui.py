"""Tests for the five gpui-workspace rules introduced 2026-05-29:
no_cross_panel_imports, no_legacy_gui_imports_in_panels,
webview_only_in_extension_handlers, first_party_manifest_must_be_gpui_view,
panel_crate_must_be_workspace_member.

Mirrors prod-side ``wylde_check/rules/_gpui.py``.
"""

from __future__ import annotations

import json
from typing import Any

from .conftest import _write


# ── Common test fixtures ─────────────────────────────────────────────


_PANEL_CARGO_CLEAN = """\
[package]
name = "wylde-panel-foo"
version = "0.1.0"

[dependencies]
wylde-theme = { path = "../../Theme" }
wylde-gui-pipe = { path = "../../Pipe" }
gpui.workspace = true
serde.workspace = true
"""


_PANEL_MANIFEST_CLEAN = {
    "schema_version": 2,
    "service": "core",
    "panels": [
        {
            "id": "foo",
            "title": "Foo",
            "source": {"kind": "gpui_view", "factory": "wylde_panel_foo::FooPanel::view"},
        }
    ],
}


_WORKSPACE_CARGO_CLEAN = """\
[workspace]
resolver = "2"
members = [
    "Shell",
    "Frontend/Theme",
    "Frontend/Pipe",
    "Frontend/Input",
    "Frontend/Extension_handlers/WebView",
    "Frontend/Panels/Foo",
]
"""


# ── Rule 33: no_cross_panel_imports ──────────────────────────────────


def test_no_cross_panel_imports_accepts_shared_infra(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Core" / "GUI" / "Frontend" / "Panels" / "Foo" / "Cargo.toml",
        _PANEL_CARGO_CLEAN,
    )
    assert wc.check_no_cross_panel_imports() == []


def test_no_cross_panel_imports_flags_sibling_panel_dep(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Core" / "GUI" / "Frontend" / "Panels" / "Foo" / "Cargo.toml",
        _PANEL_CARGO_CLEAN
        + "\nwylde-panel-bar = { path = \"../Bar\" }\n",
    )
    findings = wc.check_no_cross_panel_imports()
    assert len(findings) == 1
    assert findings[0].rule == "no_cross_panel_imports"
    assert "wylde-panel-bar" in findings[0].message
    assert findings[0].severity == "error"


def test_no_cross_panel_imports_flags_arbitrary_wylde_dep(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Core" / "GUI" / "Frontend" / "Panels" / "Foo" / "Cargo.toml",
        _PANEL_CARGO_CLEAN
        + '\nwylde-harness = { path = "../../../../rust/crates/wylde-harness" }\n',
    )
    findings = wc.check_no_cross_panel_imports()
    assert len(findings) == 1
    assert "wylde-harness" in findings[0].message


def test_no_cross_panel_imports_allows_all_four_shared_crates(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Core" / "GUI" / "Frontend" / "Panels" / "Foo" / "Cargo.toml",
        """\
[package]
name = "wylde-panel-foo"

[dependencies]
wylde-theme = { path = "../../Theme" }
wylde-gui-pipe = { path = "../../Pipe" }
wylde-gpui-input = { path = "../../Input" }
wylde-panel-registry = { path = "../../../Manifest/Extension_handlers" }
""",
    )
    assert wc.check_no_cross_panel_imports() == []


# ── Rule 34: no_legacy_gui_imports_in_panels ─────────────────────────


def test_no_legacy_imports_clean(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Core" / "GUI" / "Frontend" / "Panels" / "Foo" / "src" / "lib.rs",
        "use gpui::*;\nuse wylde_gui_pipe::HarnessClient;\n",
    )
    assert wc.check_no_legacy_gui_imports_in_panels() == []


def test_no_legacy_imports_flags_tauri_use(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Core" / "GUI" / "Frontend" / "Panels" / "Foo" / "src" / "lib.rs",
        "use tauri::Manager;\n",
    )
    findings = wc.check_no_legacy_gui_imports_in_panels()
    assert len(findings) == 1
    assert findings[0].rule == "no_legacy_gui_imports_in_panels"
    assert "tauri" in findings[0].message.lower()


def test_no_legacy_imports_ignores_doc_comments(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Core" / "GUI" / "Frontend" / "Panels" / "Foo" / "src" / "lib.rs",
        "//! Port of `InferenceBar.svelte`. Cutover deletes `src-tauri/`.\n"
        "use gpui::*;\n",
    )
    assert wc.check_no_legacy_gui_imports_in_panels() == []


# ── Rule 35: webview_only_in_extension_handlers ──────────────────────


def test_webview_rule_allows_wry_inside_handler_dir(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root
        / "Core"
        / "GUI"
        / "Frontend"
        / "Extension_handlers"
        / "WebView"
        / "src"
        / "lib.rs",
        "use wry::WebView;\n",
    )
    assert wc.check_webview_only_in_extension_handlers() == []


def test_webview_rule_flags_wry_in_panel(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Core" / "GUI" / "Frontend" / "Panels" / "Foo" / "src" / "lib.rs",
        "use wry::WebView;\n",
    )
    findings = wc.check_webview_only_in_extension_handlers()
    assert len(findings) == 1
    assert findings[0].rule == "webview_only_in_extension_handlers"
    assert "WebView" in findings[0].message
    assert findings[0].severity == "error"


def test_webview_rule_flags_wry_in_shell(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Core" / "GUI" / "Shell" / "src" / "shell_root.rs",
        "use wry::WebViewBuilder;\n",
    )
    findings = wc.check_webview_only_in_extension_handlers()
    assert len(findings) == 1
    assert "Shell/src/shell_root.rs" in findings[0].file


def test_webview_rule_ignores_wrapper_import(isolated_tree: Any) -> None:
    """Shell legitimately uses `wylde_webview::` — the wrapper crate name —
    which must not match the `wry::` regex."""
    wc, root = isolated_tree
    _write(
        root / "Core" / "GUI" / "Shell" / "src" / "shell_root.rs",
        "use wylde_webview::IframeHost;\n",
    )
    assert wc.check_webview_only_in_extension_handlers() == []


def test_webview_rule_ignores_doc_comment(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Core" / "GUI" / "Shell" / "src" / "shell_root.rs",
        "//! Wraps wry::WebView via wylde_webview::IframeHost.\n"
        "use wylde_webview::IframeHost;\n",
    )
    assert wc.check_webview_only_in_extension_handlers() == []


# ── Rule 36: first_party_manifest_must_be_gpui_view ──────────────────


def test_first_party_manifest_clean(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Core" / "GUI" / "Frontend" / "Panels" / "Foo" / "manifest.json",
        json.dumps(_PANEL_MANIFEST_CLEAN, indent=2),
    )
    assert wc.check_first_party_manifest_must_be_gpui_view() == []


def test_first_party_manifest_flags_iframe_kind(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    bad = {
        "schema_version": 2,
        "panels": [
            {
                "id": "foo",
                "source": {"kind": "iframe", "url": "http://127.0.0.1:5678"},
            }
        ],
    }
    _write(
        root / "Core" / "GUI" / "Frontend" / "Panels" / "Foo" / "manifest.json",
        json.dumps(bad),
    )
    findings = wc.check_first_party_manifest_must_be_gpui_view()
    assert len(findings) == 1
    assert findings[0].rule == "first_party_manifest_must_be_gpui_view"
    assert "iframe" in findings[0].message


def test_first_party_manifest_flags_missing_source(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Core" / "GUI" / "Frontend" / "Panels" / "Foo" / "manifest.json",
        json.dumps({"schema_version": 2, "panels": [{"id": "foo"}]}),
    )
    findings = wc.check_first_party_manifest_must_be_gpui_view()
    assert len(findings) == 1
    assert "source" in findings[0].message


def test_first_party_manifest_flags_invalid_json(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Core" / "GUI" / "Frontend" / "Panels" / "Foo" / "manifest.json",
        "{ not valid json",
    )
    findings = wc.check_first_party_manifest_must_be_gpui_view()
    assert len(findings) == 1
    assert "not valid JSON" in findings[0].message

# ── Rule 37: panel_crate_must_be_workspace_member ────────────────────


def test_workspace_member_check_clean(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(root / "Core" / "GUI" / "Cargo.toml", _WORKSPACE_CARGO_CLEAN)
    _write(
        root / "Core" / "GUI" / "Frontend" / "Panels" / "Foo" / "Cargo.toml",
        '[package]\nname = "wylde-panel-foo"\n',
    )
    assert wc.check_panel_crate_must_be_workspace_member() == []


def test_workspace_member_check_flags_unregistered_crate(isolated_tree: Any) -> None:
    """A panel exists on disk but isn't in `members = [...]`."""
    wc, root = isolated_tree
    _write(
        root / "Core" / "GUI" / "Cargo.toml",
        '[workspace]\nresolver = "2"\nmembers = ["Shell", "Frontend/Panels/Foo"]\n',
    )
    _write(
        root / "Core" / "GUI" / "Frontend" / "Panels" / "Foo" / "Cargo.toml",
        '[package]\nname = "wylde-panel-foo"\n',
    )
    _write(
        root / "Core" / "GUI" / "Frontend" / "Panels" / "Bar" / "Cargo.toml",
        '[package]\nname = "wylde-panel-bar"\n',
    )
    findings = wc.check_panel_crate_must_be_workspace_member()
    assert len(findings) == 1
    assert findings[0].rule == "panel_crate_must_be_workspace_member"
    assert "Frontend/Panels/Bar" in findings[0].message
    assert "not listed in" in findings[0].message


def test_workspace_member_check_flags_dangling_member(isolated_tree: Any) -> None:
    """`members = [...]` references a panel directory whose Cargo.toml is missing."""
    wc, root = isolated_tree
    _write(
        root / "Core" / "GUI" / "Cargo.toml",
        '[workspace]\nresolver = "2"\nmembers = ["Frontend/Panels/Ghost"]\n',
    )
    # No Frontend/Panels/Ghost/Cargo.toml — only a stray sibling.
    _write(
        root / "Core" / "GUI" / "Frontend" / "Panels" / "Foo" / "Cargo.toml",
        '[package]\nname = "wylde-panel-foo"\n',
    )
    findings = wc.check_panel_crate_must_be_workspace_member()
    rules = {f.message for f in findings}
    # We expect TWO findings: one for the dangling Ghost entry, and one
    # for the actually-present Foo crate that isn't in members.
    assert any("Ghost" in m and "no Cargo.toml exists" in m for m in rules)
    assert any("Foo" in m and "not listed in" in m for m in rules)
