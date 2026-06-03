"""LLM tool: write a workspace-scoped memory.

The active workspace_id comes from the per-conversation binding via
:func:`Core.harness.turn.current_tool_context`. If the conversation
hasn't selected a workspace, the call fails with a clear error so the
LLM doesn't silently lose the memory.
"""

from __future__ import annotations

from typing import Any, Dict


def run_memory_workspace_save(params: Dict[str, Any]) -> Dict[str, Any]:
    body = params.get("body")
    if not isinstance(body, str) or not body.strip():
        return {"status": "error", "error": "body is required"}

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
        return {
            "status": "error",
            "error": (
                "no active workspace bound to this conversation; "
                "use memory.long_term.save instead, or activate a "
                "workspace first via rag.workspaces.activate."
            ),
        }

    source = (
        f"conversation:{ctx.conversation_id}/turn:{ctx.turn_id}"
        if ctx is not None and ctx.conversation_id
        else "llm_tool_call"
    )
    entities = (
        params.get("entities") if isinstance(params.get("entities"), list) else None
    )
    record = workspace_memory.save(
        workspace_id=workspace_id,
        body=body,
        source=source,
        importance=params.get("importance"),
        entities=entities,
    )
    return {"status": "success", "memory": record.to_dict()}
