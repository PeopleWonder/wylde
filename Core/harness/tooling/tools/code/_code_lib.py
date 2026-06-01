"""Shared subprocess helper for tools/code/.

The legacy ``tool_runner_api`` returned a ``subprocess.CompletedProcess``-shaped
object and let the dispatcher unpack it. The Phase 6 contract is "tools return
plain dicts", so we normalise here. Output streams are capped at 10 KiB each
to keep a runaway script from blowing up the LLM context window.
"""

from __future__ import annotations

import subprocess
from typing import Any, Dict, List, Optional

_OUT_CAP = 10_000  # bytes per stream
_TIMEOUT_CAP = 300  # seconds, hard ceiling regardless of caller


def run_subprocess(
    args: List[str] | str,
    *,
    shell: bool = False,
    timeout: Optional[int] = None,
    cwd: Optional[str] = None,
) -> Dict[str, Any]:
    """Run a child process and return a normalised dict envelope.

    ``shell=True`` is the only way to pass a shell command string; everything
    else should be a list of args. Timeout is clamped to ``_TIMEOUT_CAP``.
    """
    t = min(int(timeout or 30), _TIMEOUT_CAP)
    try:
        proc = subprocess.run(
            args,
            shell=shell,
            capture_output=True,
            text=True,
            timeout=t,
            cwd=cwd,
        )
    except subprocess.TimeoutExpired as exc:
        return {
            "status": "error",
            "error": f"timeout after {t}s",
            "returncode": 124,
            "stdout": (exc.stdout or "")[:_OUT_CAP]
            if isinstance(exc.stdout, str)
            else "",
            "stderr": (exc.stderr or "")[:_OUT_CAP]
            if isinstance(exc.stderr, str)
            else "",
        }
    return {
        "status": "success" if proc.returncode == 0 else "error",
        "returncode": proc.returncode,
        "stdout": proc.stdout[:_OUT_CAP],
        "stderr": proc.stderr[:_OUT_CAP],
    }
