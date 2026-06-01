"""list_files — non-recursive directory listing."""

from __future__ import annotations

from pathlib import Path
from typing import Any, Dict


def run_list_files(params: Dict[str, Any]) -> Dict[str, Any]:
    path_str = params.get("path", ".")
    path = Path(str(path_str))
    if not path.exists():
        return {
            "status": "error",
            "error": f"path not found: {path_str}",
            "code": "not_found",
        }
    if not path.is_dir():
        return {"status": "error", "error": f"not a directory: {path_str}"}

    entries = []
    for f in sorted(path.iterdir()):
        is_dir = f.is_dir()
        size = None
        if not is_dir:
            try:
                size = f.stat().st_size
            except OSError:
                size = None
        entries.append(
            {
                "name": f.name,
                "type": "dir" if is_dir else "file",
                "size": size,
            }
        )

    return {
        "status": "success",
        "path": str(path),
        "files": entries,
        "count": len(entries),
    }
