"""Tests for rule 48 (``gateway_verbs_exist_in_harness_registry``).

The outbound companion to rule 38: every harness-pipe verb the Gateway
crate dispatches must be registered on the harness pipe (Rust
``ALL_PIPE_ACTIONS`` ∪ Python ``_ACTIONS``).

Mirrors prod-side ``wylde_check/rules/_gateway_contract.py``.
"""

from __future__ import annotations

from typing import Any

from .conftest import _write


# ── Shared seeders ───────────────────────────────────────────────────


def _seed_harness_registry(
    root: Any,
    *,
    rust_verbs: list[str],
    python_verbs: list[str],
) -> None:
    """Drop a synthetic harness pipe registry: a Rust ``ALL_PIPE_ACTIONS``
    array and a Python ``_ACTIONS`` dict.  Rule 48 reads the union of the
    two (via the rule-38 loader it shares)."""
    rust_lines = ",\n    ".join(f'"{v}"' for v in rust_verbs)
    _write(
        root / "rust" / "crates" / "wylde-harness" / "src" / "pipe.rs",
        "pub const ALL_PIPE_ACTIONS: &[&str] = &[\n    "
        + rust_lines
        + ",\n];\n",
    )
    py_lines = "\n    ".join(f'"{v}": _h,' for v in python_verbs)
    _write(
        root / "Core" / "harness" / "pipe" / "__init__.py",
        "def _h(p):\n    return p\n\n\n_ACTIONS = {\n    "
        + py_lines
        + "\n}\n",
    )


def _seed_gateway_route(root: Any, body: str, *, name: str = "memory.rs") -> None:
    """Drop a synthetic Gateway route file whose handler body is ``body``."""
    _write(
        root / "rust" / "crates" / "wylde-gateway" / "src" / "routes" / name,
        "use crate::routes::common::harness_dispatch;\n"
        "use crate::proxy_core::pipe_action;\n"
        "use axum::response::Response;\n"
        f"{body}\n",
    )


# ── OK cases ─────────────────────────────────────────────────────────


def test_rule48_clean_when_verb_in_rust_registry(isolated_tree: Any) -> None:
    """A verb served by the Rust harness (plural registry) passes."""
    wc, root = isolated_tree
    _seed_harness_registry(
        root,
        rust_verbs=["memory.workspaces.list", "chat.run_turn"],
        python_verbs=["tools.list"],
    )
    _seed_gateway_route(
        root,
        'pub async fn h() -> Response {\n'
        '    harness_dispatch("memory.workspaces.list", Value::Null).await\n'
        "}\n",
    )
    assert wc.check_gateway_verbs_exist_in_harness_registry() == []


def test_rule48_clean_when_verb_only_in_python_registry(isolated_tree: Any) -> None:
    """The real-world case behind audit item #3: the singular
    ``memory.workspace.*`` verbs live only in the Python ``_ACTIONS``
    dict (deferred from Rust), yet the Gateway dispatch is valid because
    the rule checks the *union* of both registries."""
    wc, root = isolated_tree
    _seed_harness_registry(
        root,
        rust_verbs=["memory.workspaces.list"],  # plural registry only
        python_verbs=["memory.workspace.list", "memory.workspace.search"],
    )
    _seed_gateway_route(
        root,
        'pub async fn h() -> Response {\n'
        '    harness_dispatch("memory.workspace.list", payload).await\n'
        "}\n",
    )
    assert wc.check_gateway_verbs_exist_in_harness_registry() == []


def test_rule48_clean_for_pipe_action_to_harness(isolated_tree: Any) -> None:
    """The raw ``pipe_action("wylde-harness", "verb", ...)`` form is
    checked too, and passes for a registered verb."""
    wc, root = isolated_tree
    _seed_harness_registry(
        root, rust_verbs=["tools.list"], python_verbs=[]
    )
    _seed_gateway_route(
        root,
        'pub async fn h() {\n'
        '    let _ = pipe_action("wylde-harness", "tools.list", json!({})).await;\n'
        "}\n",
    )
    assert wc.check_gateway_verbs_exist_in_harness_registry() == []


def test_rule48_skips_pipe_action_to_other_service(isolated_tree: Any) -> None:
    """``pipe_action`` to a non-harness service is out of scope — the
    verb need not (and won't) be in the harness registry."""
    wc, root = isolated_tree
    _seed_harness_registry(root, rust_verbs=["tools.list"], python_verbs=[])
    _seed_gateway_route(
        root,
        'pub async fn h() {\n'
        '    let _ = pipe_action("wylde-extension-bridge", "extensions.dispatch", payload).await;\n'
        "}\n",
    )
    assert wc.check_gateway_verbs_exist_in_harness_registry() == []


def test_rule48_skips_dynamic_verb(isolated_tree: Any) -> None:
    """A verb built from a parameter (MCP adapter pass-through) can't be
    statically resolved — skip, no false positive."""
    wc, root = isolated_tree
    _seed_harness_registry(root, rust_verbs=["tools.list"], python_verbs=[])
    _seed_gateway_route(
        root,
        'pub async fn h(action: &str) {\n'
        '    let _ = pipe_action("wylde-harness", action, payload).await;\n'
        "}\n",
    )
    assert wc.check_gateway_verbs_exist_in_harness_registry() == []


def test_rule48_honours_opt_out_marker(isolated_tree: Any) -> None:
    """A deliberate optional-verb probe (e.g. ``tools.get`` with a
    ``tools.list`` fallback) opts out with the inline marker."""
    wc, root = isolated_tree
    _seed_harness_registry(root, rust_verbs=["tools.list"], python_verbs=[])
    _seed_gateway_route(
        root,
        'pub async fn h() {\n'
        "    // wylde-check: optional-verb\n"
        '    let _ = pipe_action("wylde-harness", "tools.get", payload).await;\n'
        "}\n",
    )
    assert wc.check_gateway_verbs_exist_in_harness_registry() == []


# ── Failing cases ────────────────────────────────────────────────────


def test_rule48_flags_unregistered_verb(isolated_tree: Any) -> None:
    """The synthetic-mismatch case: the Gateway dispatches a verb that
    exists in NEITHER registry — a latent runtime ``no_action``."""
    wc, root = isolated_tree
    _seed_harness_registry(
        root,
        rust_verbs=["memory.workspaces.list"],
        python_verbs=["memory.workspace.list"],
    )
    _seed_gateway_route(
        root,
        'pub async fn h() -> Response {\n'
        # Typo: `workspce` — in neither registry.
        '    harness_dispatch("memory.workspce.list", payload).await\n'
        "}\n",
    )
    findings = wc.check_gateway_verbs_exist_in_harness_registry()
    assert len(findings) == 1
    assert findings[0].rule == "gateway_verbs_exist_in_harness_registry"
    assert findings[0].severity == "error"
    assert "memory.workspce.list" in findings[0].message
    assert "no_action" in findings[0].message


def test_rule48_flags_unregistered_pipe_action_verb(isolated_tree: Any) -> None:
    """Same, via the raw ``pipe_action`` form."""
    wc, root = isolated_tree
    _seed_harness_registry(root, rust_verbs=["tools.list"], python_verbs=[])
    _seed_gateway_route(
        root,
        'pub async fn h() {\n'
        '    let _ = pipe_action("wylde-harness", "tools.ghost", json!({})).await;\n'
        "}\n",
    )
    findings = wc.check_gateway_verbs_exist_in_harness_registry()
    assert len(findings) == 1
    assert "tools.ghost" in findings[0].message


def test_rule48_opt_out_only_suppresses_its_own_site(isolated_tree: Any) -> None:
    """The opt-out marker is per-call: a second unmarked bad dispatch in
    the same file still fires."""
    wc, root = isolated_tree
    _seed_harness_registry(root, rust_verbs=["tools.list"], python_verbs=[])
    _seed_gateway_route(
        root,
        'pub async fn h() {\n'
        "    // wylde-check: optional-verb\n"
        '    let _ = pipe_action("wylde-harness", "tools.optional", payload).await;\n'
        '    harness_dispatch("tools.ghost", payload).await\n'
        "}\n",
    )
    findings = wc.check_gateway_verbs_exist_in_harness_registry()
    assert len(findings) == 1
    assert "tools.ghost" in findings[0].message


# ── Wiring / no-regression ───────────────────────────────────────────


def test_rule48_registered_in_dispatcher(isolated_tree: Any) -> None:
    """The rule is wired into run_all's dispatcher under its canonical
    name (so the suite actually runs it)."""
    wc, _ = isolated_tree
    result = wc.run_all(only=["gateway_verbs_exist_in_harness_registry"])
    assert result["ok"] is True
    assert "gateway_verbs_exist_in_harness_registry" in result["data"]["summary"]["by_rule"]
