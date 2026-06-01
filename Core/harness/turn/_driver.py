"""The core chat-turn driver thread.

Owns the long-running ``_drive_turn_inner`` loop that walks the
LLM-tool-call cycle until the model produces a final response with no
further tool calls (or the loop cap hits, or cancellation fires). The
public synchronous wrappers (``start_turn`` / ``run_turn`` /
``cancel_turn``) and the workspace_id resolver live here too.

A note on cross-module private-name calls: tests reach for some
private names via ``turn._extract_tool_calls_from_content`` etc. to
either assert or patch them.  For test patches to take effect inside
this driver, we import the submodule (``from . import _streaming as
_str``) and call via the attribute (``_str._extract_tool_calls_from_content``)
rather than binding the function locally at import time.  That way a
monkeypatch against the package re-export resolves to the right object.
"""

from __future__ import annotations

import logging
import threading
import time
import uuid
from typing import Any, Callable, Dict, List, Optional

from . import _end_of_turn as _eot
from . import _request_build as _rb
from . import _streaming as _str
from ._state import (
    AssistantTurn,
    ChatFn,
    ToolCall,
    ToolContext,
    TurnState,
    _emit_tool,
    _emit_turn,
    _mark_done,
    _set_tool_context,
    get_turn,
    register_turn,
)
from ._tool_round import (
    CancelledError,
    ToolRunFn,
    _check_cancelled,
    _normalise_device_tier,
    _record_short_term,
    _resolve_tool_alias_map,
    _run_one_tool,
)

logger = logging.getLogger("wylde.harness.turn")


def _resolve_workspace_id(
    explicit: Optional[str],
    conversation_id: str,
) -> str:
    """Pick the workspace_id for this turn:

    1. Caller's explicit value, if non-empty.
    2. The conversation's persisted binding, if any.
    3. Empty string (no workspace bound).

    Side effect: when an explicit value is given, it's persisted onto
    the conversation record so future turns inherit it. This is the
    "binds workspace to conversation if not already set" line in the
    pipe surface design.
    """
    if isinstance(explicit, str) and explicit:
        try:
            from ..memory import conversation as _conv
        except ImportError:
            try:
                from Core.harness.memory import conversation as _conv
            except ImportError:
                return explicit
        try:
            _conv.set_workspace(conversation_id, explicit)
        except Exception:  # noqa: BLE001
            pass
        return explicit
    try:
        from ..memory import conversation as _conv
    except ImportError:
        try:
            from Core.harness.memory import conversation as _conv
        except ImportError:
            return ""
    try:
        return _conv.get_workspace(conversation_id) or ""
    except Exception:  # noqa: BLE001
        return ""


def start_turn(
    user_message: str,
    conversation_id: str,
    *,
    model: Optional[str] = None,
    turn_id: Optional[str] = None,
    workspace_id: Optional[str] = None,
    modality: str = "text",
    device_tier: Optional[str] = None,
    chat_fn: Optional[ChatFn] = None,
    tool_run: Optional[ToolRunFn] = None,
    list_tools_fn: Optional[Callable[[], List[Dict[str, Any]]]] = None,
) -> TurnState:
    """Kick off a turn and return its :class:`TurnState` immediately.

    The actual driver runs in a daemon thread. Callers reach the events
    via :func:`get_turn` + ``state.turn_events`` / ``state.tool_events``,
    or via the long-poll pipe actions. ``run_turn`` is the synchronous
    convenience wrapper that blocks until completion.

    ``workspace_id`` binds the conversation to a workspace. If omitted,
    we fall back to whatever the conversation already has bound; if
    nothing's bound, the turn runs without RAG / workspace-memory
    slots in the system prompt.
    """
    resolved_workspace = _resolve_workspace_id(workspace_id, conversation_id)
    state = TurnState(
        turn_id=turn_id or _new_turn_id(),
        conversation_id=conversation_id,
        workspace_id=resolved_workspace,
        modality=str(modality or "text"),
        device_tier=_normalise_device_tier(device_tier),
    )
    register_turn(state)

    chat_callable: ChatFn = chat_fn or _str._default_chat_fn
    tool_callable: ToolRunFn = tool_run or _str._default_tool_run
    tool_list_fn = list_tools_fn or _str._default_list_tools

    thread = threading.Thread(
        target=_drive_turn,
        args=(state, user_message, model, chat_callable, tool_callable, tool_list_fn),
        name=f"harness-turn-{state.turn_id[:8]}",
        daemon=True,
    )
    thread.start()
    return state


def run_turn(
    user_message: str,
    conversation_id: str,
    *,
    model: Optional[str] = None,
    turn_id: Optional[str] = None,
    workspace_id: Optional[str] = None,
    modality: str = "text",
    device_tier: Optional[str] = None,
    chat_fn: Optional[ChatFn] = None,
    tool_run: Optional[ToolRunFn] = None,
    list_tools_fn: Optional[Callable[[], List[Dict[str, Any]]]] = None,
    timeout: float = 300.0,
) -> AssistantTurn:
    """Blocking wrapper. Drives the turn synchronously and returns the result.

    Used by the ``chat.run_turn`` pipe action and by tests that don't
    want to drive the long-poll surface. The streaming pipe actions
    (:func:`Core.harness.pipe.stream_turn`, :func:`stream_tools`) work
    against the same TurnState a non-blocking ``start_turn`` produces.
    """
    state = start_turn(
        user_message,
        conversation_id,
        model=model,
        turn_id=turn_id,
        workspace_id=workspace_id,
        modality=modality,
        device_tier=device_tier,
        chat_fn=chat_fn,
        tool_run=tool_run,
        list_tools_fn=list_tools_fn,
    )
    deadline = time.monotonic() + timeout
    with state.cv:
        while not state.done:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                state.cancel_event.set()
                state.cv.wait(timeout=5.0)
                break
            state.cv.wait(timeout=remaining)
    return AssistantTurn(
        turn_id=state.turn_id,
        conversation_id=state.conversation_id,
        final_message=state.final_message,
        tool_calls_summary=list(state.tool_calls_summary),
        aborted=any(e.type == "turn_aborted" for e in state.turn_events),
        abort_reason=next(
            (
                e.data.get("reason")
                for e in state.turn_events
                if e.type == "turn_aborted"
            ),
            None,
        ),
    )


def cancel_turn(turn_id: str) -> bool:
    """Signal a turn to abort. Idempotent. Returns False if turn unknown."""
    state = get_turn(turn_id)
    if state is None:
        return False
    state.cancel_event.set()
    return True


# ── Driver internals ───────────────────────────────────────────────────


def _new_turn_id() -> str:
    return uuid.uuid4().hex[:16]


_MAX_TOOL_LOOPS = 8


def _drive_turn(
    state: TurnState,
    user_message: str,
    model: Optional[str],
    chat_fn: ChatFn,
    tool_run: ToolRunFn,
    list_tools_fn: Callable[[], List[Dict[str, Any]]],
) -> None:
    """Core loop. Runs in its own thread; never raises."""
    try:
        _drive_turn_inner(state, user_message, model, chat_fn, tool_run, list_tools_fn)
    except CancelledError:
        _emit_turn(
            state, "turn_aborted", {"turn_id": state.turn_id, "reason": "cancelled"}
        )
    except Exception as exc:  # noqa: BLE001
        logger.exception("turn %s failed", state.turn_id)
        _emit_turn(
            state,
            "turn_aborted",
            {
                "turn_id": state.turn_id,
                "reason": "error",
                "error": f"{type(exc).__name__}: {exc}",
            },
        )
    finally:
        _set_tool_context(None)
        _mark_done(state)


def _drive_turn_inner(
    state: TurnState,
    user_message: str,
    model: Optional[str],
    chat_fn: ChatFn,
    tool_run: ToolRunFn,
    list_tools_fn: Callable[[], List[Dict[str, Any]]],
) -> None:
    # Set the per-thread tool context so memory tools dispatched during
    # this turn can read the active conversation_id / turn_id /
    # workspace_id without it being plumbed via params. Cleared in the
    # outer driver's finally block.
    _set_tool_context(
        ToolContext(
            conversation_id=state.conversation_id,
            turn_id=state.turn_id,
            workspace_id=state.workspace_id,
        )
    )

    tools_catalog = list_tools_fn() or []

    # Build the alias map once per turn so the salvage parser
    # (_extract_tool_calls_from_content) can resolve dotted / snake /
    # manifest-name variants to a canonical tool id without rebuilding
    # the map on every chat step.  Pulls from the registry's
    # list_tools() (canonical ids + dotted/snake aliases) and overlays
    # the LLM-facing list_tools_fn() catalog so tests with synthetic
    # tool lists work too.
    tool_alias_map = _resolve_tool_alias_map(tools_catalog)

    # Build the message list: system prompt (with all the memory slots),
    # prior conversation history (if any), then the new user message.
    # System messages are regenerated each turn — the conversations
    # layer strips them on save.
    prior_messages = _eot._load_conversation_history(state.conversation_id)
    system_prompt = _rb._build_system_prompt_with_slots(
        state=state,
        user_message=user_message,
        tools_catalog=tools_catalog,
        chat_fn=chat_fn,
        model=model,
    )
    messages: List[Dict[str, Any]] = [
        {"role": "system", "content": system_prompt},
        *prior_messages,
        {"role": "user", "content": user_message},
    ]

    for _ in range(_MAX_TOOL_LOOPS):
        _check_cancelled(state)

        # Option A: per-chunk streaming is disabled until the salvage
        # parser handles partial chunks.  Per-token emission via
        # _on_token would leak tool-call JSON character-by-character to
        # chat.stream_turn before the end-of-step parser can scrub it,
        # violating the architectural rule that tool calls never reach
        # the user-facing stream.  Re-enable once the parser learns
        # streaming-buffer classification.
        streamed_token = False
        streamed_thinking = False

        def _on_token(chunk: str) -> None:
            # Option A: streaming disabled until salvage parser handles
            # partial chunks.  Kept callable so older chat_fn impls that
            # ignore the kwarg still type-check.
            nonlocal streamed_token
            streamed_token = True
            _emit_turn(state, "token", {"turn_id": state.turn_id, "text": chunk})

        def _on_thinking(chunk: str) -> None:
            # Option A: streaming disabled until salvage parser handles
            # partial chunks.
            nonlocal streamed_thinking
            streamed_thinking = True
            _emit_turn(state, "thinking", {"turn_id": state.turn_id, "text": chunk})

        try:
            step = chat_fn(
                messages=messages,
                tools=tools_catalog,
                model=model,
            )
        except TypeError:
            # Synthetic chat_fns from older callers may not accept the
            # extra kwargs.  Fall back to the unary signature.
            step = chat_fn(messages=messages, tools=tools_catalog, model=model)

        # Salvage tool calls the model emitted as plain content rather
        # than the structured tool_calls field.  Scrub them out of
        # step.text so chat.stream_turn never sees raw call JSON, then
        # fold recovered calls into step.tool_calls below.
        #
        # Call via the submodule attribute so test patches on
        # ``turn._extract_tool_calls_from_content`` (which the package
        # __init__ re-exports) take effect.
        cleaned_text, recovered_calls, unrecognised_calls = (
            _str._extract_tool_calls_from_content(step.text, tool_alias_map)
        )
        step.text = cleaned_text
        for u in unrecognised_calls:
            _emit_tool(
                state,
                "tool_error",
                {
                    "turn_id": state.turn_id,
                    "call_id": "",
                    "name": u.get("name") or "",
                    "reason": "tool_call_text_unrecognised",
                    "error": (
                        f"model emitted tool call {u.get('name')!r} in content "
                        "but the name doesn't resolve to a known tool"
                    ),
                },
            )
        # Append recovered calls onto whatever structured tool_calls the
        # backend already produced; the dedupe step below collapses any
        # overlap.
        for r in recovered_calls:
            step.tool_calls.append(
                ToolCall(
                    id=r["id"],
                    name=r["name"],
                    args=r["args"],
                )
            )

        # Per-turn dedupe over (name, args-hash) — applies equally to
        # structured tool_calls and salvaged-from-text ones.  Duplicates
        # fire a tool_error with reason='tool_call_text_duplicate' and
        # are dropped from the dispatch list.
        deduped: List[ToolCall] = []
        for tc in step.tool_calls:
            h = _str._call_hash(tc.name, tc.args)
            if h in state._dispatched_call_hashes:
                _emit_tool(
                    state,
                    "tool_error",
                    {
                        "turn_id": state.turn_id,
                        "call_id": tc.id,
                        "name": tc.name,
                        "reason": "tool_call_text_duplicate",
                        "error": (
                            f"duplicate tool call {tc.name!r} suppressed (same "
                            "args as a prior call this turn)"
                        ),
                    },
                )
                continue
            state._dispatched_call_hashes.add(h)
            deduped.append(tc)
        step.tool_calls = deduped

        if step.thinking and not streamed_thinking:
            _emit_turn(
                state, "thinking", {"turn_id": state.turn_id, "text": step.thinking}
            )

        if not step.tool_calls:
            # No tool calls — emit any final text as a token chunk
            # (only if the chat_fn didn't already stream it) and wrap up.
            if step.text and not streamed_token:
                _emit_turn(
                    state, "token", {"turn_id": state.turn_id, "text": step.text}
                )
            state.final_message = step.text
            messages.append({"role": "assistant", "content": step.text})
            _eot._persist_conversation(state, messages, model)
            # Architectural sweep over the files this turn touched —
            # ERROR findings fire as a tool_warning on tool_events.
            # See module docstring of write_file / edit_file for why
            # the lint moved from per-write to end-of-turn.  Wrapped so
            # a checker crash never blocks the turn_complete event.
            try:
                _eot._run_end_of_turn_architectural_check(state)
            except Exception:  # noqa: BLE001
                logger.exception(
                    "turn: end-of-turn architectural check raised for %s",
                    state.turn_id,
                )
            _emit_turn(
                state,
                "turn_complete",
                {"turn_id": state.turn_id, "final_message": step.text},
            )
            # Kick off the post-turn extractor on a daemon thread so
            # the user-facing stream doesn't wait. The extractor reads
            # the conversation we just persisted, asks the model what's
            # worth promoting, and writes verdicts to long-term /
            # workspace memory. Each write fires a memory_written event
            # on state.tool_events so the GUI can render auto-writes.
            _eot._maybe_fire_post_turn_extractor(state, model)
            return

        # Mid-stream text from the assistant before the tool call (e.g. "let me
        # look that up"). Same double-emit guard as the no-tool-calls path.
        if step.text and not streamed_token:
            _emit_turn(state, "token", {"turn_id": state.turn_id, "text": step.text})

        # Persist the assistant message that contained the tool calls so the
        # next chat call has the right context.
        messages.append(
            {
                "role": "assistant",
                "content": step.text,
                "tool_calls": [
                    {"id": tc.id, "function": {"name": tc.name, "arguments": tc.args}}
                    for tc in step.tool_calls
                ],
            }
        )

        for call in step.tool_calls:
            _check_cancelled(state)
            _run_one_tool(state, call, tool_run, messages)
            # Track in short-term memory so the LLM doesn't repeat the
            # same tool call later in the conversation. Best-effort —
            # if the conversation store fails, the turn keeps going.
            _record_short_term(state, call)

    # Hit the loop cap — model kept asking for tools without converging.
    _emit_turn(
        state,
        "turn_aborted",
        {
            "turn_id": state.turn_id,
            "reason": "tool_loop_limit",
            "error": f"exceeded {_MAX_TOOL_LOOPS} tool-call iterations without a final response",
        },
    )
