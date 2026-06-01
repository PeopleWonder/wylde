"""Persistent conversation history — one JSON file per chat.

Each saved conversation lives in its own file at
``$CONVERSATIONS_DIR/<id>.json`` (default: ``Wylde/.wylde/data/conversations/``)
so reads and writes never have to rewrite the whole list. Same on-disk schema
as the legacy ``harness/conversations/store.py`` so existing files load
without migration.

Document shape::

    {
      "id":              "<filename-safe id>",
      "title":           "<derived from first user message>",
      "created_at":      <epoch seconds>,
      "updated_at":      <epoch seconds>,
      "model":           "<active model at last save, optional>",
      "workspace_id":    "<the workspace bound to this conversation, optional>",
      "messages":        [ <wire-shaped chat messages> ],
      "working_memory":  [ <short-term memory entries — Layer 3 of the
                            memory architecture; tool calls, files
                            opened, decisions reached, summaries read.
                            Persisted with the conversation so a re-
                            opened chat doesn't re-do work it already
                            did.> ]
    }

System messages are stripped before save — they're regenerated each turn
from the system prompt builder and would only bloat the file.

Public API: :class:`InvalidConversationId`, :class:`ConversationNotFound`,
:func:`new_conversation_id`, :func:`derive_title`,
:func:`strip_system_messages`, :func:`list_conversations`,
:func:`read_conversation`, :func:`save_conversation`,
:func:`delete_conversation`, :func:`set_workspace`,
:func:`get_workspace`, :func:`append_working_memory`,
:func:`get_working_memory`, :func:`clear_working_memory`.
"""

from __future__ import annotations

import json
import os
import re
import secrets
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, List, Optional

from Core.shared.secure_file import harden_perms

from ._common import CONVERSATIONS_DIR, ensure_dir, logger

_ID_RE = re.compile(r"^[A-Za-z0-9_-]+$")
_MAX_ID_LEN = 128
_TITLE_LEN = 60


class InvalidConversationId(ValueError):
    """Caller-supplied id isn't safe to use as a filename."""


class ConversationNotFound(LookupError):
    """Requested conversation file does not exist on disk."""


def _validate_id(conv_id: Any) -> str:
    if not isinstance(conv_id, str) or not conv_id:
        raise InvalidConversationId("conversation id must be a non-empty string")
    if len(conv_id) > _MAX_ID_LEN:
        raise InvalidConversationId(
            f"conversation id is too long (>{_MAX_ID_LEN} chars)"
        )
    if not _ID_RE.match(conv_id):
        raise InvalidConversationId("conversation id may only contain [A-Za-z0-9_-]")
    return conv_id


def _path_for(conv_id: str) -> Path:
    return CONVERSATIONS_DIR / f"{conv_id}.json"


def _now() -> int:
    return int(time.time())


def new_conversation_id() -> str:
    """Mint a sortable, filename-safe id with a short random suffix."""
    stamp = (
        datetime.now(timezone.utc).strftime("%Y-%m-%dT%H-%M-%S-%fZ").replace(".", "-")
    )
    suffix = secrets.token_hex(3)
    return f"{stamp}-{suffix}"


def derive_title(messages: List[Dict[str, Any]]) -> str:
    """Use the first non-empty user message as the title (truncated)."""
    for m in messages or []:
        if not isinstance(m, dict) or m.get("role") != "user":
            continue
        content = m.get("content")
        if isinstance(content, str) and content.strip():
            return content.strip()[:_TITLE_LEN]
    return "Untitled"


def strip_system_messages(messages: List[Dict[str, Any]]) -> List[Dict[str, Any]]:
    """Drop ``role=system`` entries — they're regenerated each turn."""
    return [
        m for m in (messages or []) if isinstance(m, dict) and m.get("role") != "system"
    ]


def list_conversations() -> List[Dict[str, Any]]:
    """Return lightweight metadata for every saved chat, newest-first."""
    if not CONVERSATIONS_DIR.exists():
        return []
    metas: List[Dict[str, Any]] = []
    for entry in CONVERSATIONS_DIR.iterdir():
        if entry.suffix != ".json" or not entry.is_file():
            continue
        try:
            doc = json.loads(entry.read_text(encoding="utf-8"))
        except Exception as exc:
            logger.warning(
                "conversations: skipping unreadable %s (%s)", entry.name, exc
            )
            continue
        if not isinstance(doc, dict):
            continue
        cid = doc.get("id")
        if not isinstance(cid, str) or not cid:
            continue
        created_at = int(doc.get("created_at") or 0)
        updated_at = int(doc.get("updated_at") or created_at)
        msgs = doc.get("messages")
        msg_count = len(msgs) if isinstance(msgs, list) else 0
        metas.append(
            {
                "id": cid,
                "title": doc.get("title") or "Untitled",
                "created_at": created_at,
                "updated_at": updated_at,
                "message_count": msg_count,
                "model": doc.get("model") or "",
            }
        )
    metas.sort(key=lambda m: m["updated_at"], reverse=True)
    return metas


def read_conversation(conv_id: str) -> Dict[str, Any]:
    """Return the full conversation document. Raises if it doesn't exist."""
    cid = _validate_id(conv_id)
    path = _path_for(cid)
    if not path.exists():
        raise ConversationNotFound(f"conversation '{cid}' not found")
    try:
        doc = json.loads(path.read_text(encoding="utf-8"))
    except Exception as exc:
        raise ConversationNotFound(
            f"conversation '{cid}' is unreadable: {exc}"
        ) from exc
    if not isinstance(doc, dict):
        raise ConversationNotFound(f"conversation '{cid}' is malformed")
    return doc


def save_conversation(
    *,
    conv_id: str,
    messages: List[Dict[str, Any]],
    title: Optional[str] = None,
    model: Optional[str] = None,
    workspace_id: Optional[str] = None,
    working_memory: Optional[List[Dict[str, Any]]] = None,
) -> Dict[str, Any]:
    """Persist a conversation. ``created_at`` is preserved across updates.

    ``workspace_id`` and ``working_memory`` are preserved across saves
    when not explicitly passed. Pass an empty list to ``working_memory``
    to clear it; pass ``""`` to ``workspace_id`` to clear the binding.
    """
    cid = _validate_id(conv_id)
    safe_messages = strip_system_messages(messages or [])

    created_at = _now()
    existing_model: Optional[str] = None
    existing_workspace: Optional[str] = None
    existing_working: Optional[List[Dict[str, Any]]] = None
    try:
        existing = read_conversation(cid)
        existing_created = existing.get("created_at")
        if isinstance(existing_created, int) and existing_created > 0:
            created_at = existing_created
        existing_m = existing.get("model")
        if isinstance(existing_m, str) and existing_m:
            existing_model = existing_m
        existing_ws = existing.get("workspace_id")
        if isinstance(existing_ws, str):
            existing_workspace = existing_ws
        existing_wm = existing.get("working_memory")
        if isinstance(existing_wm, list):
            existing_working = existing_wm
    except ConversationNotFound:
        pass

    final_title = (title or "").strip() or derive_title(safe_messages)
    final_model = model if model else existing_model
    if workspace_id is None:
        final_workspace = existing_workspace
    else:
        final_workspace = workspace_id or ""
    if working_memory is None:
        final_working = existing_working if existing_working is not None else []
    else:
        final_working = list(working_memory)

    doc: Dict[str, Any] = {
        "id": cid,
        "title": final_title,
        "created_at": created_at,
        "updated_at": _now(),
        "messages": safe_messages,
        "workspace_id": final_workspace or "",
        "working_memory": final_working,
    }
    if final_model:
        doc["model"] = final_model

    path = _path_for(cid)
    try:
        ensure_dir(path.parent)
        # Atomic write: dump to temp then rename so a crash mid-write
        # doesn't leave a half-truncated file.
        tmp = path.with_suffix(".json.tmp")
        tmp.write_text(json.dumps(doc, indent=2, ensure_ascii=False), encoding="utf-8")
        os.replace(tmp, path)
        # Conversation history can carry sensitive content — owner-only.
        harden_perms(path)
    except Exception as exc:
        logger.warning("conversations: save failed for %s: %s", cid, exc)
        raise
    return doc


def delete_conversation(conv_id: str) -> bool:
    """Remove a conversation file. Returns True if a file was deleted."""
    cid = _validate_id(conv_id)
    path = _path_for(cid)
    if not path.exists():
        return False
    try:
        path.unlink()
    except Exception as exc:
        logger.warning("conversations: delete failed for %s: %s", cid, exc)
        raise
    return True


# ── Workspace + working-memory helpers (Layer 3 of the memory architecture) ──


def _read_or_empty(conv_id: str) -> Dict[str, Any]:
    """Internal helper — either the existing doc or a freshly-stamped one."""
    cid = _validate_id(conv_id)
    try:
        return read_conversation(cid)
    except ConversationNotFound:
        now = _now()
        return {
            "id": cid,
            "title": "Untitled",
            "created_at": now,
            "updated_at": now,
            "messages": [],
            "workspace_id": "",
            "working_memory": [],
        }


def set_workspace(conv_id: str, workspace_id: Optional[str]) -> Dict[str, Any]:
    """Bind ``workspace_id`` to the conversation. Pass ``None`` or empty
    to clear. Creates a stub conversation if one doesn't exist yet, so
    the GUI can call this before the first turn lands."""
    doc = _read_or_empty(conv_id)
    return save_conversation(
        conv_id=doc["id"],
        messages=doc.get("messages") or [],
        title=doc.get("title"),
        model=doc.get("model"),
        workspace_id=workspace_id or "",
        working_memory=doc.get("working_memory") or [],
    )


def get_workspace(conv_id: str) -> str:
    """Return the workspace_id bound to ``conv_id``, or '' if none."""
    try:
        doc = read_conversation(conv_id)
    except ConversationNotFound:
        return ""
    ws = doc.get("workspace_id")
    return ws if isinstance(ws, str) else ""


def append_working_memory(conv_id: str, entry: Dict[str, Any]) -> Dict[str, Any]:
    """Append one short-term entry. ``entry`` is freeform but the
    convention is ``{"kind": "<tool|file|decision|summary>", "at": <ts>,
    "data": {...}}`` — the chat-turn driver writes these as it works.
    Returns the persisted document."""
    doc = _read_or_empty(conv_id)
    working = list(doc.get("working_memory") or [])
    if not isinstance(entry, dict):
        entry = {"kind": "raw", "at": _now(), "data": str(entry)}
    entry.setdefault("at", _now())
    working.append(entry)
    return save_conversation(
        conv_id=doc["id"],
        messages=doc.get("messages") or [],
        title=doc.get("title"),
        model=doc.get("model"),
        workspace_id=doc.get("workspace_id"),
        working_memory=working,
    )


def get_working_memory(conv_id: str) -> List[Dict[str, Any]]:
    try:
        doc = read_conversation(conv_id)
    except ConversationNotFound:
        return []
    wm = doc.get("working_memory")
    return list(wm) if isinstance(wm, list) else []


def clear_working_memory(conv_id: str) -> bool:
    """Drop the short-term entries. Useful when starting a fresh task in
    an existing conversation. Returns True if anything was cleared."""
    try:
        doc = read_conversation(conv_id)
    except ConversationNotFound:
        return False
    if not doc.get("working_memory"):
        return False
    save_conversation(
        conv_id=doc["id"],
        messages=doc.get("messages") or [],
        title=doc.get("title"),
        model=doc.get("model"),
        workspace_id=doc.get("workspace_id"),
        working_memory=[],
    )
    return True


__all__ = [
    "InvalidConversationId",
    "ConversationNotFound",
    "new_conversation_id",
    "derive_title",
    "strip_system_messages",
    "list_conversations",
    "read_conversation",
    "save_conversation",
    "delete_conversation",
    "set_workspace",
    "get_workspace",
    "append_working_memory",
    "get_working_memory",
    "clear_working_memory",
]
