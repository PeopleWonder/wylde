"""Tests for the three panel-polish rules introduced 2026-05-29
(rules 41-43): rest_routes_exist_in_service, manifest_factory_resolves,
stream_call_must_handle_cancel.

Mirrors prod-side ``wylde_check/rules/_gpui_polish.py``.
"""

from __future__ import annotations

import json
from typing import Any

from .conftest import _write


# ── Shared seeders ───────────────────────────────────────────────────


def _seed_gateway_routes(root: Any, lines: str) -> None:
    """Drop a synthetic ``rust/crates/wylde-gateway/src/routes.rs`` whose
    ``Router::new().route(...)`` body is composed from ``lines``.  The
    file is parsed only for ``.route("...", method(...))`` shapes; the
    surrounding scaffold is incidental."""
    body = (
        "use axum::Router;\n"
        "use axum::routing::{get, post, delete, put};\n"
        "fn handler() {}\n"
        "pub fn router() -> Router {\n"
        "    Router::new()\n"
        f"{lines}\n"
        "}\n"
    )
    _write(root / "rust" / "crates" / "wylde-gateway" / "src" / "routes.rs", body)


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


# ── Rule 41: rest_routes_exist_in_service ────────────────────────────


def test_rule41_clean_when_route_matches(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _seed_gateway_routes(
        root,
        '        .route("/api/images/library", get(handler))\n'
        '        .route("/api/images/library/:img_id", get(handler).delete(handler))',
    )
    _seed_panel_with_ipc(
        root,
        "Images",
        ipc_body=(
            'pub const SVC_GATEWAY: &str = "wylde-gateway";\n'
            "async fn _x() {\n"
            '    let _ = wylde_gui_pipe::call(SVC_GATEWAY, "GET", "/api/images/library", None).await;\n'
            "}\n"
        ),
    )
    findings = wc.check_rest_routes_exist_in_service()
    assert findings == []


def test_rule41_clean_with_wildcard_path(isolated_tree: Any) -> None:
    """Panel side uses ``format!("/api/foo/{id}")``; route declares
    ``:img_id`` — must match."""
    wc, root = isolated_tree
    _seed_gateway_routes(
        root,
        '        .route("/api/images/library/:img_id", get(handler).delete(handler))',
    )
    _seed_panel_with_ipc(
        root,
        "Images",
        ipc_body=(
            'pub const SVC_GATEWAY: &str = "wylde-gateway";\n'
            "async fn _x(id: &str) {\n"
            '    let path = format!("/api/images/library/{id}");\n'
            '    let _ = wylde_gui_pipe::call(SVC_GATEWAY, "GET", &format!("/api/images/library/{id}"), None).await;\n'
            "}\n"
        ),
    )
    findings = wc.check_rest_routes_exist_in_service()
    assert findings == []


def test_rule41_flags_missing_route(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _seed_gateway_routes(
        root,
        '        .route("/api/images/library", get(handler))',
    )
    _seed_panel_with_ipc(
        root,
        "Images",
        ipc_body=(
            'pub const SVC_GATEWAY: &str = "wylde-gateway";\n'
            "async fn _x() {\n"
            '    let _ = wylde_gui_pipe::call(SVC_GATEWAY, "POST", "/api/images/ghost", None).await;\n'
            "}\n"
        ),
    )
    findings = wc.check_rest_routes_exist_in_service()
    assert len(findings) == 1
    assert findings[0].rule == "rest_routes_exist_in_service"
    assert "/api/images/ghost" in findings[0].message
    assert "POST" in findings[0].message
    assert findings[0].severity == "error"


def test_rule41_flags_method_mismatch(isolated_tree: Any) -> None:
    """Path matches but method doesn't — still a 404 at runtime."""
    wc, root = isolated_tree
    _seed_gateway_routes(
        root,
        '        .route("/api/images/library", get(handler))',
    )
    _seed_panel_with_ipc(
        root,
        "Images",
        ipc_body=(
            'pub const SVC_GATEWAY: &str = "wylde-gateway";\n'
            "async fn _x() {\n"
            '    let _ = wylde_gui_pipe::call(SVC_GATEWAY, "POST", "/api/images/library", None).await;\n'
            "}\n"
        ),
    )
    findings = wc.check_rest_routes_exist_in_service()
    assert len(findings) == 1
    assert "POST /api/images/library" in findings[0].message


def test_rule41_skips_action_envelope(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _seed_gateway_routes(
        root,
        '        .route("/api/images/library", get(handler))',
    )
    _seed_panel_with_ipc(
        root,
        "Foo",
        ipc_body=(
            'pub const SVC_HARNESS: &str = "wylde-harness";\n'
            "async fn _x() {\n"
            '    let _ = wylde_gui_pipe::call(SVC_HARNESS, "POST", "/__action__", None).await;\n'
            "}\n"
        ),
    )
    assert wc.check_rest_routes_exist_in_service() == []


def test_rule41_skips_non_route_indexed_services(isolated_tree: Any) -> None:
    """wylde-vpn / wylde-harness have no axum router → skip."""
    wc, root = isolated_tree
    _seed_gateway_routes(
        root,
        '        .route("/api/images/library", get(handler))',
    )
    _seed_panel_with_ipc(
        root,
        "RemoteAccess",
        ipc_body=(
            'pub const SVC_VPN: &str = "wylde-vpn";\n'
            "async fn _x() {\n"
            '    let _ = wylde_gui_pipe::call(SVC_VPN, "GET", "/api/link/status", None).await;\n'
            "}\n"
        ),
    )
    assert wc.check_rest_routes_exist_in_service() == []


def test_rule41_skips_non_literal_path(isolated_tree: Any) -> None:
    """Path passed as a parameter — out of scope, no false positive."""
    wc, root = isolated_tree
    _seed_gateway_routes(
        root,
        '        .route("/api/images/library", get(handler))',
    )
    _seed_panel_with_ipc(
        root,
        "Images",
        ipc_body=(
            'pub const SVC_GATEWAY: &str = "wylde-gateway";\n'
            "async fn _x(path: &str) {\n"
            '    let _ = wylde_gui_pipe::call(SVC_GATEWAY, "GET", path, None).await;\n'
            "}\n"
        ),
    )
    assert wc.check_rest_routes_exist_in_service() == []


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
