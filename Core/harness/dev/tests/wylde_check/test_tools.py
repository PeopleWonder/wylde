"""Tests for tool rules (tool_id_regex, tool_docstring_required) —
mirrors prod-side wylde_check/rules/_tools.py.
"""

from __future__ import annotations

import json
from typing import Any

from .conftest import _write


# ── Rule 3: tool id regex ─────────────────────────────────────────────


def test_tool_id_regex_clean_manifest(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    manifest = {
        "id": "git_status",
        "name": "git_status",
        "description": "Show working tree status",
    }
    _write(
        root
        / "Core"
        / "harness"
        / "tooling"
        / "tools"
        / "git"
        / "git_status"
        / "manifest.json",
        json.dumps(manifest),
    )
    findings = wc.check_tool_id_regex()
    assert findings == [], f"clean manifest should produce no findings; got {findings}"


def test_tool_id_regex_flags_bad_id(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    manifest = {
        "id": "BadName-WithDashes",
        "name": "BadName-WithDashes",
        "description": "x",
    }
    _write(
        root / "Core" / "harness" / "tooling" / "tools" / "x" / "y" / "manifest.json",
        json.dumps(manifest),
    )
    findings = wc.check_tool_id_regex()
    # 2 findings expected — id and name both fail the regex.
    assert len(findings) == 2
    assert all(f.rule == "tool_id_regex" for f in findings)


# Rule 3 no longer warns on missing description — rule 12 owns that
# (and emits an ERROR with a length floor).  Keeping a regression test
# here would double-count the same condition.


# ── Rule 12: tool manifest description required ───────────────────────


def _tool_manifest_path(root: Any, group: str, name: str) -> Any:
    return (
        root / "Core" / "harness" / "tooling" / "tools" / group / name / "manifest.json"
    )


def test_tool_docstring_required_flags_missing_description(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    manifest = {"id": "ok_tool", "name": "ok_tool"}
    _write(_tool_manifest_path(root, "g", "ok_tool"), json.dumps(manifest))
    findings = wc.check_tool_docstring_required()
    assert len(findings) == 1
    assert findings[0].rule == "tool_docstring_required"
    assert findings[0].severity == "error"
    assert "description" in findings[0].message


def test_tool_docstring_required_flags_short_description(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    manifest = {"id": "ok_tool", "name": "ok_tool", "description": "too short"}
    _write(_tool_manifest_path(root, "g", "ok_tool"), json.dumps(manifest))
    findings = wc.check_tool_docstring_required()
    assert len(findings) == 1
    assert findings[0].severity == "error"
    assert "too short" in findings[0].message.lower() or "≥20" in findings[0].message


def test_tool_docstring_required_clean_manifest(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    manifest = {
        "id": "git_status",
        "name": "git_status",
        "description": "Show the working tree status for the current repo.",
    }
    _write(_tool_manifest_path(root, "git", "git_status"), json.dumps(manifest))
    assert wc.check_tool_docstring_required() == []


def test_tool_docstring_required_ignores_python_modules(isolated_tree: Any) -> None:
    # No manifest, just a .py file — rule 12 is now manifest-only and
    # should not fire on raw Python sources.
    wc, root = isolated_tree
    _write(
        root / "Core" / "harness" / "tooling" / "tools" / "g" / "x" / "x.py",
        "def run():\n    return 1\n",
    )
    assert wc.check_tool_docstring_required() == []


def test_tool_docstring_required_skips_non_tools_manifests(isolated_tree: Any) -> None:
    # A manifest under data/manifests/ (service manifest) must not be
    # flagged by the tool rule.
    wc, root = isolated_tree
    _write(
        root / "data" / "manifests" / "wylde-foo.json",
        json.dumps({"service": "wylde-foo"}),
    )
    assert wc.check_tool_docstring_required() == []
