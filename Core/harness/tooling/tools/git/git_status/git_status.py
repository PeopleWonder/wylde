"""git_status — branch + porcelain file list."""

from __future__ import annotations

from typing import Any, Dict

from .._git_lib import run_git


def run_git_status(params: Dict[str, Any]) -> Dict[str, Any]:
    path = params.get("path", ".")
    res = run_git(["status", "--porcelain=v1", "--branch"], cwd=str(path))
    if res["returncode"] != 0:
        return {"status": "error", "error": res["stderr"] or "git status failed"}
    branch = ""
    files = []
    for line in str(res["stdout"]).splitlines():
        if line.startswith("##"):
            branch = line[3:].strip()
        elif len(line) >= 3:
            files.append({"status": line[:2].strip(), "path": line[3:]})
    return {"status": "success", "branch": branch, "files": files, "count": len(files)}
