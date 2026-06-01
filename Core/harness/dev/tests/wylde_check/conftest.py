"""Shared fixtures for the wylde_check test package.

Each test rebinds ``wylde_check.WYLDE_ROOT`` to ``tmp_path`` so synthetic
files are the only thing the checker sees — no false positives from the
real tree, and no side effects on it.
"""

from __future__ import annotations

import sys
from pathlib import Path
from typing import Any

import pytest


_HERE = Path(__file__).resolve()
_VAULT_ROOT = _HERE.parents[6]
if str(_VAULT_ROOT) not in sys.path:
    sys.path.insert(0, str(_VAULT_ROOT))


def _import_check() -> Any:
    try:
        from Wylde.Core.harness.dev import wylde_check

        return wylde_check
    except ImportError:
        from Core.harness.dev import wylde_check

        return wylde_check


@pytest.fixture
def isolated_tree(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Any:
    """Rebind WYLDE_ROOT to a tmp_path so each test's synthetic files
    are the only thing the rules see."""
    wc = _import_check()
    monkeypatch.setattr(wc, "WYLDE_ROOT", tmp_path)
    return wc, tmp_path


def _write(p: Path, text: str) -> None:
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(text, encoding="utf-8")
