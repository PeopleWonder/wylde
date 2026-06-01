"""System-prompt assembly with the six memory slots.

Pulled out of the driver so the prompt-building pipeline can be read
independently of the chat loop. Two public entry points:

* :func:`_build_system_prompt_with_slots` — full slot stack (long-term,
  workspace memory, short-term, persona, RAG, base instructions).
* :func:`_build_system_prompt` — the base block + tool catalog only,
  used directly by tests and the prior single-shot driver path.

Each slot fails silently when its underlying store isn't reachable so
the prompt always builds. Optional dependencies (long_term,
workspace_memory, workspaces, retrieval) are lazy-imported on first
call to keep cold-start cheap.
"""

from __future__ import annotations

import logging
from typing import Any, Dict, List, Optional

from ._state import ChatFn, TurnState

logger = logging.getLogger("wylde.harness.turn")


def _build_system_prompt_with_slots(
    *,
    state: TurnState,
    user_message: str,
    tools_catalog: List[Dict[str, Any]],
    chat_fn: ChatFn,
    model: Optional[str],
) -> str:
    """Build the chat-turn system prompt with memory slots in design order:

        1. Long-term core block (top-importance global memories).
        2. Workspace memory relevant to the current turn (semantic
           search keyed on the user message).
        3. Short-term working memory (recent tool calls / files /
           decisions, persisted on the conversation record).
        4. Workspace persona (per-workspace prompt fragment).
        5. RAG hits over the workspace files (HyDE → hybrid → rerank).
        6. The base instruction + tool catalog from the existing
           ``_build_system_prompt`` helper.

    Each slot fails silently and emits an empty string when its
    underlying store isn't reachable, so the prompt always builds.
    """
    parts: List[str] = []

    if state.modality == "voice":
        parts.append(
            "## Voice mode\n"
            "You are responding via the user's microphone, not a chat box. "
            "Keep replies concise and conversational — short sentences a "
            "person would naturally hear out loud. Avoid markdown, code "
            "fences, bullet lists, and links: they read poorly through TTS. "
            "If you need to share a code snippet or a long output, briefly "
            "describe it and offer to send it to the chat in text instead."
        )

    long_term_block = _slot_long_term()
    if long_term_block:
        parts.append("## Long-term memory\n" + long_term_block)

    workspace_block = _slot_workspace_memory(state, user_message)
    if workspace_block:
        parts.append("## Workspace memory\n" + workspace_block)

    short_term_block = _slot_short_term(state)
    if short_term_block:
        parts.append("## Recent activity (this conversation)\n" + short_term_block)

    persona_block = _slot_persona(state)
    if persona_block:
        parts.append("## Workspace persona\n" + persona_block)

    rag_block = _slot_rag(state, user_message, chat_fn=chat_fn, model=model)
    if rag_block:
        parts.append("## Workspace context (cite using [N] markers)\n" + rag_block)

    parts.append(_build_system_prompt(tools_catalog))
    return "\n\n".join(parts)


def _slot_long_term() -> str:
    try:
        from ..memory import long_term as _lt
    except ImportError:
        try:
            from Core.harness.memory import long_term as _lt
        except ImportError:
            return ""
    try:
        records = _lt.core_block(limit=5)
    except Exception:  # noqa: BLE001
        return ""
    if not records:
        return ""
    return "\n".join(f"- (importance {r.importance}) {r.body}" for r in records)


def _slot_workspace_memory(state: TurnState, query: str) -> str:
    if not state.workspace_id:
        return ""
    try:
        from ..memory import workspace_memory as _wm
    except ImportError:
        try:
            from Core.harness.memory import workspace_memory as _wm
        except ImportError:
            return ""
    try:
        hits = _wm.search(state.workspace_id, query, limit=3)
    except Exception:  # noqa: BLE001
        return ""
    if not hits:
        return ""
    return "\n".join(f"- {h.get('body', '')}" for h in hits)


def _slot_short_term(state: TurnState) -> str:
    try:
        from ..memory import conversation as _conv
    except ImportError:
        try:
            from Core.harness.memory import conversation as _conv
        except ImportError:
            return ""
    try:
        entries = _conv.get_working_memory(state.conversation_id)
    except Exception:  # noqa: BLE001
        return ""
    if not entries:
        return ""
    # Filter out entries the conversation-scoped reflection has already
    # consumed — those have been distilled into a workspace / long-term
    # memory and resurface through the memory slot. Keeping them here
    # too would double-count.
    entries = [
        e for e in entries if not (isinstance(e, dict) and e.get("superseded_by"))
    ]
    if not entries:
        return ""
    # Render the most recent 8 entries; the LLM mostly needs "what did
    # I already do" so trimming long tails keeps the prompt small.
    recent = entries[-8:]
    lines = []
    for e in recent:
        kind = e.get("kind", "raw")
        data = e.get("data", {})
        if kind == "tool":
            lines.append(
                f"- ran tool {data.get('name', '?')}({_short_args(data.get('args'))})"
            )
        else:
            summary = str(data)[:120]
            lines.append(f"- {kind}: {summary}")
    return "\n".join(lines)


def _short_args(args: Any) -> str:
    if not isinstance(args, dict):
        return ""
    bits = []
    for k, v in list(args.items())[:3]:
        s = str(v)
        if len(s) > 40:
            s = s[:40] + "…"
        bits.append(f"{k}={s}")
    return ", ".join(bits)


def _slot_persona(state: TurnState) -> str:
    if not state.workspace_id:
        return ""
    try:
        from ..memory import workspaces as _ws
    except ImportError:
        try:
            from Core.harness.memory import workspaces as _ws
        except ImportError:
            return ""
    try:
        return _ws.get_persona(state.workspace_id) or ""
    except Exception:  # noqa: BLE001
        return ""


def _slot_rag(
    state: TurnState,
    query: str,
    *,
    chat_fn: ChatFn,
    model: Optional[str],
) -> str:
    if not state.workspace_id:
        return ""
    try:
        from ..memory import retrieval as _ret
    except ImportError:
        try:
            from Core.harness.memory import retrieval as _ret
        except ImportError:
            return ""
    try:
        # HyDE costs an extra LLM round trip; only use it for non-trivial
        # queries to keep latency reasonable. ``do_rerank=False`` here
        # because the cross-encoder model isn't always available; the
        # caller can wire it in via direct ``retrieval.retrieve`` if
        # they want it.
        hits = _ret.retrieve(
            state.workspace_id,
            query,
            limit=5,
            chat_fn=chat_fn if len(query) > 30 else None,
            hyde_model=model,
            do_rerank=False,
        )
    except Exception:  # noqa: BLE001
        return ""
    return _ret.format_for_prompt(hits)


def _build_system_prompt(tools_catalog: List[Dict[str, Any]]) -> str:
    """Stub system prompt. Once :mod:`Core.harness.prompts` exposes a
    catalog API the driver will pick a named template from there; for
    now the prompt is built inline so the driver runs without prompt
    plumbing being finalised."""
    tool_lines = []
    for tool in tools_catalog[:60]:  # bounded to keep the prompt small
        if not isinstance(tool, dict):
            continue
        name = tool.get("tool_id") or tool.get("name") or ""
        desc = tool.get("description") or ""
        if name:
            tool_lines.append(f"- {name}: {desc}")
    tool_block = "\n".join(tool_lines) if tool_lines else "(no tools available)"
    return (
        "You are Wylde, a locally-hosted assistant. You can call tools "
        "to take actions or retrieve information. Respond with a tool call "
        "when you need one; otherwise produce a direct answer.\n\n"
        "Memory rule: the system automatically tracks important context "
        "from your conversation through a post-turn extraction pass — "
        "you do not need to call memory.* tools to record things you "
        "judge interesting. Use memory.long_term.save / "
        "memory.workspace.save / memory.update / memory.delete ONLY when "
        "the user has explicitly asked you to modify memory (e.g., "
        '"save this to memory", "remember that...", "forget X", '
        '"update what you remember about Y"). memory.search is fine '
        "to call any time you need to look something up.\n\n"
        f"Available tools:\n{tool_block}"
    )
