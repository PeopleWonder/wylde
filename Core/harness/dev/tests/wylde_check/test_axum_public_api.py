"""Tests for the axum public-API guard (rule 63, #290 axum containment).

Each test rebinds WYLDE_ROOT to a tmp tree of synthetic crate sources and
asserts the guard flags a fully-public axum-typed signature outside the gateway
while leaving pub(crate), the gateway, and test modules alone.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

from .conftest import _write


def _rule_module() -> Any:
    try:
        from Wylde.Core.harness.dev.wylde_check.rules import _axum_public_api as m
    except ImportError:
        from Core.harness.dev.wylde_check.rules import _axum_public_api as m
    return m


def _src(root: Path, crate: str, name: str, body: str) -> None:
    _write(root / "rust" / "crates" / crate / "src" / name, body)


def test_public_axum_return_type_outside_gateway_is_flagged(
    isolated_tree: Any,
) -> None:
    _wc, root = isolated_tree
    m = _rule_module()
    _src(
        root,
        "wylde-vpn",
        "http.rs",
        "use axum::Router;\npub fn router() -> Router {\n    Router::new()\n}\n",
    )
    findings = m.check_no_axum_types_in_public_api()
    assert len(findings) == 1
    assert findings[0].file.endswith("wylde-vpn/src/http.rs")


def test_pub_crate_is_not_flagged(isolated_tree: Any) -> None:
    _wc, root = isolated_tree
    m = _rule_module()
    _src(
        root,
        "wylde-treesitter",
        "http.rs",
        "use axum::Router;\npub(crate) fn router() -> Router {\n    Router::new()\n}\n",
    )
    assert m.check_no_axum_types_in_public_api() == []


def test_gateway_is_exempt(isolated_tree: Any) -> None:
    _wc, root = isolated_tree
    m = _rule_module()
    _src(
        root,
        "wylde-gateway",
        "routes.rs",
        "use axum::Router;\npub fn router() -> Router {\n    Router::new()\n}\n",
    )
    assert m.check_no_axum_types_in_public_api() == []


def test_multiline_signature_is_caught(isolated_tree: Any) -> None:
    _wc, root = isolated_tree
    m = _rule_module()
    _src(
        root,
        "wylde-vpn",
        "other.rs",
        "use axum::response::IntoResponse;\n"
        "pub async fn health(\n"
        ") -> impl IntoResponse {\n"
        "    ()\n"
        "}\n",
    )
    findings = m.check_no_axum_types_in_public_api()
    assert len(findings) == 1
    assert findings[0].line == 2  # anchored at the `pub async fn` line


def test_test_module_files_are_skipped(isolated_tree: Any) -> None:
    _wc, root = isolated_tree
    m = _rule_module()
    # A pub axum-typed fn under a tests/ path is not cross-crate API.
    _write(
        root / "rust" / "crates" / "wylde-vpn" / "tests" / "it.rs",
        "use axum::Router;\npub fn router() -> Router { Router::new() }\n",
    )
    assert m.check_no_axum_types_in_public_api() == []


def test_non_axum_file_is_ignored(isolated_tree: Any) -> None:
    _wc, root = isolated_tree
    m = _rule_module()
    # A crate-local `Router` type unrelated to axum (file never imports axum).
    _src(
        root,
        "wylde-other",
        "lib.rs",
        "pub struct Router;\npub fn router() -> Router {\n    Router\n}\n",
    )
    assert m.check_no_axum_types_in_public_api() == []
