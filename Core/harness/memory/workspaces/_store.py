"""Workspace registry — dataclass, JSON store, activation, eviction, persona.

The registry itself is a JSON file (``workspaces.json``) holding
workspaces in MRU order. Activation moves an entry to the head;
activating one past the configured MRU cap evicts the tail (deletes
its index folder; the durable workspace memory survives).

Cross-module concerns:

* Indexing (``_index_full``, ``_index_delta``) lives in ``_index.py``
  to keep this file focused on bookkeeping. Imported lazily inside
  ``activate`` because ``_index.py`` imports back into this module
  for the dataclass + metadata helpers.
* MRU cap settings live in ``_mru.py``. Imported lazily inside
  ``_evict_past_mru`` to break the cycle (``_mru.set_mru_limit``
  calls back into eviction).
* :func:`delete_workspace` deletes the durable workspace-memory
  folder via a lazy import of
  :mod:`Core.harness.memory.workspace_memory`. The lazy pattern is
  load-bearing — ``workspace_memory`` imports ``workspaces``, so a
  module-level import here would cycle.
"""

from __future__ import annotations

import hashlib
import json
import logging
import re
import shutil
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Dict, List, Optional

from .._common import DATA_DIR, ensure_dir

logger = logging.getLogger("wylde.harness.memory.workspaces")

# ── Constants ──────────────────────────────────────────────────────────

INDEXES_DIR: Path = DATA_DIR / "indexes"
REGISTRY_PATH: Path = DATA_DIR / "workspaces.json"

_registry_lock = threading.RLock()


# ── Public dataclasses ─────────────────────────────────────────────────


@dataclass
class Workspace:
    id: str
    path: str
    persona: str = ""
    file_count: int = 0
    last_indexed_at: float = 0.0
    last_activated_at: float = 0.0
    indexing: bool = False  # True while a refresh / reindex is mid-flight

    def to_dict(self) -> Dict[str, Any]:
        return {
            "id": self.id,
            "path": self.path,
            "persona": self.persona,
            "file_count": self.file_count,
            "last_indexed_at": self.last_indexed_at,
            "last_activated_at": self.last_activated_at,
            "indexing": self.indexing,
        }

    @classmethod
    def from_dict(cls, d: Dict[str, Any]) -> "Workspace":
        return cls(
            id=str(d.get("id", "")),
            path=str(d.get("path", "")),
            persona=str(d.get("persona", "")),
            file_count=int(d.get("file_count", 0)),
            last_indexed_at=float(d.get("last_indexed_at", 0.0)),
            last_activated_at=float(d.get("last_activated_at", 0.0)),
            indexing=bool(d.get("indexing", False)),
        )


# ── Registry IO ────────────────────────────────────────────────────────


def _load_registry() -> List[Workspace]:
    if not REGISTRY_PATH.exists():
        return []
    try:
        raw = json.loads(REGISTRY_PATH.read_text(encoding="utf-8"))
    except Exception as exc:  # noqa: BLE001
        logger.warning("workspaces: registry unreadable, treating as empty: %s", exc)
        return []
    items = raw.get("workspaces") if isinstance(raw, dict) else raw
    if not isinstance(items, list):
        return []
    return [Workspace.from_dict(it) for it in items if isinstance(it, dict)]


def _save_registry(workspaces: List[Workspace]) -> None:
    ensure_dir(REGISTRY_PATH.parent)
    payload = {"workspaces": [w.to_dict() for w in workspaces]}
    tmp = REGISTRY_PATH.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(payload, indent=2), encoding="utf-8")
    tmp.replace(REGISTRY_PATH)


def _slug_for(path: str) -> str:
    """Derive a stable, filename-safe id from a folder path.

    Keeps the last path component (sanitized) for human readability,
    appends a 6-hex hash of the absolute path for collision avoidance.
    """
    abspath = str(Path(path).expanduser().resolve())
    digest = hashlib.sha256(abspath.encode("utf-8")).hexdigest()[:6]
    base = Path(abspath).name or "workspace"
    safe = re.sub(r"[^A-Za-z0-9_-]+", "_", base)[:40] or "workspace"
    return f"{safe}-{digest}"


# ── Public API: registry queries ───────────────────────────────────────


def list_workspaces() -> List[Workspace]:
    """Workspaces in MRU order (most recent first)."""
    with _registry_lock:
        return _load_registry()


def recent_workspaces(limit: Optional[int] = None) -> List[Workspace]:
    """First N workspaces in MRU order — what the dropdown shows.
    ``limit`` defaults to the user-configured MRU cap."""
    from ._mru import get_mru_limit

    if limit is None:
        limit = get_mru_limit()
    return list_workspaces()[:limit]


def get_workspace(workspace_id: str) -> Optional[Workspace]:
    with _registry_lock:
        for w in _load_registry():
            if w.id == workspace_id:
                return w
    return None


# ── Activation + MRU ───────────────────────────────────────────────────


def activate(path: str, *, full_reindex: bool = False) -> Workspace:
    """Activate ``path`` as the current workspace.

    Three cases:

    * Path already in registry → move to head, run a delta refresh
      (or full reindex if requested), return the entry.
    * Path new → mint slug, create index dir, run a full index, prepend
      to registry, evict oldest if past MRU cap.
    * Path doesn't exist or isn't a directory → ``ValueError``.
    """
    # Lazy import: ``_index`` imports back into this module for the
    # ``Workspace`` dataclass and metadata helpers.
    from ._index import _index_delta, _index_full

    folder = Path(path).expanduser().resolve()
    if not folder.exists():
        raise ValueError(f"workspace path does not exist: {folder}")
    if not folder.is_dir():
        raise ValueError(f"workspace path is not a directory: {folder}")

    with _registry_lock:
        workspaces = _load_registry()
        slug = _slug_for(str(folder))
        existing = next((w for w in workspaces if w.id == slug), None)
        is_new = existing is None

        if existing is None:
            existing = Workspace(id=slug, path=str(folder))
            workspaces.insert(0, existing)
        else:
            # Move to head — preserves persona, file_count, etc.
            workspaces = [w for w in workspaces if w.id != slug]
            workspaces.insert(0, existing)

        existing.last_activated_at = time.time()
        # Persist the new MRU order immediately so a crash mid-index doesn't
        # forget the activation. The eviction below uses the post-insert state.
        _save_registry(workspaces)
        evicted = _evict_past_mru(workspaces)
        if evicted:
            _save_registry(workspaces)

    # Index outside the lock — embedding can take seconds.
    if is_new or full_reindex:
        _index_full(existing)
    else:
        _index_delta(existing)

    # Re-read the post-index state so the returned dataclass reflects
    # the file_count + last_indexed_at the indexer just wrote. The
    # in-place `existing` reference would otherwise show 0 / 0.0 — a
    # stale view that surprises GUI callers who expect activate() to
    # block until the workspace is queryable.
    refreshed = get_workspace(existing.id)
    return refreshed if refreshed is not None else existing


def deactivate_all() -> Any:
    """Mark no workspace active. Useful in tests; doesn't delete data."""
    # Currently a no-op since "active" is per-conversation, not global.
    # Kept on the surface so callers can express intent.
    return None


def delete_workspace(workspace_id: str) -> bool:
    """Remove a workspace from the registry and delete BOTH its index
    folder and its durable workspace-memory folder on disk.

    This is the explicit user-driven delete path. MRU eviction (handled
    in :func:`_evict_past_mru`) only deletes the index folder — the
    workspace memory survives so a re-activated evicted workspace
    starts with its LLM-curated insights ready.

    Long-term memory is unaffected.
    """
    with _registry_lock:
        workspaces = _load_registry()
        target = next((w for w in workspaces if w.id == workspace_id), None)
        if target is None:
            return False
        workspaces = [w for w in workspaces if w.id != workspace_id]
        _save_registry(workspaces)

    _delete_index_dir(workspace_id)

    # Delete the durable workspace-memory folder too — this is the
    # explicit-delete path, not eviction. Lazy-import to keep the
    # module-load order clean (workspace_memory imports workspaces).
    try:
        from .. import workspace_memory as _wm

        _wm.delete_memory_dir(workspace_id)
    except ImportError:
        try:
            from Core.harness.memory import workspace_memory as _wm

            _wm.delete_memory_dir(workspace_id)
        except ImportError:
            logger.warning(
                "workspaces: workspace_memory not importable; "
                "durable memory folder for %s not removed",
                workspace_id,
            )
    return True


def _evict_past_mru(
    workspaces: List[Workspace],
    *,
    limit: Optional[int] = None,
) -> List[str]:
    """Trim the registry to ``limit`` (default: the user-configured
    cap), deleting evicted index dirs. Workspace memory is preserved
    so a re-activated evicted workspace lands warm.

    Mutates ``workspaces`` in place. Returns the list of evicted ids
    (mostly for logging / tests). Caller is responsible for saving the
    resulting registry; we do disk deletion here because it's part of
    the eviction semantics.
    """
    # Lazy import to avoid the _store → _mru → _store cycle.
    from ._mru import get_mru_limit

    if limit is None:
        limit = get_mru_limit()
    evicted: List[str] = []
    while len(workspaces) > limit:
        victim = workspaces.pop()
        evicted.append(victim.id)
        _delete_index_dir(victim.id)
        logger.info("workspaces: evicted %s (%s) past MRU cap", victim.id, victim.path)
    return evicted


def _delete_index_dir(workspace_id: str) -> None:
    """Best-effort recursive delete of the per-workspace LanceDB folder."""
    target = INDEXES_DIR / workspace_id
    if not target.exists():
        return
    try:
        shutil.rmtree(target)
    except Exception as exc:  # noqa: BLE001
        logger.warning(
            "workspaces: failed to delete %s (will retry on next eviction): %s",
            target,
            exc,
        )


# ── Persona ────────────────────────────────────────────────────────────


def set_persona(workspace_id: str, text: str) -> bool:
    with _registry_lock:
        workspaces = _load_registry()
        for w in workspaces:
            if w.id == workspace_id:
                w.persona = str(text or "")
                _save_registry(workspaces)
                return True
    return False


def get_persona(workspace_id: str) -> str:
    w = get_workspace(workspace_id)
    return w.persona if w is not None else ""


# ── Workspace metadata helpers ─────────────────────────────────────────


def _set_indexing(workspace_id: str, flag: bool) -> None:
    with _registry_lock:
        workspaces = _load_registry()
        for w in workspaces:
            if w.id == workspace_id:
                w.indexing = flag
                if not flag:
                    w.last_indexed_at = time.time()
                _save_registry(workspaces)
                return


def _update_workspace_metadata(
    workspace_id: str, *, file_count: Optional[int] = None
) -> None:
    with _registry_lock:
        workspaces = _load_registry()
        for w in workspaces:
            if w.id == workspace_id:
                if file_count is not None:
                    w.file_count = int(file_count)
                _save_registry(workspaces)
                return
