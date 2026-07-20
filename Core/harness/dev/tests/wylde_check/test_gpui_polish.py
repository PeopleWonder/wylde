"""Tests for the panel-polish rules (rules 42-43):
manifest_factory_resolves, stream_call_must_handle_cancel.

Mirrors prod-side ``wylde_check/rules/_gpui_polish.py``.

Rule 41 (rest_routes_exist_in_service) was retired 2026-07-20; its tests
were removed with it.
"""

from __future__ import annotations

import json
from typing import Any

from .conftest import _write


# ── Shared seeders ───────────────────────────────────────────────────


def _seed_panel_with_ipc(
    root: Any,
    panel_name: str,
    *,
    panel_id: str = "foo",
    ipc_body: str,
    required_services: list[str] | None = None,
    factory: str | None = None,
) -> None:
    base = root / "Core" / "GUI" / "Frontend" / "Panels" / panel_name
    fac = factory if factory is not None else f"wylde_panel_{panel_id}::Panel::view"
    manifest = {
        "schema_version": 2,
        "service": "core",
        "panels": [
            {
                "id": panel_id,
                "title": panel_id.title(),
                "required_services": required_services or [],
                "source": {"kind": "gpui_view", "factory": fac},
            }
        ],
    }
    _write(base / "manifest.json", json.dumps(manifest))
    _write(base / "src" / "ipc.rs", ipc_body)

# ── Rule 42: manifest_factory_resolves ───────────────────────────────


def _seed_workspace_with_panel_crate(
    root: Any, panel_dir: str, crate_name: str, fn_src: str
) -> None:
    """Drop a synthetic gpui workspace with one panel crate."""
    _write(
        root / "Core" / "GUI" / "Cargo.toml",
        '[workspace]\nresolver = "2"\nmembers = ['
        f'"Frontend/Panels/{panel_dir}"'
        "]\n",
    )
    _write(
        root / "Core" / "GUI" / "Frontend" / "Panels" / panel_dir / "Cargo.toml",
        f'[package]\nname = "{crate_name}"\nversion = "0.1.0"\n',
    )
    _write(
        root / "Core" / "GUI" / "Frontend" / "Panels" / panel_dir / "src" / "lib.rs",
        fn_src,
    )


def test_rule42_clean_when_factory_resolves(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _seed_workspace_with_panel_crate(
        root,
        "Foo",
        "wylde-panel-foo",
        "pub struct Panel;\nimpl Panel {\n    pub fn view() {}\n}\n",
    )
    _write(
        root / "Core" / "GUI" / "Frontend" / "Panels" / "Foo" / "manifest.json",
        json.dumps(
            {
                "schema_version": 2,
                "service": "core",
                "panels": [
                    {
                        "id": "foo",
                        "title": "Foo",
                        "source": {
                            "kind": "gpui_view",
                            "factory": "wylde_panel_foo::Panel::view",
                        },
                    }
                ],
            }
        ),
    )
    assert wc.check_manifest_factory_resolves() == []


def test_rule42_flags_missing_crate(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    # No workspace member matches; factory points at a phantom crate.
    _write(
        root / "Core" / "GUI" / "Cargo.toml",
        '[workspace]\nresolver = "2"\nmembers = ["Frontend/Panels/Foo"]\n',
    )
    _write(
        root / "Core" / "GUI" / "Frontend" / "Panels" / "Foo" / "Cargo.toml",
        '[package]\nname = "wylde-panel-foo"\n',
    )
    _write(
        root / "Core" / "GUI" / "Frontend" / "Panels" / "Foo" / "src" / "lib.rs",
        "",
    )
    _write(
        root / "Core" / "GUI" / "Frontend" / "Panels" / "Foo" / "manifest.json",
        json.dumps(
            {
                "schema_version": 2,
                "service": "core",
                "panels": [
                    {
                        "id": "foo",
                        "source": {
                            "kind": "gpui_view",
                            "factory": "wylde_panel_ghost::Panel::view",
                        },
                    }
                ],
            }
        ),
    )
    findings = wc.check_manifest_factory_resolves()
    assert len(findings) == 1
    assert findings[0].rule == "manifest_factory_resolves"
    assert "wylde_panel_ghost" in findings[0].message
    assert findings[0].severity == "error"


def test_rule42_flags_missing_pub_fn(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _seed_workspace_with_panel_crate(
        root,
        "Foo",
        "wylde-panel-foo",
        # Has a pub struct but no `pub fn view`.
        "pub struct Panel;\n",
    )
    _write(
        root / "Core" / "GUI" / "Frontend" / "Panels" / "Foo" / "manifest.json",
        json.dumps(
            {
                "schema_version": 2,
                "service": "core",
                "panels": [
                    {
                        "id": "foo",
                        "source": {
                            "kind": "gpui_view",
                            "factory": "wylde_panel_foo::Panel::view",
                        },
                    }
                ],
            }
        ),
    )
    findings = wc.check_manifest_factory_resolves()
    assert len(findings) == 1
    assert "pub fn view" in findings[0].message


def test_rule42_skips_iframe_kind(isolated_tree: Any) -> None:
    """Iframe panels have no factory — rule 42 skips them."""
    wc, root = isolated_tree
    _seed_workspace_with_panel_crate(
        root, "Foo", "wylde-panel-foo", ""
    )
    _write(
        root / "Core" / "GUI" / "Frontend" / "Panels" / "Foo" / "manifest.json",
        json.dumps(
            {
                "schema_version": 2,
                "service": "core",
                "panels": [
                    {
                        "id": "foo",
                        "source": {"kind": "iframe", "url": "http://127.0.0.1:5678"},
                    }
                ],
            }
        ),
    )
    assert wc.check_manifest_factory_resolves() == []


def test_rule42_flags_malformed_factory_string(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _seed_workspace_with_panel_crate(
        root, "Foo", "wylde-panel-foo", "pub fn view() {}\n"
    )
    _write(
        root / "Core" / "GUI" / "Frontend" / "Panels" / "Foo" / "manifest.json",
        json.dumps(
            {
                "schema_version": 2,
                "service": "core",
                "panels": [
                    {
                        "id": "foo",
                        "source": {"kind": "gpui_view", "factory": "not::a::valid path"},
                    }
                ],
            }
        ),
    )
    findings = wc.check_manifest_factory_resolves()
    assert len(findings) == 1
    assert "not a recognized path-shape" in findings[0].message


def test_rule42_accepts_pub_async_fn(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _seed_workspace_with_panel_crate(
        root,
        "Foo",
        "wylde-panel-foo",
        "pub async fn view() {}\n",
    )
    _write(
        root / "Core" / "GUI" / "Frontend" / "Panels" / "Foo" / "manifest.json",
        json.dumps(
            {
                "schema_version": 2,
                "service": "core",
                "panels": [
                    {
                        "id": "foo",
                        "source": {"kind": "gpui_view", "factory": "wylde_panel_foo::view"},
                    }
                ],
            }
        ),
    )
    assert wc.check_manifest_factory_resolves() == []


# ── Rule 43: stream_call_must_handle_cancel ──────────────────────────


def _seed_stream_call_panel(root: Any, body: str) -> None:
    _write(
        root / "Core" / "GUI" / "Frontend" / "Panels" / "Chat" / "src" / "panel.rs",
        body,
    )


def test_rule43_clean_when_let_bound(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _seed_stream_call_panel(
        root,
        "fn _x() {\n"
        '    let stream = wylde_gui_pipe::stream_call("wylde-harness", "chat.x", json!({}));\n'
        "}\n",
    )
    assert wc.check_stream_call_must_handle_cancel() == []


def test_rule43_clean_when_assigned_to_self_field(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _seed_stream_call_panel(
        root,
        "fn _x(this: &mut Foo) {\n"
        '    this.stream = Some(wylde_gui_pipe::stream_call("wylde-harness", "chat.x", json!({})));\n'
        "}\n",
    )
    assert wc.check_stream_call_must_handle_cancel() == []


def test_rule43_clean_when_returned_via_trailing_expression(isolated_tree: Any) -> None:
    """Helper wrappers that return ``stream_call(...)`` as the trailing
    expression are the canonical safe shape — must not flag."""
    wc, root = isolated_tree
    _seed_stream_call_panel(
        root,
        "fn make() -> wylde_gui_pipe::PipeStream {\n"
        "    wylde_gui_pipe::stream_call(\n"
        '        "wylde-harness",\n'
        '        "chat.stream_turn",\n'
        "        json!({}),\n"
        "    )\n"
        "}\n",
    )
    assert wc.check_stream_call_must_handle_cancel() == []


def test_rule43_clean_when_question_mark(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _seed_stream_call_panel(
        root,
        "fn try_make() -> Result<wylde_gui_pipe::PipeStream, String> {\n"
        '    let s = wylde_gui_pipe::stream_call("wylde-harness", "chat.x", json!({}))?;\n'
        "    Ok(s)\n"
        "}\n",
    )
    assert wc.check_stream_call_must_handle_cancel() == []


def test_rule43_flags_let_underscore_discard(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _seed_stream_call_panel(
        root,
        "fn _x() {\n"
        '    let _ = wylde_gui_pipe::stream_call("wylde-harness", "chat.x", json!({}));\n'
        "}\n",
    )
    findings = wc.check_stream_call_must_handle_cancel()
    assert len(findings) == 1
    assert findings[0].rule == "stream_call_must_handle_cancel"
    assert findings[0].severity == "error"
    assert "Drop-handle field" in findings[0].message


def test_rule43_flags_naked_expression_statement(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _seed_stream_call_panel(
        root,
        "fn _x() {\n"
        '    wylde_gui_pipe::stream_call("wylde-harness", "chat.x", json!({}));\n'
        "}\n",
    )
    findings = wc.check_stream_call_must_handle_cancel()
    assert len(findings) == 1
    assert "Drop-handle field" in findings[0].message


def test_rule43_honours_opt_out_marker_same_line(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _seed_stream_call_panel(
        root,
        "fn _x() {\n"
        '    let _ = wylde_gui_pipe::stream_call("wylde-harness", "chat.x", json!({})); // wylde-check: stream-discard-ok\n'
        "}\n",
    )
    assert wc.check_stream_call_must_handle_cancel() == []


def test_rule43_honours_opt_out_marker_line_above(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _seed_stream_call_panel(
        root,
        "fn _x() {\n"
        "    // wylde-check: stream-discard-ok\n"
        '    let _ = wylde_gui_pipe::stream_call("wylde-harness", "chat.x", json!({}));\n'
        "}\n",
    )
    assert wc.check_stream_call_must_handle_cancel() == []
