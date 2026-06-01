"""rag_misses — list recent queries where retrieval missed.

Pulled forward from ``_legacy/core/wylde-rag/tools/misses.py``. Now wired
through :mod:`Wylde.Core.harness.memory.miss_log`. The new layer only
stores miss events (the legacy SQLite table held every query and used a
``gate_triggered`` flag), so ``only_gated`` is effectively always true
in practice — kept on the surface for backwards compatibility. The
``include_trace`` flag controls whether each row's freeform ``context``
blob is returned along with the core fields.
"""

from __future__ import annotations

from typing import Any, Dict

from Core.harness.memory import miss_log


def run_rag_misses(params: Dict[str, Any]) -> Dict[str, Any]:
    try:
        limit = max(1, min(1000, int(params.get("limit", 100))))
    except (TypeError, ValueError):
        limit = 100

    only_gated = bool(params.get("only_gated", True))
    include_trace = bool(params.get("include_trace", False))

    rows = miss_log.list_misses(limit=limit)
    if not include_trace:
        rows = [{k: v for k, v in r.items() if k != "context"} for r in rows]

    return {
        "validated": {
            "limit": limit,
            "only_gated": only_gated,
            "include_trace": include_trace,
        },
        "misses": rows,
        "count": len(rows),
        "summary": {"returned": len(rows)},
    }
