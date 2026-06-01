"""Tests for rule 49 (``no_python_gateway_imports``).

The Python FastAPI Gateway was deleted on 2026-05-30 and its client
libraries moved to ``Core/shared/``; no active ``.py`` file may import
the top-level ``Gateway`` package ever again.

Mirrors prod-side ``wylde_check/rules/_no_gateway_import.py``.
"""

from __future__ import annotations

from typing import Any

from .conftest import _write


# ── Shared seeder ────────────────────────────────────────────────────


def _seed_py(root: Any, body: str, *, rel: str = "Core/harness/backend/x.py") -> None:
    """Drop a synthetic active-tree ``.py`` file with ``body``.

    Default path lives under ``Core/`` (an ACTIVE_ROOT) and is NOT under
    ``wylde_check/`` so the rule actually walks it.
    """
    parts = rel.split("/")
    target = root
    for p in parts:
        target = target / p
    _write(target, body)


# ── OK cases ─────────────────────────────────────────────────────────


def test_rule49_clean_for_relocated_egress_client(isolated_tree: Any) -> None:
    """The repointed import (`Core.shared.egress_client`) passes."""
    wc, root = isolated_tree
    _seed_py(
        root,
        "from Core.shared.egress_client import forward, GatewayError\n"
        "\n\nforward(dest='web', method='GET', path='/')\n",
    )
    assert wc.check_no_python_gateway_imports() == []


def test_rule49_clean_when_no_gateway_import(isolated_tree: Any) -> None:
    """A file with no Gateway import at all passes."""
    wc, root = isolated_tree
    _seed_py(root, "import os\nimport logging\n\n\nx = os.getpid()\n")
    assert wc.check_no_python_gateway_imports() == []


def test_rule49_ignores_prose_starting_with_from_gateway(isolated_tree: Any) -> None:
    """A docstring line that merely begins with the words 'from Gateway …'
    (no `import` keyword) is prose, not an import — no false positive."""
    wc, root = isolated_tree
    _seed_py(
        root,
        '"""Egress flows.\n\n'
        "from Gateway the requests come into the harness, then fan out.\n"
        "import Gateway routes are served by the Rust crate now.\n"
        '"""\n\nx = 1\n',
    )
    assert wc.check_no_python_gateway_imports() == []


def test_rule49_ignores_longer_identifier(isolated_tree: Any) -> None:
    """`from GatewayHelper import x` targets a different package — the
    word boundary after `Gateway` keeps it from matching."""
    wc, root = isolated_tree
    _seed_py(root, "from GatewayHelper import thing\n\n\nthing()\n")
    assert wc.check_no_python_gateway_imports() == []


def test_rule49_ignores_commented_import(isolated_tree: Any) -> None:
    """A commented-out import is not a live import."""
    wc, root = isolated_tree
    _seed_py(root, "# from Gateway.client import forward\nimport os\n")
    assert wc.check_no_python_gateway_imports() == []


def test_rule49_skips_wylde_check_package(isolated_tree: Any) -> None:
    """The checker's own package + tests carry the pattern as data and
    are skipped (otherwise this rule would flag its own fixtures)."""
    wc, root = isolated_tree
    _seed_py(
        root,
        "from Gateway.client import forward\n",
        rel="Core/harness/dev/tests/wylde_check/test_fixture.py",
    )
    assert wc.check_no_python_gateway_imports() == []


# ── Failing cases ────────────────────────────────────────────────────


def test_rule49_flags_from_gateway_import(isolated_tree: Any) -> None:
    """The synthetic-failure case: a live `from Gateway.X import` of the
    deleted package."""
    wc, root = isolated_tree
    _seed_py(root, "from Gateway.client import forward, GatewayError\n")
    findings = wc.check_no_python_gateway_imports()
    assert len(findings) == 1
    assert findings[0].rule == "no_python_gateway_imports"
    assert findings[0].severity == "error"
    assert findings[0].line == 1
    assert "Core.shared.egress_client" in findings[0].message


def test_rule49_flags_wylde_prefixed_from_import(isolated_tree: Any) -> None:
    """The `Wylde.`-prefixed variant is caught too."""
    wc, root = isolated_tree
    _seed_py(root, "\nfrom Wylde.Gateway.auth import require_device\n")
    findings = wc.check_no_python_gateway_imports()
    assert len(findings) == 1
    assert findings[0].line == 2


def test_rule49_flags_bare_import_gateway(isolated_tree: Any) -> None:
    """`import Gateway` and `import Gateway.run` (optionally aliased) are
    flagged."""
    wc, root = isolated_tree
    _seed_py(
        root,
        "import Gateway\nimport Gateway.run as gw\nimport Wylde.Gateway.client\n",
    )
    findings = wc.check_no_python_gateway_imports()
    assert len(findings) == 3
    assert {f.line for f in findings} == {1, 2, 3}


def test_rule49_registered_in_dispatcher(isolated_tree: Any) -> None:
    """The rule is wired into run_all's dispatcher under its canonical
    name (so the suite actually runs it)."""
    wc, _ = isolated_tree
    result = wc.run_all(only=["no_python_gateway_imports"])
    assert result["ok"] is True
    assert "no_python_gateway_imports" in result["data"]["summary"]["by_rule"]
