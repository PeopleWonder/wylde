"""code_search_files — find files by glob (rg --files | python os.walk)."""

from __future__ import annotations

import os
import shutil
import subprocess
from fnmatch import fnmatch
from typing import Any, Dict, List

_NOISE_DIRS = (".git", "node_modules", "venv", "__pycache__")


def run_code_search_files(params: Dict[str, Any]) -> Dict[str, Any]:
    glob = params.get("glob")
    if not glob:
        return {"status": "error", "error": "'glob' is required"}
    path = str(params.get("path", "."))
    try:
        max_count = int(params.get("max_count", 500))
    except (TypeError, ValueError):
        max_count = 500

    try:
        if shutil.which("rg") is not None:
            proc = subprocess.run(
                ["rg", "--files", "--glob", str(glob), path],
                capture_output=True,
                text=True,
                timeout=60,
            )
            if proc.returncode not in (0, 1):
                return {
                    "status": "error",
                    "error": proc.stderr or f"rg failed (rc={proc.returncode})",
                }
            files: List[str] = [ln for ln in proc.stdout.splitlines() if ln.strip()][
                :max_count
            ]
            tool = "ripgrep"
        else:
            files = []
            for root, _dirs, names in os.walk(path):
                if any(seg in root for seg in _NOISE_DIRS):
                    continue
                for n in names:
                    if fnmatch(n, str(glob)):
                        files.append(os.path.join(root, n))
                        if len(files) >= max_count:
                            break
                if len(files) >= max_count:
                    break
            tool = "python-fallback"
    except Exception as exc:
        return {"status": "error", "error": str(exc)}

    return {
        "status": "success",
        "glob": glob,
        "tool": tool,
        "files": files,
        "count": len(files),
    }
