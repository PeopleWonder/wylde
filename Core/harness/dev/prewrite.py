"""Shared pre-write lint plumbing.

Used by:

* :mod:`Core.harness.dev.lint_hook` — Claude Code PostToolUse hook
  invoked after a session edits a file, prints findings to stderr,
  exits nonzero on errors.
* :mod:`Core.harness.tooling.tools.fs.write_file` and ``edit_file`` —
  the harness tools that the LLM uses; they call this BEFORE writing
  to disk so a proposed-content violation can block the write or
  surface a warning event.

Decision matrix the helper returns:

  +----------------------+--------+-------------+--------------------+
  | finding source       | sev    | decision    | event              |
  +----------------------+--------+-------------+--------------------+
  | wylde_check          | error  | block       | tool_warning       |
  | wylde_check          | warn   | allow       | tool_warning       |
  | ruff                 | error  | allow       | tool_warning       |
  | ruff                 | warn   | allow       | (silent)           |
  +----------------------+--------+-------------+--------------------+

The "block" decision means the caller (fs tool or hook) should refuse
the write and surface the findings; "allow" means proceed but emit a
warning event so a human / observer can see the issue.  Architectural
errors (wylde_check ``error`` severity) are the only blockers because
they're load-bearing — the no-internal-HTTP rule, etc., are contracts
the harness/Gateway boundary relies on.
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path
from typing import Any, Dict, List


# ── Helpers ───────────────────────────────────────────────────────────


def wylde_root() -> Path:
    """Wylde/ regardless of where this module is mounted."""
    return Path(__file__).resolve().parents[3]


def normalise_path(path: Any) -> str:
    """Return forward-slash relative-to-Wylde path for an absolute or
    relative input.  Falls back to the input as-given when it's outside
    WYLDE_ROOT (rare — tmp-file tests, etc.)."""
    if not path:
        return ""
    p = Path(str(path))
    if p.is_absolute():
        try:
            return str(p.relative_to(wylde_root())).replace("\\", "/")
        except ValueError:
            return str(p).replace("\\", "/")
    return str(p).replace("\\", "/")


# ── ruff against a string (stdin-input mode) ─────────────────────────


def lint_python_string(rel_path: str, content: str) -> List[Dict[str, Any]]:
    """Run ``ruff check --stdin-filename=<rel_path> -`` against the
    proposed content.  Returns a normalised findings list (empty on
    clean content OR if ruff isn't installed — never raises).

    The filename is passed via ``--stdin-filename`` so ruff applies its
    per-path config / extensions / ignore rules correctly even though
    the content is coming from stdin.
    """
    if not rel_path.endswith(".py"):
        return []
    argv = [
        sys.executable,
        "-m",
        "ruff",
        "check",
        "--output-format=json",
        f"--stdin-filename={rel_path}",
        "-",
    ]
    try:
        proc = subprocess.run(
            argv,
            input=content,
            capture_output=True,
            text=True,
            timeout=30.0,
            check=False,
        )
    except (FileNotFoundError, subprocess.TimeoutExpired, OSError):
        return []
    try:
        records = json.loads(proc.stdout) if proc.stdout.strip() else []
    except (ValueError, TypeError):
        return []
    out: List[Dict[str, Any]] = []
    for r in records:
        if not isinstance(r, dict):
            continue
        code = str(r.get("code") or "RUFF")
        # Ruff codes starting with E or F are typically real errors;
        # W/N/etc are warnings.  This mirrors lint_python's parser.
        severity = "error" if code.startswith(("E", "F")) else "warning"
        loc = r.get("location") or {}
        out.append(
            {
                "rule": code,
                "severity": severity,
                "file": rel_path,
                "line": int(loc.get("row") or 0),
                "message": str(r.get("message") or ""),
                "context": "",
            }
        )
    return out


# ── Single-file architectural check ──────────────────────────────────


def architectural_check(rel_path: str, content: str) -> List[Dict[str, Any]]:
    """Delegate to :func:`wylde_check.check_one_file`."""
    from Core.harness.dev import wylde_check

    result = wylde_check.check_one_file(rel_path, content)
    if not result.get("ok"):
        return []
    return list((result.get("data") or {}).get("findings") or [])


# ── Combined pre-write decision ──────────────────────────────────────


def evaluate_prewrite(
    path: Any,
    content: str,
    *,
    skip_ruff: bool = False,
) -> Dict[str, Any]:
    """Run wylde_check + ruff on the proposed (path, content) and
    decide whether to block, warn, or allow.

    Returns ``{
        "decision": "block" | "warn" | "allow",
        "findings": [...],            # combined, normalised
        "blocking_findings": [...],   # subset that triggered the block
        "warning_findings": [...],    # subset that triggers a tool_warning event
    }``.

    The split exists so the caller can show the user EXACTLY which
    findings forced the block separately from informational ones.
    """
    rel_path = normalise_path(path)
    if not isinstance(content, str):
        content = "" if content is None else str(content)

    arch_findings = architectural_check(rel_path, content)
    ruff_findings = [] if skip_ruff else lint_python_string(rel_path, content)
    all_findings = arch_findings + ruff_findings

    # Architectural errors block; everything else just warns.
    blocking = [f for f in arch_findings if f.get("severity") == "error"]
    warning = [f for f in arch_findings if f.get("severity") in {"warning", "info"}] + [
        f for f in ruff_findings if f.get("severity") == "error"
    ]

    if blocking:
        decision = "block"
    elif warning:
        decision = "warn"
    else:
        decision = "allow"

    return {
        "decision": decision,
        "findings": all_findings,
        "blocking_findings": blocking,
        "warning_findings": warning,
    }


__all__ = [
    "wylde_root",
    "normalise_path",
    "lint_python_string",
    "architectural_check",
    "evaluate_prewrite",
]
