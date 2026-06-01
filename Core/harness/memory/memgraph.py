"""Thin client to the Memgraph service (``Wylde/Core/Memgraph/``).

The Memgraph service stays as a separate process — it owns the Neo4j-shaped
relational graph (Bolt @ ``bolt://127.0.0.1:7687`` by default) and exposes a
named-pipe API at ``\\\\.\\pipe\\wylde-memgraph``. The Lifecycle daemon
spawns it as a subprocess in its Phase 2c boot step.

This module is the **client only**. CRUD goes through here; heavy enrichment
(community detection, summaries, multi-hop reasoning over thousands of
nodes) lives in N8N workflows per the Wylde N8N principle.

Wire format: u32 big-endian length + msgpack body. The actual transport is
shared across services in :mod:`Wylde.Core.Network` (Phase 4c — placeholder
import below). Until that lands this client speaks the same wire shape as
the legacy ``ipc.py`` so it can plug into the existing service unchanged.

Public surface mirrors the legacy service routes (one Python function per
route):

* :func:`health`          — ``GET /health``
* :func:`ensure_schema`   — ``POST /ensure_schema``
* :func:`upsert`          — ``POST /upsert``
* :func:`delete_path`     — ``POST /delete_path``
* :func:`delete_workspace`— ``POST /delete_workspace``
* :func:`traverse`        — ``POST /traverse``
* :func:`relate`          — ``POST /relate``
* :func:`unrelate`        — ``POST /unrelate``
* :func:`multihop`        — ``POST /multihop``
* :func:`stats`           — ``GET /stats``

Failure model: every call returns a ``MemgraphReply`` with ``ok``, ``data``,
``error``. Network errors and msgpack-decode failures surface as ``ok=False``
with ``error.code`` set; they don't raise. Callers that want the raise
semantics of legacy ``ipc.call`` can use :func:`call_or_raise`.
"""

from __future__ import annotations

import struct
from dataclasses import dataclass
from typing import Any, Dict, List, Optional

# TODO(phase 4c): replace this raw pipe handling with a shared transport
# from ``Wylde.Core.Network``. Today the body of _send_pipe matches the wire
# format used by ``_legacy/core/wylde-rag/ipc.py`` (u32 BE length + msgpack).
try:
    import msgpack
except ImportError:  # pragma: no cover — production deps include msgpack
    msgpack = None  # noqa: N816 — module-level alias

# Windows-only pipe transport. On non-Windows we fall through to HTTP fallback.
try:
    import win32file
    import win32pipe
    import pywintypes

    _HAVE_PIPE = True
except ImportError:  # pragma: no cover — Linux/macOS path
    _HAVE_PIPE = False

import os

import requests

from ._common import MEMGRAPH_PIPE_NAME, MEMGRAPH_SERVICE_NAME, logger

_DEFAULT_TIMEOUT_S = 5.0
_HTTP_FALLBACK_URL = os.getenv("WYLDE_MEMGRAPH_URL", "http://127.0.0.1:8010").rstrip(
    "/"
)
_PROTOCOL_VERSION = 1
_LOG = logger.getChild("memgraph")


@dataclass
class MemgraphReply:
    """Envelope returned by every client call.

    Mirrors the ``Reply`` shape from the legacy ipc module so consumers that
    know the old API can switch with no behavioural change.
    """

    ok: bool
    data: Any = None
    error: Optional[Dict[str, Any]] = None
    transport: str = "pipe"
    duration_ms: int = 0


# ─── Transport ──────────────────────────────────────────────────────────────


def _send_pipe(
    method: str, http_verb: str, payload: Dict[str, Any], timeout: float
) -> MemgraphReply:
    """Send one request through the Windows named pipe."""
    if msgpack is None or not _HAVE_PIPE:
        return MemgraphReply(
            ok=False,
            error={"code": "internal_error", "message": "pipe transport unavailable"},
            transport="pipe",
        )
    body = msgpack.packb(
        {
            "v": _PROTOCOL_VERSION,
            "method": method,
            "verb": http_verb,
            "data": payload or {},
        }
    )
    framed = struct.pack(">I", len(body)) + body
    handle = None
    try:
        handle = win32file.CreateFile(
            MEMGRAPH_PIPE_NAME,
            win32file.GENERIC_READ | win32file.GENERIC_WRITE,
            0,
            None,
            win32file.OPEN_EXISTING,
            0,
            None,
        )
        win32pipe.SetNamedPipeHandleState(
            handle, win32pipe.PIPE_READMODE_MESSAGE, None, None
        )
        win32file.WriteFile(handle, framed)
        # Read length-prefixed reply.
        _, header = win32file.ReadFile(handle, 4)
        (n,) = struct.unpack(">I", header)
        _, raw = win32file.ReadFile(handle, n)
        reply = msgpack.unpackb(raw, raw=False)
        return MemgraphReply(
            ok=bool(reply.get("ok", False)),
            data=reply.get("data"),
            error=reply.get("error"),
            transport="pipe",
        )
    except (pywintypes.error, OSError) as exc:
        _LOG.debug("pipe call %s failed: %s", method, exc)
        return MemgraphReply(
            ok=False,
            error={"code": "connection_refused", "message": str(exc)},
            transport="pipe",
        )
    finally:
        if handle is not None:
            try:
                win32file.CloseHandle(handle)
            except Exception:
                pass


def _send_http(
    method: str, http_verb: str, payload: Dict[str, Any], timeout: float
) -> MemgraphReply:
    """HTTP fallback — used on non-Windows or when the pipe is down."""
    url = _HTTP_FALLBACK_URL + method
    try:
        if http_verb.upper() == "GET":
            resp = requests.get(url, params=payload or {}, timeout=timeout)
        else:
            resp = requests.post(url, json=payload or {}, timeout=timeout)
        if not resp.ok:
            return MemgraphReply(
                ok=False,
                error={"code": "internal_error", "message": f"HTTP {resp.status_code}"},
                transport="http",
            )
        return MemgraphReply(ok=True, data=resp.json(), transport="http")
    except requests.RequestException as exc:
        return MemgraphReply(
            ok=False,
            error={"code": "connection_refused", "message": str(exc)},
            transport="http",
        )


def _send(
    method: str,
    http_verb: str = "POST",
    payload: Optional[Dict[str, Any]] = None,
    timeout: float = _DEFAULT_TIMEOUT_S,
) -> MemgraphReply:
    """Try pipe first, fall back to HTTP. The pipe is the canonical transport
    on Windows; the HTTP path is for cross-platform dev only.
    """
    payload = payload or {}
    if _HAVE_PIPE and msgpack is not None:
        reply = _send_pipe(method, http_verb, payload, timeout)
        if reply.ok or (reply.error or {}).get("code") not in ("connection_refused",):
            return reply
        _LOG.debug("memgraph pipe unreachable; falling back to HTTP")
    return _send_http(method, http_verb, payload, timeout)


def call_or_raise(
    method: str,
    http_verb: str = "POST",
    payload: Optional[Dict[str, Any]] = None,
    timeout: float = _DEFAULT_TIMEOUT_S,
) -> Any:
    """Send a request and return ``data`` on success; raise on failure.

    Mirrors the legacy ``ipc.call`` semantics for callers that prefer
    exceptions over envelopes.
    """
    reply = _send(method, http_verb=http_verb, payload=payload, timeout=timeout)
    if not reply.ok:
        err = reply.error or {}
        raise RuntimeError(
            f"memgraph {method} failed: {err.get('code', '?')}: {err.get('message', '')}"
        )
    return reply.data


# ─── Public API (one function per service route) ───────────────────────────


def health(timeout: float = 2.0) -> MemgraphReply:
    """``GET /health`` — ``{"ok": bool}``."""
    return _send("/health", http_verb="GET", timeout=timeout)


def ensure_schema(timeout: float = 10.0) -> MemgraphReply:
    """``POST /ensure_schema`` — idempotent index creation."""
    return _send("/ensure_schema", http_verb="POST", timeout=timeout)


def upsert(chunks: List[Dict[str, Any]], timeout: float = 30.0) -> MemgraphReply:
    """``POST /upsert`` — ``body: {"chunks": [{id, path, symbol, language, entities}, ...]}``."""
    return _send(
        "/upsert", http_verb="POST", payload={"chunks": chunks}, timeout=timeout
    )


def delete_path(path: str, timeout: float = 10.0) -> MemgraphReply:
    """``POST /delete_path`` — drop chunks/edges for a single source path."""
    return _send(
        "/delete_path", http_verb="POST", payload={"path": path}, timeout=timeout
    )


def delete_workspace(workspace_id: str, timeout: float = 30.0) -> MemgraphReply:
    """``POST /delete_workspace`` — drop everything for a workspace."""
    return _send(
        "/delete_workspace",
        http_verb="POST",
        payload={"workspace": workspace_id},
        timeout=timeout,
    )


def traverse(
    entities: List[str], *, max_hops: int = 2, limit: int = 50, timeout: float = 10.0
) -> MemgraphReply:
    """``POST /traverse`` — entity-anchored chunk discovery."""
    return _send(
        "/traverse",
        http_verb="POST",
        payload={"entities": entities, "max_hops": max_hops, "limit": limit},
        timeout=timeout,
    )


def relate(
    rel_type: str, pairs: List[Dict[str, str]], timeout: float = 10.0
) -> MemgraphReply:
    """``POST /relate`` — typed Entity→Entity edges.

    ``rel_type`` must be one of CALLS, IMPORTS, INHERITS, CONFIGURES, EXPOSES
    (validated server-side). ``pairs`` is a list of ``{"source": .., "target": ..}``.
    """
    return _send(
        "/relate",
        http_verb="POST",
        payload={"rel_type": rel_type, "pairs": pairs},
        timeout=timeout,
    )


def unrelate(
    rel_type: str, pairs: List[Dict[str, str]], timeout: float = 10.0
) -> MemgraphReply:
    """``POST /unrelate`` — remove typed edges."""
    return _send(
        "/unrelate",
        http_verb="POST",
        payload={"rel_type": rel_type, "pairs": pairs},
        timeout=timeout,
    )


def multihop(
    start: List[str], *, max_hops: int = 3, limit: int = 50, timeout: float = 15.0
) -> MemgraphReply:
    """``POST /multihop`` — multi-hop traversal from seed entities."""
    return _send(
        "/multihop",
        http_verb="POST",
        payload={"start": start, "max_hops": max_hops, "limit": limit},
        timeout=timeout,
    )


def upsert_edge(
    source: str,
    label: str,
    target: str,
    *,
    weight_delta: float = 1.0,
    timeout: float = 10.0,
) -> MemgraphReply:
    """``POST /upsert_edge`` — create or *strengthen* a weighted edge.

    ``MERGE``-style upsert of a ``source -[label]-> target`` edge: if the
    edge already exists its ``weight`` is incremented by ``weight_delta``,
    otherwise the edge is created with ``weight = weight_delta``.

    This is the write half of the reader→writer graph-feedback loop
    (:mod:`rag_feedback`) — a successful cited retrieval strengthens the
    entity→chunk edges that produced it, a miss leaves a low-weight trail.
    ``source`` / ``target`` are node identifiers (an entity name or a chunk
    id); node resolution and the exact MERGE semantics are owned
    server-side. The route is a forward-looking client extension — older
    Memgraph service builds without it simply reply ``ok=False``, which the
    feedback layer treats as a best-effort skip.
    """
    return _send(
        "/upsert_edge",
        http_verb="POST",
        payload={
            "source": source,
            "label": label,
            "target": target,
            "weight_delta": weight_delta,
        },
        timeout=timeout,
    )


def stats(timeout: float = 5.0) -> MemgraphReply:
    """``GET /stats`` — ``{"ok": bool, "entities": int, "chunks": int, "mentions": int}``."""
    return _send("/stats", http_verb="GET", timeout=timeout)


# Heavy enrichment (community detection, summaries) NOT exposed here — those
# go through N8N workflows per the Wylde N8N principle.


__all__ = [
    "MEMGRAPH_SERVICE_NAME",
    "MEMGRAPH_PIPE_NAME",
    "MemgraphReply",
    "call_or_raise",
    "health",
    "ensure_schema",
    "upsert",
    "delete_path",
    "delete_workspace",
    "traverse",
    "relate",
    "unrelate",
    "multihop",
    "upsert_edge",
    "stats",
]
