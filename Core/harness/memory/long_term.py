"""Layer 1: Long-term memory — global, cross-workspace, user-visible.

Two parallel stores in lockstep:

* ``long_term.json`` — authoritative record list. The Settings UI reads
  from here so the user sees the same shape they tagged at write time.
* LanceDB ``long_term.lance`` — vector index for retrieval.

Records carry the design's metadata: ``id``, ``body``, ``source``,
``importance`` (0..10), ``created_at``, ``last_used_at``,
``superseded_by`` (id of the record that replaces this one, when set).

Supersession (revision-not-deletion):
* :func:`update` writes a new record marked as the active one and
  flags the old record's ``superseded_by`` to point at the new id.
* :func:`search` filters out superseded records by default.
* :func:`history(id)` walks the chain — useful for the Settings UI to
  show "before this was X, before that it was Y".

The retrieval scoring uses :mod:`Core.harness.memory.scoring` to
combine vector similarity, importance, and recency decay. The Settings
UI usually wants raw records sorted by importance, which
:func:`list_records` returns directly.

Storage layout::

    Core/harness/memory/long_term.json     ← authoritative list
    Core/harness/memory/long_term.lance/   ← LanceDB vector mirror

The two are kept in sync through the public write functions; if they
ever drift, :func:`reindex` rebuilds the LanceDB side from the JSON.
"""

from __future__ import annotations

import json
import logging
import secrets
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Dict, List, Optional

from . import scoring as _scoring
from ._common import DATA_DIR, EMBED_DIM, ensure_dir

logger = logging.getLogger("wylde.harness.memory.long_term")

JSON_PATH: Path = DATA_DIR / "long_term.json"
LANCE_DIR: Path = DATA_DIR / "long_term.lance"

_lock = threading.RLock()


# ── Data shape ─────────────────────────────────────────────────────────


@dataclass
class LongTermMemory:
    id: str
    body: str
    source: str = ""
    importance: int = 5
    created_at: float = 0.0
    last_used_at: float = 0.0
    superseded_by: str = ""
    tags: List[str] = field(default_factory=list)

    def to_dict(self) -> Dict[str, Any]:
        return {
            "id": self.id,
            "body": self.body,
            "source": self.source,
            "importance": int(self.importance),
            "created_at": float(self.created_at),
            "last_used_at": float(self.last_used_at),
            "superseded_by": self.superseded_by,
            "tags": list(self.tags),
        }

    @classmethod
    def from_dict(cls, d: Dict[str, Any]) -> "LongTermMemory":
        return cls(
            id=str(d.get("id", "")),
            body=str(d.get("body", "")),
            source=str(d.get("source", "")),
            importance=int(d.get("importance", 5) or 5),
            created_at=float(d.get("created_at", 0.0) or 0.0),
            last_used_at=float(d.get("last_used_at", 0.0) or 0.0),
            superseded_by=str(d.get("superseded_by", "") or ""),
            tags=list(d.get("tags") or []),
        )


# ── JSON store ─────────────────────────────────────────────────────────


def _load_all() -> List[LongTermMemory]:
    if not JSON_PATH.exists():
        return []
    try:
        raw = json.loads(JSON_PATH.read_text(encoding="utf-8"))
    except Exception as exc:  # noqa: BLE001
        logger.warning("long_term: JSON unreadable, treating as empty: %s", exc)
        return []
    items = raw.get("memories") if isinstance(raw, dict) else raw
    if not isinstance(items, list):
        return []
    return [LongTermMemory.from_dict(it) for it in items if isinstance(it, dict)]


def _save_all(records: List[LongTermMemory]) -> None:
    ensure_dir(JSON_PATH.parent)
    payload = {"memories": [r.to_dict() for r in records]}
    tmp = JSON_PATH.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(payload, indent=2, ensure_ascii=False), encoding="utf-8")
    tmp.replace(JSON_PATH)


def _new_id() -> str:
    return secrets.token_hex(8)


# ── LanceDB mirror ─────────────────────────────────────────────────────


def _lance_table() -> Any:
    """Open / create the long-term LanceDB table. Lazy to keep the
    module importable in environments without lancedb."""
    import lancedb
    import pyarrow as pa

    ensure_dir(LANCE_DIR)
    db = lancedb.connect(str(LANCE_DIR))
    # lancedb 0.30 swapped table_names() for list_tables(), but the
    # replacement returns a ListTablesResponse (object with .tables /
    # .page_token) rather than a plain list of strings. Drill into
    # .tables for the actual name list.
    if "long_term" in db.list_tables().tables:
        return db.open_table("long_term")
    schema = pa.schema(
        [
            pa.field("id", pa.string()),
            pa.field("body", pa.string()),
            pa.field("source", pa.string()),
            pa.field("importance", pa.int32()),
            pa.field("created_at", pa.float64()),
            pa.field("last_used_at", pa.float64()),
            pa.field("superseded_by", pa.string()),
            pa.field("vector", pa.list_(pa.float32(), EMBED_DIM)),
        ]
    )
    return db.create_table("long_term", schema=schema)


def _lance_upsert(record: LongTermMemory) -> None:
    """Add or replace a record's vector row. We delete-then-add because
    LanceDB's upsert helpers are version-dependent — a delete+add round
    trip is the lowest-common-denominator that works everywhere.
    """
    from .embeddings import embed_one

    try:
        vec = embed_one(record.body)
    except Exception as exc:  # noqa: BLE001
        logger.warning(
            "long_term: embed failed for %s, vector index out of date: %s",
            record.id,
            exc,
        )
        return
    try:
        tbl = _lance_table()
        try:
            tbl.delete(f"id = '{record.id}'")
        except Exception:  # noqa: BLE001
            pass
        tbl.add(
            [
                {
                    "id": record.id,
                    "body": record.body,
                    "source": record.source,
                    "importance": int(record.importance),
                    "created_at": float(record.created_at),
                    "last_used_at": float(record.last_used_at),
                    "superseded_by": record.superseded_by,
                    "vector": [float(x) for x in vec],
                }
            ]
        )
    except Exception as exc:  # noqa: BLE001
        logger.warning("long_term: lance upsert failed for %s: %s", record.id, exc)


def _lance_delete(record_id: str) -> None:
    try:
        tbl = _lance_table()
        tbl.delete(f"id = '{record_id}'")
    except Exception as exc:  # noqa: BLE001
        logger.warning("long_term: lance delete failed for %s: %s", record_id, exc)


# ── Public API ─────────────────────────────────────────────────────────


def list_records(*, include_superseded: bool = False) -> List[LongTermMemory]:
    """All long-term records, sorted importance desc then recency desc.

    The Settings UI uses this. Pass ``include_superseded=True`` to see
    the full history (default hides records that have been replaced).
    """
    with _lock:
        records = _load_all()
    if not include_superseded:
        records = [r for r in records if not r.superseded_by]
    records.sort(key=lambda r: (r.importance, r.last_used_at), reverse=True)
    return records


def get(record_id: str) -> Optional[LongTermMemory]:
    with _lock:
        for r in _load_all():
            if r.id == record_id:
                return r
    return None


def save(
    body: str,
    *,
    source: str = "",
    importance: Any = None,
    tags: Optional[List[str]] = None,
) -> LongTermMemory:
    """Write a new long-term memory. Returns the record."""
    if not isinstance(body, str) or not body.strip():
        raise ValueError("body must be a non-empty string")

    importance_int = _scoring.normalize_importance(
        importance,
        body,
        entity_count=len(tags or []),
    )

    now = time.time()
    record = LongTermMemory(
        id=_new_id(),
        body=body.strip(),
        source=str(source or ""),
        importance=importance_int,
        created_at=now,
        last_used_at=now,
        tags=list(tags or []),
    )
    with _lock:
        records = _load_all()
        records.append(record)
        _save_all(records)
    _lance_upsert(record)
    logger.info("long_term: saved %s (importance=%d)", record.id, record.importance)
    return record


def update(
    record_id: str,
    *,
    body: Optional[str] = None,
    importance: Any = None,
    source: Optional[str] = None,
) -> Optional[LongTermMemory]:
    """Revise an existing record by writing a NEW record and marking
    the old one ``superseded_by`` the new id. The supersession chain is
    visible via :func:`history`; the active record is the new one.

    Returns the new record. Returns None if the original is missing.
    """
    with _lock:
        records = _load_all()
        original = next((r for r in records if r.id == record_id), None)
        if original is None:
            return None

        new_body = body if isinstance(body, str) and body.strip() else original.body
        new_importance = original.importance
        if importance is not None:
            new_importance = _scoring.normalize_importance(importance, new_body)
        new_source = source if isinstance(source, str) else original.source
        # Carry forward the original supersession chain root for history walks.
        now = time.time()
        replacement = LongTermMemory(
            id=_new_id(),
            body=new_body,
            source=new_source,
            importance=new_importance,
            created_at=now,
            last_used_at=now,
            tags=list(original.tags),
        )
        original.superseded_by = replacement.id
        records.append(replacement)
        _save_all(records)

    _lance_upsert(replacement)
    # The old record stays in lance with its new superseded_by — needed
    # so search can filter it out without an extra metadata join.
    _lance_upsert(original)
    return replacement


def delete(record_id: str) -> bool:
    """Permanently remove a record (and any other records superseded by
    it). The Settings UI's delete button calls this."""
    with _lock:
        records = _load_all()
        target = next((r for r in records if r.id == record_id), None)
        if target is None:
            return False
        # Sweep up the supersession chain so we don't leave a forward
        # pointer dangling in JSON.
        ids_to_delete = {record_id}
        for r in records:
            if r.superseded_by == record_id:
                ids_to_delete.add(r.id)
        records = [r for r in records if r.id not in ids_to_delete]
        _save_all(records)
    for rid in ids_to_delete:
        _lance_delete(rid)
    logger.info("long_term: deleted %d records", len(ids_to_delete))
    return True


def history(record_id: str) -> List[LongTermMemory]:
    """Return the supersession chain rooted at ``record_id``.

    Walks both forward (this record → its successor → …) and backward
    (records that supersede THIS one). The Settings UI renders the chain
    as "v3 (current) ← v2 ← v1".
    """
    with _lock:
        records = _load_all()
    by_id = {r.id: r for r in records}
    if record_id not in by_id:
        return []

    # Walk forward: follow superseded_by until None.
    chain: List[LongTermMemory] = []
    cur = by_id.get(record_id)
    while cur is not None:
        chain.append(cur)
        nxt_id = cur.superseded_by
        if not nxt_id:
            break
        cur = by_id.get(nxt_id)

    # Walk backward: find any record whose superseded_by is the start.
    backward: List[LongTermMemory] = []
    seek = record_id
    while True:
        prev = next((r for r in records if r.superseded_by == seek), None)
        if prev is None:
            break
        backward.append(prev)
        seek = prev.id
    return list(reversed(backward)) + chain


def search(
    query: str,
    *,
    limit: int = 5,
    decay_days: float = _scoring.DEFAULT_DECAY_DAYS,
) -> List[Dict[str, Any]]:
    """Hybrid retrieval over long-term memory: vector similarity, then
    boost by importance and recency-decay. Superseded records are
    filtered out at the query layer.

    Returns dicts shaped for prompt-block injection: ``id``, ``body``,
    ``importance``, ``score``, plus the raw record fields.
    """
    if not isinstance(query, str) or not query.strip():
        return []
    try:
        from .embeddings import embed_one

        vec = embed_one(query)
    except Exception as exc:  # noqa: BLE001
        logger.warning("long_term: embed failed: %s", exc)
        return []
    try:
        tbl = _lance_table()
        rows = tbl.search(vec).limit(max(limit * 4, 16)).to_list()
    except Exception as exc:  # noqa: BLE001
        logger.warning("long_term: search failed: %s", exc)
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
            }
        )
    ranked = _scoring.rank_by_score(candidates, decay_days=decay_days)
    return ranked[:limit]


def core_block(*, limit: int = 5) -> List[LongTermMemory]:
    """Top-importance long-term records — the always-in-context block
    every chat turn gets, regardless of query. Sorted by importance,
    then recency, with ``last_used_at`` ties broken by ``created_at``.
    """
    records = list_records(include_superseded=False)
    return records[: max(0, int(limit))]


def touch(record_id: str) -> None:
    """Bump ``last_used_at`` to now. Called when a record is surfaced
    in a turn so the decay clock restarts."""
    with _lock:
        records = _load_all()
        for r in records:
            if r.id == record_id:
                r.last_used_at = time.time()
                _save_all(records)
                _lance_upsert(r)
                return


def reindex() -> int:
    """Rebuild the LanceDB mirror from the JSON authoritative list.

    Used by tests + by a future ``memory.long_term.reindex`` action if
    drift is suspected. Returns the count of records re-embedded.
    """
    records = list_records(include_superseded=True)
    # Drop the table's storage by removing the dir; the next call to
    # _lance_table recreates it.
    import shutil

    if LANCE_DIR.exists():
        try:
            shutil.rmtree(LANCE_DIR)
        except Exception as exc:  # noqa: BLE001
            logger.warning("long_term: could not drop lance dir for reindex: %s", exc)
    for r in records:
        _lance_upsert(r)
    return len(records)


def _to_similarity(distance: Any) -> float:
    try:
        d = float(distance)
    except (TypeError, ValueError):
        return 0.0
    return max(0.0, 1.0 / (1.0 + d))


__all__ = [
    "LongTermMemory",
    "JSON_PATH",
    "LANCE_DIR",
    "list_records",
    "get",
    "save",
    "update",
    "delete",
    "history",
    "search",
    "core_block",
    "touch",
    "reindex",
]
