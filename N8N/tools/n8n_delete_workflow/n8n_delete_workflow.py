"""n8n_delete_workflow — gated DELETE /rest/workflows/<id>.

Most destructive of the n8n tools — the workflow definition, history,
and any active triggers are removed permanently. Manifest declares
``requires_confirmation: true``; the runner enforces it.
"""

from __future__ import annotations

from typing import Any, Dict


def run_n8n_delete_workflow(params: Dict[str, Any]) -> Dict[str, Any]:
    workflow_id = params.get("workflow_id") if isinstance(params, dict) else None
    if not workflow_id:
        return {"error": "workflow_id is required"}
    try:
        from N8N.client import delete_workflow
    except ImportError as exc:
        return {"error": f"N8N.client not importable: {exc}"}
    return delete_workflow(str(workflow_id))
