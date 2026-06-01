"""rag_index — incremental indexing trigger.

Pulled forward from ``_legacy/core/wylde-rag/tools/index.py``. The legacy
tool called ``..rag_api.start_index_task(paths, force=...)`` which ran the
ingest pipeline inside the wylde-rag service.

Per the Wylde N8N principle, the new ingestion pipeline lives entirely in
N8N. This tool fires the trigger via :func:`Wylde.Core.harness.memory.ingest.trigger_ingest`,
which posts to the N8N webhook and returns the execution id. Any unchanged
files are still skipped — that's the ingest workflow's responsibility, not
this tool's.

Failure model: ingest.trigger_ingest fails open (returns ``{ok: False,
error: "unreachable"}`` when N8N isn't running) so this tool surfaces
those errors verbatim rather than crashing the runner.
"""

from __future__ import annotations

import os
from typing import Any, Dict, List, Optional

from .....memory import ingest as _ingest


def _default_target() -> str:
    """Best-effort workspace root.

    Honour ``WYLDE_WORKSPACE_ROOT`` if set, otherwise fall back to the
    current working directory. Callers can always override via params.
    """
    return os.getenv("WYLDE_WORKSPACE_ROOT") or os.getcwd()


def run_rag_index(params: Dict[str, Any]) -> Dict[str, Any]:
    raw_paths = params.get("paths")
    paths: Optional[List[str]] = None
    if raw_paths:
        if not isinstance(raw_paths, (list, tuple)):
            return {
                "status": "error",
                "error": "'paths' must be an array of relative path strings",
            }
        paths = [str(p) for p in raw_paths if str(p).strip()]

    target_path = str(params.get("target_path") or _default_target())
    workspace_id = str(params.get("workspace_id") or "default")
    force = bool(params.get("force", False))

    options: Dict[str, Any] = {}
    if force:
        options["force_reindex"] = True

    result = _ingest.trigger_ingest(
        target_path=target_path,
        workspace_id=workspace_id,
        paths=paths,
        options=options or None,
    )

    if not result.get("ok"):
        return {
            "status": "error",
            "error": result.get("error") or "ingest trigger failed",
            "detail": result.get("detail") or "",
            "target_path": target_path,
            "paths": paths or [],
            "force": force,
        }

    return {
        "status": "started",
        "target_path": target_path,
        "workspace_id": workspace_id,
        "paths": paths or [],
        "force": force,
        "n8n": {k: v for k, v in result.items() if k != "ok"},
    }
