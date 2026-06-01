"""show_diff — generate a unified diff between two files OR two strings."""

from __future__ import annotations

import difflib
from typing import Any, Dict, List


def _read(path: str) -> List[str]:
    with open(path, "r", encoding="utf-8") as fh:
        return fh.readlines()


def run_show_diff(params: Dict[str, Any]) -> Dict[str, Any]:
    a_path = params.get("a_path")
    b_path = params.get("b_path")
    a = params.get("a")
    b = params.get("b")

    if a_path and b_path:
        try:
            a_lines = _read(str(a_path))
            b_lines = _read(str(b_path))
        except OSError as exc:
            return {"status": "error", "error": str(exc)}
        a_label, b_label = str(a_path), str(b_path)
    elif a is not None and b is not None:
        a_str, b_str = str(a), str(b)
        a_lines = (a_str if a_str.endswith("\n") else a_str + "\n").splitlines(
            keepends=True
        )
        b_lines = (b_str if b_str.endswith("\n") else b_str + "\n").splitlines(
            keepends=True
        )
        a_label = str(params.get("a_label", "a"))
        b_label = str(params.get("b_label", "b"))
    else:
        return {
            "status": "error",
            "error": "provide either (a_path, b_path) or (a, b) strings",
        }

    try:
        context = int(params.get("context", 3))
    except (TypeError, ValueError):
        context = 3

    diff = list(
        difflib.unified_diff(
            a_lines, b_lines, fromfile=a_label, tofile=b_label, n=context
        )
    )
    diff_text = "".join(diff)
    return {
        "status": "success",
        "diff": diff_text,
        "lines": len(diff),
        "changed": len(diff) > 0,
    }
