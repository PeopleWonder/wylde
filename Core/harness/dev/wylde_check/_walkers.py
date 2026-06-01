"""Filesystem-walk helpers shared by every rule.

These helpers read ``WYLDE_ROOT`` dynamically from the package module so
the test suite's ``monkeypatch.setattr(wc, "WYLDE_ROOT", tmp_path)``
flows through every rule call.
"""

from __future__ import annotations

import sys as _sys
from pathlib import Path
from typing import List, Optional, Tuple

from ._config import ACTIVE_ROOTS, EXCLUDED_DIRS

# Resolve the parent package object dynamically so monkeypatch on
# ``wylde_check.WYLDE_ROOT`` flows through regardless of which import
# alias the caller used (``Core.harness.dev.wylde_check`` vs the
# ``Wylde.``-prefixed variant tests use).
_pkg = _sys.modules[__name__.rsplit(".", 1)[0]]


def _to_rel(path: Path) -> str:
    """Forward-slash relative path from WYLDE_ROOT."""
    try:
        return str(path.relative_to(_pkg.WYLDE_ROOT)).replace("\\", "/")
    except ValueError:
        return str(path).replace("\\", "/")


def _is_excluded(path: Path) -> bool:
    rel = _to_rel(path)
    for d in EXCLUDED_DIRS:
        if rel == d or rel.startswith(d + "/") or f"/{d}/" in rel:
            return True
    return False


def _walk(
    extensions: Tuple[str, ...], roots: Optional[Tuple[str, ...]] = None
) -> List[Path]:
    """Yield every file under ``roots`` (default ACTIVE_ROOTS) whose
    suffix is in ``extensions``, skipping EXCLUDED_DIRS.

    Deduplicates so a root like ``""`` (WYLDE_ROOT itself) plus
    ``"Core"`` doesn't visit each Core file twice.
    """
    selected_roots = roots if roots is not None else ACTIVE_ROOTS
    out: List[Path] = []
    seen: set = set()
    for root in selected_roots:
        base = _pkg.WYLDE_ROOT / root
        if not base.exists():
            continue
        for p in base.rglob("*"):
            if not p.is_file():
                continue
            if _is_excluded(p):
                continue
            if p.suffix.lower() not in extensions:
                continue
            key = p.resolve()
            if key in seen:
                continue
            seen.add(key)
            out.append(p)
    return out


def _read_text(path: Path) -> str:
    try:
        return path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError):
        return ""


def _is_test_path(rel: str) -> bool:
    name = rel.rsplit("/", 1)[-1]
    return (
        "/tests/" in rel
        or rel.startswith("tests/")
        or name.startswith("test_")
        or rel.endswith("_test.py")
    )
