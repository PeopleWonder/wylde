"""Shared helpers for the harness pipe action handlers.

Lives in the package so every submodule (_chat, _models, _memory, ...) can
import the same ``_ActionError`` / ``_payload_dict`` / lazy-module helpers
without back-referencing ``__init__`` (which would create a partial-import
order trap during package load).
"""

from __future__ import annotations

import logging
from typing import Any, Dict

logger = logging.getLogger("wylde.harness.pipe")

SERVICE_NAME = "wylde-harness"

_DEFAULT_POLL_WAIT_MS = 5000
_MAX_POLL_WAIT_MS = 25000


class _ActionError(Exception):
    """Raised by handlers to surface a structured error through the pipe.

    The shared ipc module wraps generic exceptions as ``{code: "handler",
    message: ...}``; this subclass lets handlers pick a specific code.
    """

    def __init__(self, code: str, message: str) -> None:
        super().__init__(message)
        self.code = code
        self.message = message


def _payload_dict(payload: Any) -> Dict[str, Any]:
    if not isinstance(payload, dict):
        raise _ActionError("bad_request", "payload must be a map")
    return payload


# ── Lazy-import helpers ────────────────────────────────────────────────
#
# Each helper imports its target module on first call so the harness
# avoids pulling in heavy dependencies (memory.long_term, backend.ollama_client,
# Voice/*) until the corresponding action actually fires.


def _ws_module() -> Any:
    try:
        from ..memory import workspaces as _ws

        return _ws
    except ImportError:
        from Core.harness.memory import workspaces as _ws

        return _ws


def _ws_mem_module() -> Any:
    try:
        from ..memory import workspace_memory as _wm

        return _wm
    except ImportError:
        from Core.harness.memory import workspace_memory as _wm

        return _wm


def _conv_module() -> Any:
    try:
        from ..memory import conversation as _conv

        return _conv
    except ImportError:
        from Core.harness.memory import conversation as _conv

        return _conv


def _long_term_module() -> Any:
    try:
        from ..memory import long_term as _lt

        return _lt
    except ImportError:
        from Core.harness.memory import long_term as _lt

        return _lt


def _reflection_module() -> Any:
    try:
        from ..memory import reflection as _r

        return _r
    except ImportError:
        from Core.harness.memory import reflection as _r

        return _r


def _ollama_client_module() -> Any:
    try:
        from ..backend import ollama_client as _oc

        return _oc
    except ImportError:
        from Core.harness.backend import ollama_client as _oc

        return _oc


def _model_state_module() -> Any:
    try:
        from ..backend import model_state as _ms

        return _ms
    except ImportError:
        from Core.harness.backend import model_state as _ms

        return _ms
