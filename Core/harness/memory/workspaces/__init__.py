"""Workspace registry — folder-based RAG indexes, MRU 5, per-conversation binding.

A workspace is one user-picked folder. Each gets its own LanceDB store
under ``Core/harness/memory/indexes/<slug>/`` containing two tables:

* ``files``  — embeddings of the folder's contents (per-chunk rows)
* ``memory`` — the workspace memory layer (LLM-written notes scoped to
  this folder; lives next to the file index so MRU eviction takes both
  out together — the design's "dies with it" semantics)

A separate per-workspace ``persona.txt`` carries the optional
prompt-fragment a user can attach via the Settings UI; loaded into the
chat-turn system prompt when the workspace is active.

The registry itself is a JSON file (``workspaces.json``) holding
workspaces in MRU order. Activation moves an entry to the head;
activating a 6th workspace evicts the tail (deletes its index folder
+ workspace memory entirely).

Indexing has two paths:

* **Delta refresh** — :func:`refresh_workspace` walks the folder, re-
  embeds files whose mtime is newer than the cached entry. Cheap.
  Triggered automatically by :func:`activate`.
* **Full rebuild** — :func:`reindex_workspace` ignores cache, re-
  embeds everything. Triggered by the GUI "Reindex" button.

Files that look binary (best-effort by NUL-byte sniff or oversize)
are skipped with a log line so the indexer never blocks on a giant
PDF or video. Per-workspace concurrency is single-threaded — index
mutations always run on the caller's thread; the GUI gates the
button so two reindexes for the same workspace never race.

This is the package shim — the implementation lives in submodules
(`_store`, `_mru`, `_index`, `_search`) and is re-exported here so
existing call sites that do
``from Core.harness.memory.workspaces import activate`` keep working.
"""

from __future__ import annotations

from ._index import (
    refresh_workspace,
    reindex_workspace,
    status,
    workspace_index_dir,
)
from ._mru import (
    MRU_LIMIT,
    MRU_LIMIT_DEFAULT,
    MRU_LIMIT_MAX,
    MRU_LIMIT_MIN,
    SETTINGS_PATH,
    get_mru_limit,
    set_mru_limit,
)
from ._search import search_files
from ._store import (
    INDEXES_DIR,
    REGISTRY_PATH,
    Workspace,
    activate,
    delete_workspace,
    get_persona,
    get_workspace,
    list_workspaces,
    recent_workspaces,
    set_persona,
)

# Private helpers re-exported so tests that reach into the package
# (e.g. ``ws._slug_for``) keep working after the registry split.
from ._store import _slug_for  # noqa: F401

__all__ = [
    "MRU_LIMIT",
    "MRU_LIMIT_DEFAULT",
    "MRU_LIMIT_MIN",
    "MRU_LIMIT_MAX",
    "SETTINGS_PATH",
    "get_mru_limit",
    "set_mru_limit",
    "Workspace",
    "INDEXES_DIR",
    "REGISTRY_PATH",
    "list_workspaces",
    "recent_workspaces",
    "get_workspace",
    "activate",
    "delete_workspace",
    "set_persona",
    "get_persona",
    "reindex_workspace",
    "refresh_workspace",
    "status",
    "search_files",
    "workspace_index_dir",
]
