"""Unified service-to-service transport layer.

Discovery tells you *where* a service is. This module tells you *how* to talk
to it. Windows named pipes first, HTTP loopback as fallback.

Usage at the call site::

    from Core.shared import ipc
    data = ipc.call("tool-runner", "execute", {"lang": "python", "code": "..."})

Usage in a service's ``run.py``::

    from Core.shared import ipc
    ipc.serve("tool-runner", app, port=8001)   # replaces app.run(port=8001)

Transport selection (env ``WYLDE_TRANSPORT``):

* ``pipe`` — default. Try named pipe first; fall back to HTTP if unavailable.
* ``http`` — force HTTP everywhere. For debugging or external reachability.

Pipe naming convention: ``\\\\.\\pipe\\wylde-<service-name>``.
Wire format: u32 big-endian length prefix + msgpack body.

This is the package façade for the split that replaced the monolithic
``Core/shared/ipc.py``. The wire datatypes / env config live in
:mod:`._wire`; outbound calls and the pipe client are in :mod:`._client`;
the inbound pipe server is in :mod:`._server`; action-based dispatch is
in :mod:`._actions`; the per-call audit log is in :mod:`._observability`.
External callers only need the names re-exported here; the private
re-exports below are convenience for ``Core/shared/tests/test_ipc.py``,
which probes a handful of internals (``_size``, ``_resolve``,
``_pick_backend``, ``_pipe_negcache``, ``_Instance``, ``PipeTimeout``)
through the package namespace.
"""

from __future__ import annotations

from ._actions import (  # noqa: F401
    list_actions,
    register_action,
    unregister_action,
)
from ._client import (  # noqa: F401
    PipeTimeout,
    call,
    call_action,
    register_handler,
    send,
    send_action,
    _is_pipe_dead,
    _mark_pipe_dead,
    _pick_backend,
    _resolve,
)
from ._observability import _log_call, _size  # noqa: F401
from ._server import (  # noqa: F401
    PipeServer,
    serve,
    serve_forever_background,
    supports_ipc,
)
from ._wire import (  # noqa: F401
    IPC_VERSION,
    LOG_PATH,
    PIPE_NEGCACHE_SECONDS,
    IpcError,
    Reply,
    _Instance,
    _pipe_negcache,
)

__all__ = [
    "send",
    "call",
    "send_action",
    "call_action",
    "serve",
    "Reply",
    "IpcError",
    "PipeServer",
    "register_handler",
    "register_action",
    "unregister_action",
    "list_actions",
    "serve_forever_background",
    "supports_ipc",
]
