"""memory.* action handlers — long-term, workspace, short-term, reflection."""

from __future__ import annotations

from typing import Any, Dict

from ._common import (
    _ActionError,
    _conv_module,
    _long_term_module,
    _payload_dict,
    _reflection_module,
    _ws_mem_module,
)


# ── Memory: long-term ──────────────────────────────────────────────────


def _memory_long_term_list_action(payload: Any) -> Dict[str, Any]:
    include = False
    if isinstance(payload, dict):
        include = bool(payload.get("include_superseded", False))
    lt = _long_term_module()
    records = [r.to_dict() for r in lt.list_records(include_superseded=include)]
    return {"memories": records, "count": len(records)}


def _memory_long_term_search_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    query = p.get("query")
    if not isinstance(query, str) or not query.strip():
        raise _ActionError("bad_request", "query is required")
    limit = max(1, min(50, int(p.get("k") or p.get("limit") or 5)))
    return {"hits": _long_term_module().search(query, limit=limit)}


def _memory_long_term_save_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    body = p.get("body")
    if not isinstance(body, str) or not body.strip():
        raise _ActionError("bad_request", "body is required")
    record = _long_term_module().save(
        body=body,
        source=str(p.get("source") or "settings_ui"),
        importance=p.get("importance"),
        tags=p.get("tags") if isinstance(p.get("tags"), list) else None,
    )
    result: Dict[str, Any] = record.to_dict()
    return result


def _memory_long_term_update_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    rid = p.get("id")
    if not isinstance(rid, str) or not rid:
        raise _ActionError("bad_request", "id is required")
    record = _long_term_module().update(
        rid,
        body=p.get("body") if isinstance(p.get("body"), str) else None,
        importance=p.get("importance"),
        source=p.get("source") if isinstance(p.get("source"), str) else None,
    )
    if record is None:
        raise _ActionError("not_found", f"memory {rid!r} not found")
    result: Dict[str, Any] = record.to_dict()
    return result


def _memory_long_term_delete_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    rid = p.get("id")
    if not isinstance(rid, str) or not rid:
        raise _ActionError("bad_request", "id is required")
    return {"ok": _long_term_module().delete(rid), "id": rid}


def _memory_long_term_history_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    rid = p.get("id")
    if not isinstance(rid, str) or not rid:
        raise _ActionError("bad_request", "id is required")
    chain = _long_term_module().history(rid)
    return {"id": rid, "chain": [r.to_dict() for r in chain]}


# ── Memory: workspace ─────────────────────────────────────────────────


def _memory_workspace_list_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    wsid = p.get("workspace_id")
    if not isinstance(wsid, str) or not wsid:
        raise _ActionError("bad_request", "workspace_id is required")
    include = bool(p.get("include_superseded", False))
    records = [
        r.to_dict()
        for r in _ws_mem_module().list_records(
            wsid,
            include_superseded=include,
        )
    ]
    return {"memories": records, "count": len(records), "workspace_id": wsid}


def _memory_workspace_search_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    wsid = p.get("workspace_id")
    query = p.get("query")
    if not isinstance(wsid, str) or not wsid:
        raise _ActionError("bad_request", "workspace_id is required")
    if not isinstance(query, str) or not query.strip():
        raise _ActionError("bad_request", "query is required")
    limit = max(1, min(50, int(p.get("k") or p.get("limit") or 5)))
    return {"hits": _ws_mem_module().search(wsid, query, limit=limit)}


def _memory_workspace_save_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    wsid = p.get("workspace_id")
    body = p.get("body")
    if not isinstance(wsid, str) or not wsid:
        raise _ActionError("bad_request", "workspace_id is required")
    if not isinstance(body, str) or not body.strip():
        raise _ActionError("bad_request", "body is required")
    entities = p.get("entities") if isinstance(p.get("entities"), list) else None
    record = _ws_mem_module().save(
        workspace_id=wsid,
        body=body,
        source=str(p.get("source") or ""),
        importance=p.get("importance"),
        entities=entities,
    )
    result: Dict[str, Any] = record.to_dict()
    return result


def _memory_workspace_update_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    wsid = p.get("workspace_id")
    rid = p.get("id")
    if not isinstance(wsid, str) or not wsid:
        raise _ActionError("bad_request", "workspace_id is required")
    if not isinstance(rid, str) or not rid:
        raise _ActionError("bad_request", "id is required")
    record = _ws_mem_module().update(
        wsid,
        rid,
        body=p.get("body") if isinstance(p.get("body"), str) else None,
        importance=p.get("importance"),
        entities=p.get("entities") if isinstance(p.get("entities"), list) else None,
    )
    if record is None:
        raise _ActionError("not_found", f"memory {rid!r} not in {wsid!r}")
    result: Dict[str, Any] = record.to_dict()
    return result


def _memory_workspace_delete_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    wsid = p.get("workspace_id")
    rid = p.get("id")
    if not isinstance(wsid, str) or not wsid:
        raise _ActionError("bad_request", "workspace_id is required")
    if not isinstance(rid, str) or not rid:
        raise _ActionError("bad_request", "id is required")
    return {"ok": _ws_mem_module().delete(wsid, rid), "workspace_id": wsid, "id": rid}


def _memory_workspace_curate_action(payload: Any) -> Dict[str, Any]:
    """Trigger LLM-driven curation. Same shape as ``memory.reflect`` —
    returns ``skipped=True`` because chat_fn isn't injectable across
    the pipe; the scheduler / Python callers run it for real with a
    chat function. The action exists so a future GUI button or
    scheduler-status surface has a stable place to call."""
    p = _payload_dict(payload)
    wsid = p.get("workspace_id")
    if not isinstance(wsid, str) or not wsid:
        raise _ActionError("bad_request", "workspace_id is required")
    result = _ws_mem_module().curate(wsid, chat_fn=None)
    out: Dict[str, Any] = result.to_dict()
    return out


# ── Memory: short-term ────────────────────────────────────────────────


def _memory_short_term_get_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    cid = p.get("conversation_id")
    if not isinstance(cid, str) or not cid:
        raise _ActionError("bad_request", "conversation_id is required")
    return {
        "working_memory": _conv_module().get_working_memory(cid),
        "conversation_id": cid,
    }


def _memory_short_term_append_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    cid = p.get("conversation_id")
    entry = p.get("entry")
    if not isinstance(cid, str) or not cid:
        raise _ActionError("bad_request", "conversation_id is required")
    if not isinstance(entry, dict):
        raise _ActionError("bad_request", "entry must be a map")
    doc = _conv_module().append_working_memory(cid, entry)
    return {"conversation_id": cid, "working_memory": doc.get("working_memory") or []}


def _memory_short_term_clear_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    cid = p.get("conversation_id")
    if not isinstance(cid, str) or not cid:
        raise _ActionError("bad_request", "conversation_id is required")
    return {"cleared": _conv_module().clear_working_memory(cid), "conversation_id": cid}


# ── Memory: reflection ────────────────────────────────────────────────


def _memory_reflect_action(payload: Any) -> Dict[str, Any]:
    """Run a consolidation cycle. Pipe callers must provide their own
    chat function via the wrapper layer — we don't have access to the
    turn driver's chat_fn here. For now this returns a structured
    "skipped" envelope unless the harness pipe is upgraded with a
    chat-fn-injection mechanism. The action exists so tests / future
    schedulers / UI buttons can plumb a chat function in via the
    Python API directly."""
    p = _payload_dict(payload)
    scope = p.get("scope")
    if not isinstance(scope, str) or not scope:
        raise _ActionError("bad_request", "scope is required")
    # The pipe layer can't inject chat_fn — caller has to use the
    # Python API for now. Document by returning skipped.
    result = _reflection_module().reflect(scope=scope, chat_fn=None)
    out: Dict[str, Any] = result.to_dict()
    return out
