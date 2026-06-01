"""GPUI Cargo-workspace rule (rule 37).

Carved out of :mod:`wylde_check.rules._gpui` when that file crossed the
flat 700-LOC cap.  Hosts the single rule that lives at the workspace-
membership boundary — the rest of the gpui suite stays in ``_gpui.py``.

* :func:`check_panel_crate_must_be_workspace_member` — every
  ``Core/GUI/Frontend/Panels/*/Cargo.toml`` on disk must appear in
  the ``members = [...]`` array of ``Core/GUI/Cargo.toml``, and vice
  versa.  Either-direction failure either skips the crate at build
  time or makes ``cargo metadata`` refuse the workspace.
"""

from __future__ import annotations

import re
import sys as _sys
from pathlib import Path
from typing import List, Tuple

from .. import Finding
from .._walkers import _is_excluded, _read_text, _to_rel
from ._gpui import (
    GPUI_PANELS_ROOT,
    GPUI_WORKSPACE_CARGO,
    GPUI_WORKSPACE_ROOT,
)

_pkg = _sys.modules[__name__.rsplit(".", 2)[0]]


def _parse_workspace_members(text: str) -> List[str]:
    """Pull every path string out of the ``[workspace] members = [...]``
    array.  Tolerates multi-line + per-line comments — the canonical
    ``Core/GUI/Cargo.toml`` uses both.

    Returns the path strings in declaration order (duplicates retained
    so the rule can flag them downstream)."""
    members: List[str] = []
    m = re.search(r"\bmembers\s*=\s*\[", text)
    if not m:
        return members
    rest = text[m.end():]
    depth = 1
    i = 0
    n = len(rest)
    while i < n and depth > 0:
        ch = rest[i]
        if ch == "[":
            depth += 1
            i += 1
            continue
        if ch == "]":
            depth -= 1
            i += 1
            continue
        if ch in ('"', "'"):
            quote = ch
            j = i + 1
            while j < n and rest[j] != quote:
                if rest[j] == "\\" and j + 1 < n:
                    j += 2
                    continue
                j += 1
            if j < n:
                members.append(rest[i + 1 : j])
                i = j + 1
                continue
        i += 1
    return members


def check_panel_crate_must_be_workspace_member() -> List[Finding]:
    """Cross-check ``Core/GUI/Cargo.toml`` members against the actual
    panel-crate Cargo.toml files on disk.

    Two-direction check:

      * Every ``Frontend/Panels/<X>/Cargo.toml`` that exists must have
        its workspace-relative path in the ``members`` array.
      * Every ``Frontend/Panels/<X>`` entry in ``members`` must resolve
        to a real ``Cargo.toml`` on disk.

    Either direction failing means the gpui workspace will either skip
    a real crate at build time, or ``cargo metadata`` will refuse the
    dangling entry.
    """
    out: List[Finding] = []
    workspace_cargo = _pkg.WYLDE_ROOT / GPUI_WORKSPACE_CARGO
    if not workspace_cargo.exists():
        return out
    workspace_text = _read_text(workspace_cargo)
    if not workspace_text:
        return out
    workspace_rel = _to_rel(workspace_cargo)
    # Members are stored relative to Core/GUI/.  We compare panel paths
    # to the same form so the check is path-shape-agnostic.
    declared = [m.rstrip("/").replace("\\", "/") for m in _parse_workspace_members(workspace_text)]
    declared_panel_members = {m for m in declared if m.startswith("Frontend/Panels/")}

    actual_panel_dirs: List[Tuple[str, Path]] = []
    panels_base = _pkg.WYLDE_ROOT / GPUI_PANELS_ROOT
    if panels_base.exists():
        for child in sorted(panels_base.iterdir()):
            if not child.is_dir():
                continue
            cargo = child / "Cargo.toml"
            if cargo.exists() and not _is_excluded(cargo):
                # Path relative to Core/GUI/.
                rel_to_gui = f"Frontend/Panels/{child.name}"
                actual_panel_dirs.append((rel_to_gui, cargo))

    actual_panel_paths = {p for p, _ in actual_panel_dirs}

    # Missing-from-members: panel exists on disk but isn't declared.
    for rel_to_gui, cargo in actual_panel_dirs:
        if rel_to_gui not in declared_panel_members:
            out.append(
                Finding(
                    rule="panel_crate_must_be_workspace_member",
                    severity="error",
                    file=_to_rel(cargo),
                    line=0,
                    message=(
                        f"Panel crate {rel_to_gui!r} is not listed in "
                        f"`members = [...]` of {GPUI_WORKSPACE_CARGO}; "
                        f"cargo will skip it on build."
                    ),
                )
            )

    # Dangling member entry: members[] references a path with no Cargo.toml.
    for declared_path in declared_panel_members:
        if declared_path in actual_panel_paths:
            continue
        crate_dir = _pkg.WYLDE_ROOT / GPUI_WORKSPACE_ROOT / declared_path
        if (crate_dir / "Cargo.toml").exists():
            continue
        out.append(
            Finding(
                rule="panel_crate_must_be_workspace_member",
                severity="error",
                file=workspace_rel,
                line=0,
                message=(
                    f"`members = [...]` references {declared_path!r} but "
                    f"no Cargo.toml exists at that path; "
                    f"`cargo metadata` will refuse the workspace."
                ),
            )
        )
    return out
