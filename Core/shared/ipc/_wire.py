"""Wire-level config + datatypes for the shared IPC transport.

This module owns the env-derived constants, the optional-dependency imports
(msgpack, win32, discovery), the public data types (:class:`Reply`,
:class:`IpcError`, :class:`_Instance`), and the mutable process-identity
global ``_SELF_NAME``. Submodules pull these via ``from . import _wire as
_w`` to ensure mutations to ``_SELF_NAME`` (set by :func:`._server.serve`
and read by every transport path) are observed live, not snapshotted at
import time. The pipe negative-cache and HTTP session pool also live here
for the same reason — single shared mutable home, accessed by client and
server submodules.
"""

from __future__ import annotations

import logging
import os
import threading
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Dict, List, Optional

import requests  # noqa: F401  — re-exported for client/server modules

logger = logging.getLogger(__name__)

# ── Optional dependencies ─────────────────────────────────────────────
try:
    import msgpack

    _HAS_MSGPACK = True
except ImportError:
    msgpack = None
    _HAS_MSGPACK = False

try:
    import discovery

    _HAS_DISCOVERY = True
except ImportError:
    discovery = None
    _HAS_DISCOVERY = False

# pywin32 only exists on Windows; keep the rest of the module importable so
# tests on non-Windows CI don't explode at import time.
try:
    import pywintypes
    import win32file
    import win32pipe
    import winerror

    _HAS_WIN32 = True
except ImportError:
    pywintypes = win32file = win32pipe = winerror = None
    _HAS_WIN32 = False


# ── Env toggles ────────────────────────────────────────────────────────
_TRANSPORT = os.getenv("WYLDE_TRANSPORT", "pipe").lower().strip()
if _TRANSPORT not in ("pipe", "http"):
    logger.warning("ipc: unknown WYLDE_TRANSPORT=%r; defaulting to pipe", _TRANSPORT)
    _TRANSPORT = "pipe"

IPC_DISABLE = os.getenv("WYLDE_IPC_DISABLE", "").lower() in ("1", "true", "yes")
DEFAULT_TIMEOUT = float(os.getenv("WYLDE_IPC_TIMEOUT", "30"))
LOG_PATH = Path(os.getenv("WYLDE_IPC_LOG", "logs/ipc.jsonl"))

_SELF_NAME = os.getenv("WYLDE_SERVICE_NAME", "unknown")

# Pool sizing
PIPE_POOL_MAX = int(os.getenv("WYLDE_PIPE_POOL", "4"))
PIPE_CONNECT_TIMEOUT_MS = int(os.getenv("WYLDE_PIPE_CONNECT_TIMEOUT_MS", "2000"))
PIPE_NEGCACHE_SECONDS = 30.0

# Wire protocol version. Bumped when the envelope shape or handshake changes
# in a breaking way. v1 is the first version to carry a handshake.
IPC_VERSION = 1

# Frame read timeouts. The "body" timeout bounds how long a mid-frame stall can
# hang a handler (the bug this hardening addresses). The "idle" timeout bounds
# how long a pipe may sit between complete frames before we reap it. Handshake
# and ping timeouts are kept short because they are interactive control paths.
FRAME_READ_TIMEOUT = float(os.getenv("WYLDE_IPC_READ_TIMEOUT", "30"))
IDLE_READ_TIMEOUT = float(os.getenv("WYLDE_IPC_IDLE_TIMEOUT", "300"))
HANDSHAKE_TIMEOUT = float(os.getenv("WYLDE_IPC_HANDSHAKE_TIMEOUT", "5"))
HEARTBEAT_IDLE_SECONDS = float(os.getenv("WYLDE_IPC_HEARTBEAT_IDLE", "60"))

_pipe_negcache: Dict[str, float] = {}
_pipe_negcache_lock = threading.Lock()

_sessions: Dict[str, requests.Session] = {}
_sessions_lock = threading.Lock()


# ── Public data types ─────────────────────────────────────────────────
@dataclass
class Reply:
    ok: bool
    data: Any = None
    error: Optional[Dict[str, Any]] = None
    transport: str = ""
    duration_ms: float = 0.0

    def raise_for_error(self) -> "Reply":
        if not self.ok:
            code = (self.error or {}).get("code", "unknown")
            msg = (self.error or {}).get("message", "ipc call failed")
            raise IpcError(code, msg, self.error or {})
        return self


class IpcError(Exception):
    def __init__(
        self, code: str, message: str, details: Optional[Dict[str, Any]] = None
    ):
        self.code = code
        self.message = message
        self.details = details or {}
        super().__init__(f"{code}: {message}")


@dataclass
class _Instance:
    address: str
    port: int
    tags: List[str] = field(default_factory=list)
    meta: Dict[str, str] = field(default_factory=dict)
    pipe_only: bool = False
    # Discovery-cache expiry timestamp (monotonic seconds). Populated by
    # ``_resolve_via_discovery`` and read on subsequent lookups to skip a
    # repeat discovery call when the cached instance is still fresh. Kept
    # on the dataclass (rather than tacked on as a dynamic attr) so strict
    # type checkers don't need a ``# type: ignore`` to set it.
    _expires: float = 0.0

    @property
    def supports_pipe(self) -> bool:
        return (
            self.pipe_only or "ipc=pipe" in self.tags or self.meta.get("ipc") == "pipe"
        )

    @property
    def url(self) -> str:
        return f"http://{self.address}:{self.port}"
