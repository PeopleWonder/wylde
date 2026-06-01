"""Layer 2: Workspace memory — scoped per-workspace, durable across MRU eviction.

Lives **outside** the per-workspace file-index folder so the LLM-
curated insights about a project survive when the indexable files
get evicted from the disk cache. Storage layout::

    Core/harness/memory/indexes/<slug>/files.lance       ← evicted on MRU fall-off
    Core/harness/memory/workspace_memories/<slug>/       ← durable; only the
        memory.json                                          explicit user
        memory.lance/                                        delete removes this

Re-activating a previously-evicted workspace re-indexes the files from
scratch but the workspace memory is still on disk — the LLM has its
key insights ready immediately. ``delete_workspace`` (explicit user
delete) removes both the index AND the memory folder.

Memgraph integration: when a memory is saved with ``entities[]``, we
write entity nodes and ``MENTIONED_IN`` edges via
:mod:`Core.harness.memory.memgraph`. The graph layer is best-effort —
if Memgraph isn't running, the save still succeeds and the entity
edges just don't get recorded.

Curation: :func:`curate` runs the LLM over current memories in batches
asking for keep / supersede / merge verdicts and applies them. Same
pattern as :mod:`Core.harness.memory.reflection` — pipe action returns
``skipped=True`` because chat_fn isn't injectable across the wire;
direct Python callers (the scheduler, tests) pass a chat_fn and run it
for real.

This is the package shim — implementation lives in submodules
(`_store`, `_search`, `_curate`) and is re-exported here. Tests that
``monkeypatch.setattr("Core.harness.memory.workspace_memory.search",
stub)`` rebind the package-level name correctly because of these
re-exports.
"""

from __future__ import annotations

from ._curate import CurationResult, curate
from ._search import search
from ._store import (
    WORKSPACE_MEMORIES_DIR,
    WorkspaceMemory,
    delete,
    delete_memory_dir,
    get,
    list_records,
    save,
    update,
)

# Private helpers re-exported so reflection.py (and tests that
# monkeypatch the package-level name) can reach them. They live in
# ``_store`` but the rest of the codebase imports the package, not
# the submodule.
from ._store import (  # noqa: F401
    _lance_delete,
    _lance_upsert,
    _load,
    _record_entities,
    _save,
)

__all__ = [
    "WorkspaceMemory",
    "WORKSPACE_MEMORIES_DIR",
    "CurationResult",
    "list_records",
    "get",
    "save",
    "update",
    "delete",
    "search",
    "curate",
    "delete_memory_dir",
]
