"""read_file — read a file's text contents."""

from __future__ import annotations

from pathlib import Path
from typing import Any, Dict

_CONTENT_CAP = 100_000  # bytes


def run_read_file(params: Dict[str, Any]) -> Dict[str, Any]:
    path_str = params.get("path")
    if not path_str:
        return {"status": "error", "error": "'path' is required"}
    path = Path(str(path_str))
    if not path.exists():
        return {
            "status": "error",
            "error": f"file not found: {path_str}",
            "code": "not_found",
        }
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except OSError as exc:
        return {"status": "error", "error": str(exc)}
    truncated = len(text) > _CONTENT_CAP
    return {
        "status": "success",
        "path": str(path),
        "content": text[:_CONTENT_CAP],
        "size": len(text),
        "truncated": truncated,
    }
