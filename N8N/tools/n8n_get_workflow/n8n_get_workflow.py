"""n8n_get_workflow — fetch a workflow definition by ID.

Read-only; no confirmation gate.
"""

from __future__ import annotations

from typing import Any, Dict


def run_n8n_get_workflow(params: Dict[str, Any]) -> Dict[str, Any]:
    workflow_id = params.get("workflow_id")
    if workflow_id is None or workflow_id == "":
        return {"error": "workflow_id is required"}
    try:
        from N8N.client import get_workflow
    except ImportError as exc:
        return {"error": f"N8N.client not importable: {exc}"}
    return get_workflow(str(workflow_id))
