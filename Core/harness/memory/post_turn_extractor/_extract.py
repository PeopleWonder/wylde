"""Public entry points + verdict / result dataclasses.

This submodule owns the public surface:

* :class:`Verdict` and :class:`ExtractionResult` dataclasses.
* :func:`extract_post_turn` — synchronous extraction over the last turn.
* :func:`fire_in_background` — daemon-thread wrapper used by the turn
  driver after ``turn_complete``.

Orchestration only: prompt building, dedup, and LLM-text parsing live
in ``._prompt``; the verdict → memory-store mutation lives in
``._persist``.
"""

from __future__ import annotations

import logging
import threading
from dataclasses import dataclass, field
from typing import Any, Callable, Dict, List, Optional

from .. import conversation as _conversation
from ._persist import _apply_verdict
from ._prompt import (
    _MAX_VERDICTS,
    _ask,
    _build_context,
    _is_near_duplicate,
    _parse_verdicts,
)

logger = logging.getLogger("wylde.harness.memory.post_turn_extractor")


# ── Result / verdict types ─────────────────────────────────────────────


@dataclass
class Verdict:
    action: str
    body: str = ""
    importance: int = 5
    target_id: str = ""
    raw: Dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> Dict[str, Any]:
        return {
            "action": self.action,
            "body": self.body,
            "importance": int(self.importance),
            "target_id": self.target_id,
            "raw": dict(self.raw),
        }


@dataclass
class ExtractionResult:
    conversation_id: str
    turn_id: str
    verdicts: List[Verdict] = field(default_factory=list)
    written: List[Dict[str, Any]] = field(default_factory=list)
    skipped: bool = False
    skip_reason: str = ""

    def to_dict(self) -> Dict[str, Any]:
        return {
            "conversation_id": self.conversation_id,
            "turn_id": self.turn_id,
            "verdicts": [v.to_dict() for v in self.verdicts],
            "written": list(self.written),
            "skipped": self.skipped,
            "skip_reason": self.skip_reason,
        }


# ── Public surface ─────────────────────────────────────────────────────


def extract_post_turn(
    conversation_id: str,
    turn_id: str,
    *,
    chat_fn: Optional[Callable[..., Any]] = None,
    model: Optional[str] = None,
    on_memory_written: Optional[Callable[..., None]] = None,
    already_saved: Optional[List[Dict[str, Any]]] = None,
) -> ExtractionResult:
    """Run one extraction pass over the just-completed turn.

    ``chat_fn`` follows the same signature the scheduler uses
    (``messages, tools, model`` kwargs, returns an object with ``.text``).
    ``on_memory_written(record, scope)`` is called for each write so the
    caller can fire ``memory_written`` events on its tool stream.

    ``already_saved`` lists memories the LLM explicitly saved during the
    same turn (each ``{memory_id, body}``). The extractor's prompt is
    told not to re-save substantially-similar content, and a
    token-overlap guard in :func:`_apply_verdict` catches misses on
    small models that ignore the instruction.

    Returns an :class:`ExtractionResult` describing what was written.
    Failures are surfaced as ``skipped=True`` with a reason; the function
    never raises.
    """
    if not isinstance(conversation_id, str) or not conversation_id:
        return ExtractionResult(
            conversation_id="",
            turn_id=turn_id,
            skipped=True,
            skip_reason="missing conversation_id",
        )
    if chat_fn is None:
        return ExtractionResult(
            conversation_id=conversation_id,
            turn_id=turn_id,
            skipped=True,
            skip_reason="no chat_fn supplied",
        )

    context = _build_context(conversation_id, turn_id, already_saved=already_saved)
    if not context:
        return ExtractionResult(
            conversation_id=conversation_id,
            turn_id=turn_id,
            skipped=True,
            skip_reason="conversation has no recent activity",
        )

    text = _ask(chat_fn, context, model)
    if not text:
        return ExtractionResult(
            conversation_id=conversation_id,
            turn_id=turn_id,
            skipped=True,
            skip_reason="extractor returned empty",
        )

    verdicts = _parse_verdicts(text)
    if not verdicts:
        return ExtractionResult(
            conversation_id=conversation_id,
            turn_id=turn_id,
            skipped=True,
            skip_reason="no parseable verdicts",
        )

    workspace_id = _conversation.get_workspace(conversation_id)
    already_bodies = [
        str(s.get("body") or "")
        for s in (already_saved or [])
        if isinstance(s, dict) and (s.get("body") or "").strip()
    ]
    written: List[Dict[str, Any]] = []
    for v in verdicts[:_MAX_VERDICTS]:
        # Defense-in-depth dedup: skip the verdict if its body
        # substantially overlaps with anything the LLM already saved
        # this turn. The prompt asks the model to dedupe; this catches
        # small models that ignore the instruction.
        if v.action != "noop" and v.body and _is_near_duplicate(v.body, already_bodies):
            logger.info(
                "post_turn_extractor: skipping near-duplicate verdict "
                "(body=%r matches already-saved this turn)",
                v.body[:80],
            )
            continue
        try:
            row = _apply_verdict(v, conversation_id, turn_id, workspace_id)
        except Exception as exc:  # noqa: BLE001
            logger.exception("post_turn_extractor: apply failed: %s", exc)
            continue
        if row is not None:
            written.append(row)
            # Future verdicts in this same pass shouldn't duplicate this
            # one either — append the just-written body so the guard
            # catches a model that emits the same fact twice.
            already_bodies.append(row.get("body") or "")
            if on_memory_written is not None:
                try:
                    on_memory_written(record=row, scope=row.get("scope"))
                except Exception:  # noqa: BLE001
                    logger.exception("post_turn_extractor: on_memory_written raised")

    return ExtractionResult(
        conversation_id=conversation_id,
        turn_id=turn_id,
        verdicts=verdicts,
        written=written,
        skipped=not bool(written),
        skip_reason="all verdicts were noop / failed / dupe" if not written else "",
    )


def fire_in_background(
    conversation_id: str,
    turn_id: str,
    *,
    chat_fn: Optional[Callable[..., Any]],
    model: Optional[str] = None,
    on_memory_written: Optional[Callable[..., None]] = None,
    already_saved: Optional[List[Dict[str, Any]]] = None,
) -> threading.Thread:
    """Fire :func:`extract_post_turn` on a daemon thread and return the
    thread handle. Used by the turn driver after ``turn_complete``;
    chat-turn callers don't wait on it."""
    t = threading.Thread(
        target=_run_safely,
        args=(
            conversation_id,
            turn_id,
            chat_fn,
            model,
            on_memory_written,
            already_saved,
        ),
        name=f"post-turn-extract-{turn_id[:8]}",
        daemon=True,
    )
    t.start()
    return t


def _run_safely(
    conversation_id: str,
    turn_id: str,
    chat_fn: Optional[Callable[..., Any]],
    model: Optional[str],
    on_memory_written: Optional[Callable[..., None]],
    already_saved: Optional[List[Dict[str, Any]]] = None,
) -> None:
    try:
        result = extract_post_turn(
            conversation_id,
            turn_id,
            chat_fn=chat_fn,
            model=model,
            on_memory_written=on_memory_written,
            already_saved=already_saved,
        )
        if result.skipped:
            logger.info(
                "post_turn_extractor: %s/%s skipped (%s)",
                conversation_id,
                turn_id,
                result.skip_reason,
            )
        else:
            logger.info(
                "post_turn_extractor: %s/%s wrote %d entries",
                conversation_id,
                turn_id,
                len(result.written),
            )
    except Exception:  # noqa: BLE001
        logger.exception(
            "post_turn_extractor: background run for %s/%s crashed",
            conversation_id,
            turn_id,
        )
