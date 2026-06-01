"""edit_file — replace a literal substring in a file.

The architectural lint that used to gate each edit was moved to the
end of every chat turn (see
:func:`Core.harness.turn._run_end_of_turn_architectural_check`).
The tool now just applies the substitution and records the path on
the active turn; one ``wylde_check`` pass at end-of-turn covers every
file the turn touched.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any, Dict


def _record_for_end_of_turn(path_str: str) -> None:
    """Best-effort: tell the active turn this path was edited so the
    end-of-turn architectural sweep covers it.  See the matching helper
    in :mod:`...write_file.write_file` for the sys.modules walk
    rationale (same module file can load under two names in test
    environments, each with its own thread-local context)."""
    import sys as _sys

    seen: set = set()
    _rec: Any = None
    try:
        from Core.harness.turn import record_file_written as _rec_fn

        _rec = _rec_fn
    except ImportError:
        _rec = None
    if _rec is not None:
        seen.add(id(_rec))
        try:
            _rec(path_str)
        except Exception:  # noqa: BLE001
            pass

    for mod_name, mod in list(_sys.modules.items()):
        if mod is None:
            continue
        if not (mod_name == "turn" or mod_name.endswith(".harness.turn")):
            continue
        helper = getattr(mod, "record_file_written", None)
        if helper is None or id(helper) in seen:
            continue
        seen.add(id(helper))
        try:
            helper(path_str)
        except Exception:  # noqa: BLE001
            continue


def run_edit_file(params: Dict[str, Any]) -> Dict[str, Any]:
    path_str = params.get("path")
    old_text = params.get("old_text")
    new_text = params.get("new_text")
    if not path_str:
        return {"status": "error", "error": "'path' is required"}
    if old_text is None or new_text is None:
        return {"status": "error", "error": "'old_text' and 'new_text' are required"}

    path = Path(str(path_str))
    if not path.exists():
        return {
            "status": "error",
            "error": f"file not found: {path_str}",
            "code": "not_found",
        }
    try:
        content = path.read_text(encoding="utf-8")
    except OSError as exc:
        return {"status": "error", "error": str(exc)}

    occurrences = content.count(str(old_text))
    if occurrences == 0:
        return {"status": "error", "error": f"pattern not found in {path_str}"}

    new_content = content.replace(str(old_text), str(new_text))

    try:
        path.write_text(new_content, encoding="utf-8")
    except OSError as exc:
        return {"status": "error", "error": str(exc)}

    _record_for_end_of_turn(str(path))
    return {
        "status": "success",
        "path": str(path),
        "replacements": occurrences,
    }
