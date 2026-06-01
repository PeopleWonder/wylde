"""Service entrypoint: register HTTP routes + pipe actions, start the pipe alias + workers.

Manifest writing lives in ``Core/resource_monitor/run.py`` per the
service-owns-manifest convention; ``install()`` only wires the HTTP
routes, pipe action handlers, and pipe surface that the broker exposes.

The Flask routes and pipe actions both dispatch into the same
underlying helpers (``_try_grant``, ``_registry``, ``_state_snapshot``,
``_model_cache``), so there is exactly one implementation of each
operation regardless of which transport the caller used. The action
surface (`vram.reserve`, `vram.release`, ...) matches the Rust port at
``rust/crates/wylde-vram-broker/src/service.rs`` byte-for-byte, so the
``WYLDE_WYLDE_VRAM_BROKER_IMPL`` strangler-fig switch is safe to flip
without breaking Python clients.
"""

from __future__ import annotations

import logging
import time
from typing import Any, Dict, List

from flask import Flask, Response, jsonify, request

from Core.shared.ipc import IpcError, register_action, unregister_action

from .config import _DEFAULT_TTL, _MODEL_CACHE_TTL_S
from .model_cache import _model_cache
from .policy import _try_grant
from .registry import _init_nvml, _refresh_nvml, _registry
from .workers import _start_background, _state_snapshot

logger = logging.getLogger(__name__)

# Action names exposed to pipe callers. Kept as a module-level tuple so
# ``_reset_for_tests`` can sweep the registry without re-listing each name.
_ACTION_NAMES = (
    "vram.reserve",
    "vram.release",
    "vram.heartbeat",
    "vram.state",
    "vram.leases",
    "vram.cache",
    "vram.evict",
)


def _start_pipe_alias(app: Flask) -> None:
    """Start a PipeServer for 'vram-broker' backed by the Flask app.

    Creates ``\\\\.\\pipe\\wylde-vram-broker`` so the Tauri list_pipes
    call returns 'vram-broker' and the dashboard shows the broker as
    active. All /vram/* requests routed through this pipe reach the
    same handlers the Flask app serves on its HTTP port.
    """
    try:
        from Core.shared import ipc

        server = ipc.PipeServer("vram-broker", app)
        server.start()
        logger.info("vram_broker: pipe alias started (\\\\.\\pipe\\wylde-vram-broker)")
    except Exception as e:
        logger.warning("vram_broker: pipe alias start failed: %s", e)


def install(app: Flask, gpu_available: bool = True) -> None:
    """Register /vram/* routes on `app` and start background threads.

    Safe to call more than once; subsequent calls are no-ops so the
    boot wrapper can call install() during initial boot and again
    after a late nvml probe without double-registering routes.
    """
    if getattr(app, "_vram_broker_installed", False):
        return
    app._vram_broker_installed = True  # type: ignore[attr-defined]

    if gpu_available:
        _init_nvml()
        _refresh_nvml()

    @app.route("/vram/reserve", methods=["POST"])
    def _vram_reserve() -> tuple[Response, int]:
        req = request.get_json(silent=True) or {}
        result = _try_grant(req)
        if result.get("ok"):
            return jsonify(result["lease"]), 200
        err = result["error"]
        # Map domain codes to HTTP — 409 for "can't fit now", 400 for bad input.
        status = 400 if err.get("code") == "invalid_request" else 409
        return jsonify(
            {
                "error": err.get("message"),
                "code": err.get("code"),
                "details": err.get("details", {}),
            }
        ), status

    @app.route("/vram/release", methods=["POST"])
    def _vram_release() -> tuple[Response, int]:
        body = request.get_json(silent=True) or {}
        lease_id = str(body.get("lease_id", ""))
        lease = _registry.remove(lease_id)
        if lease is None:
            return jsonify({"ok": True, "known": False}), 200
        logger.info(
            "vram_broker: release lease=%s service=%s model=%s",
            lease.lease_id[:8],
            lease.service,
            lease.model,
        )
        return jsonify({"ok": True, "known": True, "freed_bytes": lease.bytes}), 200

    @app.route("/vram/heartbeat", methods=["POST"])
    def _vram_heartbeat() -> tuple[Response, int]:
        body = request.get_json(silent=True) or {}
        lease_id = str(body.get("lease_id", ""))
        ttl = float(body.get("ttl", _DEFAULT_TTL))
        lease = _registry.touch(lease_id, ttl)
        if lease is None:
            return jsonify(
                {"error": "lease not found or already reaped", "code": "not_found"}
            ), 404
        return jsonify(
            {"lease_id": lease.lease_id, "expires_at": lease.expires_at}
        ), 200

    @app.route("/vram/state", methods=["GET"])
    def _vram_state() -> tuple[Response, int]:
        _refresh_nvml()
        return jsonify(_state_snapshot()), 200

    @app.route("/vram/leases", methods=["GET"])
    def _vram_leases() -> tuple[Response, int]:
        return jsonify(
            {"leases": [lease.to_wire() for lease in _registry.all_leases()]}
        ), 200

    @app.route("/vram/cache", methods=["GET"])
    def _vram_cache() -> tuple[Response, int]:
        """Return the recently-used (service, model) cache.

        Used by services to decide whether to skip cold-start work, if
        their model is in the warm cache, they should reuse it. The cache
        is purely informational; cold/warm decisions still go through
        /vram/reserve.
        """
        entries = _model_cache.all()
        return jsonify(
            {
                "ttl_s": _MODEL_CACHE_TTL_S,
                "entries": [
                    {
                        "service": e.service,
                        "model": e.model,
                        "bytes": e.bytes,
                        "last_used": e.last_used,
                        "warm_for": max(
                            0.0, _MODEL_CACHE_TTL_S - (time.time() - e.last_used)
                        ),
                    }
                    for e in sorted(entries, key=lambda x: -x.last_used)
                ],
            }
        ), 200

    @app.route("/vram/evict", methods=["POST"])
    def _vram_evict_broker() -> tuple[Response, int]:
        """Manual admin eviction: drop a lease by id as if it had timed out.
        Used by the GUI's 'unload' button and by tests. Does NOT signal the
        owning service — this is a force-drop, so the service must have
        already released its model externally (or the user accepts that
        accounting will drift until the service exits)."""
        body = request.get_json(silent=True) or {}
        lease_id = str(body.get("lease_id", ""))
        lease = _registry.remove(lease_id)
        if lease is None:
            return jsonify({"ok": False, "error": "not_found"}), 404
        return jsonify({"ok": True, "freed_bytes": lease.bytes}), 200

    _register_actions()
    _start_pipe_alias(app)
    _start_background()


def _register_actions() -> None:
    """Bind every ``vram.*`` action onto the shared IPC registry.

    Handlers call the same underlying helpers the Flask routes use, so
    the route and action surfaces stay byte-equivalent. Errors are
    surfaced via :class:`IpcError` so the dispatcher emits a structured
    ``{ok:False, error:{code, message, details}}`` reply that matches the
    Rust impl's ``Reply::err`` shape.
    """
    register_action("vram.reserve", _action_reserve)
    register_action("vram.release", _action_release)
    register_action("vram.heartbeat", _action_heartbeat)
    register_action("vram.state", _action_state)
    register_action("vram.leases", _action_leases)
    register_action("vram.cache", _action_cache)
    register_action("vram.evict", _action_evict)


def _action_reserve(payload: Any) -> Dict[str, Any]:
    """Request a VRAM lease; may preempt lower-priority holders."""
    req = payload if isinstance(payload, dict) else {}
    result = _try_grant(req)
    if result.get("ok"):
        # Return the bare lease dict, with optional preempted /
        # soft_eviction / dedup as sibling fields. Matches the Rust
        # handler's shape so cross-impl callers see identical payloads.
        out: Dict[str, Any] = dict(result["lease"])
        for key in ("preempted", "soft_eviction", "dedup"):
            if key in result:
                out[key] = result[key]
        return out
    err = result["error"]
    raise IpcError(
        str(err.get("code", "unknown")),
        str(err.get("message", "reserve failed")),
        err.get("details") or None,
    )


def _action_release(payload: Any) -> Dict[str, Any]:
    """Release a lease by id; idempotent."""
    body = payload if isinstance(payload, dict) else {}
    lease_id = str(body.get("lease_id", ""))
    lease = _registry.remove(lease_id)
    if lease is None:
        return {"ok": True, "known": False}
    logger.info(
        "vram_broker: release lease=%s service=%s model=%s",
        lease.lease_id[:8],
        lease.service,
        lease.model,
    )
    return {"ok": True, "known": True, "freed_bytes": lease.bytes}


def _action_heartbeat(payload: Any) -> Dict[str, Any]:
    """Extend a lease's expiry by ttl seconds."""
    body = payload if isinstance(payload, dict) else {}
    lease_id = str(body.get("lease_id", ""))
    ttl = float(body.get("ttl", _DEFAULT_TTL))
    lease = _registry.touch(lease_id, ttl)
    if lease is None:
        raise IpcError("not_found", "lease not found or already reaped")
    return {"lease_id": lease.lease_id, "expires_at": lease.expires_at}


def _action_state(_payload: Any) -> Dict[str, Any]:
    """Snapshot of GPU totals, leases, model cache, and config."""
    _refresh_nvml()
    return _state_snapshot()


def _action_leases(_payload: Any) -> Dict[str, Any]:
    """List all live leases."""
    return {"leases": [lease.to_wire() for lease in _registry.all_leases()]}


def _action_cache(_payload: Any) -> Dict[str, Any]:
    """List the soft-LRU (service, model) keep-warm cache."""
    now = time.time()
    entries: List[Dict[str, Any]] = [
        {
            "service": e.service,
            "model": e.model,
            "bytes": e.bytes,
            "last_used": e.last_used,
            "warm_for": max(0.0, _MODEL_CACHE_TTL_S - (now - e.last_used)),
        }
        for e in sorted(_model_cache.all(), key=lambda x: -x.last_used)
    ]
    return {"ttl_s": _MODEL_CACHE_TTL_S, "entries": entries}


def _action_evict(payload: Any) -> Dict[str, Any]:
    """Force-drop a lease by id; does NOT signal the owning service."""
    body = payload if isinstance(payload, dict) else {}
    lease_id = str(body.get("lease_id", ""))
    lease = _registry.remove(lease_id)
    if lease is None:
        raise IpcError("not_found", "lease not found")
    return {"ok": True, "freed_bytes": lease.bytes}


def stop() -> None:
    from .workers import _threads

    _threads.stop.set()
    try:
        from Core.shared.manifest import stop_heartbeat

        stop_heartbeat("vram-broker")
    except Exception:
        pass


def _reset_for_tests() -> None:
    """Exposed for tests so they can reset state between cases. Delegates to
    the per-submodule resets — keeps the registry / threads bindings stable
    so the shim's re-exports stay live."""
    from . import registry, workers

    stop()
    for name in _ACTION_NAMES:
        unregister_action(name)
    registry._reset()
    workers._reset()
