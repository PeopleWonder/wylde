"""Shared subprocess wrapper for tools/git/.

The legacy ``git_tools.py`` returned a ``_Result`` object so the Flask
dispatcher could unpack returncode/stdout/stderr. The Phase 6 contract is
"tools return plain dicts", so we normalise here. The helper deliberately
keeps things minimal — each tool builds its own ``args`` list and decides
how to shape the success payload.
"""

from __future__ import annotations

import subprocess
from typing import Dict, List


def run_git(args: List[str], cwd: str, timeout: int = 60) -> Dict[str, str | int]:
    """Run ``git <args>`` in ``cwd``. Returns ``{returncode, stdout, stderr}``.

    Never raises on non-zero exit; the caller distinguishes success from
    failure based on ``returncode``. Timeout is the only escape hatch.
    """
    proc = subprocess.run(
        ["git", *args],
        cwd=cwd,
        capture_output=True,
        text=True,
        timeout=timeout,
    )
    return {
        "returncode": proc.returncode,
        "stdout": proc.stdout,
        "stderr": proc.stderr,
    }
