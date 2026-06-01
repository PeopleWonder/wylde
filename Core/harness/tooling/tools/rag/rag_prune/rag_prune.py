"""rag_prune — delete memories matching the given filters.

Pulled forward from ``_legacy/core/wylde-rag/tools/memory_prune.py``. The
legacy tool dispatched to ``..memory.prune`` inside the wylde-rag service.
The new equivalent lives in :func:`Wylde.Core.harness.memory.vector_store.prune_rows`,
which has the same filter shape (``before_ts``, ``memory_type``,
``score_lt``) and the same "at least one filter required" guard.

Safety: requires ``confirm=true`` to actually run. Without confirm, the
tool reports what would be matched and stops — letting an LLM dry-run a
prune before committing.

Note: the tool name was ``memory_prune`` in the legacy world but is
``rag_prune`` here per the Phase 6 plan (cleaner namespace under tools/rag/).
"""

from __future__ import annotations

from typing import Any, Dict, Optional

from .....memory import vector_store as _vs


def run_rag_prune(params: Dict[str, Any]) -> Dict[str, Any]:
    confirm = bool(params.get("confirm", False))

    before_ts: Optional[float] = None
    if params.get("before_ts") is not None:
        try:
            before_ts = float(params["before_ts"])
        except (TypeError, ValueError):
            return {"status": "error", "error": "'before_ts' must be a number"}

    memory_type = params.get("memory_type") or ""
    memory_type = str(memory_type).strip() or None

    score_lt: Optional[float] = None
    if params.get("score_lt") is not None:
        try:
            score_lt = float(params["score_lt"])
        except (TypeError, ValueError):
            return {"status": "error", "error": "'score_lt' must be a number"}

    if before_ts is None and memory_type is None and score_lt is None:
        return {
            "status": "error",
            "error": "at least one filter required: before_ts, memory_type, or score_lt",
        }

    try:
        max_delete = max(1, min(10000, int(params.get("max_delete", 500))))
    except (TypeError, ValueError):
        max_delete = 500

    if not confirm:
        # Dry-run: count candidates without deleting. Mirrors the legacy
        # behaviour where confirm=false short-circuited before the delete.
        candidates = _vs.list_rows(
            memory_type=memory_type,
            score_lt=score_lt,
            limit=max_delete,
        )
        if before_ts is not None:
            candidates = [r for r in candidates if r.get("created_at", 0) < before_ts]
        return {
            "status": "dry_run",
            "would_delete": len(candidates),
            "filters": {
                "before_ts": before_ts,
                "memory_type": memory_type,
                "score_lt": score_lt,
            },
            "max_delete": max_delete,
            "note": "Set confirm=true to actually delete.",
        }

    try:
        result = _vs.prune_rows(
            before_ts=before_ts,
            memory_type=memory_type,
            score_lt=score_lt,
            max_delete=max_delete,
        )
    except Exception as exc:
        return {"status": "error", "error": f"{type(exc).__name__}: {exc}"}

    if isinstance(result, dict) and "error" in result:
        return {"status": "error", "error": result["error"]}

    return {
        "status": "ok",
        "deleted": result.get("deleted", 0) if isinstance(result, dict) else 0,
        "ids": result.get("ids", []) if isinstance(result, dict) else [],
        "filters": {
            "before_ts": before_ts,
            "memory_type": memory_type,
            "score_lt": score_lt,
        },
    }
