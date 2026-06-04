"""conversations.* action handlers — id mint + list/get/delete + active.

Rust port (2026-06-04, Memory Slice B)
--------------------------------------
The conversation-lifecycle verbs now live in Rust
(``rust/crates/wylde-harness/src/memory/conversations/``). These handlers
became thin forwarders to the Rust ``wylde-harness`` pipe, mirroring the
``memory.short_term.*`` cutover in ``_memory.py``. The underlying
conversation-document store (``..memory.conversation``) is UNCHANGED and
stays load-bearing for ``memory.reflect`` — the Rust list/read/delete path
shares the same ``<conversations_dir>/<id>.json`` files, so the two sides
interleave safely. Only the pipe-verb implementation moved off Python. If
the Rust pipe is unreachable the verb raises ``harness_unavailable`` (no
in-process fallback, same as the chat.* / short_term forwarders).

``conversations.get_active`` / ``conversations.set_active`` are net-new
verbs (Slice B persistence): they live ONLY in Rust, so there is no
``conversation.py`` counterpart — the forwarders below are the whole
Python surface for them.
"""

from __future__ import annotations

from typing import Any, Dict, Optional

from ._common import _ActionError, _payload_dict

# Reply error codes meaning "the Rust pipe didn't actually serve this"
# (binary down, daemon mis-spawn, verb not registered). Mirrors the
# ``_SHORT_TERM_FALLBACK_CODES`` set in ``_memory.py`` — but deliberately
# WITHOUT ``not_found``: for ``conversations.get`` a ``not_found`` is a
# genuine service-level reply (the conversation doesn't exist), NOT a
# transport failure, so it must propagate verbatim rather than collapse to
# ``harness_unavailable``.
_CONVERSATIONS_FALLBACK_CODES = {
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

_CONVERSATIONS_FORWARD_TIMEOUT = 15.0


def _forward_conversations_to_rust(
    action: str, payload: Dict[str, Any]
) -> Optional[Dict[str, Any]]:
    """Forward one ``conversations.*`` action to the Rust ``wylde-harness``
    pipe and return the reply ``data`` dict.

    Returns ``None`` on a transport-class failure so the caller raises
    ``harness_unavailable``; a genuine service-level error (the Rust
    handler's ``bad_request`` / ``not_found``) is re-raised verbatim as an
    :class:`_ActionError` so the wire shape stays identical. Mirrors
    ``_memory.py::_forward_short_term_to_rust``.
    """
    try:
        from Core.shared.ipc import send_action as _ipc_send_action
    except ImportError:  # pragma: no cover — IPC shim always present in prod
        return None
    try:
        reply = _ipc_send_action(
            "wylde-harness", action, payload, timeout=_CONVERSATIONS_FORWARD_TIMEOUT
        )
    except Exception:  # noqa: BLE001 — transport failures become harness_unavailable
        return None

    if not getattr(reply, "ok", False):
        err = getattr(reply, "error", None) or {}
        code = err.get("code") if isinstance(err, dict) else None
        if code in _CONVERSATIONS_FALLBACK_CODES:
            return None
        message = ""
        if isinstance(err, dict):
            message = str(
                err.get("message") or err.get("code") or "rust_conversations_error"
            )
        raise _ActionError(str(code or "rust_conversations_error"), message)

    data = getattr(reply, "data", None)
    if not isinstance(data, dict):
        return None
    return data


def _forward_or_unavailable(action: str, payload: Dict[str, Any]) -> Dict[str, Any]:
    data = _forward_conversations_to_rust(action, payload)
    if data is None:
        raise _ActionError(
            "harness_unavailable",
            f"wylde-harness pipe is unreachable ({action})",
        )
    return data


def _conversations_new_action(_payload: Any) -> Dict[str, Any]:
    """Mint a fresh, sortable, filename-safe conversation id."""
    return _forward_or_unavailable("conversations.new", {})


def _conversations_list_action(_payload: Any) -> Dict[str, Any]:
    """Lightweight metadata for every saved chat, newest-first."""
    return _forward_or_unavailable("conversations.list", {})


def _conversations_get_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    cid = p.get("id")
    if not isinstance(cid, str) or not cid:
        raise _ActionError("bad_request", "id is required")
    return _forward_or_unavailable("conversations.get", {"id": cid})


def _conversations_delete_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    cid = p.get("id")
    if not isinstance(cid, str) or not cid:
        raise _ActionError("bad_request", "id is required")
    return _forward_or_unavailable("conversations.delete", {"id": cid})


def _conversations_get_active_action(_payload: Any) -> Dict[str, Any]:
    """Read the persisted active-conversation selection (`{id}`)."""
    return _forward_or_unavailable("conversations.get_active", {})


def _conversations_set_active_action(payload: Any) -> Dict[str, Any]:
    """Persist the active-conversation selection. An empty / absent id
    clears it (the Rust side treats `""` as "no selection")."""
    p = _payload_dict(payload)
    cid = p.get("id")
    cid = cid if isinstance(cid, str) else ""
    return _forward_or_unavailable("conversations.set_active", {"id": cid})
