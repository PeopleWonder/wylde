"""Outbound IPC: ``send`` / ``call`` plus backend selection, HTTP, and the
pipe client (handle pool, handshake, framing).

This module is the caller side of the pipe protocol. Inbound — i.e. the
``PipeServer`` that listens on ``\\\\.\\pipe\\wylde-<service>`` and dispatches
into a Flask app — lives in :mod:`._server`.
"""

from __future__ import annotations

import logging
import os
import queue
import threading
import time
import uuid
from typing import Any, Dict, List, Optional

import requests

from . import _wire as _w
from ._wire import (
    DEFAULT_TIMEOUT,
    HANDSHAKE_TIMEOUT,
    HEARTBEAT_IDLE_SECONDS,
    FRAME_READ_TIMEOUT,
    IDLE_READ_TIMEOUT,
    IPC_VERSION,
    PIPE_CONNECT_TIMEOUT_MS,
    PIPE_NEGCACHE_SECONDS,
    PIPE_POOL_MAX,
    Reply,
    _Instance,
)
from ._actions import _ACTION_DISPATCH_PATH
from ._observability import _log_call, _size

logger = logging.getLogger(__name__)


# ── Public API ────────────────────────────────────────────────────────
def send(
    service: str,
    method: str,
    data: Any = None,
    timeout: float = DEFAULT_TIMEOUT,
    http_verb: str = "POST",
) -> Reply:
    """Fire one request at `service`, return a Reply."""
    t0 = time.perf_counter()

    inst = _resolve(service)
    if inst is None:
        dur = (time.perf_counter() - t0) * 1000
        reply = Reply(
            ok=False,
            error={
                "code": "not_found",
                "message": f"no healthy instance of {service!r}",
            },
            transport="none",
            duration_ms=dur,
        )
        _log_call(service, method, reply, bytes_in=_size(data), bytes_out=0)
        return reply

    backend = _pick_backend(inst)
    if backend == "pipe":
        reply = _send_pipe(service, method, data, timeout, http_verb)
        if (
            not reply.ok
            and reply.error
            and reply.error.get("code")
            in (
                "pipe_unavailable",
                "pipe_connect",
            )
        ):
            _mark_pipe_dead(service)
            # Look up HTTP fallback via discovery only when pipe actually fails.
            http_inst = _resolve_via_discovery(service)
            if http_inst and http_inst.port:
                reply = _send_http(service, http_inst, method, data, timeout, http_verb)
    else:
        reply = _send_http(service, inst, method, data, timeout, http_verb)

    reply.duration_ms = (time.perf_counter() - t0) * 1000
    _log_call(
        service,
        method,
        reply,
        bytes_in=_size(data),
        bytes_out=_size(reply.data),
    )
    return reply


def call(service: str, method: str, data: Any = None, **kw: Any) -> Any:
    """Like send(), but raises on error and returns .data directly."""
    return send(service, method, data, **kw).raise_for_error().data


def register_handler(app: Any, method: str, path: Optional[str] = None) -> None:
    """Record that `method` maps to a Flask route. Pipe server uses this
    table to dispatch; if not registered, method is treated as the path
    directly ("/" + method)."""
    _handler_registry.setdefault(id(app), {})[method] = path or f"/{method}"


_handler_registry: Dict[int, Dict[str, str]] = {}


# ── Backend selection ─────────────────────────────────────────────────
def _pick_backend(inst: _Instance) -> str:
    if _w.IPC_DISABLE or _w._TRANSPORT == "http":
        return "http"
    if not _w._HAS_WIN32 or not _w._HAS_MSGPACK:
        return "http"
    if not inst.supports_pipe:
        return "http"
    if _is_pipe_dead(inst.address if inst.address else "default"):
        return "http"
    return "pipe"


def _mark_pipe_dead(service: str) -> None:
    with _w._pipe_negcache_lock:
        _w._pipe_negcache[service] = time.time() + PIPE_NEGCACHE_SECONDS


def _is_pipe_dead(key: str) -> bool:
    with _w._pipe_negcache_lock:
        until = _w._pipe_negcache.get(key)
        if until is None:
            return False
        if time.time() > until:
            _w._pipe_negcache.pop(key, None)
            return False
        return True


# ── Discovery integration ─────────────────────────────────────────────
_resolve_cache: Dict[str, _Instance] = {}
_resolve_cache_lock = threading.Lock()
_RESOLVE_TTL_SECONDS = 30.0


def _resolve(service: str) -> Optional[_Instance]:
    """Resolve `service` to an instance.

     Hot path: in pipe mode, synthesize the pipe-only instance by convention
    , no discovery lookup, no blocking. The pipe name is `wylde-<service>`
     regardless of what discovery says. Discovery is only consulted when we
     need an HTTP fallback (transport=http, or pipe connect failed).
    """
    if (
        _w._TRANSPORT == "pipe"
        and _w._HAS_WIN32
        and _w._HAS_MSGPACK
        and not _w.IPC_DISABLE
    ):
        return _Instance(
            address="127.0.0.1",
            port=0,
            tags=["ipc=pipe"],
            meta={"ipc": "pipe"},
            pipe_only=True,
        )
    return _resolve_via_discovery(service)


def _resolve_via_discovery(service: str) -> Optional[_Instance]:
    with _resolve_cache_lock:
        inst = _resolve_cache.get(service)
        if inst is not None and inst._expires > time.time():
            return inst

    if not _w._HAS_DISCOVERY:
        return None
    try:
        instances = _w.discovery.get_healthy_instances(service)
    except Exception as e:  # noqa: BLE001
        logger.debug("ipc: discovery.get_healthy_instances(%s) raised: %s", service, e)
        instances = []
    if not instances:
        return None
    first = instances[0]
    inst = _Instance(
        address=first.get("address", "127.0.0.1"),
        port=int(first.get("port") or 0),
        tags=list(first.get("tags") or []),
        meta=dict(first.get("meta") or {}),
        _expires=time.time() + _RESOLVE_TTL_SECONDS,
    )
    with _resolve_cache_lock:
        _resolve_cache[service] = inst
    return inst


# ── HTTP backend ──────────────────────────────────────────────────────
def _session_for(service: str) -> requests.Session:
    with _w._sessions_lock:
        sess = _w._sessions.get(service)
        if sess is None:
            sess = requests.Session()
            sess.headers.update({"X-Wylde-IPC-Caller": _w._SELF_NAME})
            _w._sessions[service] = sess
        return sess


def _send_http(
    service: str,
    inst: _Instance,
    method: str,
    data: Any,
    timeout: float,
    http_verb: str,
) -> Reply:
    if not inst.port:
        return Reply(
            ok=False,
            error={
                "code": "no_http_port",
                "message": f"service {service} has no HTTP port registered",
            },
            transport="http",
        )
    path = method if method.startswith("/") else f"/{method}"
    url = f"{inst.url}{path}"
    sess = _session_for(service)
    try:
        if http_verb.upper() == "GET":
            resp = sess.get(
                url, params=data if isinstance(data, dict) else None, timeout=timeout
            )
        else:
            resp = sess.request(http_verb.upper(), url, json=data, timeout=timeout)
    except requests.RequestException as e:
        return Reply(
            ok=False,
            error={"code": "transport", "message": str(e), "details": {"url": url}},
            transport="http",
        )

    if resp.status_code >= 400:
        err_body: Any
        try:
            err_body = resp.json()
        except ValueError:
            err_body = {"body": resp.text[:2000]}
        msg = err_body.get("error") if isinstance(err_body, dict) else None
        return Reply(
            ok=False,
            error={
                "code": f"http_{resp.status_code}",
                "message": msg or f"HTTP {resp.status_code}",
                "details": err_body
                if isinstance(err_body, dict)
                else {"body": str(err_body)[:2000]},
            },
            transport="http",
        )
    try:
        payload = resp.json()
    except ValueError:
        payload = resp.text
    return Reply(ok=True, data=payload, transport="http")


# ── Pipe client ───────────────────────────────────────────────────────
def _pipe_name(service: str) -> str:
    # Strip leading "wylde-" so "wylde-rag" → \\.\pipe\wylde-rag, not wylde-wylde-rag.  # wylde-check: dead-ref-ok
    name = service.removeprefix("wylde-")
    return rf"\\.\pipe\wylde-{name}"


def pipe_exists(service: str) -> bool:
    """Cheap, non-blocking check: is \\\\.\\pipe\\wylde-<service> up right now?

    Used by fletch-web's lazy-start path to decide whether to spawn a  # wylde-check: dead-ref-ok
    service's start_*.bat before proxying the first request. Returns
    False on non-Windows so lazy-start becomes a no-op there.
    """
    if not _w._HAS_WIN32:
        return False
    try:
        return os.path.exists(_pipe_name(service))
    except OSError:
        return False


class PipeTimeout(IOError):
    """Raised when a pipe read exceeds its deadline.

    A subclass of IOError so existing `except (pywintypes.error, IOError, OSError)`
    handlers still catch it; new code can distinguish timeouts by type.
    """


class _PipeHandle:
    """One connected pipe handle. Not thread-safe."""

    __slots__ = ("handle", "service", "last_used", "peer_version")

    def __init__(self, service: str, handle: Any) -> None:
        self.service = service
        self.handle = handle
        self.last_used = time.monotonic()
        self.peer_version = 0  # set by handshake; 0 means unknown / v0 peer

    def send_frame(self, payload: bytes) -> None:
        header = len(payload).to_bytes(4, "big")
        _w.win32file.WriteFile(self.handle, header + payload)
        self.last_used = time.monotonic()

    def read_frame(
        self,
        header_timeout: Optional[float] = None,
        body_timeout: Optional[float] = None,
    ) -> bytes:
        """Read one length-prefixed frame.

        `header_timeout` bounds the wait for a new frame to begin (idle wait);
        `body_timeout` bounds how long we wait for the rest of a frame once the
        4-byte header has been received. Keeping them distinct lets idle pooled
        connections wait minutes for the next request while still catching
        malformed / truncated frames quickly.
        """
        if header_timeout is None:
            header_timeout = IDLE_READ_TIMEOUT
        if body_timeout is None:
            body_timeout = FRAME_READ_TIMEOUT
        hdr = self._read_exact(4, header_timeout)
        n = int.from_bytes(hdr, "big")
        if n <= 0 or n > 64 * 1024 * 1024:
            raise IOError(f"pipe frame size out of range: {n}")
        body = self._read_exact(n, body_timeout)
        self.last_used = time.monotonic()
        return body

    def _read_exact(self, n: int, timeout_seconds: float) -> bytes:
        """Read exactly n bytes, or raise PipeTimeout after `timeout_seconds`.

        Uses PeekNamedPipe to poll for availability so a stalled peer cannot
        park a worker thread indefinitely. Backoff grows from 1ms to 100ms so
        latency stays low when data is flowing and CPU stays quiet when idle.
        """
        deadline = time.monotonic() + timeout_seconds
        chunks: List[bytes] = []
        remaining = n
        sleep = 0.001
        while remaining > 0:
            try:
                _, avail, _ = _w.win32pipe.PeekNamedPipe(self.handle, 0)
            except _w.pywintypes.error as e:
                raise IOError(f"PeekNamedPipe failed: {e}")
            if avail <= 0:
                if time.monotonic() >= deadline:
                    raise PipeTimeout(
                        f"pipe read timeout after {timeout_seconds:.1f}s "
                        f"(still waiting for {remaining} of {n} bytes)"
                    )
                time.sleep(sleep)
                sleep = min(sleep * 2, 0.1)
                continue
            sleep = 0.001
            to_read = min(remaining, int(avail), 65536)
            hr, data = _w.win32file.ReadFile(self.handle, to_read)
            if hr != 0:
                raise IOError(f"ReadFile hr={hr}")
            if not data:
                raise IOError("pipe closed mid-frame")
            chunks.append(bytes(data))
            remaining -= len(data)
        return b"".join(chunks)

    def close(self) -> None:
        try:
            _w.win32file.CloseHandle(self.handle)
        except Exception:
            pass


class _PipePool:
    """Pool of pipe handles per (service)."""

    def __init__(self) -> None:
        self._pools: Dict[str, "queue.LifoQueue[_PipeHandle]"] = {}
        self._lock = threading.Lock()

    def _pool(self, service: str) -> "queue.LifoQueue[_PipeHandle]":
        with self._lock:
            q = self._pools.get(service)
            if q is None:
                q = queue.LifoQueue(maxsize=PIPE_POOL_MAX)
                self._pools[service] = q
            return q

    def acquire(self, service: str) -> _PipeHandle:
        """Hand out a ready-to-use pipe handle.

        Pulls from the pool first, but any handle that has been idle for longer
        than HEARTBEAT_IDLE_SECONDS is pinged before reuse. If the ping fails
        we drop the handle and try the next one, ultimately falling through to
        a fresh connect. This reaps silently-dead connections without making
        hot-path calls pay for it.
        """
        q = self._pool(service)
        while True:
            try:
                h: _PipeHandle = q.get_nowait()
            except queue.Empty:
                return self._connect(service)
            if time.monotonic() - h.last_used > HEARTBEAT_IDLE_SECONDS:
                if not _ping(h):
                    h.close()
                    continue
            return h

    def release(self, h: _PipeHandle) -> None:
        q = self._pool(h.service)
        try:
            q.put_nowait(h)
        except queue.Full:
            h.close()

    def drop(self, h: _PipeHandle) -> None:
        """Discard a handle (e.g. broken connection)."""
        h.close()

    @staticmethod
    def _connect(service: str) -> _PipeHandle:
        name = _pipe_name(service)
        # Try CreateFile directly first — when the pipe exists and has a free
        # instance this is instant. Fall back to WaitNamedPipe only if all
        # instances are busy (ERROR_PIPE_BUSY).
        deadline = time.time() + (PIPE_CONNECT_TIMEOUT_MS / 1000.0)
        last_err: Optional[Exception] = None
        handle = None
        while time.time() < deadline:
            try:
                handle = _w.win32file.CreateFile(
                    name,
                    _w.win32file.GENERIC_READ | _w.win32file.GENERIC_WRITE,
                    0,
                    None,
                    _w.win32file.OPEN_EXISTING,
                    0,
                    None,
                )
                try:
                    _w.win32pipe.SetNamedPipeHandleState(
                        handle,
                        _w.win32pipe.PIPE_READMODE_BYTE,
                        None,
                        None,
                    )
                except _w.pywintypes.error:
                    pass
                break
            except _w.pywintypes.error as e:
                last_err = e
                handle = None
                if e.winerror == _w.winerror.ERROR_PIPE_BUSY:
                    try:
                        _w.win32pipe.WaitNamedPipe(name, 500)
                    except _w.pywintypes.error:
                        pass
                    continue
                if e.winerror in (
                    _w.winerror.ERROR_FILE_NOT_FOUND,
                    _w.winerror.ERROR_PATH_NOT_FOUND,
                ):
                    time.sleep(0.02)
                    continue
                break
        if handle is None:
            raise ConnectionError(f"CreateFile({name}) failed: {last_err}")

        h = _PipeHandle(service, handle)
        try:
            _perform_handshake(h, service)
        except Exception as e:
            h.close()
            raise ConnectionError(f"handshake with {service!r} failed: {e}")
        return h


def _perform_handshake(h: _PipeHandle, service: str) -> None:
    """Client-side handshake. Backward-compatible with pre-v1 servers.

    We send a handshake frame (distinguished by the `wylde_ipc` key) and read
    the response under HANDSHAKE_TIMEOUT. A v1+ server replies with a
    handshake record; a pre-v1 server treats our frame as a normal request
    and returns a 404-ish error, we silently accept that and mark the peer
    as v0 so the next real request just works.
    """
    frame = _w.msgpack.packb(
        {
            "wylde_ipc": IPC_VERSION,
            "caller": _w._SELF_NAME,
            "service": service,
        },
        use_bin_type=True,
    )
    h.send_frame(frame)
    body = h.read_frame(
        header_timeout=HANDSHAKE_TIMEOUT, body_timeout=HANDSHAKE_TIMEOUT
    )
    try:
        resp = _w.msgpack.unpackb(body, raw=False)
    except Exception as e:
        raise IOError(f"handshake response decode failed: {e}")
    if not isinstance(resp, dict):
        raise IOError("handshake response is not a map")
    if "wylde_ipc" in resp:
        if not resp.get("ok"):
            err = resp.get("error") or {}
            code = err.get("code", "handshake_rejected")
            msg = err.get("message", "handshake rejected")
            raise IOError(f"{code}: {msg}")
        ver = resp.get("wylde_ipc")
        h.peer_version = int(ver) if isinstance(ver, int) else 0
    else:
        # Pre-v1 server that treated our handshake as a bogus request. That's
        # fine — its error reply is drained, the next real request will work.
        h.peer_version = 0


def _ping(h: _PipeHandle) -> bool:
    """Round-trip a ping frame. Returns True if the peer is still responsive.

    Called by the pool before handing out an idle-too-long handle; a failure
    triggers reap-and-reconnect so callers never see a half-dead socket.
    """
    try:
        frame = _w.msgpack.packb(
            {
                "ver": IPC_VERSION,
                "id": "ping",
                "method": "/__ping__",
                "data": None,
            },
            use_bin_type=True,
        )
        h.send_frame(frame)
        body = h.read_frame(
            header_timeout=HANDSHAKE_TIMEOUT, body_timeout=HANDSHAKE_TIMEOUT
        )
        resp = _w.msgpack.unpackb(body, raw=False)
        return isinstance(resp, dict) and resp.get("ok") is True
    except Exception:
        return False


_pool = _PipePool()


def _send_pipe(
    service: str,
    method: str,
    data: Any,
    timeout: float,
    http_verb: str,
) -> Reply:
    if not _w._HAS_WIN32 or not _w._HAS_MSGPACK:
        return Reply(
            ok=False,
            error={
                "code": "pipe_unavailable",
                "message": "pywin32 or msgpack not installed",
            },
            transport="pipe",
        )

    path = method if method.startswith("/") else f"/{method}"
    req_id = uuid.uuid4().hex
    envelope = {
        "ver": IPC_VERSION,
        "id": req_id,
        "method": path,
        "http_verb": http_verb.upper(),
        "data": data,
        "meta": {
            "deadline_ms": int(timeout * 1000),
            "caller": _w._SELF_NAME,
        },
    }
    try:
        payload = _w.msgpack.packb(envelope, use_bin_type=True)
    except (TypeError, ValueError) as e:
        return Reply(
            ok=False,
            error={"code": "encode", "message": f"msgpack encode failed: {e}"},
            transport="pipe",
        )

    try:
        handle = _pool.acquire(service)
    except ConnectionError as e:
        return Reply(
            ok=False,
            error={"code": "pipe_connect", "message": str(e)},
            transport="pipe",
        )

    # Use the caller's timeout as the body-read deadline so a stalled peer
    # can't hang the caller past what they asked for. The header wait is the
    # same: once we've written the request, we expect a reply within `timeout`.
    try:
        handle.send_frame(payload)
        body = handle.read_frame(header_timeout=timeout, body_timeout=timeout)
    except PipeTimeout as e:
        _pool.drop(handle)
        return Reply(
            ok=False,
            error={"code": "pipe_timeout", "message": str(e)},
            transport="pipe",
        )
    except (_w.pywintypes.error, IOError, OSError) as e:
        _pool.drop(handle)
        return Reply(
            ok=False,
            error={"code": "pipe_io", "message": str(e)},
            transport="pipe",
        )
    else:
        _pool.release(handle)

    try:
        resp = _w.msgpack.unpackb(body, raw=False)
    except Exception as e:  # noqa: BLE001
        return Reply(
            ok=False,
            error={"code": "decode", "message": f"msgpack decode failed: {e}"},
            transport="pipe",
        )

    if not isinstance(resp, dict):
        return Reply(
            ok=False,
            error={"code": "bad_response", "message": "response is not a map"},
            transport="pipe",
        )

    if resp.get("ok"):
        return Reply(ok=True, data=resp.get("data"), transport="pipe")
    return Reply(
        ok=False,
        error=resp.get("error") or {"code": "unknown", "message": "unknown error"},
        transport="pipe",
    )


# ── Action helpers ────────────────────────────────────────────────────
def send_action(
    service: str,
    action: str,
    payload: Any = None,
    timeout: float = DEFAULT_TIMEOUT,
) -> Reply:
    """Invoke a pipe-only action handler on `service` and return the Reply.

    Wraps :func:`send` with the `/__action__` envelope contract so callers
    don't have to remember the dispatch sentinel. Use this when calling a
    surface that registered handlers via :func:`register_action` instead of
    Flask routes.
    """
    return send(
        service,
        _ACTION_DISPATCH_PATH,
        {"action": action, "payload": payload},
        timeout=timeout,
        http_verb="POST",
    )


def call_action(
    service: str,
    action: str,
    payload: Any = None,
    timeout: float = DEFAULT_TIMEOUT,
) -> Any:
    """Like :func:`send_action`, but raises on error and returns `.data`."""
    return send_action(service, action, payload, timeout=timeout).raise_for_error().data
