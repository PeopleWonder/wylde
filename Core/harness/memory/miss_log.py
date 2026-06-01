"""Structured RAG-quality log — misses, feedback, and chunk retrieval frequency.

Pulled forward from ``_legacy/core/wylde-rag/miss_log.py``. The legacy
module ran inside a Flask service and used SQLite. The new harness needs
the same conceptual surface but cheaper plumbing: this module records
three log-shaped event streams as plain files under
``Wylde/.wylde/data/miss_log/`` (the directory comes from
:data:`._common.DATA_DIR`, matching the convention used by
:mod:`.conversation`).

What gets logged where::

    misses.jsonl       append-only — one row per failed RAG query
    feedback.jsonl     append-only — one row per user thumb up/down/comment
    chunk_usage.json   counter dict — { chunk_id: { count, first_seen, last_used } }

JSONL beats SQLite here for two reasons:

* the events are single-writer-mostly and we don't need ad-hoc SQL;
* the harness is moving toward dep-light memory modules.

If a future caller needs richer querying, the JSONL files are trivial to
load into a DataFrame or feed back into LanceDB.

Public surface:

* :func:`record_miss`       — append a miss event for a failed query.
* :func:`record_feedback`   — record +1 / 0 / -1 (and optional comment) on a prior result.
* :func:`record_chunk_use`  — bump the retrieval counter for a chunk id.
* :func:`list_misses`       — read recent miss rows (optional ``since`` filter).
* :func:`chunk_usage`       — top-N most-retrieved chunks.
"""

from __future__ import annotations

import json
import os
import secrets
import threading
import time
from pathlib import Path
from typing import Any, Dict, List, Optional

from ._common import DATA_DIR, ensure_dir, logger

# Storage layout under DATA_DIR. Kept as module-level paths so tests can
# point WYLDE_DATA_DIR at a tmp dir and the rewrite is automatic.
_DIR: Path = DATA_DIR / "miss_log"
_MISSES_PATH: Path = _DIR / "misses.jsonl"
_FEEDBACK_PATH: Path = _DIR / "feedback.jsonl"
_CHUNKS_PATH: Path = _DIR / "chunk_usage.json"

# In-process lock; this layer is single-process so a thread lock is enough.
# Cross-process writers should serialise through their own queue if needed.
_lock = threading.Lock()


def _now() -> float:
    return time.time()


def _new_id() -> str:
    """Short, sortable id for a miss row."""
    return f"{int(_now() * 1000):x}-{secrets.token_hex(3)}"


def _append_jsonl(path: Path, row: Dict[str, Any]) -> None:
    ensure_dir(path.parent)
    line = json.dumps(row, ensure_ascii=False, default=str)
    with _lock:
        with open(path, "a", encoding="utf-8") as fh:
            fh.write(line + "\n")


def _read_jsonl(path: Path) -> List[Dict[str, Any]]:
    if not path.exists():
        return []
    out: List[Dict[str, Any]] = []
    try:
        with open(path, "r", encoding="utf-8") as fh:
            for raw in fh:
                raw = raw.strip()
                if not raw:
                    continue
                try:
                    out.append(json.loads(raw))
                except ValueError as exc:
                    logger.warning(
                        "miss_log: skipping malformed row in %s: %s", path.name, exc
                    )
    except OSError as exc:
        logger.warning("miss_log: read failed for %s: %s", path, exc)
    return out


def _load_chunks() -> Dict[str, Dict[str, Any]]:
    if not _CHUNKS_PATH.exists():
        return {}
    try:
        doc = json.loads(_CHUNKS_PATH.read_text(encoding="utf-8"))
    except (OSError, ValueError) as exc:
        logger.warning("miss_log: chunk_usage unreadable, starting fresh: %s", exc)
        return {}
    return doc if isinstance(doc, dict) else {}


def _save_chunks(doc: Dict[str, Dict[str, Any]]) -> None:
    ensure_dir(_CHUNKS_PATH.parent)
    tmp = _CHUNKS_PATH.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(doc, ensure_ascii=False, indent=2), encoding="utf-8")
    os.replace(tmp, _CHUNKS_PATH)


# ── Write APIs ──────────────────────────────────────────────────────────────


def record_miss(query: str, context: Optional[Dict[str, Any]] = None) -> str:
    """Append a miss row for a failed RAG query. Returns the assigned id.

    ``context`` is freeform — typically retrieval scores, gate reason,
    candidate chunk ids, total_ms — anything useful for later triage.
    """
    if not isinstance(query, str):
        raise TypeError("query must be a string")
    row: Dict[str, Any] = {
        "id": _new_id(),
        "ts": _now(),
        "query": query,
        "context": dict(context) if isinstance(context, dict) else {},
    }
    try:
        _append_jsonl(_MISSES_PATH, row)
    except OSError as exc:
        logger.warning("miss_log.record_miss failed: %s", exc)
    row_id: str = row["id"]
    return row_id


def log_query(
    query: str,
    *,
    workspace_id: str = "",
    hits: Optional[List[Any]] = None,
    tier: Optional[str] = None,
) -> str:
    """Log a RAG query — auto-called from :func:`rag.search` so every
    retrieval gets a record on disk.

    Returns the assigned query_id; downstream tools (rag_feedback,
    rag_chunk_usage) reference the query by this id.

    The implementation routes through :func:`record_miss` because the
    JSONL row shape is the same; the ``context`` block carries
    ``hit_count`` and ``missed=True`` when hits is empty so
    :func:`list_misses` returns ONLY the empty-hit queries (matching
    the legacy "miss-only" surface the rag_misses tool expects).
    Queries that DID return hits land in the same JSONL but are
    filtered back out at read time by ``hit_count > 0``.
    """
    if not isinstance(query, str) or not query.strip():
        return ""
    chunk_ids: List[str] = []
    for h in hits or []:
        if isinstance(h, dict):
            cid = h.get("id") or h.get("chunk_id")
        else:
            cid = h
        if cid is not None:
            chunk_ids.append(str(cid))

    context: Dict[str, Any] = {
        "workspace_id": workspace_id or "",
        "hit_count": len(chunk_ids),
        "missed": len(chunk_ids) == 0,
        "hit_ids": chunk_ids[:20],  # cap so the JSONL row stays small
    }
    if tier:
        context["tier"] = str(tier)
    return record_miss(query, context=context)


def record_feedback(
    result_id: Any,
    rating: int,
    comment: Optional[str] = None,
) -> bool:
    """Append a feedback event tied to a prior result/query id.

    ``rating`` must be in ``{-1, 0, 1}``. Returns True when the event was
    written; False on disk error so callers can surface a soft failure.
    """
    try:
        rating_i = int(rating)
    except (TypeError, ValueError) as exc:
        raise ValueError("rating must be -1, 0, or 1") from exc
    if rating_i not in (-1, 0, 1):
        raise ValueError("rating must be -1, 0, or 1")

    row: Dict[str, Any] = {
        "ts": _now(),
        "result_id": result_id,
        "rating": rating_i,
    }
    if comment:
        row["comment"] = str(comment)
    try:
        _append_jsonl(_FEEDBACK_PATH, row)
        return True
    except OSError as exc:
        logger.warning("miss_log.record_feedback failed: %s", exc)
        return False


def record_chunk_use(chunk_id: str) -> None:
    """Bump the per-chunk retrieval counter. Cheap, called per retrieval."""
    if not isinstance(chunk_id, str) or not chunk_id:
        return
    now = _now()
    with _lock:
        chunks = _load_chunks()
        entry = chunks.get(chunk_id)
        if entry is None:
            chunks[chunk_id] = {"count": 1, "first_seen": now, "last_used": now}
        else:
            entry["count"] = int(entry.get("count", 0)) + 1
            entry["last_used"] = now
            entry.setdefault("first_seen", now)
        try:
            _save_chunks(chunks)
        except OSError as exc:
            logger.warning("miss_log.record_chunk_use failed: %s", exc)


# ── Read APIs ───────────────────────────────────────────────────────────────


def list_misses(
    since: Optional[float] = None,
    limit: int = 100,
) -> List[Dict[str, Any]]:
    """Return recent miss rows, newest-first.

    A row counts as a miss if ``context.missed`` is True OR if
    ``hit_count`` is 0 — both shapes appear because :func:`log_query`
    writes every query (hit or miss) but only flags miss rows.
    Queries that returned hits land in the JSONL but are filtered out
    here so the surface matches the rag_misses tool's expectations.

    ``since`` is an epoch-seconds cutoff; rows older than it are dropped.
    ``limit`` clamps the result size.
    """
    rows = _read_jsonl(_MISSES_PATH)

    # Filter to actual misses. Legacy rows (no context.missed key)
    # default to "miss" to preserve backward compat with the original
    # record_miss-only surface — anything explicitly logged via that
    # function was a miss by definition.
    def _is_miss(row: Dict[str, Any]) -> bool:
        ctx = row.get("context") or {}
        if not isinstance(ctx, dict):
            return True
        if "missed" in ctx:
            return bool(ctx.get("missed"))
        if "hit_count" in ctx:
            try:
                return int(ctx.get("hit_count") or 0) == 0
            except (TypeError, ValueError):
                return True
        return True

    rows = [r for r in rows if _is_miss(r)]
    if since is not None:
        try:
            cutoff = float(since)
            rows = [r for r in rows if float(r.get("ts", 0.0)) >= cutoff]
        except (TypeError, ValueError):
            pass
    rows.sort(key=lambda r: float(r.get("ts", 0.0)), reverse=True)
    try:
        n = max(0, int(limit))
    except (TypeError, ValueError):
        n = 100
    return rows[:n]


def chunk_usage(top: int = 20) -> List[Dict[str, Any]]:
    """Return the top-N most-retrieved chunks, descending by count."""
    chunks = _load_chunks()
    rows = [{"chunk_id": cid, **info} for cid, info in chunks.items()]
    rows.sort(key=lambda r: int(r.get("count", 0)), reverse=True)
    try:
        n = max(0, int(top))
    except (TypeError, ValueError):
        n = 20
    return rows[:n]


__all__ = [
    "record_miss",
    "log_query",
    "record_feedback",
    "record_chunk_use",
    "list_misses",
    "chunk_usage",
]
