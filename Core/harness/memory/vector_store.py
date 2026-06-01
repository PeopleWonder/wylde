"""Vector store wrapper (LanceDB).

Wraps LanceDB so the rest of the harness sees a backend-agnostic API. If the
project ever swaps to Qdrant / Chroma / Weaviate, the change is contained to
this file.

Schema (single ``memories`` table)::

    id          string   sha256(content + str(created_at))[:16]
    content     string   raw memory text
    memory_type string   tier or legacy type ("core" | "episodic" | ...)
    created_at  float64  unix epoch seconds
    score       float32  importance / relevance, lower = pruned first
    session_id  string   originating session, optional
    source_path string   related file path or signature JSON, optional
    vector      list<float32>(EMBED_DIM)

Public surface:

* :func:`add_row`             — persist one memory entry, return its id.
* :func:`search_vectors`      — vector search, optional type filter.
* :func:`list_rows`           — scan + filter (no vector search).
* :func:`count_rows`          — total row count.
* :func:`delete_rows`         — batch delete by id.
* :func:`prune_rows`          — conditional delete with filters.
* :func:`disk_usage_mb`       — directory size for cap-aware purge.
* :func:`consolidate_exact`   — deterministic dedup pass.
* :func:`consolidate_similar` — cosine-similarity clustering + merge.
* :func:`maintain`            — cap-purge + deterministic + similar consolidation.
"""

from __future__ import annotations

import hashlib
import time
from pathlib import Path
from typing import Any, Dict, List, Optional

from ._common import (
    DATA_DIR,
    EMBED_DIM,
    MEMORY_COLD_MAX_MB,
    MEMORY_CONSOLIDATION_SIMILARITY,
    MEMORY_CONSOLIDATION_THRESHOLD,
    ensure_dir,
    logger,
)

_MEM_TABLE = "memories"
_db_mem: Any = None
_table_mem: Any = None


# ─── Schema + connection ────────────────────────────────────────────────────


def _schema() -> Any:
    import pyarrow as pa

    return pa.schema(
        [
            pa.field("id", pa.string()),
            pa.field("content", pa.string()),
            pa.field("memory_type", pa.string()),
            pa.field("created_at", pa.float64()),
            pa.field("score", pa.float32()),
            pa.field("session_id", pa.string()),
            pa.field("source_path", pa.string()),
            pa.field("vector", pa.list_(pa.float32(), EMBED_DIM)),
        ]
    )


def _get_table() -> Any:
    global _db_mem, _table_mem
    if _table_mem is not None:
        return _table_mem
    import lancedb

    ensure_dir(DATA_DIR)
    _db_mem = lancedb.connect(str(DATA_DIR))
    if _MEM_TABLE in _db_mem.table_names():
        _table_mem = _db_mem.open_table(_MEM_TABLE)
    else:
        _table_mem = _db_mem.create_table(_MEM_TABLE, schema=_schema())
        logger.info("vector_store: created LanceDB table '%s'", _MEM_TABLE)
    return _table_mem


def _row_id(content: str, ts: float) -> str:
    return hashlib.sha256(f"{content}:{ts}".encode()).hexdigest()[:16]


def _strip_vec(rows: List[Dict[str, Any]]) -> List[Dict[str, Any]]:
    return [{k: v for k, v in r.items() if k != "vector"} for r in rows]


# ─── Write ──────────────────────────────────────────────────────────────────


def add_row(
    *,
    content: str,
    memory_type: str = "custom",
    score: float = 1.0,
    session_id: str = "",
    source_path: str = "",
    vector: Optional[List[float]] = None,
) -> str:
    """Persist a row and return its id. Caller must supply the embedding."""
    if vector is None:
        from .embeddings import embed_one  # local import keeps cold-start cheap

        vector = embed_one(content)

    ts = time.time()
    rid = _row_id(content, ts)
    tbl = _get_table()
    tbl.add(
        [
            {
                "id": rid,
                "content": content,
                "memory_type": memory_type,
                "created_at": ts,
                "score": float(score),
                "session_id": session_id or "",
                "source_path": source_path or "",
                "vector": [float(x) for x in vector],
            }
        ]
    )
    logger.debug("vector_store: saved id=%s type=%s", rid, memory_type)
    return rid


# ─── Read ───────────────────────────────────────────────────────────────────


def search_vectors(
    query_vec: List[float],
    *,
    memory_type: Optional[str] = None,
    limit: int = 10,
) -> List[Dict[str, Any]]:
    """Vector search with optional type filter. Strips the vector column out."""
    tbl = _get_table()
    try:
        q = tbl.search(query_vec, vector_column_name="vector")
        if memory_type:
            safe = memory_type.replace("'", "''")
            q = q.where(f"memory_type = '{safe}'")
        rows = q.limit(limit).to_list()
    except Exception as exc:
        logger.warning("vector_store: search error: %s", exc)
        rows = []
    return _strip_vec(rows)


def list_rows(
    *,
    memory_type: Optional[str] = None,
    since_ts: Optional[float] = None,
    score_lt: Optional[float] = None,
    limit: int = 100,
) -> List[Dict[str, Any]]:
    """Scan the table with optional filters. No vector search."""
    tbl = _get_table()
    rows = tbl.to_pandas().to_dict("records")

    if memory_type:
        rows = [r for r in rows if r.get("memory_type") == memory_type]
    if since_ts is not None:
        rows = [r for r in rows if r.get("created_at", 0) >= since_ts]
    if score_lt is not None:
        rows = [r for r in rows if float(r.get("score", 1.0)) < score_lt]

    rows.sort(key=lambda r: r.get("created_at", 0), reverse=True)
    rows = rows[:limit]
    return _strip_vec(rows)


def count_rows() -> int:
    try:
        return int(_get_table().count_rows())
    except Exception:
        return 0


# ─── Delete / prune ─────────────────────────────────────────────────────────


def delete_rows(ids: List[str]) -> int:
    """Delete rows by id. Returns count deleted."""
    if not ids:
        return 0
    tbl = _get_table()
    id_list = ", ".join(f"'{i}'" for i in ids)
    tbl.delete(f"id IN ({id_list})")
    logger.info("vector_store: deleted %d rows by id", len(ids))
    return len(ids)


def prune_rows(
    *,
    before_ts: Optional[float] = None,
    memory_type: Optional[str] = None,
    score_lt: Optional[float] = None,
    max_delete: int = 10000,
) -> Dict[str, Any]:
    """Delete rows matching ALL supplied filters. At least one filter required."""
    if before_ts is None and memory_type is None and score_lt is None:
        return {
            "error": "at least one filter required (before_ts, memory_type, or score_lt)"
        }

    rows = list_rows(memory_type=memory_type, score_lt=score_lt, limit=max_delete)
    if before_ts is not None:
        rows = [r for r in rows if r.get("created_at", 0) < before_ts]
    ids = [r["id"] for r in rows]
    delete_rows(ids)
    logger.info(
        "vector_store: pruned %d rows (type=%s before_ts=%s score_lt=%s)",
        len(ids),
        memory_type,
        before_ts,
        score_lt,
    )
    return {"deleted": len(ids), "ids": ids[:50]}


# ─── Disk usage ─────────────────────────────────────────────────────────────


def disk_usage_mb() -> float:
    """Total bytes used by the memories LanceDB table directory, in MiB."""
    mem_dir = Path(DATA_DIR) / f"{_MEM_TABLE}.lance"
    if not mem_dir.exists():
        return 0.0
    total = sum(f.stat().st_size for f in mem_dir.rglob("*") if f.is_file())
    return total / (1024 * 1024)


# ─── Consolidation ──────────────────────────────────────────────────────────


def consolidate_exact() -> Dict[str, int]:
    """Three deterministic passes:

    1. Exact-content duplicates: same ``(memory_type, content)`` — keep
       highest-score, then most recent.
    2. Source-path conflicts: same ``(memory_type, source_path)`` with
       non-empty path — keep newest.
    3. Soft-stale: ``score==0`` and older than 30 days — drop.
    """
    try:
        df = _get_table().to_pandas()
    except Exception as exc:
        logger.warning("vector_store: consolidate cannot read table: %s", exc)
        return {"exact_dupes": 0, "path_conflicts": 0, "soft_stale": 0}

    if df.empty:
        return {"exact_dupes": 0, "path_conflicts": 0, "soft_stale": 0}

    delete_ids: set[str] = set()

    df_sorted = df.sort_values(["score", "created_at"], ascending=[False, False])
    seen: dict[tuple[str, str], str] = {}
    for _, row in df_sorted.iterrows():
        key = (str(row["memory_type"]), str(row["content"]))
        if key not in seen:
            seen[key] = str(row["id"])
        else:
            delete_ids.add(str(row["id"]))
    exact_dupes = len(delete_ids)

    path_conflicts = 0
    df_paths = df_sorted[df_sorted["source_path"].astype(str).str.len() > 0]
    seen_paths: dict[tuple[str, str], str] = {}
    for _, row in df_paths.sort_values("created_at", ascending=False).iterrows():
        rid = str(row["id"])
        if rid in delete_ids:
            continue
        key = (str(row["memory_type"]), str(row["source_path"]))
        if key not in seen_paths:
            seen_paths[key] = rid
        else:
            delete_ids.add(rid)
            path_conflicts += 1

    cutoff = time.time() - 30 * 24 * 3600
    soft_stale = 0
    for _, row in df.iterrows():
        rid = str(row["id"])
        if rid in delete_ids:
            continue
        if (
            float(row.get("score", 1.0)) <= 0.0
            and float(row.get("created_at", 0)) < cutoff
        ):
            delete_ids.add(rid)
            soft_stale += 1

    if delete_ids:
        delete_rows(list(delete_ids))
        logger.info(
            "vector_store: consolidate dropped %d (exact=%d, path=%d, stale=%d)",
            len(delete_ids),
            exact_dupes,
            path_conflicts,
            soft_stale,
        )

    return {
        "exact_dupes": exact_dupes,
        "path_conflicts": path_conflicts,
        "soft_stale": soft_stale,
    }


def consolidate_similar(
    threshold: float = MEMORY_CONSOLIDATION_SIMILARITY,
) -> Dict[str, Any]:
    """Cluster near-duplicates by cosine similarity and merge each cluster.

    For each cluster of >1 members: keep the most recently created row,
    bump its score to ``max(cluster scores)``, delete the rest.
    """
    t0 = time.time()
    try:
        import numpy as np
    except ImportError:
        logger.warning("consolidate_similar: numpy unavailable")
        return {
            "error": "numpy not available",
            "clusters_found": 0,
            "memories_merged": 0,
        }

    try:
        df = _get_table().to_pandas()
    except Exception as exc:
        return {"error": str(exc), "clusters_found": 0, "memories_merged": 0}

    if len(df) < 2:
        return {"clusters_found": 0, "memories_merged": 0, "elapsed_ms": 0}

    raw = np.array(df["vector"].tolist(), dtype=np.float32)
    norms = np.linalg.norm(raw, axis=1, keepdims=True)
    norms[norms == 0] = 1.0
    normed = raw / norms
    sim = normed @ normed.T

    n = len(df)
    parent = list(range(n))

    def _find(x: int) -> int:
        while parent[x] != x:
            parent[x] = parent[parent[x]]
            x = parent[x]
        return x

    def _union(x: int, y: int) -> None:
        px, py = _find(x), _find(y)
        if px != py:
            parent[px] = py

    pairs = np.argwhere(np.triu(sim, k=1) >= threshold)
    for i, j in pairs:
        _union(int(i), int(j))

    from collections import defaultdict

    groups: dict[int, list[int]] = defaultdict(list)
    for idx in range(n):
        groups[_find(idx)].append(idx)
    multi = {root: idxs for root, idxs in groups.items() if len(idxs) > 1}
    if not multi:
        return {
            "clusters_found": 0,
            "memories_merged": 0,
            "elapsed_ms": int((time.time() - t0) * 1000),
        }

    ids_col = df["id"].tolist()
    created_col = df["created_at"].tolist()
    score_col = df["score"].tolist()
    tbl = _get_table()
    merged = 0

    for _, idxs in multi.items():
        canonical_idx = max(idxs, key=lambda i: float(created_col[i]))
        canonical_id = str(ids_col[canonical_idx])
        others = [str(ids_col[i]) for i in idxs if i != canonical_idx]
        max_score = max(float(score_col[i]) for i in idxs)
        try:
            safe_id = canonical_id.replace("'", "''")
            tbl.update(where=f"id = '{safe_id}'", values={"score": float(max_score)})
        except Exception as exc:
            logger.debug(
                "consolidate_similar: score update failed for %s: %s", canonical_id, exc
            )
        delete_rows(others)
        merged += len(others)

    elapsed = int((time.time() - t0) * 1000)
    logger.info(
        "consolidate_similar: %d clusters, %d memories merged in %dms",
        len(multi),
        merged,
        elapsed,
    )
    return {
        "clusters_found": len(multi),
        "memories_merged": merged,
        "elapsed_ms": elapsed,
    }


def maintain() -> Dict[str, Any]:
    """Cap-aware purge + deterministic + similarity consolidation.

    LLM-driven episodic→semantic promotion is NOT here — that lives in
    :mod:`rag` because it needs the backend bridge.
    """
    cap_mb = MEMORY_COLD_MAX_MB
    used_mb = disk_usage_mb()
    purged = 0

    if used_mb > cap_mb:
        target_mb = cap_mb * 0.80
        logger.warning(
            "vector_store: %.1f MiB exceeds cap %.1f MiB — purging to %.1f MiB",
            used_mb,
            cap_mb,
            target_mb,
        )
        try:
            df = _get_table().to_pandas()
        except Exception as exc:
            return {"purged": 0, "used_mb": used_mb, "error": str(exc)}
        df = df.sort_values(["score", "created_at"], ascending=[True, True])
        total = len(df)
        est_bpr = (used_mb * 1024 * 1024) / max(total, 1)
        bytes_to_drop = max(0, used_mb - target_mb) * 1024 * 1024
        rows_to_drop = min(total, int(bytes_to_drop / est_bpr) + 1)
        ids = [str(r) for r in df["id"].head(rows_to_drop).tolist()]
        if ids:
            delete_rows(ids)
            purged = len(ids)
            logger.info("vector_store: auto-purged %d rows", purged)

    consolidated = consolidate_exact()
    similar: Dict[str, Any] = {}
    if count_rows() >= MEMORY_CONSOLIDATION_THRESHOLD:
        try:
            similar = consolidate_similar()
        except Exception as exc:
            logger.warning("consolidate_similar failed: %s", exc)
            similar = {"error": str(exc)}

    return {
        "used_mb": disk_usage_mb(),
        "cap_mb": cap_mb,
        "row_count": count_rows(),
        "purged": purged,
        "consolidated": consolidated,
        "similar_merge": similar,
    }


__all__ = [
    "add_row",
    "search_vectors",
    "list_rows",
    "count_rows",
    "delete_rows",
    "prune_rows",
    "disk_usage_mb",
    "consolidate_exact",
    "consolidate_similar",
    "maintain",
]
