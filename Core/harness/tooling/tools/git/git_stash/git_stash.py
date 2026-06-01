"""git_stash — push / pop / list / drop stash entries."""

from __future__ import annotations

from typing import Any, Dict

from .._git_lib import run_git


def run_git_stash(params: Dict[str, Any]) -> Dict[str, Any]:
    path = params.get("path", ".")
    action = params.get("action", "push")
    msg = params.get("message", "")

    if action == "push":
        args = ["stash", "push"]
        if msg:
            args.extend(["-m", str(msg)])
    elif action == "pop":
        args = ["stash", "pop"]
    elif action == "list":
        args = ["stash", "list"]
    elif action == "drop":
        args = ["stash", "drop"]
    else:
        return {"status": "error", "error": f"unknown action: {action}"}

    res = run_git(args, cwd=str(path))
    if res["returncode"] != 0:
        return {
            "status": "error",
            "error": res["stderr"] or res["stdout"] or "git stash failed",
        }
    return {"status": "success", "action": action, "output": str(res["stdout"]).strip()}
