"""Storage layer for workspace memory: paths, JSON store, LanceDB mirror, CRUD.

Two parallel stores per workspace:

* JSON file at ``workspace_memories/<slug>/memory.json`` — the source
  of truth, holds the full record history including soft-deleted /
  superseded entries for audit walks.
* LanceDB table at ``workspace_memories/<slug>/memory.lance/`` —
  vector mirror used by :func:`Core.harness.memory.workspace_memory.search`.

Writes go through :func:`save` / :func:`update` / :func:`delete` and
keep both stores in sync. The Memgraph entity edges are written
best-effort via :func:`_record_entities`, which lazy-imports
:mod:`Core.harness.memory.memgraph` — graph writes never block the
save path.
"""

from __future__ import annotations

import json
import logging
import secrets
import shutil
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Dict, List, Optional

from .. import scoring as _scoring
from .._common import DATA_DIR, EMBED_DIM, ensure_dir

logger = logging.getLogger("wylde.harness.memory.workspace")

_lock = threading.RLock()


# ── Storage paths (durable — outside the index folder) ────────────────


WORKSPACE_MEMORIES_DIR: Path = DATA_DIR / "workspace_memories"


def _memory_dir(workspace_id: str) -> Path:
    """Per-workspace durable memory directory. NOT under ``indexes/``,
    so MRU eviction of the file index doesn't take this with it."""
    return ensure_dir(WORKSPACE_MEMORIES_DIR / workspace_id)


def delete_memory_dir(workspace_id: str) -> bool:
    """Recursively remove the durable workspace memory folder.

    Invoked on explicit user delete of a workspace. (The former Python
    caller ``workspaces.delete_workspace`` was removed in the
    config-file-backed redesign — 2026-06-05; Rust now owns the
    workspace registry.) MRU eviction must NOT call this.
    Returns True if a folder was removed.
    """
    target = WORKSPACE_MEMORIES_DIR / workspace_id
    if not target.exists():
        return False
    try:
        shutil.rmtree(target)
        return True
    except Exception as exc:  # noqa: BLE001
        logger.warning(
            "workspace_memory: failed to delete durable memory dir %s: %s",
            target,
            exc,
        )
        return False


@dataclass
class WorkspaceMemory:
    id: str
    workspace_id: str
    body: str
    source: str = ""
    importance: int = 5
    created_at: float = 0.0
    last_used_at: float = 0.0
    superseded_by: str = ""
    entities: List[str] = field(default_factory=list)

    def to_dict(self) -> Dict[str, Any]:
        return {
            "id": self.id,
            "workspace_id": self.workspace_id,
            "body": self.body,
            "source": self.source,
            "importance": int(self.importance),
            "created_at": float(self.created_at),
            "last_used_at": float(self.last_used_at),
            "superseded_by": self.superseded_by,
            "entities": list(self.entities),
        }

    @classmethod
    def from_dict(cls, d: Dict[str, Any]) -> "WorkspaceMemory":
        return cls(
            id=str(d.get("id", "")),
            workspace_id=str(d.get("workspace_id", "")),
            body=str(d.get("body", "")),
            source=str(d.get("source", "")),
            importance=int(d.get("importance", 5) or 5),
            created_at=float(d.get("created_at", 0.0) or 0.0),
            last_used_at=float(d.get("last_used_at", 0.0) or 0.0),
            superseded_by=str(d.get("superseded_by", "") or ""),
            entities=list(d.get("entities") or []),
        )


# ── Storage paths ──────────────────────────────────────────────────────


def _json_path(workspace_id: str) -> Path:
    return _memory_dir(workspace_id) / "memory.json"


def _lance_dir(workspace_id: str) -> Path:
    return ensure_dir(_memory_dir(workspace_id) / "memory.lance")


# ── JSON store ─────────────────────────────────────────────────────────


def _load(workspace_id: str) -> List[WorkspaceMemory]:
    path = _json_path(workspace_id)
    if not path.exists():
        return []
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
    except Exception as exc:  # noqa: BLE001
        logger.warning("workspace_memory: %s JSON unreadable: %s", workspace_id, exc)
        return []
    items = raw.get("memories") if isinstance(raw, dict) else raw
    if not isinstance(items, list):
        return []
    return [WorkspaceMemory.from_dict(it) for it in items if isinstance(it, dict)]


def _save(workspace_id: str, records: List[WorkspaceMemory]) -> None:
    path = _json_path(workspace_id)
    ensure_dir(path.parent)
    payload = {"memories": [r.to_dict() for r in records]}
    tmp = path.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(payload, indent=2, ensure_ascii=False), encoding="utf-8")
    tmp.replace(path)


def _new_id() -> str:
    return secrets.token_hex(8)


# ── LanceDB mirror ─────────────────────────────────────────────────────


def _lance_table(workspace_id: str) -> Any:
    import lancedb
    import pyarrow as pa

    db_path = _lance_dir(workspace_id)
    db = lancedb.connect(str(db_path))
    # lancedb 0.30 list_tables() returns a ListTablesResponse object
    # (.tables list + .page_token), not a plain list of strings.
    if "workspace_memory" in db.list_tables().tables:
        return db.open_table("workspace_memory")
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
    return db.create_table("workspace_memory", schema=schema)


def _lance_upsert(record: WorkspaceMemory) -> None:
    from ..embeddings import embed_one

    try:
        vec = embed_one(record.body)
    except Exception as exc:  # noqa: BLE001
        logger.warning("workspace_memory: embed failed for %s: %s", record.id, exc)
        return
    try:
        tbl = _lance_table(record.workspace_id)
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
        logger.warning(
            "workspace_memory: lance upsert failed for %s: %s", record.id, exc
        )


def _lance_delete(workspace_id: str, record_id: str) -> None:
    try:
        tbl = _lance_table(workspace_id)
        tbl.delete(f"id = '{record_id}'")
    except Exception as exc:  # noqa: BLE001
        logger.warning(
            "workspace_memory: lance delete failed for %s: %s", record_id, exc
        )


# ── Memgraph entity edges ──────────────────────────────────────────────


def _record_entities(record: WorkspaceMemory) -> None:
    """Best-effort write of entity → memory edges into Memgraph.

    Each entity becomes (or is upserted as) a ``:Entity`` node; the
    memory chunk becomes a ``:Chunk`` node tagged with the workspace
    id; ``MENTIONED_IN`` edges connect entities to the chunk.

    If Memgraph isn't reachable the save still wins — we log the
    failure and move on. The vector mirror is enough for retrieval to
    work without the graph layer.
    """
    if not record.entities:
        return
    try:
        from .. import memgraph as _mg
    except ImportError:
        return
    try:
        _mg.upsert(
            chunks=[
                {
                    "id": record.id,
                    "path": f"workspace:{record.workspace_id}:memory",
                    "symbol": "memory",
                    "language": "memory",
                    "workspace": record.workspace_id,
                    "entities": list(record.entities),
                }
            ],
        )
    except Exception as exc:  # noqa: BLE001
        logger.debug(
            "workspace_memory: graph write skipped (memgraph unreachable): %s",
            exc,
        )


# ── Public API ─────────────────────────────────────────────────────────


def list_records(
    workspace_id: str,
    *,
    include_superseded: bool = False,
) -> List[WorkspaceMemory]:
    with _lock:
        records = _load(workspace_id)
    if not include_superseded:
        records = [r for r in records if not r.superseded_by]
    records.sort(key=lambda r: (r.importance, r.last_used_at), reverse=True)
    return records


def get(workspace_id: str, record_id: str) -> Optional[WorkspaceMemory]:
    with _lock:
        for r in _load(workspace_id):
            if r.id == record_id:
                return r
    return None


def save(
    workspace_id: str,
    body: str,
    *,
    source: str = "",
    importance: Any = None,
    entities: Optional[List[str]] = None,
) -> WorkspaceMemory:
    """Write a new workspace-scoped memory."""
    if not isinstance(body, str) or not body.strip():
        raise ValueError("body must be a non-empty string")
    if not isinstance(workspace_id, str) or not workspace_id:
        raise ValueError("workspace_id is required")

    ent_list = list(entities or [])
    importance_int = _scoring.normalize_importance(
        importance,
        body,
        entity_count=len(ent_list),
    )

    now = time.time()
    record = WorkspaceMemory(
        id=_new_id(),
        workspace_id=workspace_id,
        body=body.strip(),
        source=str(source or ""),
        importance=importance_int,
        created_at=now,
        last_used_at=now,
        entities=ent_list,
    )
    with _lock:
        records = _load(workspace_id)
        records.append(record)
        _save(workspace_id, records)
    _lance_upsert(record)
    _record_entities(record)
    logger.info(
        "workspace_memory: saved %s in %s (importance=%d, entities=%d)",
        record.id,
        workspace_id,
        record.importance,
        len(ent_list),
    )
    return record


def update(
    workspace_id: str,
    record_id: str,
    *,
    body: Optional[str] = None,
    importance: Any = None,
    entities: Optional[List[str]] = None,
) -> Optional[WorkspaceMemory]:
    """Revision-not-deletion. Writes a new record, marks the old one
    ``superseded_by`` the new id."""
    with _lock:
        records = _load(workspace_id)
        original = next((r for r in records if r.id == record_id), None)
        if original is None:
            return None

        new_body = body if isinstance(body, str) and body.strip() else original.body
        new_importance = original.importance
        if importance is not None:
            new_importance = _scoring.normalize_importance(importance, new_body)
        new_entities = (
            list(entities) if entities is not None else list(original.entities)
        )

        now = time.time()
        replacement = WorkspaceMemory(
            id=_new_id(),
            workspace_id=workspace_id,
            body=new_body,
            source=original.source,
            importance=new_importance,
            created_at=now,
            last_used_at=now,
            entities=new_entities,
        )
        original.superseded_by = replacement.id
        records.append(replacement)
        _save(workspace_id, records)

    _lance_upsert(replacement)
    _lance_upsert(original)
    _record_entities(replacement)
    return replacement


def delete(workspace_id: str, record_id: str) -> bool:
    with _lock:
        records = _load(workspace_id)
        if not any(r.id == record_id for r in records):
            return False
        ids = {record_id}
        for r in records:
            if r.superseded_by == record_id:
                ids.add(r.id)
        records = [r for r in records if r.id not in ids]
        _save(workspace_id, records)
    for rid in ids:
        _lance_delete(workspace_id, rid)
    return True
