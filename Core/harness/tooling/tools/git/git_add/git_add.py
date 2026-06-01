"""git_add — stage one or more files."""

from __future__ import annotations

from typing import Any, Dict

from .._git_lib import run_git


def run_git_add(params: Dict[str, Any]) -> Dict[str, Any]:
    path = params.get("path", ".")
    files = params.get("files")
    if not files:
        return {"status": "error", "error": "'files' (list or string) is required"}
    if isinstance(files, str):
        files = [files]
    res = run_git(["add", "--", *[str(f) for f in files]], cwd=str(path))
    if res["returncode"] != 0:
        return {"status": "error", "error": res["stderr"] or "git add failed"}
    return {"status": "success", "added": list(files), "count": len(files)}
