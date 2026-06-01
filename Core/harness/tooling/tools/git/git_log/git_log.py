"""git_log — recent commit history."""

from __future__ import annotations

from typing import Any, Dict, List

from .._git_lib import run_git


def run_git_log(params: Dict[str, Any]) -> Dict[str, Any]:
    path = params.get("path", ".")
    try:
        limit = int(params.get("limit", 20))
    except (TypeError, ValueError):
        limit = 20
    sep, rec = "\x1f", "\x1e"
    fmt = sep.join(["%H", "%h", "%an", "%ae", "%at", "%s"]) + rec
    res = run_git(["log", f"-n{limit}", f"--pretty=format:{fmt}"], cwd=str(path))
    if res["returncode"] != 0:
        return {"status": "error", "error": res["stderr"] or "git log failed"}
    commits: List[Dict[str, Any]] = []
    for chunk in str(res["stdout"]).split(rec):
        chunk = chunk.strip("\n")
        if not chunk:
            continue
        parts = chunk.split(sep)
        if len(parts) < 6:
            continue
        commits.append(
            {
                "sha": parts[0],
                "short": parts[1],
                "author": parts[2],
                "email": parts[3],
                "ts": int(parts[4]) if parts[4].isdigit() else 0,
                "subject": parts[5],
            }
        )
    return {"status": "success", "commits": commits, "count": len(commits)}
