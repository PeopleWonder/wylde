"""run_test_file — run a single test file (auto-detects framework)."""

from __future__ import annotations

from typing import Any, Dict

from .._test_lib import detect_framework, execute


def run_run_test_file(params: Dict[str, Any]) -> Dict[str, Any]:
    path = str(params.get("path", "."))
    file_arg = params.get("file")
    if not file_arg:
        return {"status": "error", "error": "'file' is required"}
    framework = str(params.get("framework") or detect_framework(path))
    try:
        timeout = int(params.get("timeout", 300))
    except (TypeError, ValueError):
        timeout = 300
    if framework == "unknown":
        return {
            "status": "error",
            "error": "could not detect test framework — pass `framework` explicitly",
        }
    return execute(framework, path, str(file_arg), timeout)
