"""Chat-turn driver — package shim.

One function — :func:`run_turn` — that takes a user message and walks
through the LLM-tool-call loop until the model produces a final response
without further tool calls. Per-turn state plus two event streams
(user-facing tokens and server-side tool activity) are kept on a
:class:`TurnState`, which the pipe layer drains via long-poll cursor
actions.

The two streams are wire-level disjoint by design: ``token`` /
``thinking`` / ``turn_complete`` / ``turn_aborted`` go to
``state.turn_events``; ``tool_dispatched`` / ``tool_result`` /
``tool_error`` go to ``state.tool_events``. Consumers see only their own
event types, no GUI-side filtering.

Implementation is split across the submodules of this package:

* :mod:`._state` — per-turn data shapes, thread-local tool context,
  process-wide turn registry, low-level emit helpers.
* :mod:`._request_build` — system-prompt assembly with the six memory
  slots.
* :mod:`._streaming` — production chat backend wiring, assistant-content
  tool-call salvage parser, default runner shims.
* :mod:`._tool_round` — tier gating, per-call dispatch, memory-write
  event emission.
* :mod:`._end_of_turn` — post-turn extractor wiring, conversation IO,
  end-of-turn architectural check.
* :mod:`._driver` — the long-running ``_drive_turn`` thread loop and
  the public ``start_turn`` / ``run_turn`` / ``cancel_turn`` entry
  points.

This package's ``__init__`` re-exports the public surface plus the
private names tests reach for (``_extract_tool_calls_from_content``,
``_call_hash``, ``install_post_turn_extractor_chat_fn``, etc.) so
imports of the form ``from Core.harness.turn import X`` continue to
resolve.
"""

from __future__ import annotations

# Public surface — listed in __all__ below.
from ._driver import (
    cancel_turn,
    run_turn,
    start_turn,
)
from ._state import (
    AssistantTurn,
    ChatFn,
    ChatStep,
    ToolCall,
    ToolContext,
    TurnEvent,
    TurnState,
    current_tool_context,
    get_turn,
    list_turns,
    reap_turn,
    record_file_written,
    register_turn,
)
from ._tool_round import (
    CancelledError,
    ToolRunFn,
)

# Private re-exports — surfaced for tests and the pipe action layer that
# reach for them through the package root. Each item below is consumed
# by at least one test or sibling module via ``Core.harness.turn.<name>``.
from ._driver import (  # noqa: F401 — re-exported for test/sibling access
    _MAX_TOOL_LOOPS,
    _drive_turn,
    _drive_turn_inner,
    _new_turn_id,
    _resolve_workspace_id,
)
from ._end_of_turn import (  # noqa: F401 — re-exported for test/sibling access
    _collect_llm_saves,
    _conversation_module,
    _load_conversation_history,
    _maybe_fire_post_turn_extractor,
    _persist_conversation,
    _post_turn_chat_fn_override,
    _post_turn_synchronous,
    _resolve_post_turn_chat_fn,
    _run_end_of_turn_architectural_check,
    install_post_turn_extractor_chat_fn,
)
from ._request_build import (  # noqa: F401 — re-exported for test/sibling access
    _build_system_prompt,
    _build_system_prompt_with_slots,
    _short_args,
    _slot_long_term,
    _slot_persona,
    _slot_rag,
    _slot_short_term,
    _slot_workspace_memory,
)
from ._state import (  # noqa: F401 — re-exported for test/sibling access
    _emit_tool,
    _emit_turn,
    _mark_done,
    _set_tool_context,
    _tool_context,
    _turns,
    _turns_lock,
    logger,
)
from ._streaming import (  # noqa: F401 — re-exported for test/sibling access
    _FENCED_JSON_RE,
    _TOOL_TAG_PATTERNS,
    _call_hash,
    _default_chat_fn,
    _default_list_tools,
    _default_tool_run,
    _extract_tool_calls_from_content,
    _find_balanced_braces,
    _params_to_json_schema,
    _parse_one_call,
    _select_default_model,
    _tools_to_wire,
)
from ._tool_round import (  # noqa: F401 — re-exported for test/sibling access
    _DEFAULT_TIER,
    _MEMORY_WRITE_TOOL_IDS,
    _VALID_TIERS,
    TIER_DESTRUCTIVE,
    TIER_READ_ONLY,
    TIER_TOOL_USE,
    _canonicalise_tool_id,
    _check_cancelled,
    _check_tier_gate,
    _normalise_device_tier,
    _record_short_term,
    _resolve_tool_alias_map,
    _run_one_tool,
    _stringify,
    _unwrap_runner_envelope,
    emit_memory_written,
)

__all__ = [
    "AssistantTurn",
    "CancelledError",
    "ChatFn",
    "ChatStep",
    "ToolCall",
    "ToolContext",
    "ToolRunFn",
    "TurnEvent",
    "TurnState",
    "cancel_turn",
    "current_tool_context",
    "get_turn",
    "list_turns",
    "reap_turn",
    "record_file_written",
    "register_turn",
    "run_turn",
    "start_turn",
]
