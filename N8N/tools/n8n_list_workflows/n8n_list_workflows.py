"""n8n_list_workflows — read-only catalog of workflows in n8n.

Thin wrapper over :func:`Wylde.N8N.client.list_workflows`. The harness
runner expects ``run_<tool_id>(params)`` to return a dict; we forward
the client's result verbatim. No confirmation gate — read-only.
"""

from __future__ import annotations

from typing import Any, Dict


def run_n8n_list_workflows(params: Dict[str, Any]) -> Dict[str, Any]:
    del params  # this tool takes no parameters
    try:
        from N8N.client import list_workflows
    except ImportError as exc:
        return {"error": f"N8N.client not importable: {exc}"}
    return list_workflows()
