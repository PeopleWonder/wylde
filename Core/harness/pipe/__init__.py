r"""Harness pipe — ``\\.\pipe\wylde-harness``.

Central pipe-action dispatcher for the harness daemon. Public entry
points are :func:`start` and :func:`stop`; everything else (the
action handlers, ``_ACTIONS`` registry, ``_ActionError``) is private
but re-exported here so tests and the ``gui_action_contract`` rule can
keep reaching them through ``Core.harness.pipe``.

The split is by surface: chat / tools / models / rag.workspaces /
memory / conversations / prompts each live in a sibling ``_*.py``
module. ``_common.py`` owns ``_ActionError``, ``_payload_dict``, and
the lazy-import helpers shared across the handlers.
"""

from __future__ import annotations

import threading
from typing import Any, Callable

from ._common import SERVICE_NAME, _ActionError, logger
from ._common import (  # noqa: F401 — re-exported for test access via Core.harness.pipe
    _model_state_module,
    _ollama_client_module,
)
from ._chat import (
    _cancel_action,
    _run_turn_action,
    _start_turn_action,
)
from ._conversations import (
    _conversations_delete_action,
    _conversations_get_action,
    _conversations_list_action,
    _conversations_new_action,
)
from ._memory import (
    _memory_long_term_delete_action,
    _memory_long_term_history_action,
    _memory_long_term_list_action,
    _memory_long_term_save_action,
    _memory_long_term_search_action,
    _memory_long_term_update_action,
    _memory_reflect_action,
    _memory_short_term_append_action,
    _memory_short_term_clear_action,
    _memory_short_term_get_action,
    _memory_workspace_curate_action,
    _memory_workspace_delete_action,
    _memory_workspace_list_action,
    _memory_workspace_save_action,
    _memory_workspace_search_action,
    _memory_workspace_update_action,
)
from ._models import (
    _models_delete_action,
    _models_get_default_action,
    _models_get_profile_action,
    _models_list_action,
    _models_set_active_action,
    _models_set_default_action,
    _models_show_action,
    _models_synthesize_action,
    _models_transcribe_action,
    _models_unload_action,
)
from ._prompts import (
    _prompts_delete_preset_action,
    _prompts_list_action,
    _prompts_save_action,
    _prompts_save_preset_action,
    _prompts_set_active_action,
)
from ._rag_workspaces import (
    _rag_workspaces_activate_action,
    _rag_workspaces_delete_action,
    _rag_workspaces_get_mru_limit_action,
    _rag_workspaces_get_persona_action,
    _rag_workspaces_list_action,
    _rag_workspaces_recent_action,
    _rag_workspaces_reindex_action,
    _rag_workspaces_set_mru_limit_action,
    _rag_workspaces_set_persona_action,
    _rag_workspaces_status_action,
)
from ._tools import _tools_list_action, _tools_run_action

_started = False
_started_lock = threading.Lock()


_ACTIONS = {
    "chat.start_turn": _start_turn_action,
    "chat.run_turn": _run_turn_action,
    "chat.cancel": _cancel_action,
    "tools.list": _tools_list_action,
    "tools.run": _tools_run_action,
    "models.list": _models_list_action,
    "models.transcribe": _models_transcribe_action,
    "models.synthesize": _models_synthesize_action,
    "models.get_profile": _models_get_profile_action,
    "models.show": _models_show_action,
    "models.delete": _models_delete_action,
    "models.unload": _models_unload_action,
    "models.set_active": _models_set_active_action,
    "models.set_default": _models_set_default_action,
    "models.get_default": _models_get_default_action,
    # RAG workspaces
    "rag.workspaces.list": _rag_workspaces_list_action,
    "rag.workspaces.recent": _rag_workspaces_recent_action,
    "rag.workspaces.activate": _rag_workspaces_activate_action,
    "rag.workspaces.reindex": _rag_workspaces_reindex_action,
    "rag.workspaces.status": _rag_workspaces_status_action,
    "rag.workspaces.delete": _rag_workspaces_delete_action,
    "rag.workspaces.set_persona": _rag_workspaces_set_persona_action,
    "rag.workspaces.get_persona": _rag_workspaces_get_persona_action,
    "rag.workspaces.get_mru_limit": _rag_workspaces_get_mru_limit_action,
    "rag.workspaces.set_mru_limit": _rag_workspaces_set_mru_limit_action,
    # Memory: long-term
    "memory.long_term.list": _memory_long_term_list_action,
    "memory.long_term.search": _memory_long_term_search_action,
    "memory.long_term.save": _memory_long_term_save_action,
    "memory.long_term.update": _memory_long_term_update_action,
    "memory.long_term.delete": _memory_long_term_delete_action,
    "memory.long_term.history": _memory_long_term_history_action,
    # Memory: workspace
    "memory.workspace.list": _memory_workspace_list_action,
    "memory.workspace.search": _memory_workspace_search_action,
    "memory.workspace.save": _memory_workspace_save_action,
    "memory.workspace.update": _memory_workspace_update_action,
    "memory.workspace.delete": _memory_workspace_delete_action,
    "memory.workspace.curate": _memory_workspace_curate_action,
    # Memory: short-term
    "memory.short_term.get": _memory_short_term_get_action,
    "memory.short_term.append": _memory_short_term_append_action,
    "memory.short_term.clear": _memory_short_term_clear_action,
    # Memory: reflection
    "memory.reflect": _memory_reflect_action,
    # Conversations
    "conversations.new": _conversations_new_action,
    "conversations.list": _conversations_list_action,
    "conversations.get": _conversations_get_action,
    "conversations.delete": _conversations_delete_action,
    # System prompts
    "prompts.list": _prompts_list_action,
    "prompts.save": _prompts_save_action,
    "prompts.save_preset": _prompts_save_preset_action,
    "prompts.set_active": _prompts_set_active_action,
    "prompts.delete_preset": _prompts_delete_preset_action,
}


def _wrap_handler(handler: Callable[[Any], Any]) -> Callable[[Any], Any]:
    """Translate ``_ActionError`` into the wire envelope the dispatcher
    expects. Other exceptions bubble — the shared ipc layer wraps them."""

    def _wrapped(payload: Any) -> Any:
        try:
            return handler(payload)
        except _ActionError as exc:
            # Re-raise as a plain Exception so the dispatcher's generic
            # catch wraps it; embed the structured code in the message.
            raise RuntimeError(f"[{exc.code}] {exc.message}")

    _wrapped.__name__ = getattr(handler, "__name__", "wrapped")
    return _wrapped


def _register_actions() -> Any:
    try:
        from Core.shared import ipc
    except ImportError as exc:
        logger.warning("harness pipe: ipc not importable (%s) — pipe disabled", exc)
        return None
    for name, handler in _ACTIONS.items():
        ipc.register_action(name, _wrap_handler(handler))
    logger.info("harness pipe: registered %d actions", len(_ACTIONS))
    return ipc


def _build_stub_app() -> Any:
    """Minimal Flask app for the ipc fallback. Action dispatch never
    falls through to this in practice; the empty app is just so the
    PipeServer initialiser has something to hold."""
    from flask import Flask

    app = Flask("wylde-harness")

    @app.route("/health", methods=["GET"])
    def _health() -> Any:  # pragma: no cover
        return {"ok": True, "service": SERVICE_NAME}

    return app


def start() -> bool:
    """Start the harness pipe in a daemon thread.

    Returns True if the pipe is now serving (or was already serving),
    False if the dependencies aren't available (msgpack/pywin32 missing,
    or non-Windows). Safe to call multiple times — second call is a no-op.
    """
    global _started
    with _started_lock:
        if _started:
            return True
        ipc = _register_actions()
        if ipc is None:
            return False
        try:
            ipc.serve_forever_background(SERVICE_NAME, _build_stub_app())
        except Exception as exc:  # noqa: BLE001
            logger.warning("harness pipe: serve_forever_background failed (%s)", exc)
            return False
        _started = True
        logger.info("harness pipe: serving \\\\.\\pipe\\%s", SERVICE_NAME)
        return True


def stop() -> Any:
    """No-op for now. The PipeServer doesn't expose a shutdown hook in
    the shared module; the pipe drains when the daemon process exits.
    Reserved here so future graceful-shutdown work has a place to land.
    """
    return None


__all__ = ["SERVICE_NAME", "start", "stop"]
