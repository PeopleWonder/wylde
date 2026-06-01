"""lint_python — ruff wrapper.

Runs ``ruff check --output-format=json`` against the supplied path
(default: the entire active Wylde tree) with the canonical excludes
baked in.  Returns a normalised findings list.
"""

from __future__ import annotations

import sys
from typing import Any, Dict

from .._lint_lib import (
    PY_EXCLUDES,
    envelope_error,
    envelope_ok,
    parse_ruff_json,
    run_lint_subprocess,
    wylde_root,
)


def run_lint_python(params: Dict[str, Any]) -> Dict[str, Any]:
    root = wylde_root()
    path = str(params.get("path") or ".")
    select = params.get("select")

    argv = [sys.executable, "-m", "ruff", "check", "--output-format=json"]
    for exclude in PY_EXCLUDES:
        argv.extend(["--exclude", exclude])
    if select:
        argv.extend(["--select", str(select)])
    argv.append(path)

    result = run_lint_subprocess(argv, cwd=root, timeout=120.0)
    if not result["ok"]:
        return envelope_error(
            result.get("error") or "ruff invocation failed",
            tool="lint_python",
        )

    findings = parse_ruff_json(result["stdout"])
    return envelope_ok(
        findings,
        tool="lint_python",
        summary_extra={
            "exit_code": result["returncode"],
            "stderr_preview": result["stderr"][:500] if result["stderr"] else "",
        },
    )
