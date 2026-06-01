"""n8n_get_execution — fetch an execution's status payload by ID.

Read-only; no confirmation gate.
"""

from __future__ import annotations

from typing import Any, Dict


def run_n8n_get_execution(params: Dict[str, Any]) -> Dict[str, Any]:
    execution_id = params.get("execution_id")
    if execution_id is None or execution_id == "":
        return {"error": "execution_id is required"}
    try:
        from N8N.client import get_execution
    except ImportError as exc:
        return {"error": f"N8N.client not importable: {exc}"}
    return get_execution(str(execution_id))
