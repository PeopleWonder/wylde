"""run_tests — run the project's test suite (auto-detects framework)."""

from __future__ import annotations

from typing import Any, Dict

from .._test_lib import detect_framework, execute


def run_run_tests(params: Dict[str, Any]) -> Dict[str, Any]:
    path = str(params.get("path", "."))
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
    return execute(framework, path, None, timeout)
