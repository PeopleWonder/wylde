"""LLM tool: search memories by scope."""

from __future__ import annotations

from typing import Any, Dict


def run_memory_search(params: Dict[str, Any]) -> Dict[str, Any]:
    scope = params.get("scope")
    query = params.get("query")
    if scope not in ("long_term", "workspace"):
        return {"status": "error", "error": "scope must be 'long_term' or 'workspace'"}
    if not isinstance(query, str) or not query.strip():
        return {"status": "error", "error": "query is required"}
    try:
        k = int(params.get("k") or 5)
    except (TypeError, ValueError):
        k = 5
    k = max(1, min(50, k))

    if scope == "long_term":
        from Core.harness.memory import long_term

        return {"status": "success", "hits": long_term.search(query, limit=k)}

    from Core.harness.memory import workspace_memory
    from Core.harness.memory import conversation as _conv
    from Core.harness._tool_context import current_tool_context

    ctx = current_tool_context()
    workspace_id = ""
    if ctx is not None and ctx.workspace_id:
        workspace_id = ctx.workspace_id
    elif ctx is not None and ctx.conversation_id:
        workspace_id = _conv.get_workspace(ctx.conversation_id)
    if not workspace_id:
        return {"status": "error", "error": "no active workspace"}
    return {
        "status": "success",
        "hits": workspace_memory.search(workspace_id, query, limit=k),
    }
