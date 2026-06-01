"""apply_patch — apply a unified-diff patch to a file.

Pure-Python implementation, ported from the legacy diff_tools.py. Supports
multi-hunk patches; verifies that deletions actually match the source so a
mis-aligned patch fails loudly instead of corrupting the file.
"""

from __future__ import annotations

import os
import re
from pathlib import Path
from typing import Any, Dict, List, Tuple

_HUNK_RE = re.compile(r"^@@\s+-(\d+)(?:,(\d+))?\s+\+(\d+)(?:,(\d+))?\s+@@")


def _read_lines(path: str) -> List[str]:
    with open(path, "r", encoding="utf-8") as fh:
        return fh.readlines()


def _apply_hunks(original: List[str], patch: str) -> Tuple[List[str], int]:
    result = list(original)
    offset = 0
    lines = patch.splitlines(keepends=True)
    i = 0
    hunks = 0
    while i < len(lines):
        m = _HUNK_RE.match(lines[i])
        if not m:
            i += 1
            continue
        old_start = int(m.group(1))
        old_count = int(m.group(2) or 1)
        cursor = old_start - 1 + offset
        i += 1
        new_block: List[str] = []
        consumed_old = 0
        while (
            i < len(lines)
            and not _HUNK_RE.match(lines[i])
            and not lines[i].startswith("--- ")
            and not lines[i].startswith("+++ ")
        ):
            patch_line = lines[i]
            if patch_line.startswith("+"):
                new_block.append(patch_line[1:])
            elif patch_line.startswith("-"):
                src = (
                    result[cursor + consumed_old]
                    if cursor + consumed_old < len(result)
                    else None
                )
                if src is None or src.rstrip("\n") != patch_line[1:].rstrip("\n"):
                    raise ValueError(
                        f"hunk mismatch at line {cursor + consumed_old + 1}: "
                        f"expected {patch_line[1:]!r}, got {src!r}"
                    )
                consumed_old += 1
            elif patch_line.startswith(" "):
                new_block.append(patch_line[1:])
                consumed_old += 1
            elif patch_line.startswith("\\"):
                pass  # "\ No newline at end of file"
            else:
                new_block.append(patch_line)
            i += 1
        _ = old_count  # kept for spec parity; tolerant on mismatch
        result[cursor : cursor + consumed_old] = new_block
        offset += len(new_block) - consumed_old
        hunks += 1
    return result, hunks


def run_apply_patch(params: Dict[str, Any]) -> Dict[str, Any]:
    path = params.get("path")
    patch = params.get("patch")
    if not path or not patch:
        return {"status": "error", "error": "'path' and 'patch' are required"}
    if not os.path.isfile(str(path)):
        return {
            "status": "error",
            "error": f"file not found: {path}",
            "code": "not_found",
        }

    try:
        original = _read_lines(str(path))
    except OSError as exc:
        return {"status": "error", "error": str(exc)}

    try:
        new_lines, hunks = _apply_hunks(original, str(patch))
    except ValueError as exc:
        return {"status": "error", "error": str(exc)}

    if bool(params.get("dry_run", False)):
        return {
            "status": "success",
            "hunks_applied": hunks,
            "dry_run": True,
            "preview_bytes": sum(len(line) for line in new_lines),
        }

    try:
        Path(str(path)).write_text("".join(new_lines), encoding="utf-8")
    except OSError as exc:
        return {"status": "error", "error": str(exc)}
    return {"status": "success", "hunks_applied": hunks, "path": str(path)}
