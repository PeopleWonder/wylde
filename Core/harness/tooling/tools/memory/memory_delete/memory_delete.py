"""LLM tool: delete a memory by id. Confirmation-gated."""

from __future__ import annotations

from typing import Any, Dict


def run_memory_delete(params: Dict[str, Any]) -> Dict[str, Any]:
    scope = params.get("scope")
    rid = params.get("id")
    if scope not in ("long_term", "workspace"):
        return {"status": "error", "error": "scope must be 'long_term' or 'workspace'"}
    if not isinstance(rid, str) or not rid:
        return {"status": "error", "error": "id is required"}

    from Core.harness._tool_context import current_tool_context

    if scope == "long_term":
        from Core.harness.memory import long_term

        return {"status": "success", "deleted": long_term.delete(rid), "id": rid}

    from Core.harness.memory import workspace_memory
    from Core.harness.memory import conversation as _conv

    ctx = current_tool_context()
    workspace_id = ""
    if ctx is not None and ctx.workspace_id:
        workspace_id = ctx.workspace_id
    elif ctx is not None and ctx.conversation_id:
        workspace_id = _conv.get_workspace(ctx.conversation_id)
    if not workspace_id:
        return {
            "status": "error",
            "error": "no active workspace; cannot scope-resolve workspace memory",
        }
    return {
        "status": "success",
        "deleted": workspace_memory.delete(workspace_id, rid),
        "id": rid,
    }
