"""Vector-similarity search over the workspace-memory LanceDB mirror.

Returns scored candidate dicts ready for the retrieval layer's hybrid
rerank. Superseded records are filtered out so the default search
surface never returns soft-deleted entries — callers wanting history
walk the JSON store via :func:`list_records(include_superseded=True)`.
"""

from __future__ import annotations

import logging
from typing import Any, Dict, List

from .. import scoring as _scoring
from ._store import _lance_table

logger = logging.getLogger("wylde.harness.memory.workspace")


def search(
    workspace_id: str,
    query: str,
    *,
    limit: int = 5,
    decay_days: float = _scoring.DEFAULT_DECAY_DAYS,
) -> List[Dict[str, Any]]:
    if not isinstance(query, str) or not query.strip():
        return []
    try:
        from ..embeddings import embed_one

        vec = embed_one(query)
    except Exception as exc:  # noqa: BLE001
        logger.warning("workspace_memory: embed failed: %s", exc)
        return []
    try:
        tbl = _lance_table(workspace_id)
        rows = tbl.search(vec).limit(max(limit * 4, 16)).to_list()
    except Exception as exc:  # noqa: BLE001
        logger.warning("workspace_memory: search failed for %s: %s", workspace_id, exc)
        return []

    candidates = []
    for r in rows:
        if r.get("superseded_by"):
            continue
        sim = _to_similarity(r.get("_distance"))
        candidates.append(
            {
                "id": r.get("id"),
                "body": r.get("body"),
                "source": r.get("source"),
                "importance": int(r.get("importance") or 5),
                "created_at": float(r.get("created_at") or 0.0),
                "last_used_at": float(r.get("last_used_at") or 0.0),
                "similarity": sim,
                "workspace_id": workspace_id,
            }
        )
    return _scoring.rank_by_score(candidates, decay_days=decay_days)[:limit]


def _to_similarity(distance: Any) -> float:
    try:
        d = float(distance)
    except (TypeError, ValueError):
        return 0.0
    return max(0.0, 1.0 / (1.0 + d))
