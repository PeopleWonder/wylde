"""Tests for action rules (action_registry, action_docstring_required)
— mirrors prod-side wylde_check/rules/_actions.py. Rule 9
(gui_action_contract) was retired at the slice-11 cutover.
"""

from __future__ import annotations

from typing import Any

from .conftest import _write


# ── Rule 4: action registry consistency ───────────────────────────────


def test_action_registry_flags_duplicates(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Core" / "harness" / "pipe" / "__init__.py",
        'register_action("foo.bar", handler1)\nregister_action("foo.bar", handler2)\n',
    )
    findings = wc.check_action_registry()
    assert len(findings) == 1
    assert findings[0].rule == "action_registry"
    assert findings[0].line == 2  # the duplicate line


def test_action_registry_clean_single_registration(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Core" / "harness" / "pipe" / "__init__.py",
        'register_action("foo.bar", h1)\nregister_action("foo.baz", h2)\n',
    )
    assert wc.check_action_registry() == []


# ── Rule 23: action handler docstring required ─────────────────────


def test_action_docstring_required_flags_missing(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Voice" / "pipe.py",
        "def _h_voice_toggle(payload):\n"
        "    return {}\n"
        '_ACTIONS = {"voice.toggle": _h_voice_toggle}\n',
    )
    findings = wc.check_action_docstring_required()
    assert len(findings) == 1
    assert findings[0].rule == "action_docstring_required"
    assert findings[0].severity == "error"
    assert "_h_voice_toggle" in findings[0].message


def test_action_docstring_required_accepts_long_docstring(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Voice" / "pipe.py",
        "def _h_voice_toggle(payload):\n"
        '    """Toggle the voice capture state on/off."""\n'
        "    return {}\n"
        '_ACTIONS = {"voice.toggle": _h_voice_toggle}\n',
    )
    assert wc.check_action_docstring_required() == []


def test_action_docstring_required_picks_up_register_action(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "device_gate" / "pipe.py",
        "def _h_check(payload):\n"
        "    return {}\n"
        'register_action("devices.check", _h_check)\n',
    )
    findings = wc.check_action_docstring_required()
    assert len(findings) == 1
    assert "_h_check" in findings[0].message
