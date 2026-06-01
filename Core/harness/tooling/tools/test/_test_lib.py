"""Shared framework-detection + execution for tools/test/.

Both tools (run_tests, run_test_file) need the same detection + parser
plumbing. Splitting the helpers out keeps each tool's `run_*` thin.
"""

from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
from typing import Any, Dict, Optional, Tuple


# ── Framework detection ──────────────────────────────────────────────────


def detect_framework(path: str) -> str:
    """Return one of: pytest | vitest | jest | unknown."""
    if (
        os.path.isfile(os.path.join(path, "pytest.ini"))
        or os.path.isfile(os.path.join(path, "pyproject.toml"))
        or os.path.isfile(os.path.join(path, "setup.cfg"))
    ):
        return "pytest"
    pkg = os.path.join(path, "package.json")
    if os.path.isfile(pkg):
        try:
            with open(pkg, "r", encoding="utf-8") as fh:
                data = json.load(fh)
        except (OSError, json.JSONDecodeError):
            return "unknown"
        deps = {**data.get("dependencies", {}), **data.get("devDependencies", {})}
        if "vitest" in deps:
            return "vitest"
        if "jest" in deps:
            return "jest"
    return "unknown"


# ── Output parsers ───────────────────────────────────────────────────────

_PYTEST_SUMMARY = re.compile(
    r"=+\s*"
    r"(?:(\d+)\s+failed)?[,\s]*"
    r"(?:(\d+)\s+passed)?[,\s]*"
    r"(?:(\d+)\s+skipped)?[,\s]*"
    r"(?:(\d+)\s+errors?)?"
)
_JS_SUMMARY = re.compile(
    r"Tests?:\s+(?:(\d+)\s+failed,?\s*)?(?:(\d+)\s+passed,?\s*)?(?:(\d+)\s+skipped,?\s*)?"
)


def _parse_pytest(stdout: str) -> Dict[str, Any]:
    failed = passed = skipped = errors = 0
    for line in reversed(stdout.splitlines()):
        if "passed" in line or "failed" in line or "error" in line:
            m = _PYTEST_SUMMARY.search(line)
            if m and any(m.groups()):
                failed = int(m.group(1) or 0)
                passed = int(m.group(2) or 0)
                skipped = int(m.group(3) or 0)
                errors = int(m.group(4) or 0)
                break
    failures = []
    in_failures = False
    for line in stdout.splitlines():
        if line.startswith("FAILED "):
            failures.append(line[len("FAILED ") :].strip())
        elif line.startswith("=") and "FAILURES" in line:
            in_failures = True
        elif in_failures and line.startswith("_"):
            failures.append(line.strip("_ ").strip())
    return {
        "framework": "pytest",
        "passed": passed,
        "failed": failed,
        "skipped": skipped,
        "errors": errors,
        "failures": failures[:50],
    }


def _parse_js(stdout: str, framework: str) -> Dict[str, Any]:
    failed = passed = skipped = 0
    for line in stdout.splitlines():
        m = _JS_SUMMARY.search(line)
        if m and any(m.groups()):
            failed = int(m.group(1) or 0)
            passed = int(m.group(2) or 0)
            skipped = int(m.group(3) or 0)
            break
    failures = []
    for line in stdout.splitlines():
        if "FAIL " in line or line.startswith("✗") or line.startswith("× "):
            failures.append(line.strip())
    return {
        "framework": framework,
        "passed": passed,
        "failed": failed,
        "skipped": skipped,
        "errors": 0,
        "failures": failures[:50],
    }


# ── Command builder + executor ───────────────────────────────────────────


def _build_cmd(
    framework: str, path: str, file_arg: Optional[str]
) -> Tuple[list, Optional[str]]:
    if framework == "pytest":
        cmd = ["pytest", "-q", "--no-header", "--tb=short"]
        if file_arg:
            cmd.append(file_arg)
        return cmd, None
    if framework == "vitest":
        local = os.path.join(path, "node_modules", ".bin", "vitest")
        if os.path.isfile(local) or os.path.isfile(local + ".cmd"):
            cmd = [
                local
                + (
                    ".cmd" if os.name == "nt" and os.path.isfile(local + ".cmd") else ""
                ),
                "run",
                "--reporter=default",
            ]
        else:
            if shutil.which("npx") is None:
                return [], "vitest not installed and npx unavailable"
            cmd = ["npx", "--yes", "vitest", "run", "--reporter=default"]
        if file_arg:
            cmd.append(file_arg)
        return cmd, None
    if framework == "jest":
        local = os.path.join(path, "node_modules", ".bin", "jest")
        if os.path.isfile(local) or os.path.isfile(local + ".cmd"):
            cmd = [
                local
                + (".cmd" if os.name == "nt" and os.path.isfile(local + ".cmd") else "")
            ]
        else:
            if shutil.which("npx") is None:
                return [], "jest not installed and npx unavailable"
            cmd = ["npx", "--yes", "jest"]
        if file_arg:
            cmd.append(file_arg)
        return cmd, None
    return [], f"unknown framework: {framework}"


def execute(
    framework: str, path: str, file_arg: Optional[str], timeout: int
) -> Dict[str, Any]:
    cmd, err = _build_cmd(framework, path, file_arg)
    if err:
        return {"status": "error", "error": err, "framework": framework}
    try:
        proc = subprocess.run(
            cmd, cwd=path, capture_output=True, text=True, timeout=timeout, shell=False
        )
    except subprocess.TimeoutExpired:
        return {
            "status": "error",
            "error": f"timeout after {timeout}s",
            "framework": framework,
        }
    except FileNotFoundError as exc:
        return {"status": "error", "error": str(exc), "framework": framework}

    parsed = (
        _parse_pytest(proc.stdout)
        if framework == "pytest"
        else _parse_js(proc.stdout, framework)
    )
    parsed["status"] = "success" if proc.returncode == 0 else "error"
    parsed["returncode"] = proc.returncode
    parsed["stdout"] = proc.stdout[-8000:]
    parsed["stderr"] = proc.stderr[-4000:]
    return parsed
