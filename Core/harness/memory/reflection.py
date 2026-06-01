"""Reflection / consolidation cycles.

Periodically the LLM scans recent low-level memories and synthesises
higher-level insights. The synthesised reflection becomes a NEW
long-term (or workspace) memory with importance >= the inputs', and
the inputs are marked ``superseded_by`` the reflection so they fade
from default retrieval but remain visible in the Settings history.

This module provides the cycle as a single callable function — both
the harness pipe action ``memory.reflect`` and any future scheduler
hook drive it the same way.

Scheduling is the caller's responsibility (the chat-turn driver does
NOT run this inline — reflection involves an LLM call which would
wreck turn latency). Recommended trigger points:

* Conversation close — when the user archives or switches away from a
  conversation.
* Idle window — a background timer fires after N minutes of no
  activity. Implement this once a daemon-side scheduler exists.
* Manual — Settings UI button "Consolidate memories now".
"""

from __future__ import annotations

import logging
import time
from dataclasses import dataclass
from typing import Any, Callable, Dict, List, Optional, Tuple

from . import long_term as _long_term
from . import workspace_memory as _ws_mem
from . import scoring as _scoring
from . import conversation as _conversation
from . import workspaces as _workspaces
from .long_term import LongTermMemory
from .workspace_memory import WorkspaceMemory

logger = logging.getLogger("wylde.harness.memory.reflection")


# Default window: synthesise from the last 24 hours of non-superseded,
# non-reflection memories. The "non-reflection" guard prevents an
# infinite escalator where reflections of reflections of reflections
# inflate forever.
DEFAULT_WINDOW_DAYS = 1.0
REFLECTION_TAG = "reflection"


# ── Reflection prompt ──────────────────────────────────────────────────


_REFLECTION_SYSTEM = (
    "You are a memory consolidator. You read a list of low-level memories "
    "and produce ONE higher-level insight that summarises them — a stable "
    "fact or pattern the system should remember as a single unit going "
    "forward. Output one paragraph, no list markers, no preamble. If the "
    "inputs don't share a coherent theme worth promoting, output the literal "
    "string NOTHING."
)


def _format_inputs(inputs: List[Dict[str, Any]]) -> str:
    lines = []
    for i, m in enumerate(inputs, start=1):
        body = m.get("body") or ""
        importance = m.get("importance", 5)
        lines.append(f"{i}. (importance {importance}) {body}")
    return "\n".join(lines)


# ── Public surface ─────────────────────────────────────────────────────


@dataclass
class ReflectionResult:
    scope: str
    inputs_considered: int
    reflection_id: Optional[str]
    reflection_body: str
    superseded_ids: List[str]
    skipped: bool = False
    skip_reason: str = ""

    def to_dict(self) -> Dict[str, Any]:
        return {
            "scope": self.scope,
            "inputs_considered": self.inputs_considered,
            "reflection_id": self.reflection_id,
            "reflection_body": self.reflection_body,
            "superseded_ids": list(self.superseded_ids),
            "skipped": self.skipped,
            "skip_reason": self.skip_reason,
        }


def reflect(
    scope: str,
    *,
    chat_fn: Optional[Callable[..., Any]] = None,
    model: Optional[str] = None,
    window_days: float = DEFAULT_WINDOW_DAYS,
    min_inputs: int = 3,
) -> ReflectionResult:
    """Run one consolidation cycle for ``scope``.

    ``scope`` is one of:
    * ``"long_term"`` — synthesise across all global memories.
    * ``"workspace:<id>"`` — synthesise within one workspace's memory.
    * ``"conversation:<id>"`` — placeholder; conversation-scoped
      reflection over the short-term log isn't promoted to a stored
      memory layer (working memory dies with the conversation), so this
      currently returns a no-op result.

    Returns a :class:`ReflectionResult`. If ``chat_fn`` isn't supplied
    (or the model output is "NOTHING" / empty), no reflection is
    written; the result carries ``skipped=True`` with a reason.
    """
    scope = (scope or "").strip()
    if not scope:
        return ReflectionResult(
            scope="",
            inputs_considered=0,
            reflection_id=None,
            reflection_body="",
            superseded_ids=[],
            skipped=True,
            skip_reason="empty scope",
        )

    if chat_fn is None:
        return ReflectionResult(
            scope=scope,
            inputs_considered=0,
            reflection_id=None,
            reflection_body="",
            superseded_ids=[],
            skipped=True,
            skip_reason="no chat_fn supplied",
        )

    if scope == "long_term":
        return _reflect_long_term(
            chat_fn=chat_fn, model=model, window_days=window_days, min_inputs=min_inputs
        )
    if scope.startswith("workspace:"):
        ws_id = scope.split(":", 1)[1]
        return _reflect_workspace(
            workspace_id=ws_id,
            chat_fn=chat_fn,
            model=model,
            window_days=window_days,
            min_inputs=min_inputs,
        )
    if scope.startswith("conversation:"):
        conv_id = scope.split(":", 1)[1]
        return _reflect_conversation(
            conversation_id=conv_id,
            chat_fn=chat_fn,
            model=model,
            min_inputs=min_inputs,
        )
    return ReflectionResult(
        scope=scope,
        inputs_considered=0,
        reflection_id=None,
        reflection_body="",
        superseded_ids=[],
        skipped=True,
        skip_reason=f"unknown scope {scope!r}",
    )


def _select_inputs_long_term(*, window_days: float) -> List[Any]:
    """Pull recent, non-reflection long-term memories within the window."""
    now = time.time()
    cutoff = now - window_days * _scoring.SECONDS_PER_DAY
    out = []
    for r in _long_term.list_records(include_superseded=False):
        if r.last_used_at < cutoff:
            continue
        if REFLECTION_TAG in r.tags:
            continue
        out.append(r)
    return out


def _select_inputs_workspace(workspace_id: str, *, window_days: float) -> List[Any]:
    now = time.time()
    cutoff = now - window_days * _scoring.SECONDS_PER_DAY
    out = []
    for r in _ws_mem.list_records(workspace_id, include_superseded=False):
        if r.last_used_at < cutoff:
            continue
        # Workspace memories don't carry a `tags` field; we use the
        # source string as a coarse "is this already a reflection?"
        # signal — reflections are tagged via the source field below.
        if r.source.startswith("reflection:"):
            continue
        out.append(r)
    return out


def _ask_model(
    chat_fn: Callable[..., Any],
    inputs_block: str,
    model: Optional[str],
) -> str:
    messages = [
        {"role": "system", "content": _REFLECTION_SYSTEM},
        {"role": "user", "content": inputs_block},
    ]
    try:
        step = chat_fn(messages=messages, tools=[], model=model)
    except Exception as exc:  # noqa: BLE001
        logger.warning("reflection: chat_fn raised: %s", exc)
        return ""
    text = getattr(step, "text", "")
    return (text or "").strip()


def _reflect_long_term(
    *,
    chat_fn: Any,
    model: Any,
    window_days: float,
    min_inputs: int,
) -> ReflectionResult:
    inputs = _select_inputs_long_term(window_days=window_days)
    if len(inputs) < min_inputs:
        return ReflectionResult(
            scope="long_term",
            inputs_considered=len(inputs),
            reflection_id=None,
            reflection_body="",
            superseded_ids=[],
            skipped=True,
            skip_reason=f"need {min_inputs} inputs, have {len(inputs)}",
        )
    inputs_block = _format_inputs([r.to_dict() for r in inputs])
    text = _ask_model(chat_fn, inputs_block, model)
    if not text or text.upper() == "NOTHING":
        return ReflectionResult(
            scope="long_term",
            inputs_considered=len(inputs),
            reflection_id=None,
            reflection_body="",
            superseded_ids=[],
            skipped=True,
            skip_reason=f"model declined: {text or '(empty)'}",
        )
    # Importance: max(input_importances, 7) — a synthesis should be at
    # least as load-bearing as the most-important input it summarises,
    # bumped to "reflection-worthy" floor.
    importance = max(7, max((r.importance for r in inputs), default=7))
    new_record = _long_term.save(
        body=text,
        source="reflection:long_term",
        importance=importance,
        tags=[REFLECTION_TAG],
    )
    superseded_ids: List[str] = []
    for r in inputs:
        result = _long_term.update(
            r.id,
            body=r.body,
            importance=r.importance,
        )
        # update() supersedes via its own logic; we want the original to
        # point at the reflection instead. Re-tag.
        if result is not None:
            # Roll the original's superseded_by to the reflection id by
            # writing the long_term JSON directly. This is the only
            # surgical hook we have; the public API only supports
            # supersession via update which would create a redundant
            # copy.
            _link_supersession(r.id, new_record.id)
            superseded_ids.append(r.id)
            # Roll back the redundant copy update created.
            _long_term.delete(result.id)
    return ReflectionResult(
        scope="long_term",
        inputs_considered=len(inputs),
        reflection_id=new_record.id,
        reflection_body=text,
        superseded_ids=superseded_ids,
    )


def _reflect_workspace(
    *,
    workspace_id: str,
    chat_fn: Any,
    model: Any,
    window_days: float,
    min_inputs: int,
) -> ReflectionResult:
    inputs = _select_inputs_workspace(workspace_id, window_days=window_days)
    if len(inputs) < min_inputs:
        return ReflectionResult(
            scope=f"workspace:{workspace_id}",
            inputs_considered=len(inputs),
            reflection_id=None,
            reflection_body="",
            superseded_ids=[],
            skipped=True,
            skip_reason=f"need {min_inputs} inputs, have {len(inputs)}",
        )
    inputs_block = _format_inputs([r.to_dict() for r in inputs])
    text = _ask_model(chat_fn, inputs_block, model)
    if not text or text.upper() == "NOTHING":
        return ReflectionResult(
            scope=f"workspace:{workspace_id}",
            inputs_considered=len(inputs),
            reflection_id=None,
            reflection_body="",
            superseded_ids=[],
            skipped=True,
            skip_reason=f"model declined: {text or '(empty)'}",
        )
    importance = max(7, max((r.importance for r in inputs), default=7))
    # Union the entity sets so the consolidation keeps Memgraph edges.
    all_entities: List[str] = []
    seen: set = set()
    for r in inputs:
        for e in r.entities:
            if e not in seen:
                seen.add(e)
                all_entities.append(e)
    new_record = _ws_mem.save(
        workspace_id=workspace_id,
        body=text,
        source="reflection:workspace",
        importance=importance,
        entities=all_entities,
    )
    superseded_ids: List[str] = []
    for r in inputs:
        _link_supersession_ws(workspace_id, r.id, new_record.id)
        superseded_ids.append(r.id)
    return ReflectionResult(
        scope=f"workspace:{workspace_id}",
        inputs_considered=len(inputs),
        reflection_id=new_record.id,
        reflection_body=text,
        superseded_ids=superseded_ids,
    )


# ── Conversation-scoped reflection ─────────────────────────────────────
#
# Working memory entries the chat-turn driver wrote ("ran tool X",
# "decided Y", "summarised Z") get distilled into ONE durable insight.
# Where it lands depends on whether the conversation is bound to a
# workspace:
#
# * conversation.get_workspace(conv_id) → some workspace_id that's
#   still in the registry  →  workspace_memory.save(workspace_id, ...)
# * otherwise (no binding, or the workspace was evicted)
#                                          →  long_term.save(...)
#
# After the synthesis is written, the consumed working-memory entries
# get a ``superseded_by`` marker pointing at the new record. The
# chat-turn driver's short-term slot filters those out so the next
# turn's prompt sees the synthesis instead of the raw breadcrumbs.


def _select_inputs_conversation(conversation_id: str) -> List[Dict[str, Any]]:
    """Pull non-superseded working-memory entries from the conversation."""
    try:
        entries = _conversation.get_working_memory(conversation_id)
    except Exception:  # noqa: BLE001
        return []
    return [e for e in entries if not e.get("superseded_by")]


def _format_conversation_inputs(entries: List[Dict[str, Any]]) -> str:
    """Render working-memory entries the same way ``_format_inputs``
    renders memory records — one numbered line per entry. Working memory
    entries don't carry importance, so we tag them with their kind."""
    lines = []
    for i, e in enumerate(entries, start=1):
        kind = e.get("kind") or "raw"
        data = e.get("data")
        if isinstance(data, dict):
            # Render a tool entry as "ran tool X(args=..)" and other
            # dicts as a compact key=value strip — the synthesis prompt
            # cares about the gist, not the JSON shape.
            if kind == "tool":
                name = data.get("name") or "?"
                lines.append(f"{i}. ({kind}) ran tool {name}")
            else:
                bits = []
                for k, v in list(data.items())[:4]:
                    bits.append(f"{k}={str(v)[:80]}")
                lines.append(f"{i}. ({kind}) " + ", ".join(bits))
        else:
            text = str(data) if data is not None else ""
            lines.append(f"{i}. ({kind}) {text[:200]}")
    return "\n".join(lines)


def _conversation_target(conversation_id: str) -> Tuple[str, str]:
    """Resolve where a conversation reflection should land.

    Returns ``("workspace", workspace_id)`` when the conversation is
    bound to a workspace that's still present in the registry, or
    ``("long_term", "")`` otherwise. A binding to a since-evicted
    workspace falls back to long-term — the user's intent ("durable
    insight from this chat") still holds even if the workspace went
    away.
    """
    try:
        ws_id = _conversation.get_workspace(conversation_id)
    except Exception:  # noqa: BLE001
        ws_id = ""
    if not ws_id:
        return ("long_term", "")
    try:
        record = _workspaces.get_workspace(ws_id)
    except Exception:  # noqa: BLE001
        record = None
    if record is None:
        return ("long_term", "")
    return ("workspace", ws_id)


def _supersede_working_memory(
    conversation_id: str,
    consumed: List[Dict[str, Any]],
    reflection_id: str,
) -> None:
    """Mark each entry in ``consumed`` as superseded by ``reflection_id``.

    Working-memory entries are dicts without stable ids; we match on
    object identity within the same list (entries we just read from
    ``get_working_memory`` are copies, so we instead match by index +
    rendered shape). Practical approach: rewrite the whole list,
    flagging anything that compares equal to a consumed entry.
    """
    try:
        existing = _conversation.get_working_memory(conversation_id)
    except Exception:  # noqa: BLE001
        return
    if not existing:
        return
    consumed_signatures = [_entry_signature(e) for e in consumed]
    updated: List[Dict[str, Any]] = []
    for entry in existing:
        if not isinstance(entry, dict):
            updated.append(entry)
            continue
        if entry.get("superseded_by"):
            updated.append(entry)
            continue
        if _entry_signature(entry) in consumed_signatures:
            new_entry = dict(entry)
            new_entry["superseded_by"] = reflection_id
            updated.append(new_entry)
        else:
            updated.append(entry)
    try:
        doc = _conversation.read_conversation(conversation_id)
    except Exception:  # noqa: BLE001
        return
    try:
        _conversation.save_conversation(
            conv_id=doc["id"],
            messages=doc.get("messages") or [],
            title=doc.get("title"),
            model=doc.get("model"),
            workspace_id=doc.get("workspace_id"),
            working_memory=updated,
        )
    except Exception as exc:  # noqa: BLE001
        logger.warning(
            "reflection: working-memory rewrite failed for %s: %s",
            conversation_id,
            exc,
        )


def _entry_signature(entry: Dict[str, Any]) -> Tuple[Any, ...]:
    """Coarse fingerprint for matching working-memory entries before /
    after the supersession write. Includes ``kind``, ``at``, and a
    JSON-stable string of ``data`` so two entries with the same shape
    don't collide unless they're literally the same record."""
    import json as _json

    try:
        data_repr = _json.dumps(entry.get("data"), sort_keys=True, default=str)
    except Exception:  # noqa: BLE001
        data_repr = str(entry.get("data"))
    return (entry.get("kind"), entry.get("at"), data_repr)


def _reflect_conversation(
    *,
    conversation_id: str,
    chat_fn: Any,
    model: Any,
    min_inputs: int,
) -> ReflectionResult:
    scope = f"conversation:{conversation_id}"
    if not conversation_id:
        return ReflectionResult(
            scope=scope,
            inputs_considered=0,
            reflection_id=None,
            reflection_body="",
            superseded_ids=[],
            skipped=True,
            skip_reason="empty conversation_id",
        )

    inputs = _select_inputs_conversation(conversation_id)
    if len(inputs) < min_inputs:
        return ReflectionResult(
            scope=scope,
            inputs_considered=len(inputs),
            reflection_id=None,
            reflection_body="",
            superseded_ids=[],
            skipped=True,
            skip_reason=f"need {min_inputs} inputs, have {len(inputs)}",
        )

    inputs_block = _format_conversation_inputs(inputs)
    text = _ask_model(chat_fn, inputs_block, model)
    if not text or text.upper() == "NOTHING":
        return ReflectionResult(
            scope=scope,
            inputs_considered=len(inputs),
            reflection_id=None,
            reflection_body="",
            superseded_ids=[],
            skipped=True,
            skip_reason=f"model declined: {text or '(empty)'}",
        )

    target, target_workspace = _conversation_target(conversation_id)

    # Importance: working-memory entries don't carry one; default to 7
    # (reflection-worthy floor used by long_term + workspace branches).
    importance = 7

    new_record: LongTermMemory | WorkspaceMemory
    if target == "workspace":
        new_record = _ws_mem.save(
            workspace_id=target_workspace,
            body=text,
            source=f"reflection:conversation:{conversation_id}",
            importance=importance,
        )
    else:
        new_record = _long_term.save(
            body=text,
            source=f"reflection:conversation:{conversation_id}",
            importance=importance,
            tags=[REFLECTION_TAG],
        )

    # Stamp the consumed working-memory entries as superseded so the
    # chat-turn driver's short-term slot stops surfacing them. The
    # entries remain on disk for audit / history; just hidden from
    # default reads.
    _supersede_working_memory(conversation_id, inputs, new_record.id)
    superseded_ids = [
        # Working-memory entries don't have stable ids — surface a
        # signature instead so callers can correlate which inputs we
        # consumed without inventing fake ids.
        f"wm:{conversation_id}:{i}"
        for i, _ in enumerate(inputs)
    ]

    return ReflectionResult(
        scope=scope,
        inputs_considered=len(inputs),
        reflection_id=new_record.id,
        reflection_body=text,
        superseded_ids=superseded_ids,
    )


# ── Surgical supersession helpers ──────────────────────────────────────
#
# The public ``update`` functions on long_term / workspace_memory write
# a new record AND link the supersession; for reflection we already
# have the new record (the synthesis), so we just need to set the
# old records' superseded_by fields pointing at it. These helpers
# do the JSON edit + lance refresh directly.


def _link_supersession(old_id: str, new_id: str) -> None:
    records = _long_term._load_all()
    for r in records:
        if r.id == old_id:
            r.superseded_by = new_id
            _long_term._save_all(records)
            _long_term._lance_upsert(r)
            return


def _link_supersession_ws(workspace_id: str, old_id: str, new_id: str) -> None:
    records = _ws_mem._load(workspace_id)
    for r in records:
        if r.id == old_id:
            r.superseded_by = new_id
            _ws_mem._save(workspace_id, records)
            _ws_mem._lance_upsert(r)
            return


__all__ = [
    "DEFAULT_WINDOW_DAYS",
    "REFLECTION_TAG",
    "ReflectionResult",
    "reflect",
]
