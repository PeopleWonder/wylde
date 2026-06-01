"""code_search — regex search across files (rg with Python fallback)."""

from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
from fnmatch import fnmatch
from typing import Any, Dict, List

_NOISE_DIRS = (".git", "node_modules", "venv", "__pycache__", "dist", "build")


def _have_rg() -> bool:
    return shutil.which("rg") is not None


def _rg_search(
    pattern: str,
    path: str,
    file_type: str,
    glob: str,
    context: int,
    case_insensitive: bool,
    max_count: int,
) -> List[Dict[str, Any]]:
    args = ["rg", "--json", "--no-heading"]
    if case_insensitive:
        args.append("-i")
    if context:
        args.extend(["-C", str(context)])
    if file_type:
        args.extend(["-t", file_type])
    if glob:
        args.extend(["-g", glob])
    args.extend(["-m", str(max_count), pattern, path])

    proc = subprocess.run(args, capture_output=True, text=True, timeout=60)
    if proc.returncode not in (0, 1):  # 1 = no matches; not an error
        raise RuntimeError(proc.stderr or f"rg failed (rc={proc.returncode})")

    matches: List[Dict[str, Any]] = []
    for line in proc.stdout.splitlines():
        if not line.strip():
            continue
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        if ev.get("type") != "match":
            continue
        d = ev.get("data", {})
        text = d.get("lines", {}).get("text", "")
        matches.append(
            {
                "path": d.get("path", {}).get("text", ""),
                "line": d.get("line_number"),
                "text": text.rstrip("\n"),
            }
        )
        if len(matches) >= max_count:
            break
    return matches


def _python_fallback(
    pattern: str,
    path: str,
    glob: str,
    case_insensitive: bool,
    max_count: int,
) -> List[Dict[str, Any]]:
    flags = re.IGNORECASE if case_insensitive else 0
    rx = re.compile(pattern, flags)
    out: List[Dict[str, Any]] = []
    for root, _dirs, files in os.walk(path):
        if any(seg in root for seg in _NOISE_DIRS):
            continue
        for fname in files:
            if glob and not fnmatch(fname, glob):
                continue
            full = os.path.join(root, fname)
            try:
                with open(full, "r", encoding="utf-8", errors="ignore") as fh:
                    for i, line in enumerate(fh, 1):
                        if rx.search(line):
                            out.append(
                                {"path": full, "line": i, "text": line.rstrip("\n")}
                            )
                            if len(out) >= max_count:
                                return out
            except OSError:
                continue
    return out


def run_code_search(params: Dict[str, Any]) -> Dict[str, Any]:
    pattern = params.get("pattern")
    if not pattern:
        return {"status": "error", "error": "'pattern' is required"}
    path = str(params.get("path", "."))
    file_type = str(params.get("file_type", ""))
    glob = str(params.get("glob", ""))
    try:
        context = int(params.get("context", 0))
        max_count = int(params.get("max_count", 500))
    except (TypeError, ValueError):
        context, max_count = 0, 500
    case_insensitive = bool(params.get("case_insensitive", False))

    try:
        if _have_rg():
            matches = _rg_search(
                str(pattern),
                path,
                file_type,
                glob,
                context,
                case_insensitive,
                max_count,
            )
            tool = "ripgrep"
        else:
            matches = _python_fallback(
                str(pattern), path, glob, case_insensitive, max_count
            )
            tool = "python-fallback"
    except Exception as exc:
        return {"status": "error", "error": str(exc)}

    return {
        "status": "success",
        "pattern": pattern,
        "tool": tool,
        "matches": matches,
        "count": len(matches),
    }
