"""execute_bash — run a shell command line.

Ported from the legacy ``tool_runner_api.TOOLS_CONFIG['execute_bash']``. Uses
``shell=True`` to honour the same surface area as before; the runner has no
sandbox of its own, so the caller is responsible for vetting the command.
"""

from __future__ import annotations

from typing import Any, Dict

from .._code_lib import run_subprocess


def run_execute_bash(params: Dict[str, Any]) -> Dict[str, Any]:
    command = params.get("command")
    if not command:
        return {"status": "error", "error": "'command' is required"}
    timeout = int(params.get("timeout", 30))
    return run_subprocess(str(command), shell=True, timeout=timeout)
