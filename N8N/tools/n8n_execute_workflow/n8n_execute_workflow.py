"""n8n_execute_workflow — kick off a workflow run.

Execution is treated as non-mutating at the catalog level: the
*workflow itself* may have side effects, but invoking an existing,
audited workflow is no riskier than calling any other tool. Mutating
operations (create/edit/delete of workflow definitions) are the ones
behind the confirmation gate.
"""

from __future__ import annotations

from typing import Any, Dict


def run_n8n_execute_workflow(params: Dict[str, Any]) -> Dict[str, Any]:
    workflow_id = params.get("workflow_id")
    if workflow_id is None or workflow_id == "":
        return {"error": "workflow_id is required"}
    inputs = params.get("inputs") or {}
    if not isinstance(inputs, dict):
        return {"error": "inputs must be an object"}
    try:
        from N8N.client import execute_workflow
    except ImportError as exc:
        return {"error": f"N8N.client not importable: {exc}"}
    return execute_workflow(str(workflow_id), inputs=inputs)
