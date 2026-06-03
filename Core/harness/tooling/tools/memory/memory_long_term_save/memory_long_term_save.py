"""LLM tool: write a global long-term memory.

The ``source`` field is auto-stamped with the active conversation
context (turn driver passes it via thread-locals — see
:mod:`Core.harness.turn`). Falls back to "llm_tool_call" when no
context is present (e.g. tests calling the runner directly).
"""

from __future__ import annotations

from typing import Any, Dict


def run_memory_long_term_save(params: Dict[str, Any]) -> Dict[str, Any]:
    body = params.get("body")
    if not isinstance(body, str) or not body.strip():
        return {"status": "error", "error": "body is required"}

    from Core.harness.memory import long_term
    from Core.harness._tool_context import current_tool_context

    ctx = current_tool_context()
    source = (
        f"conversation:{ctx.conversation_id}/turn:{ctx.turn_id}"
        if ctx is not None and ctx.conversation_id
        else "llm_tool_call"
    )
    tags = params.get("tags") if isinstance(params.get("tags"), list) else None
    record = long_term.save(
        body=body,
        source=source,
        importance=params.get("importance"),
        tags=tags,
    )
    return {"status": "success", "memory": record.to_dict()}
