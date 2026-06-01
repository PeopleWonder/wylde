"""git_commit — create a commit with a message."""

from __future__ import annotations

from typing import Any, Dict

from .._git_lib import run_git


def run_git_commit(params: Dict[str, Any]) -> Dict[str, Any]:
    path = params.get("path", ".")
    message = params.get("message")
    if not message:
        return {"status": "error", "error": "'message' is required"}
    allow_empty = bool(params.get("allow_empty", False))
    args = ["commit", "-m", str(message)]
    if allow_empty:
        args.append("--allow-empty")
    res = run_git(args, cwd=str(path))
    if res["returncode"] != 0:
        return {
            "status": "error",
            "error": res["stderr"] or res["stdout"] or "git commit failed",
        }
    return {"status": "success", "output": str(res["stdout"]).strip()}
