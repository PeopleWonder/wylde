"""git_branch — list, create, switch, or delete branches."""

from __future__ import annotations

from typing import Any, Dict

from .._git_lib import run_git


def run_git_branch(params: Dict[str, Any]) -> Dict[str, Any]:
    path = params.get("path", ".")
    action = params.get("action", "list")
    name = params.get("name")

    if action == "list":
        res = run_git(["branch", "--all", "--no-color"], cwd=str(path))
        if res["returncode"] != 0:
            return {"status": "error", "error": res["stderr"] or "git branch failed"}
        current = None
        branches = []
        for line in str(res["stdout"]).splitlines():
            line = line.rstrip()
            if not line:
                continue
            if line.startswith("*"):
                current = line[2:].strip()
                branches.append(current)
            else:
                branches.append(line[2:].strip())
        return {"status": "success", "current": current, "branches": branches}

    if action in ("create", "switch", "delete"):
        if not name:
            return {"status": "error", "error": f"'name' required for action={action}"}
        if action == "create":
            res = run_git(["checkout", "-b", str(name)], cwd=str(path))
        elif action == "switch":
            res = run_git(["checkout", str(name)], cwd=str(path))
        else:
            res = run_git(["branch", "-D", str(name)], cwd=str(path))
        if res["returncode"] != 0:
            return {
                "status": "error",
                "error": res["stderr"] or res["stdout"] or "git branch failed",
            }
        return {
            "status": "success",
            "action": action,
            "name": name,
            "output": str(res["stdout"]).strip(),
        }

    return {"status": "error", "error": f"unknown action: {action}"}
