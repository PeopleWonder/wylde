"""rag_chunk_usage — per-chunk retrieval counts.

Pulled forward from ``_legacy/core/wylde-rag/tools/chunk_usage.py``. Now
wired through :mod:`Wylde.Core.harness.memory.miss_log`. The new layer
tracks retrieval frequency only (not the legacy ``cite_count`` vs
``retrieve_count`` split), so the ``dead_only`` flag is preserved on
the surface but no longer filters — every row reflects a chunk that
``record_chunk_use`` saw at least once. Documented here so the planner
notices when a richer signal would help.
"""

from __future__ import annotations

from typing import Any, Dict

from Core.harness.memory import miss_log


def run_rag_chunk_usage(params: Dict[str, Any]) -> Dict[str, Any]:
    dead_only = bool(params.get("dead_only", False))
    try:
        limit = max(1, min(10000, int(params.get("limit", 100))))
    except (TypeError, ValueError):
        limit = 100

    rows = miss_log.chunk_usage(top=limit)

    return {
        "validated": {"dead_only": dead_only, "limit": limit},
        "rows": rows,
        "count": len(rows),
    }
