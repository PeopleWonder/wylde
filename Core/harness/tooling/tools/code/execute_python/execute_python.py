"""execute_python — run a Python snippet via ``python -c``.

Ported from the legacy ``tool_runner_api.TOOLS_CONFIG['execute_python']``.
Same surface (``code`` + ``timeout``), same ceilings (timeout clamped to 300s,
streams capped at 10 KiB).
"""

from __future__ import annotations

import sys
from typing import Any, Dict

from .._code_lib import run_subprocess


def run_execute_python(params: Dict[str, Any]) -> Dict[str, Any]:
    code = params.get("code")
    if not code:
        return {"status": "error", "error": "'code' is required"}
    timeout = int(params.get("timeout", 30))
    return run_subprocess([sys.executable, "-c", str(code)], timeout=timeout)
