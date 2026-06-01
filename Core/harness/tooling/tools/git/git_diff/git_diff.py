"""git_diff — unified diff of unstaged, staged, or commit-range changes."""

from __future__ import annotations

from typing import Any, Dict

from .._git_lib import run_git


def run_git_diff(params: Dict[str, Any]) -> Dict[str, Any]:
    path = params.get("path", ".")
    staged = bool(params.get("staged", False))
    base = params.get("base")
    head = params.get("head")
    file_arg = params.get("file")

    args = ["diff"]
    if staged:
        args.append("--cached")
    if base and head:
        args.append(f"{base}..{head}")
    elif base:
        args.append(str(base))
    if file_arg:
        args.extend(["--", str(file_arg)])

    res = run_git(args, cwd=str(path), timeout=120)
    if res["returncode"] != 0:
        return {"status": "error", "error": res["stderr"] or "git diff failed"}
    diff_text = str(res["stdout"])
    return {
        "status": "success",
        "diff": diff_text,
        "lines": diff_text.count("\n"),
    }
