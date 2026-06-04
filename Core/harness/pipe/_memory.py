"""memory.* action handlers — long-term, workspace, short-term, reflection."""

from __future__ import annotations

from typing import Any, Dict, Optional

from ._common import (
    _ActionError,
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
#
# Short-term Rust port (2026-06-04)
# ---------------------------------
# The three working-memory verbs now live in Rust
# (``rust/crates/wylde-harness/src/memory/short_term/``). These handlers
# became thin forwarders to the Rust ``wylde-harness`` pipe, mirroring the
# chat.* Phase 5.D cutover in ``_chat.py``. The underlying conversation
# document store (``..memory.conversation``) is UNCHANGED and stays
# load-bearing for ``conversations.*`` and ``memory.reflect``, which still
# read/write ``working_memory`` on the same JSON files — the Rust
# merge-save preserves every sibling field so the two sides interleave
# safely. Only the pipe-verb implementation moved off Python. If the Rust
# pipe is unreachable the verb raises ``harness_unavailable`` (no
# in-process fallback, same as the chat.* forwarders).

# Reply error codes meaning "the Rust pipe didn't actually serve this"
# (binary down, daemon mis-spawn, verb not registered). Mirrors the
# ``_TRANSPORT_FALLBACK_CODES`` set in ``_chat.py``.
_SHORT_TERM_FALLBACK_CODES = {
    "not_found",
    "pipe_unavailable",
    "pipe_connect",
    "pipe_timeout",
    "pipe_io",
    "handshake_timeout",
    "handshake_io",
    "handshake_rejected",
    "no_action",
    "not_implemented",
}

_SHORT_TERM_FORWARD_TIMEOUT = 15.0


def _forward_short_term_to_rust(
    action: str, payload: Dict[str, Any]
) -> Optional[Dict[str, Any]]:
    """Forward one ``memory.short_term.*`` action to the Rust
    ``wylde-harness`` pipe and return the reply ``data`` dict.

    Returns ``None`` on a transport-class failure so the caller raises
    ``harness_unavailable``; a genuine service-level error (e.g. the Rust
    handler's ``bad_request``) is re-raised verbatim as an
    :class:`_ActionError` so the wire shape stays identical. Mirrors
    ``_chat.py::_forward_chat_action_to_rust``.
    """
    try:
        from Core.shared.ipc import send_action as _ipc_send_action
    except ImportError:  # pragma: no cover — IPC shim always present in prod
        return None
    try:
        reply = _ipc_send_action(
            "wylde-harness", action, payload, timeout=_SHORT_TERM_FORWARD_TIMEOUT
        )
    except Exception:  # noqa: BLE001 — transport failures become harness_unavailable
        return None

    if not getattr(reply, "ok", False):
        err = getattr(reply, "error", None) or {}
        code = err.get("code") if isinstance(err, dict) else None
        if code in _SHORT_TERM_FALLBACK_CODES:
            return None
        message = ""
        if isinstance(err, dict):
            message = str(
                err.get("message") or err.get("code") or "rust_short_term_error"
            )
        raise _ActionError(str(code or "rust_short_term_error"), message)

    data = getattr(reply, "data", None)
    if not isinstance(data, dict):
        return None
    return data


def _memory_short_term_get_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    cid = p.get("conversation_id")
    if not isinstance(cid, str) or not cid:
        raise _ActionError("bad_request", "conversation_id is required")
    data = _forward_short_term_to_rust(
        "memory.short_term.get", {"conversation_id": cid}
    )
    if data is None:
        raise _ActionError(
            "harness_unavailable",
            "wylde-harness pipe is unreachable (memory.short_term.get)",
        )
    return data


def _memory_short_term_append_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    cid = p.get("conversation_id")
    entry = p.get("entry")
    if not isinstance(cid, str) or not cid:
        raise _ActionError("bad_request", "conversation_id is required")
    if not isinstance(entry, dict):
        raise _ActionError("bad_request", "entry must be a map")
    data = _forward_short_term_to_rust(
        "memory.short_term.append", {"conversation_id": cid, "entry": entry}
    )
    if data is None:
        raise _ActionError(
            "harness_unavailable",
            "wylde-harness pipe is unreachable (memory.short_term.append)",
        )
    return data


def _memory_short_term_clear_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    cid = p.get("conversation_id")
    if not isinstance(cid, str) or not cid:
        raise _ActionError("bad_request", "conversation_id is required")
    data = _forward_short_term_to_rust(
        "memory.short_term.clear", {"conversation_id": cid}
    )
    if data is None:
        raise _ActionError(
            "harness_unavailable",
            "wylde-harness pipe is unreachable (memory.short_term.clear)",
        )
    return data


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
