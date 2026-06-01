"""LLM tool: revise an existing memory by id."""

from __future__ import annotations

from typing import Any, Dict, Optional, Union

from Core.harness.memory.long_term import LongTermMemory
from Core.harness.memory.workspace_memory import WorkspaceMemory


def run_memory_update(params: Dict[str, Any]) -> Dict[str, Any]:
    scope = params.get("scope")
    rid = params.get("id")
    if scope not in ("long_term", "workspace"):
        return {"status": "error", "error": "scope must be 'long_term' or 'workspace'"}
    if not isinstance(rid, str) or not rid:
        return {"status": "error", "error": "id is required"}

    from Core.harness.turn import current_tool_context

    record: Optional[Union[LongTermMemory, WorkspaceMemory]]
    if scope == "long_term":
        from Core.harness.memory import long_term

        record = long_term.update(
            rid,
            body=params.get("body") if isinstance(params.get("body"), str) else None,
            importance=params.get("importance"),
        )
        if record is None:
            return {"status": "error", "error": f"memory {rid!r} not found"}
        return {"status": "success", "memory": record.to_dict()}

    # workspace scope
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
    record = workspace_memory.update(
        workspace_id,
        rid,
        body=params.get("body") if isinstance(params.get("body"), str) else None,
        importance=params.get("importance"),
    )
    if record is None:
        return {
            "status": "error",
            "error": f"memory {rid!r} not in workspace {workspace_id!r}",
        }
    return {"status": "success", "memory": record.to_dict()}
