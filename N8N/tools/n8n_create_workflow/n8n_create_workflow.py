"""n8n_create_workflow — gated POST /rest/workflows.

The manifest declares ``requires_confirmation: true``. The runner is
responsible for the gate; this entrypoint is reached only after the
gate has cleared (or auto-mode is on). The function itself does not
re-check the gate — keeping the dispatch policy in one place.
"""

from __future__ import annotations

from typing import Any, Dict


def run_n8n_create_workflow(params: Dict[str, Any]) -> Dict[str, Any]:
    if not isinstance(params, dict):
        return {"error": "params must be an object"}
    name = params.get("name")
    if not name:
        return {"error": "name is required"}
    payload = {
        "name": name,
        "nodes": params.get("nodes", []),
        "connections": params.get("connections", {}),
        "active": bool(params.get("active", False)),
    }
    try:
        from N8N.client import create_workflow
    except ImportError as exc:
        return {"error": f"N8N.client not importable: {exc}"}
    return create_workflow(payload)
