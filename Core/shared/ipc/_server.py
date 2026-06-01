"""Inbound IPC: the pipe server, ``serve`` entry point, and the dispatch
loop that bridges incoming pipe requests into a Flask / ASGI app.

The pipe protocol matches what :mod:`._client` writes:

  ``[u32 big-endian length][msgpack body]``

Bodies are request envelopes with ``method`` / ``http_verb`` / ``data``;
the server invokes the matching route through the framework's test client
so routing, middleware, and authn behave identically to an HTTP request.
"""

from __future__ import annotations

import datetime
import json
import logging
import os
import threading
import time
from pathlib import Path
from typing import Any, Callable, Dict, List, Optional

from . import _wire as _w
from ._wire import (
    HANDSHAKE_TIMEOUT,
    IPC_VERSION,
)
from ._actions import _ACTION_DISPATCH_PATH, _REGISTERED_ACTIONS, _dispatch_action
from ._client import PipeTimeout, _PipeHandle, _pipe_name, _resolve

logger = logging.getLogger(__name__)


# Wylde repo root for contract-file writes. Mirrors the resolution in
# Core/shared/manifest.py so an explicit WYLDE_ROOT env wins over the
# import-relative fallback.
_WYLDE_ROOT = Path(os.getenv("WYLDE_ROOT", Path(__file__).parent.parent.parent.parent))


def _write_action_contract(service_name: str) -> None:
    """Write data/contracts/actions/<service>.json with registered actions.

    Snapshots :data:`._actions._REGISTERED_ACTIONS` and writes a JSON
    document the wylde_check rules read as the cross-language source of
    truth for "what actions does <service> expose". Atomic via tmpfile +
    ``os.replace`` so a partial write can't poison the next rule run.

    Best-effort: failures are logged and swallowed because a failed
    contract write must not prevent the pipe server from starting.
    """
    try:
        contracts_dir = _WYLDE_ROOT / "data" / "contracts" / "actions"
        contracts_dir.mkdir(parents=True, exist_ok=True)
        contract_path = contracts_dir / f"{service_name}.json"
        # Snapshot the registry under its lock so we don't see a torn
        # state if a handler registers concurrently during startup.
        from ._actions import _action_handlers_lock

        with _action_handlers_lock:
            details = {k: dict(v) for k, v in _REGISTERED_ACTIONS.items()}
        payload = {
            "service": service_name,
            "actions": sorted(details.keys()),
            "details": details,
            "written_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        }
        tmp_path = contract_path.with_suffix(f".{os.getpid()}.tmp")
        tmp_path.write_text(json.dumps(payload, indent=2, sort_keys=True))
        os.replace(tmp_path, contract_path)
        logger.info(
            "ipc: wrote action contract %s (%d action(s))",
            contract_path,
            len(payload["actions"]),
        )
    except Exception as e:  # noqa: BLE001
        logger.warning(
            "ipc: failed to write action contract for %s: %s", service_name, e
        )


def supports_ipc(service: str) -> bool:
    inst = _resolve(service)
    return bool(inst and inst.supports_pipe)


def serve(
    service: str,
    app: Any,
    port: Optional[int] = None,
    host: str = "127.0.0.1",
    register: bool = True,
    tags: Optional[List[str]] = None,
    meta: Optional[Dict[str, str]] = None,
) -> None:
    r"""Replacement for `app.run()`. Blocks the calling thread serving IPC
    requests on \\.\pipe\wylde-<service>. HTTP is only bound if
    WYLDE_TRANSPORT=http.

    Services migrating from HTTP flip one line:

        app.run(host='0.0.0.0', port=PORT, threaded=True)
            ↓
        ipc.serve('my-service', app, port=PORT)
    """
    _w._SELF_NAME = service

    # Action-contract dump runs before any pipe accept loop starts.
    # register_action() calls in service modules have already executed by
    # the time the service hits serve(), so the snapshot is complete here.
    # Rust services (when they land) write the same contract file shape
    # from their own startup, keeping the wylde_check rule language-agnostic.
    _write_action_contract(service)

    use_pipe = _w._TRANSPORT == "pipe" and _w._HAS_WIN32 and _w._HAS_MSGPACK
    use_http = _w._TRANSPORT == "http" or not use_pipe

    if use_pipe:
        logger.info(
            "ipc: starting pipe server for %s on \\\\.\\pipe\\wylde-%s",
            service,
            service,
        )
        server = PipeServer(service, app)
        server.start()
    else:
        if _w._TRANSPORT == "pipe" and not _w._HAS_WIN32:
            logger.warning("ipc: pywin32 missing; falling back to HTTP")
        if _w._TRANSPORT == "pipe" and not _w._HAS_MSGPACK:
            logger.warning("ipc: msgpack missing; falling back to HTTP")

    # Register with discovery so callers can find us. Pipe support is
    # announced via the `ipc=pipe` tag and `meta.ipc=pipe` for zeroconf.
    if register:
        all_tags = list(tags or [])
        if use_pipe and "ipc=pipe" not in all_tags:
            all_tags.append("ipc=pipe")
        all_meta = dict(meta or {})
        if use_pipe:
            all_meta["ipc"] = "pipe"
            all_meta["pipe"] = f"wylde-{service.removeprefix('wylde-')}"
        try:
            if _w._HAS_DISCOVERY:
                _w.discovery.register_service(
                    name=service,
                    address=host,
                    port=int(port or 0),
                    tags=all_tags,
                    meta=all_meta,
                )
                _w.discovery.install_signal_handlers(service)
        except Exception as e:  # noqa: BLE001
            logger.warning("ipc: discovery registration failed: %s", e)

    # Self-attest the serve_loop phase so wylde_check sees the full
    # startup sequence in the manifest without AST-walking source.
    try:
        from Core.shared.manifest import mark_serve_loop_entered

        mark_serve_loop_entered(service)
    except Exception as e:  # noqa: BLE001
        logger.debug("ipc: serve_loop attestation failed for %s: %s", service, e)

    if use_http:
        if port is None:
            raise ValueError("serve(): port is required when WYLDE_TRANSPORT=http")
        logger.info("ipc: starting HTTP server on %s:%d", host, port)
        app.run(host=host, port=port, debug=False, threaded=True)
    else:
        # Pipe-only mode — block forever on the pipe server. When that thread
        # exits (e.g. on shutdown signal) we return normally.
        try:
            _run_forever()
        except KeyboardInterrupt:
            pass


def serve_forever_background(
    service: str,
    app: Any = None,
    bridge: Optional[Callable[[Any, str, Any], Any]] = None,
) -> None:
    """Start a pipe server in a daemon thread. Compatibility entry point.

    Use `serve()` in new code; this stays for call sites that kept their own
    `app.run()` and just want pipe support added on top.
    """
    _w._SELF_NAME = service
    # Dump the action contract regardless of whether the pipe transport is
    # available — the contract is the cross-language source of truth and a
    # missing pipe stack must not deprive wylde_check of it.
    _write_action_contract(service)
    if _w._TRANSPORT != "pipe" or not _w._HAS_WIN32 or not _w._HAS_MSGPACK:
        logger.info(
            "ipc: pipe server skipped (transport=%s, win32=%s, msgpack=%s)",
            _w._TRANSPORT,
            _w._HAS_WIN32,
            _w._HAS_MSGPACK,
        )
        return
    if app is None:
        logger.warning(
            "ipc: serve_forever_background called without app; "
            "pipe will accept but have nothing to dispatch"
        )
        return
    server = PipeServer(service, app, external_bridge=bridge)
    server.start()
    # Self-attest the serve_loop phase so wylde_check sees the full startup
    # sequence in the manifest. Matches the attestation in serve(); without
    # this, services using the background variant (e.g. Voice) end with a
    # three-phase startup_sequence and trip rule 18.
    try:
        from Core.shared.manifest import mark_serve_loop_entered

        mark_serve_loop_entered(service)
    except Exception as e:  # noqa: BLE001
        logger.debug("ipc: serve_loop attestation failed for %s: %s", service, e)


def _run_forever() -> None:
    """Block the main thread while pipe server threads do their work."""
    stop = threading.Event()
    import signal

    for sig in (signal.SIGINT, signal.SIGTERM):
        try:
            signal.signal(sig, lambda *_: stop.set())
        except (ValueError, OSError):
            pass
    while not stop.is_set():
        stop.wait(timeout=1.0)


# ── Pipe server ───────────────────────────────────────────────────────
class PipeServer:
    """Accepts pipe connections and dispatches each request to a Flask app.

    Dispatch uses the Flask test client, which runs the request through the
    same routing and middleware stack as an HTTP request, no duplicate
    handler logic. Every connection gets its own worker thread.
    """

    def __init__(
        self,
        service: str,
        app: Any,
        external_bridge: Optional[Callable[[Any, str, Any], Any]] = None,
    ):
        if not _w._HAS_WIN32:
            raise RuntimeError("PipeServer requires pywin32")
        if not _w._HAS_MSGPACK:
            raise RuntimeError("PipeServer requires msgpack")
        self.service = service
        self.app = app
        self.external_bridge = external_bridge
        self.pipe_name = _pipe_name(service)
        self._stopped = threading.Event()
        self._accept_thread: Optional[threading.Thread] = None
        self._client_local = threading.local()

    def start(self) -> None:
        self._accept_thread = threading.Thread(
            target=self._accept_loop,
            name=f"ipc-accept-{self.service}",
            daemon=True,
        )
        self._accept_thread.start()

    def stop(self) -> None:
        self._stopped.set()

    # Accept one connection at a time, spawning a worker per client. A new
    # pipe instance is created for the next accept so multiple clients can
    # connect concurrently (up to PIPE_UNLIMITED_INSTANCES).
    def _accept_loop(self) -> None:
        while not self._stopped.is_set():
            try:
                handle = _w.win32pipe.CreateNamedPipe(
                    self.pipe_name,
                    _w.win32pipe.PIPE_ACCESS_DUPLEX,
                    (
                        _w.win32pipe.PIPE_TYPE_BYTE
                        | _w.win32pipe.PIPE_READMODE_BYTE
                        | _w.win32pipe.PIPE_WAIT
                    ),
                    _w.win32pipe.PIPE_UNLIMITED_INSTANCES,
                    65536,
                    65536,
                    0,
                    None,
                )
            except _w.pywintypes.error as e:
                logger.error("ipc: CreateNamedPipe(%s) failed: %s", self.pipe_name, e)
                time.sleep(1.0)
                continue

            try:
                _w.win32pipe.ConnectNamedPipe(handle, None)
            except _w.pywintypes.error as e:
                # ERROR_PIPE_CONNECTED (535) means a client raced us and
                # connected before ConnectNamedPipe got called — that's
                # success, not failure.
                if e.winerror != _w.winerror.ERROR_PIPE_CONNECTED:
                    logger.debug("ipc: ConnectNamedPipe: %s", e)
                    try:
                        _w.win32file.CloseHandle(handle)
                    except Exception:
                        pass
                    continue

            worker = threading.Thread(
                target=self._handle_client,
                args=(handle,),
                name=f"ipc-worker-{self.service}",
                daemon=True,
            )
            worker.start()

    def _handle_client(self, handle: Any) -> None:
        peer = _PipeHandle(self.service, handle)
        # first_request carries over a non-handshake first frame from a pre-v1
        # client so we dispatch it instead of silently dropping it.
        first_request: Optional[Dict[str, Any]] = None
        try:
            # ── Handshake phase ─────────────────────────────────────────
            # The initial frame must arrive within HANDSHAKE_TIMEOUT; a client
            # that connects and then disappears without writing can't camp on
            # a worker thread. If the first frame isn't a handshake record
            # (pre-v1 caller) we accept it as a normal request, backward
            # compatibility during rolling upgrades.
            try:
                body = peer.read_frame(
                    header_timeout=HANDSHAKE_TIMEOUT, body_timeout=HANDSHAKE_TIMEOUT
                )
            except PipeTimeout:
                self._send_error(
                    peer,
                    None,
                    "handshake_timeout",
                    "no first frame within handshake window",
                )
                return
            except (_w.pywintypes.error, IOError):
                return

            try:
                first = _w.msgpack.unpackb(body, raw=False)
            except Exception as e:  # noqa: BLE001
                self._send_error(peer, None, "decode", f"msgpack decode failed: {e}")
                return

            if isinstance(first, dict) and "wylde_ipc" in first:
                client_ver = first.get("wylde_ipc")
                if (
                    not isinstance(client_ver, int)
                    or client_ver < 1
                    or client_ver > IPC_VERSION
                ):
                    self._send_error(
                        peer,
                        None,
                        "version_mismatch",
                        f"client ipc version {client_ver!r} not supported; "
                        f"this server speaks v1..{IPC_VERSION}",
                    )
                    return
                peer.peer_version = client_ver
                try:
                    peer.send_frame(
                        _w.msgpack.packb(
                            {
                                "wylde_ipc": IPC_VERSION,
                                "ok": True,
                                "service": self.service,
                            },
                            use_bin_type=True,
                        )
                    )
                except (_w.pywintypes.error, IOError, OSError):
                    return
            else:
                # Pre-v1 client — first frame is already a request. Reuse it
                # below so we don't drop the very first call.
                if isinstance(first, dict):
                    first_request = first
                else:
                    self._send_error(peer, None, "bad_request", "request is not a map")
                    return

            # ── Request loop ────────────────────────────────────────────
            while not self._stopped.is_set():
                if first_request is not None:
                    req = first_request
                    first_request = None
                else:
                    try:
                        body = peer.read_frame()
                    except PipeTimeout as e:
                        # A stalled frame is fatal for this connection: the
                        # byte stream is no longer aligned. Tell the caller,
                        # then tear down.
                        self._send_error(peer, None, "read_timeout", str(e))
                        return
                    except (_w.pywintypes.error, IOError):
                        return
                    try:
                        req = _w.msgpack.unpackb(body, raw=False)
                    except Exception as e:  # noqa: BLE001
                        self._send_error(
                            peer, None, "decode", f"msgpack decode failed: {e}"
                        )
                        continue

                if not isinstance(req, dict):
                    self._send_error(peer, None, "bad_request", "request is not a map")
                    continue

                req_id = req.get("id") or ""
                method = req.get("method") or "/"
                http_verb = (req.get("http_verb") or "POST").upper()
                data = req.get("data")

                # Built-in control methods, answered in-band without hitting
                # the Flask/ASGI app, so they work even if the app is jammed.
                if method in ("/__ping__", "__ping__"):
                    try:
                        peer.send_frame(
                            _w.msgpack.packb(
                                {
                                    "id": req_id,
                                    "ok": True,
                                    "data": {"pong": True, "ver": IPC_VERSION},
                                },
                                use_bin_type=True,
                            )
                        )
                    except (_w.pywintypes.error, IOError, OSError):
                        return
                    continue
                if method in ("/__handshake__", "__handshake__"):
                    try:
                        peer.send_frame(
                            _w.msgpack.packb(
                                {
                                    "id": req_id,
                                    "ok": True,
                                    "data": {
                                        "ver": IPC_VERSION,
                                        "service": self.service,
                                    },
                                },
                                use_bin_type=True,
                            )
                        )
                    except (_w.pywintypes.error, IOError, OSError):
                        return
                    continue

                # Action dispatch — pipe-only handlers that never touch
                # the Flask app. Used by surfaces that must remain
                # unreachable over HTTP regardless of WYLDE_TRANSPORT.
                if method == _ACTION_DISPATCH_PATH:
                    reply = _dispatch_action(data)
                    reply["id"] = req_id
                    try:
                        peer.send_frame(_w.msgpack.packb(reply, use_bin_type=True))
                    except (_w.pywintypes.error, IOError, OSError):
                        return
                    continue

                try:
                    reply = self._dispatch(method, http_verb, data)
                    reply["id"] = req_id
                    peer.send_frame(_w.msgpack.packb(reply, use_bin_type=True))
                except (_w.pywintypes.error, IOError, OSError):
                    # Transport is gone — no point trying to send an error.
                    return
                except Exception as e:  # noqa: BLE001
                    logger.exception("ipc: dispatch failed")
                    # Handler exceptions must not break the connection, send
                    # a structured error and continue servicing the same pipe.
                    try:
                        self._send_error(
                            peer, req_id, "handler", f"{type(e).__name__}: {e}"
                        )
                    except Exception:
                        logger.exception("ipc: failed to send error frame")
                        return
        finally:
            # Best-effort cleanup: disconnect the server side of the pipe and
            # release the OS handle. Both must run even if an exception got us
            # here, otherwise we leak file descriptors and pipe instances.
            try:
                _w.win32pipe.DisconnectNamedPipe(handle)
            except Exception:
                pass
            try:
                peer.close()
            except Exception:
                pass

    def _dispatch(self, method: str, http_verb: str, data: Any) -> Dict[str, Any]:
        if self.external_bridge is not None:
            try:
                result = self.external_bridge(self.app, method, data)
                return {"ok": True, "data": result, "status": 200}
            except Exception as e:  # noqa: BLE001
                return {
                    "ok": False,
                    "error": {
                        "code": "handler",
                        "message": f"{type(e).__name__}: {e}",
                    },
                }

        # Default path: run the route via its test client. Supports both
        # Flask (app.test_client()) and ASGI/FastAPI (starlette TestClient).
        path = method if method.startswith("/") else f"/{method}"
        client: Any = getattr(self._client_local, "client", None)
        if client is None:
            if hasattr(self.app, "test_client") and callable(
                getattr(self.app, "test_client")
            ):
                client = self.app.test_client()  # Flask
                self._client_local.kind = "flask"
            else:
                from starlette.testclient import TestClient

                client = TestClient(self.app)  # FastAPI / Starlette
                self._client_local.kind = "asgi"
            self._client_local.client = client
        client_any: Any = client

        kind = self._client_local.kind

        headers = {"X-Wylde-IPC-Caller": _w._SELF_NAME}

        if kind == "flask":
            kwargs: Dict[str, Any] = {"method": http_verb, "headers": headers}
            if http_verb == "GET":
                if isinstance(data, dict):
                    kwargs["query_string"] = data
            else:
                if isinstance(data, (bytes, bytearray)):
                    kwargs["data"] = bytes(data)
                elif data is not None:
                    kwargs["json"] = data
            resp = client_any.open(path, **kwargs)
            raw = resp.get_data()
        else:  # ASGI
            req_kwargs: Dict[str, Any] = {"headers": headers}
            if http_verb == "GET":
                if isinstance(data, dict):
                    req_kwargs["params"] = data
            else:
                if isinstance(data, (bytes, bytearray)):
                    req_kwargs["content"] = bytes(data)
                elif data is not None:
                    req_kwargs["json"] = data
            resp = client_any.request(http_verb, path, **req_kwargs)
            raw = resp.content

        # Parse body → Python value. Prefer JSON, fall back to text/bytes.
        ctype = (resp.headers.get("Content-Type") or "").lower()
        payload: Any
        if "application/json" in ctype:
            try:
                payload = json.loads(raw.decode("utf-8") if raw else "null")
            except (ValueError, UnicodeDecodeError):
                payload = raw.decode("utf-8", errors="replace")
        else:
            try:
                payload = raw.decode("utf-8")
            except UnicodeDecodeError:
                payload = raw  # binary

        if resp.status_code >= 400:
            err_msg = (
                payload.get("error") if isinstance(payload, dict) else None
            ) or f"HTTP {resp.status_code}"
            return {
                "ok": False,
                "status": resp.status_code,
                "error": {
                    "code": f"http_{resp.status_code}",
                    "message": err_msg,
                    "details": payload
                    if isinstance(payload, dict)
                    else {"body": str(payload)[:2000]},
                },
            }
        return {"ok": True, "status": resp.status_code, "data": payload}

    def _send_error(
        self, peer: _PipeHandle, req_id: Optional[str], code: str, message: str
    ) -> None:
        try:
            peer.send_frame(
                _w.msgpack.packb(
                    {
                        "id": req_id or "",
                        "ok": False,
                        "error": {"code": code, "message": message},
                    },
                    use_bin_type=True,
                )
            )
        except Exception:
            pass
