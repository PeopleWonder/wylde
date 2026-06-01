"""conversations.* action handlers — id mint + list/get/delete."""

from __future__ import annotations

from typing import Any, Dict

from ._common import _ActionError, _conv_module, _payload_dict


def _conversations_new_action(_payload: Any) -> Dict[str, Any]:
    """Mint a fresh, sortable, filename-safe conversation id."""
    return {"id": _conv_module().new_conversation_id()}


def _conversations_list_action(_payload: Any) -> Dict[str, Any]:
    """Lightweight metadata for every saved chat, newest-first."""
    metas = _conv_module().list_conversations()
    return {"conversations": metas, "count": len(metas)}


def _conversations_get_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    cid = p.get("id")
    if not isinstance(cid, str) or not cid:
        raise _ActionError("bad_request", "id is required")
    conv = _conv_module()
    try:
        result: Dict[str, Any] = conv.read_conversation(cid)
        return result
    except conv.InvalidConversationId as exc:
        raise _ActionError("bad_request", str(exc))
    except conv.ConversationNotFound as exc:
        raise _ActionError("not_found", str(exc))


def _conversations_delete_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    cid = p.get("id")
    if not isinstance(cid, str) or not cid:
        raise _ActionError("bad_request", "id is required")
    conv = _conv_module()
    try:
        deleted = conv.delete_conversation(cid)
    except conv.InvalidConversationId as exc:
        raise _ActionError("bad_request", str(exc))
    return {"ok": deleted, "id": cid}
