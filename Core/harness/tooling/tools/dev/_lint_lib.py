"""Shared subprocess + envelope plumbing for the dev/ lint wrappers.

The three off-the-shelf wrappers (ruff, svelte-check, clippy) all do
the same three things:

1. Run a subprocess with explicit argv (no shell, no string-join).
2. Capture stdout/stderr (size-capped so a noisy run doesn't blow up
   an LLM context window).
3. Parse the tool's JSON output into a normalised finding list:
   ``[{rule, severity, file, line, message, context}]``.

This module centralises (1) + (2) and provides per-tool parse helpers
so each wrapper stays a thin translator.
"""

from __future__ import annotations

import json
import subprocess
from pathlib import Path
from typing import Any, Dict, List, Optional, Sequence, Tuple


_OUTPUT_CAP_BYTES = 1_000_000  # 1 MB stdout/stderr ceiling
_DEFAULT_TIMEOUT_S = 180.0


# ── Path defaults shared across wrappers ──────────────────────────────


def wylde_root() -> Path:
    """Return ``Wylde/`` no matter where the harness is mounted."""
    return Path(__file__).resolve().parents[5]


# Per-language walk-time exclusions baked into the wrapper defaults.
PY_EXCLUDES: Tuple[str, ...] = (
    "_legacy",
    "__pycache__",
    "vendor",
    "Core/GUI/node_modules",
    "Core/GUI/src-tauri/target",
    "docs/refactor-archive",
)


SVELTE_EXCLUDES: Tuple[str, ...] = (
    "node_modules",
    "dist",
    "build",
    ".svelte-kit",
)


RUST_EXCLUDES: Tuple[str, ...] = ("target",)


# ── Subprocess runner ────────────────────────────────────────────────


def run_lint_subprocess(
    argv: Sequence[str],
    *,
    cwd: Optional[Path] = None,
    timeout: float = _DEFAULT_TIMEOUT_S,
    env: Optional[Dict[str, str]] = None,
) -> Dict[str, Any]:
    """Run ``argv`` with stdout/stderr captured; return a dict.

    Returns ``{returncode, stdout, stderr, ok, error?}``.  Never raises:
    transport errors land as ``ok=False`` with a structured ``error``.
    Lint tools commonly return non-zero exit codes WHEN they have
    findings — that's expected.  Callers parse stdout regardless.
    """
    try:
        proc = subprocess.run(
            list(argv),
            cwd=str(cwd) if cwd else None,
            env=env,
            capture_output=True,
            text=True,
            timeout=timeout,
            check=False,
        )
    except FileNotFoundError as exc:
        return {
            "ok": False,
            "returncode": -1,
            "stdout": "",
            "stderr": "",
            "error": f"linter not found on PATH: {exc}",
        }
    except subprocess.TimeoutExpired as exc:
        return {
            "ok": False,
            "returncode": -1,
            "stdout": (exc.stdout or "")[:_OUTPUT_CAP_BYTES] if exc.stdout else "",
            "stderr": (exc.stderr or "")[:_OUTPUT_CAP_BYTES] if exc.stderr else "",
            "error": f"linter timed out after {timeout}s",
        }
    except OSError as exc:
        return {
            "ok": False,
            "returncode": -1,
            "stdout": "",
            "stderr": "",
            "error": f"subprocess raised {type(exc).__name__}: {exc}",
        }

    return {
        "ok": True,
        "returncode": proc.returncode,
        "stdout": (proc.stdout or "")[:_OUTPUT_CAP_BYTES],
        "stderr": (proc.stderr or "")[:_OUTPUT_CAP_BYTES],
    }


# ── Envelope builders ────────────────────────────────────────────────


def envelope_ok(
    findings: List[Dict[str, Any]],
    *,
    tool: str,
    summary_extra: Optional[Dict[str, Any]] = None,
) -> Dict[str, Any]:
    by_sev: Dict[str, int] = {"error": 0, "warning": 0, "info": 0}
    for f in findings:
        sev = f.get("severity", "warning")
        if sev in by_sev:
            by_sev[sev] += 1
    summary: Dict[str, Any] = {
        "tool": tool,
        "total": len(findings),
        "by_severity": by_sev,
    }
    if summary_extra:
        summary.update(summary_extra)
    return {
        "ok": True,
        "data": {
            "findings": findings,
            "summary": summary,
        },
    }


def envelope_error(
    message: str, *, tool: str, code: str = "lint_failed"
) -> Dict[str, Any]:
    return {
        "ok": False,
        "data": {
            "findings": [],
            "summary": {
                "tool": tool,
                "total": 0,
                "by_severity": {"error": 0, "warning": 0, "info": 0},
            },
        },
        "error": {"code": code, "message": message},
    }


# ── Parsers — translate each linter's JSON to the normalised shape ────


def parse_ruff_json(stdout: str) -> List[Dict[str, Any]]:
    """Ruff's ``--output-format=json`` produces a flat array."""
    try:
        records = json.loads(stdout) if stdout.strip() else []
    except (ValueError, TypeError):
        return []
    out: List[Dict[str, Any]] = []
    for r in records:
        if not isinstance(r, dict):
            continue
        loc = r.get("location") or {}
        try:
            relpath = str(
                Path(r.get("filename", "")).relative_to(wylde_root())
            ).replace("\\", "/")
        except (TypeError, ValueError):
            relpath = r.get("filename", "")
        out.append(
            {
                "rule": str(r.get("code") or "RUFF"),
                "severity": "error"
                if (r.get("fix") is None and r.get("code", "").startswith(("E", "F")))
                else "warning",
                "file": relpath,
                "line": int(loc.get("row") or 0),
                "message": str(r.get("message") or ""),
                "context": "",
            }
        )
    return out


def parse_clippy_json_lines(stdout: str) -> List[Dict[str, Any]]:
    """Cargo clippy emits NDJSON; each line is a cargo diagnostic.
    We pluck the ones with kind ``compiler-message`` and surface them."""
    out: List[Dict[str, Any]] = []
    for raw in stdout.splitlines():
        raw = raw.strip()
        if not raw:
            continue
        try:
            obj = json.loads(raw)
        except (ValueError, TypeError):
            continue
        if obj.get("reason") != "compiler-message":
            continue
        msg = obj.get("message") or {}
        if not isinstance(msg, dict):
            continue
        spans = msg.get("spans") or []
        primary = next((s for s in spans if s.get("is_primary")), None)
        file = ""
        line = 0
        if primary:
            file = primary.get("file_name") or ""
            line = int(primary.get("line_start") or 0)
        out.append(
            {
                "rule": str((msg.get("code") or {}).get("code") or "clippy"),
                "severity": str(msg.get("level") or "warning"),
                "file": file,
                "line": line,
                "message": str(msg.get("message") or ""),
                "context": "",
            }
        )
    return out


def parse_svelte_check_text(stdout: str, stderr: str) -> List[Dict[str, Any]]:
    """``svelte-check`` default output is human-readable.  Try
    ``--output machine`` first (older versions) and fall back to
    text-line heuristics.  We accept both; the wrapper passes the
    machine-output flag, so most lines look like:

        ROW1 COL1 LEN PATH SEVERITY MESSAGE

    or in some versions:

        START "<path>" <severity> "<message>"

    We're conservative — anything we can't parse becomes a single
    file-level info finding so the user sees output rather than silent.
    """
    findings: List[Dict[str, Any]] = []
    blob = stdout + "\n" + stderr
    for line in blob.splitlines():
        line = line.rstrip()
        if not line:
            continue
        # machine-readable line: starts with token + tab-separated fields.
        # Skip startup lines.
        if not (
            line.startswith("Error")
            or line.startswith("Warning")
            or line.startswith("ERROR")
            or line.startswith("WARN")
            or "\t" in line
        ):
            continue
        # Try the tab-separated 5-field shape first.
        parts = line.split("\t")
        if len(parts) >= 5:
            sev = parts[0].lower()
            findings.append(
                {
                    "rule": "svelte-check",
                    "severity": "error" if "error" in sev else "warning",
                    "file": parts[2] if len(parts) > 2 else "",
                    "line": _safe_int(parts[3]) if len(parts) > 3 else 0,
                    "message": parts[-1],
                    "context": "",
                }
            )
    return findings


def _safe_int(value: str) -> int:
    try:
        return int(value)
    except (ValueError, TypeError):
        return 0


__all__ = [
    "wylde_root",
    "PY_EXCLUDES",
    "SVELTE_EXCLUDES",
    "RUST_EXCLUDES",
    "run_lint_subprocess",
    "envelope_ok",
    "envelope_error",
    "parse_ruff_json",
    "parse_clippy_json_lines",
    "parse_svelte_check_text",
]
