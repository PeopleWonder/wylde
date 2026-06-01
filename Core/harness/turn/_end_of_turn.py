"""End-of-turn wiring — post-turn extractor trigger, conversation
load/save, and the per-file architectural check sweep over files written
during the turn.

The driver calls these once a turn reaches ``turn_complete`` (the model
produced a final answer without further tool calls). Failures here are
swallowed — by the time we get here the user has already seen the
assistant's reply, so tearing the turn down would be net-negative.
"""

from __future__ import annotations

import logging
from typing import Any, Callable, Dict, List, Optional

from ._state import TurnState, _emit_tool
from ._tool_round import emit_memory_written

logger = logging.getLogger("wylde.harness.turn")


# ── Post-turn extractor wiring ─────────────────────────────────────────


# Test seam: when set, ``_maybe_fire_post_turn_extractor`` uses this
# instead of building the production chat_fn. Tests assign a synthetic
# chat_fn here to drive the extractor deterministically.
_post_turn_chat_fn_override: Optional[Callable[..., Any]] = None
# When True, the extractor runs synchronously instead of on a daemon
# thread. Tests flip this on so they can assert against the resulting
# memory rows without polling.
_post_turn_synchronous = False


def install_post_turn_extractor_chat_fn(
    chat_fn: Optional[Callable[..., Any]],
    *,
    synchronous: bool = False,
) -> None:
    """Test seam: replace the production chat_fn the post-turn
    extractor would normally fetch from the scheduler. Pass ``None``
    to clear. ``synchronous=True`` makes the extractor run on the
    driver thread so test assertions don't race the daemon thread."""
    global _post_turn_chat_fn_override, _post_turn_synchronous
    _post_turn_chat_fn_override = chat_fn
    _post_turn_synchronous = bool(synchronous)


def _resolve_post_turn_chat_fn() -> Optional[Callable[..., Any]]:
    """Pick the chat_fn the extractor will use.

    Test override wins; production falls back to
    :func:`Core.harness.memory.scheduler.default_chat_fn` (the same
    factory the reflection scheduler uses). If neither is available,
    the extractor skips with ``no chat_fn supplied``.
    """
    if _post_turn_chat_fn_override is not None:
        return _post_turn_chat_fn_override
    try:
        from ..memory.scheduler import default_chat_fn
    except ImportError:
        try:
            from Core.harness.memory.scheduler import default_chat_fn
        except ImportError:
            return None
    try:
        return default_chat_fn()
    except Exception:  # noqa: BLE001
        logger.exception("turn: post-turn extractor chat_fn factory raised")
        return None


def _collect_llm_saves(state: "TurnState") -> List[Dict[str, Any]]:
    """Pull every ``memory_written`` event with ``source="llm_tool"``
    that fired during this turn — the LLM-driven explicit saves. The
    extractor uses this to dedupe its own verdicts so a turn that
    explicitly writes ``X`` doesn't get a near-identical auto-write
    for the same content moments later."""
    saves: List[Dict[str, Any]] = []
    # Snapshot under the cv to avoid racing with any late
    # _emit_tool calls — the driver thread is the only writer at this
    # point but we still take the lock for defensiveness.
    with state.cv:
        events = list(state.tool_events)
    for ev in events:
        if ev.type != "memory_written":
            continue
        if ev.data.get("source") != "llm_tool":
            continue
        body = ev.data.get("body") or ""
        memory_id = ev.data.get("memory_id") or ""
        if body or memory_id:
            saves.append({"memory_id": memory_id, "body": body})
    return saves


def _run_end_of_turn_architectural_check(state: "TurnState") -> None:
    """Re-examine the files this turn wrote against the per-file
    ``wylde_check`` rules and surface ERROR findings as a
    ``tool_warning`` event on ``state.tool_events``.

    Replaces the legacy per-write block in ``write_file`` / ``edit_file``
    (see those modules' docstrings).  Runs the single-file fast path
    (:func:`Core.harness.dev.wylde_check.check_one_file`), not the full
    tree sweep — cross-file rules (manifest_paths, action_registry,
    gateway_scope, gui_*, etc.) belong in the end-of-task verification
    gate, not on every turn.

    Best-effort — wrapped by the caller's try/except so a checker
    crash never tears the turn down after the user already saw the
    assistant's reply.  Architectural ``warning`` findings are dropped
    here on purpose: they're low signal per-file and would flood the
    tool stream with noise.  The Stop-event hook on the Claude Code
    side still surfaces them at end-of-task.
    """
    from pathlib import Path as _Path

    paths = list(state.files_written)
    if not paths:
        return
    try:
        from ..dev import prewrite as _prewrite
        from ..dev import wylde_check as _wc
    except ImportError:
        try:
            from Core.harness.dev import prewrite as _prewrite
            from Core.harness.dev import wylde_check as _wc
        except ImportError:
            return

    error_findings: List[Dict[str, Any]] = []
    for path_str in paths:
        try:
            rel = _prewrite.normalise_path(path_str)
            content = _Path(path_str).read_text(encoding="utf-8")
        except OSError:
            # File deleted / unreadable post-write — nothing to check.
            continue
        try:
            result = _wc.check_one_file(rel, content)
        except Exception:  # noqa: BLE001
            logger.exception("turn: wylde_check.check_one_file raised for %s", path_str)
            continue
        if not result.get("ok"):
            continue
        for f in (result.get("data") or {}).get("findings") or []:
            if f.get("severity") == "error":
                error_findings.append(f)

    if not error_findings:
        return
    _emit_tool(
        state,
        "tool_warning",
        {
            "turn_id": state.turn_id,
            "source": "wylde_check_end_of_turn",
            "findings": error_findings[:20],
            "truncated": len(error_findings) > 20,
            "files_checked": paths,
        },
    )


def _maybe_fire_post_turn_extractor(
    state: "TurnState",
    model: Optional[str],
) -> None:
    """Trigger the extractor after a clean turn_complete.

    Best-effort — failures here must NEVER bubble out of the driver.
    The extractor itself is wrapped in its own try/except, but we
    swallow lookup / import errors at this seam too.
    """
    try:
        from ..memory import post_turn_extractor as _pte
    except ImportError:
        try:
            from Core.harness.memory import post_turn_extractor as _pte
        except ImportError:
            return

    chat_fn = _resolve_post_turn_chat_fn()
    if chat_fn is None:
        # No production chat_fn (e.g., Ollama not configured) and no
        # test override. Nothing to do — the extractor would skip
        # anyway, save the import.
        return

    already_saved = _collect_llm_saves(state)

    def _on_write(*, record: Dict[str, Any], scope: Optional[str] = None) -> None:
        emit_memory_written(
            state,
            source="auto",
            scope=str(scope or record.get("scope") or "long_term"),
            memory_id=str(record.get("memory_id") or record.get("id") or ""),
            body=str(record.get("body") or ""),
            importance=int(record.get("importance") or 5),
            extra={"action": str(record.get("action") or "")},
        )

    if _post_turn_synchronous:
        try:
            _pte.extract_post_turn(
                state.conversation_id,
                state.turn_id,
                chat_fn=chat_fn,
                model=model,
                on_memory_written=_on_write,
                already_saved=already_saved,
            )
        except Exception:  # noqa: BLE001
            logger.exception(
                "turn: synchronous post-turn extractor crashed for %s",
                state.turn_id,
            )
        return

    try:
        _pte.fire_in_background(
            state.conversation_id,
            state.turn_id,
            chat_fn=chat_fn,
            model=model,
            on_memory_written=_on_write,
            already_saved=already_saved,
        )
    except Exception:  # noqa: BLE001
        logger.exception(
            "turn: post-turn extractor spawn failed for %s",
            state.turn_id,
        )


# ── Conversation IO ───────────────────────────────────────────────────


def _conversation_module() -> Any:
    """Lazy import so tests that don't touch storage don't pay for it.

    Falls back to ``None`` if the module isn't importable (e.g. on a
    minimal env). Callers handle that path by skipping load / save.
    """
    try:
        from ..memory import conversation as _conv

        return _conv
    except Exception:
        try:
            from Core.harness.memory import conversation as _conv

            return _conv
        except Exception:
            return None


def _load_conversation_history(conversation_id: str) -> List[Dict[str, Any]]:
    """Read prior messages for ``conversation_id`` and return them
    in wire shape, stripped of system messages (those are regenerated
    each turn from the live tool catalog).

    Returns ``[]`` for unknown / unreadable ids and for environments
    where the conversations module isn't importable. This is the
    "single-shot turn" fallback the driver had before history was wired.
    """
    if not isinstance(conversation_id, str) or not conversation_id:
        return []
    conv = _conversation_module()
    if conv is None:
        return []
    try:
        doc = conv.read_conversation(conversation_id)
    except conv.ConversationNotFound:
        return []
    except conv.InvalidConversationId:
        # Caller passed an id that won't pass the validator (e.g.
        # contains "/" or "?"). Don't refuse the turn — just skip
        # loading and let the caller pick a fresh id next time.
        logger.warning(
            "turn: conversation id %r failed validation; skipping history load",
            conversation_id,
        )
        return []
    except Exception as exc:  # noqa: BLE001
        logger.warning("turn: history load failed for %s: %s", conversation_id, exc)
        return []
    msgs = doc.get("messages") if isinstance(doc, dict) else None
    if not isinstance(msgs, list):
        return []
    return [m for m in msgs if isinstance(m, dict) and m.get("role") != "system"]


def _persist_conversation(
    state: TurnState,
    messages: List[Dict[str, Any]],
    model: Optional[str],
) -> None:
    """Save the running conversation after a turn completes.

    The driver passes the full wire-shape message list including the
    fresh system prompt; ``save_conversation`` strips system messages
    before writing. Fatal save errors are logged and swallowed — a
    failed save is bad but shouldn't tear the turn down after the user
    already saw the assistant's reply.
    """
    if not isinstance(state.conversation_id, str) or not state.conversation_id:
        return
    conv = _conversation_module()
    if conv is None:
        return
    try:
        conv.save_conversation(
            conv_id=state.conversation_id,
            messages=messages,
            model=model,
        )
    except conv.InvalidConversationId:
        # Same fallback as the load path — skip rather than abort.
        logger.warning(
            "turn: conversation id %r failed validation; skipping history save",
            state.conversation_id,
        )
    except Exception as exc:  # noqa: BLE001
        logger.warning(
            "turn: history save failed for %s: %s", state.conversation_id, exc
        )
