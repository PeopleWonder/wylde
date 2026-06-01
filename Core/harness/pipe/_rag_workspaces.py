"""rag.workspaces.* action handlers — workspace lifecycle + persona + MRU."""

from __future__ import annotations

from typing import Any, Dict

from ._common import (
    _ActionError,
    _conv_module,
    _payload_dict,
    _ws_module,
    logger,
)


def _rag_workspaces_list_action(_payload: Any) -> Dict[str, Any]:
    ws = _ws_module()
    return {"workspaces": [w.to_dict() for w in ws.list_workspaces()]}


def _rag_workspaces_recent_action(payload: Any) -> Dict[str, Any]:
    ws = _ws_module()
    cap = ws.get_mru_limit()
    n = cap
    if isinstance(payload, dict):
        try:
            n = int(payload.get("limit") or n)
        except (TypeError, ValueError):
            pass
    n = max(0, min(n, cap))
    return {"workspaces": [w.to_dict() for w in ws.recent_workspaces(limit=n)]}


def _rag_workspaces_get_mru_limit_action(_payload: Any) -> Dict[str, Any]:
    ws = _ws_module()
    return {
        "limit": ws.get_mru_limit(),
        "min": ws.MRU_LIMIT_MIN,
        "max": ws.MRU_LIMIT_MAX,
        "default": ws.MRU_LIMIT_DEFAULT,
    }


def _rag_workspaces_set_mru_limit_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    if "limit" not in p:
        raise _ActionError("bad_request", "limit is required")
    ws = _ws_module()
    try:
        new_limit = ws.set_mru_limit(p.get("limit"))
    except ValueError as exc:
        raise _ActionError("bad_request", str(exc))
    return {
        "limit": new_limit,
        "workspaces": [w.to_dict() for w in ws.list_workspaces()],
    }


def _rag_workspaces_activate_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    path = p.get("path")
    if not isinstance(path, str) or not path:
        raise _ActionError("bad_request", "path is required")
    full = bool(p.get("full_reindex", False))
    ws = _ws_module()
    try:
        record = ws.activate(path, full_reindex=full)
    except ValueError as exc:
        raise _ActionError("bad_request", str(exc))
    conv_id = p.get("conversation_id")
    if isinstance(conv_id, str) and conv_id:
        try:
            _conv_module().set_workspace(conv_id, record.id)
        except Exception as exc:  # noqa: BLE001
            logger.warning(
                "activate: conversation bind failed for %s: %s", conv_id, exc
            )
    result: Dict[str, Any] = record.to_dict()
    return result


def _rag_workspaces_reindex_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    wsid = p.get("workspace_id")
    if not isinstance(wsid, str) or not wsid:
        raise _ActionError("bad_request", "workspace_id is required")
    ws = _ws_module()
    try:
        record = ws.reindex_workspace(wsid)
    except ValueError as exc:
        raise _ActionError("not_found", str(exc))
    result: Dict[str, Any] = record.to_dict()
    return result


def _rag_workspaces_status_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    wsid = p.get("workspace_id")
    if not isinstance(wsid, str) or not wsid:
        raise _ActionError("bad_request", "workspace_id is required")
    status: Dict[str, Any] = _ws_module().status(wsid)
    return status


def _rag_workspaces_delete_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    wsid = p.get("workspace_id")
    if not isinstance(wsid, str) or not wsid:
        raise _ActionError("bad_request", "workspace_id is required")
    ok = _ws_module().delete_workspace(wsid)
    return {"ok": ok, "workspace_id": wsid}


def _rag_workspaces_set_persona_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    wsid = p.get("workspace_id")
    text = p.get("text", "")
    if not isinstance(wsid, str) or not wsid:
        raise _ActionError("bad_request", "workspace_id is required")
    ok = _ws_module().set_persona(wsid, text if isinstance(text, str) else "")
    return {"ok": ok, "workspace_id": wsid}


def _rag_workspaces_get_persona_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    wsid = p.get("workspace_id")
    if not isinstance(wsid, str) or not wsid:
        raise _ActionError("bad_request", "workspace_id is required")
    return {"workspace_id": wsid, "persona": _ws_module().get_persona(wsid)}
