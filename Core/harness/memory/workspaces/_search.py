"""File-index vector search.

Returns chunks with path, content, and similarity. Callers (notably
``retrieval.py``) layer BM25 / rerank on top of these raw vector
hits.
"""

from __future__ import annotations

import logging
from typing import Any, Dict, List

from ._index import _files_table

logger = logging.getLogger("wylde.harness.memory.workspaces")


def search_files(
    workspace_id: str, query: str, *, limit: int = 8
) -> List[Dict[str, Any]]:
    """Vector search over the workspace's file index. Returns chunks
    with their path, content, and similarity. Caller layers BM25 /
    rerank on top — see ``retrieval.py``.
    """
    if not query:
        return []
    try:
        tbl = _files_table(workspace_id)
    except Exception as exc:  # noqa: BLE001
        logger.warning(
            "workspaces: search_files open failed for %s: %s", workspace_id, exc
        )
        return []
    from ..embeddings import embed_one

    try:
        vec = embed_one(query)
    except Exception as exc:  # noqa: BLE001
        logger.warning("workspaces: embed_one failed for query: %s", exc)
        return []
    try:
        rows = tbl.search(vec).limit(limit).to_list()
    except Exception as exc:  # noqa: BLE001
        logger.warning("workspaces: vector search failed: %s", exc)
        return []
    out = []
    for r in rows:
        out.append(
            {
                "id": r.get("id"),
                "path": r.get("path"),
                "chunk_idx": r.get("chunk_idx"),
                "content": r.get("content"),
                "mtime": r.get("mtime"),
                "similarity": _from_distance(r.get("_distance")),
            }
        )
    return out


def _from_distance(distance: Any) -> float:
    """LanceDB returns ``_distance`` for L2 / cosine. Smaller is closer.
    Convert to a 0..1 similarity-ish score for downstream callers that
    expect bigger=better. Crude but consistent across the layer."""
    try:
        d = float(distance)
    except (TypeError, ValueError):
        return 0.0
    return max(0.0, 1.0 / (1.0 + d))
