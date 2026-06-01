"""rag_reindex — drop and rebuild the entire index from scratch.

Pulled forward from ``_legacy/core/wylde-rag/tools/reindex.py``. The legacy
tool called ``start_index_task(INDEX_PATHS, force=True)`` which dropped and
rebuilt the corpus inside the wylde-rag service.

In the new architecture, ingestion is an N8N workflow (see
:mod:`Wylde.Core.harness.memory.ingest`). This tool fires that workflow with
``force_reindex=true`` so it rebuilds from scratch.

The actual deletion of the existing vector store / graph rows is the
ingest workflow's responsibility — this tool is just the trigger.
"""

from __future__ import annotations

import os
from typing import Any, Dict

from .....memory import ingest as _ingest


def _default_target() -> str:
    return os.getenv("WYLDE_WORKSPACE_ROOT") or os.getcwd()


def run_rag_reindex(params: Dict[str, Any]) -> Dict[str, Any]:
    target_path = str(params.get("target_path") or _default_target())
    workspace_id = str(params.get("workspace_id") or "default")

    result = _ingest.trigger_ingest(
        target_path=target_path,
        workspace_id=workspace_id,
        options={"force_reindex": True, "wipe_first": True},
    )

    if not result.get("ok"):
        return {
            "status": "error",
            "error": result.get("error") or "reindex trigger failed",
            "detail": result.get("detail") or "",
            "target_path": target_path,
        }

    return {
        "status": "started",
        "target_path": target_path,
        "workspace_id": workspace_id,
        "force": True,
        "n8n": {k: v for k, v in result.items() if k != "ok"},
    }
