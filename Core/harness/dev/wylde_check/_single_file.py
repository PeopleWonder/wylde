"""Per-file check helpers used by pre-write hooks.

The architectural rules that don't need the full tree all reduce
cleanly to a per-file (rel_path, content) check.  Rules that DO need
cross-file state (gui_*, the gpui contract rules, the lifecycle rules)
are skipped here — the full ``run_all()`` catches those.

Trimmed 2026-07-20 (dead-rule retirement): the per-file twins for
``no_internal_http``, ``import_paths``, ``tool_id_regex``,
``tool_docstring_required``, ``logging_setup_only`` and
``no_external_subprocess`` were removed with their parent rules.  Left
in place they would have kept enforcing deleted rules invisibly through
``prewrite.py`` / ``lint_hook.py``.
"""

from __future__ import annotations

from typing import List

from . import Finding
from ._config import (
    DEAD_REF_ALLOWLISTED_FILES,
    DEAD_SERVICE_NAMES,
    PIPE_NAME_GOOD_RE,
    PIPE_NAME_REF_RE,
    PIPE_NAME_TYPO_RE,
)
from .rules._arch import _line_has_dead_ref_marker


def _check_dead_refs_lines(rel_path: str, content: str) -> List[Finding]:
    if rel_path in DEAD_REF_ALLOWLISTED_FILES:
        return []
    if rel_path.endswith("dev/wylde_check.py") or "/dev/wylde_check/" in rel_path:
        return []
    out: List[Finding] = []
    for lineno, line in enumerate(content.splitlines(), start=1):
        if _line_has_dead_ref_marker(line):
            continue
        for name in DEAD_SERVICE_NAMES:
            if name in line:
                out.append(
                    Finding(
                        rule="dead_service_refs",
                        severity="warning",
                        file=rel_path,
                        line=lineno,
                        message=(
                            f"Reference to dead service {name!r}; "
                            f"renamed/removed during refactor."
                        ),
                        context=line.strip()[:200],
                    )
                )
                break
    return out


def _check_pipe_name_convention_lines(rel_path: str, content: str) -> List[Finding]:
    """Rule 17 reduced to a single (rel_path, content) input."""
    if rel_path.endswith("dev/wylde_check.py") or "/dev/wylde_check/" in rel_path:
        return []
    if rel_path.endswith("dev/tests/test_wylde_check.py"):
        return []
    out: List[Finding] = []
    seen: set = set()
    for lineno, line in enumerate(content.splitlines(), start=1):
        for m in PIPE_NAME_REF_RE.finditer(line):
            name = m.group(0)
            if PIPE_NAME_GOOD_RE.match(name):
                continue
            if (name, lineno) in seen:
                continue
            seen.add((name, lineno))
            out.append(
                Finding(
                    rule="pipe_name_convention",
                    severity="error",
                    file=rel_path,
                    line=lineno,
                    message=(
                        f"Pipe name {name!r} does not match the "
                        f"`^wylde-[a-z][a-z0-9-]*$` convention."
                    ),
                    context=line.strip()[:200],
                )
            )
        for m in PIPE_NAME_TYPO_RE.finditer(line):
            name = m.group(1)
            if (name, lineno) in seen:
                continue
            seen.add((name, lineno))
            out.append(
                Finding(
                    rule="pipe_name_convention",
                    severity="error",
                    file=rel_path,
                    line=lineno,
                    message=(
                        f"Pipe name {name!r} uses underscores; the "
                        f"convention is dash-separated."
                    ),
                    context=line.strip()[:200],
                )
            )
    return out
