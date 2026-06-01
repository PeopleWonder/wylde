"""Per-file check helpers used by pre-write hooks.

The architectural rules that don't need the full tree all reduce
cleanly to a per-file (rel_path, content) check.  Rules that DO need
cross-file state (manifest_paths, action_registry, gateway_scope,
gui_*, spawn_paths_exist, run_py_*) are skipped here — the full
``run_all()`` catches those.
"""

from __future__ import annotations

import json
from typing import List

from . import Finding
from ._config import (
    DEAD_REF_ALLOWLISTED_FILES,
    DEAD_SERVICE_NAMES,
    HTTP_CLIENT_PATTERNS,
    LOGGING_SETUP_PATTERNS,
    PIPE_NAME_GOOD_RE,
    PIPE_NAME_REF_RE,
    PIPE_NAME_TYPO_RE,
    SUBPROCESS_PATTERNS,
    TOOL_ID_RE,
)
from ._walkers import _is_test_path
from .rules._arch import (
    _WYLDE_CORE_IMPORT_RE,
    _is_no_http_exempt,
    _line_has_dead_ref_marker,
    _line_targets_internal,
)
from .rules._runtime import _is_subprocess_allowed


def _check_no_http_lines(rel_path: str, content: str) -> List[Finding]:
    """Rule 1 reduced to a single (rel_path, content) input."""
    if _is_no_http_exempt(rel_path):
        return []
    if (
        "/tests/" in rel_path
        or rel_path.endswith("_test.py")
        or rel_path.startswith("tests/")
    ):
        return []
    out: List[Finding] = []
    for lineno, line in enumerate(content.splitlines(), start=1):
        stripped = line.lstrip()
        if stripped.startswith("#") or stripped.startswith("//"):
            continue
        for pat in HTTP_CLIENT_PATTERNS:
            if pat.search(line) and _line_targets_internal(line):
                out.append(
                    Finding(
                        rule="no_internal_http",
                        severity="error",
                        file=rel_path,
                        line=lineno,
                        message=(
                            "Internal HTTP call detected outside Gateway / "
                            "Ollama client / database driver scope.  Use the "
                            "pipe transport for Wylde-internal traffic."
                        ),
                        context=line.strip()[:200],
                    )
                )
                break
    return out


def _check_import_paths_lines(rel_path: str, content: str) -> List[Finding]:
    if not rel_path.endswith(".py"):
        return []
    if "/tests/" in rel_path or rel_path.endswith("_test.py"):
        return []
    out: List[Finding] = []
    for lineno, line in enumerate(content.splitlines(), start=1):
        stripped = line.lstrip()
        if stripped.startswith("#"):
            continue
        if _WYLDE_CORE_IMPORT_RE.search(line):
            out.append(
                Finding(
                    rule="import_paths",
                    severity="warning",
                    file=rel_path,
                    line=lineno,
                    message=(
                        "Use bare `Core.*` import path; `Wylde.Core.*` is "
                        "non-canonical in active code."
                    ),
                    context=line.strip()[:200],
                )
            )
    return out


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


def _check_tool_id_lines(rel_path: str, content: str) -> List[Finding]:
    if not (rel_path.endswith("manifest.json") and "/tools/" in rel_path):
        return []
    try:
        data = json.loads(content)
    except (ValueError, TypeError):
        return [
            Finding(
                rule="tool_id_regex",
                severity="error",
                file=rel_path,
                line=0,
                message="manifest.json is not valid JSON",
            )
        ]
    if not isinstance(data, dict):
        return []
    out: List[Finding] = []
    for fld in ("id", "name"):
        v = data.get(fld)
        if v is None:
            out.append(
                Finding(
                    rule="tool_id_regex",
                    severity="error",
                    file=rel_path,
                    line=0,
                    message=f"manifest is missing required field {fld!r}",
                )
            )
        elif not isinstance(v, str) or not TOOL_ID_RE.match(v):
            out.append(
                Finding(
                    rule="tool_id_regex",
                    severity="error",
                    file=rel_path,
                    line=0,
                    message=(
                        f"manifest {fld}={v!r} doesn't match canonical "
                        f"regex /^[a-z][a-z0-9_]*(?:\\.[a-z][a-z0-9_]*)*$/"
                    ),
                )
            )
    return out


def _check_tool_docstring_lines(rel_path: str, content: str) -> List[Finding]:
    """Rule 12 reduced to a single (rel_path, content) input.

    Post W1.5: only ``manifest.json`` is inspected.  Tool implementations
    in any language are allowed; the manifest's ``description`` field is
    the single source of truth for the LLM-facing contract.
    """
    if not (rel_path.endswith("manifest.json") and "/tools/" in rel_path):
        return []
    try:
        data = json.loads(content)
    except (ValueError, TypeError):
        # tool_id_regex already flags malformed JSON; don't double-fire.
        return []
    if not isinstance(data, dict):
        return []
    description = data.get("description")
    if not isinstance(description, str) or not description.strip():
        return [
            Finding(
                rule="tool_docstring_required",
                severity="error",
                file=rel_path,
                line=0,
                message=(
                    "Tool manifest is missing a non-empty 'description'.  "
                    "The harness surfaces this to the LLM as the tool's "
                    "contract — describe what the tool does, its arguments, "
                    "and what it returns."
                ),
            )
        ]
    if len(description.strip()) < 20:
        return [
            Finding(
                rule="tool_docstring_required",
                severity="error",
                file=rel_path,
                line=0,
                message=(
                    f"Tool manifest 'description' is too short "
                    f"({len(description.strip())} chars; need ≥20)."
                ),
            )
        ]
    return []


def _check_logging_setup_lines(rel_path: str, content: str) -> List[Finding]:
    """Rule 13 reduced to a single (rel_path, content) input."""
    if not rel_path.endswith(".py"):
        return []
    if rel_path == "Core/shared/logging_setup.py":
        return []
    if _is_test_path(rel_path):
        return []
    out: List[Finding] = []
    for lineno, line in enumerate(content.splitlines(), start=1):
        stripped = line.lstrip()
        if stripped.startswith("#"):
            continue
        for pat in LOGGING_SETUP_PATTERNS:
            if pat.search(line):
                out.append(
                    Finding(
                        rule="logging_setup_only",
                        severity="error",
                        file=rel_path,
                        line=lineno,
                        message=(
                            "Direct logging.basicConfig / addHandler call "
                            "detected.  Replace with "
                            "`Core.shared.logging_setup.configure_logging(...)`."
                        ),
                        context=line.strip()[:200],
                    )
                )
                break
    return out


def _check_no_external_subprocess_lines(rel_path: str, content: str) -> List[Finding]:
    """Rule 14 reduced to a single (rel_path, content) input."""
    if not rel_path.endswith(".py"):
        return []
    if _is_test_path(rel_path):
        return []
    if _is_subprocess_allowed(rel_path):
        return []
    out: List[Finding] = []
    for lineno, line in enumerate(content.splitlines(), start=1):
        stripped = line.lstrip()
        if stripped.startswith("#"):
            continue
        for pat in SUBPROCESS_PATTERNS:
            if pat.search(line):
                out.append(
                    Finding(
                        rule="no_external_subprocess",
                        severity="error",
                        file=rel_path,
                        line=lineno,
                        message=(
                            "Subprocess spawning is restricted to the "
                            "Lifecycle daemon and a narrow allowlist."
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
