"""write_file — write text to a file (creates parent dirs as needed).

The architectural lint that used to gate each individual write was
moved to the end of every chat turn (see
:func:`Core.harness.turn._run_end_of_turn_architectural_check`).
Rationale: with 19+ rules and Colossus-1 backend latency, paying the
linter per-write made multi-write turns crawl when only the final
state matters.  The tool now just writes and records the path on the
active turn; one ``wylde_check`` pass at end-of-turn covers every
file the turn touched.

Outside a turn (tests calling the tool directly), the recording
helper silently no-ops — the file write itself is unaffected.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any, Dict


def _record_for_end_of_turn(path_str: str) -> None:
    """Best-effort: tell the active turn this path was written so the
    end-of-turn architectural sweep covers it.

    Production uses the canonical ``Core.harness.turn`` import.  In
    test environments where ``sys.path`` contains both Wylde's parent
    and Wylde itself, the same module file can also be loaded under a
    second name with its own ``threading.local`` tool context — we
    walk ``sys.modules`` and invoke ``record_file_written`` on every
    loaded copy.  Non-active modules see ``current_tool_context() is
    None`` and silently no-op, so calling all of them is safe and the
    right one always wins.
    """
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


def run_write_file(params: Dict[str, Any]) -> Dict[str, Any]:
    path_str = params.get("path")
    content = params.get("content")
    if not path_str:
        return {"status": "error", "error": "'path' is required"}
    if content is None:
        return {"status": "error", "error": "'content' is required"}

    path = Path(str(path_str))
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(str(content), encoding="utf-8")
    except OSError as exc:
        return {"status": "error", "error": str(exc)}

    _record_for_end_of_turn(str(path))
    return {
        "status": "success",
        "path": str(path),
        "bytes_written": len(str(content).encode("utf-8")),
    }
