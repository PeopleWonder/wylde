"""n8n_edit_workflow — gated PATCH /rest/workflows/<id>."""

from __future__ import annotations

from typing import Any, Dict


def run_n8n_edit_workflow(params: Dict[str, Any]) -> Dict[str, Any]:
    if not isinstance(params, dict):
        return {"error": "params must be an object"}
    workflow_id = params.get("workflow_id")
    if not workflow_id:
        return {"error": "workflow_id is required"}

    payload: Dict[str, Any] = {}
    for key in ("name", "nodes", "connections", "active"):
        if key in params and params[key] is not None:
            payload[key] = params[key]
    if not payload:
        return {"error": "No updatable fields provided"}

    try:
        from N8N.client import edit_workflow
    except ImportError as exc:
        return {"error": f"N8N.client not importable: {exc}"}
    return edit_workflow(str(workflow_id), payload)
