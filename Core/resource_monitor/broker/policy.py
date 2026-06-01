"""Reservation policy: priority admission, preemption, eviction signalling."""

from __future__ import annotations

import logging
import time
import uuid
from typing import Any, Dict, List, Optional

from .config import _DEFAULT_TTL, _EVICT_TIMEOUT_S, _GRACE_PERIOD_S, _SAFETY_MARGIN
from .model_cache import _model_cache
from .registry import Lease, _registry

logger = logging.getLogger(__name__)


def _try_grant(req: Dict[str, Any]) -> Dict[str, Any]:
    """Core reservation logic. Returns either
      {"ok": True, "lease": {...}}
    or
      {"ok": False, "error": {"code": ..., "details": {...}}}.
    """
    service = str(req.get("service", "unknown"))
    model = str(req.get("model", "unknown"))
    nbytes = int(req.get("bytes", 0))
    priority = int(req.get("priority", 40))
    ttl = float(req.get("ttl", _DEFAULT_TTL))
    preempt = bool(req.get("preempt", False))
    pid = int(req.get("pid", 0))
    nonce = str(req.get("client_nonce", ""))

    if nbytes <= 0:
        return {
            "ok": False,
            "error": {"code": "invalid_request", "message": "bytes must be positive"},
        }

    total = _registry.total()
    if total and nbytes > total - _SAFETY_MARGIN:
        return {
            "ok": False,
            "error": {
                "code": "would_exceed_total",
                "message": f"request {nbytes} exceeds total VRAM {total} "
                f"(minus safety margin {_SAFETY_MARGIN})",
                "details": {
                    "total_bytes": total,
                    "requested_bytes": nbytes,
                    "safety_margin": _SAFETY_MARGIN,
                },
            },
        }

    # Dedupe retries: a client that retries with the same nonce after a
    # successful grant but lost response gets the same lease back.
    existing = _registry.find_by_nonce(nonce)
    if existing is not None:
        return {"ok": True, "lease": existing.to_wire(), "dedup": True}

    # Fast path.
    if nbytes <= _registry.free_for_grant():
        return {
            "ok": True,
            "lease": _grant(
                service, model, nbytes, priority, ttl, pid, nonce
            ).to_wire(),
        }

    # No room. Build blocker list (leases of strictly lower priority).
    blockers = [
        lease
        for lease in _registry.all_leases()
        if not lease.synthetic and lease.priority < priority
    ]
    blockers.sort(key=lambda lease: lease.priority)  # evict lowest first

    freeable = sum(lease.bytes for lease in blockers)
    free_now = _registry.free_for_grant()

    if freeable + free_now < nbytes:
        # Even evicting every lower-priority lease wouldn't help, this
        # reservation must wait for a higher-or-equal priority lease to
        # release on its own.
        return _insufficient(nbytes, priority, blocker_snapshot=_all_blockers_view())

    if not preempt:
        return _insufficient(nbytes, priority, blocker_snapshot=_all_blockers_view())

    # Preemption path. Try a graceful soft-eviction first: signal each blocker
    # via /vram/please-evict and wait `_GRACE_PERIOD_S` for them to release.
    # Anything still holding after grace gets a hard /vram/evict before we
    # give up. This protects in-flight inference from being cut mid-token.
    to_evict: List[Lease] = []
    accum = free_now
    for lease in blockers:
        if accum >= nbytes:
            break
        to_evict.append(lease)
        accum += lease.bytes

    if _GRACE_PERIOD_S > 0:
        for lease in to_evict:
            _signal_soft_evict(lease, grace_s=_GRACE_PERIOD_S)
        soft_deadline = time.time() + _GRACE_PERIOD_S
        while time.time() < soft_deadline:
            if nbytes <= _registry.free_for_grant():
                return {
                    "ok": True,
                    "lease": _grant(
                        service, model, nbytes, priority, ttl, pid, nonce
                    ).to_wire(),
                    "preempted": [lease.lease_id for lease in to_evict],
                    "soft_eviction": True,
                }
            time.sleep(0.2)

    # Hard evict anything that didn't yield gracefully.
    for lease in to_evict:
        if _registry.get(lease.lease_id) is not None:
            _signal_evict(lease)

    deadline = time.time() + _EVICT_TIMEOUT_S
    while time.time() < deadline:
        if nbytes <= _registry.free_for_grant():
            return {
                "ok": True,
                "lease": _grant(
                    service, model, nbytes, priority, ttl, pid, nonce
                ).to_wire(),
                "preempted": [lease.lease_id for lease in to_evict],
                "soft_eviction": False,
            }
        time.sleep(0.1)

    # Preemption didn't free enough in time. Return structured error naming
    # the services that didn't yield, so the GUI can escalate to the user.
    still_held = [
        lease for lease in to_evict if _registry.get(lease.lease_id) is not None
    ]
    return _insufficient(
        nbytes,
        priority,
        blocker_snapshot=_all_blockers_view(),
        unresponsive=[lease.service for lease in still_held],
    )


def _grant(
    service: str,
    model: str,
    nbytes: int,
    priority: int,
    ttl: float,
    pid: int,
    nonce: str,
) -> Lease:
    now = time.time()
    lease = Lease(
        lease_id=uuid.uuid4().hex,
        service=service,
        model=model,
        bytes=nbytes,
        priority=priority,
        granted_at=now,
        expires_at=now + ttl,
        heartbeat_at=now,
        pid=pid,
        synthetic=False,
        client_nonce=nonce,
    )
    _registry.add(lease)
    _model_cache.touch(service, model, nbytes, priority)
    logger.info(
        "vram_broker: grant service=%s model=%s bytes=%d priority=%d lease=%s",
        service,
        model,
        nbytes,
        priority,
        lease.lease_id[:8],
    )
    return lease


def _insufficient(
    nbytes: int,
    priority: int,
    blocker_snapshot: List[Dict[str, Any]],
    unresponsive: Optional[List[str]] = None,
) -> Dict[str, Any]:
    details: Dict[str, Any] = {
        "requested_bytes": nbytes,
        "requester_priority": priority,
        "free_bytes": _registry.free_for_grant(),
        "total_bytes": _registry.total(),
        "blockers": blocker_snapshot,
    }
    if unresponsive:
        details["unresponsive_services"] = unresponsive
    return {
        "ok": False,
        "error": {
            "code": "insufficient_vram",
            "message": f"cannot satisfy {nbytes} bytes at priority {priority}",
            "details": details,
        },
    }


def _all_blockers_view() -> List[Dict[str, Any]]:
    return [
        {
            "lease_id": lease.lease_id,
            "service": lease.service,
            "model": lease.model,
            "bytes": lease.bytes,
            "priority": lease.priority,
            "synthetic": lease.synthetic,
        }
        for lease in sorted(_registry.all_leases(), key=lambda x: -x.priority)
    ]


def _signal_evict(lease: Lease) -> None:
    """Fire a HARD evict request at the lease's owning service. Best-effort —
    services that haven't implemented /vram/evict will 404, and the reaper
    will eventually time them out instead."""
    try:
        from Core.shared import ipc
    except ImportError:
        logger.debug("vram_broker: ipc missing, cannot signal evict")
        return
    try:
        ipc.send(
            lease.service,
            "/vram/evict",
            {"lease_id": lease.lease_id, "model": lease.model},
            timeout=min(_EVICT_TIMEOUT_S, 2.0),
        )
        logger.info(
            "vram_broker: HARD evict signal sent to %s lease=%s",
            lease.service,
            lease.lease_id[:8],
        )
    except Exception as e:
        logger.debug(
            "vram_broker: hard evict signal to %s failed: %s", lease.service, e
        )


def _signal_soft_evict(lease: Lease, *, grace_s: float) -> None:
    """Send a graceful eviction request — the service is asked to finish
    its current task and release within `grace_s` seconds. If the service
    has not implemented /vram/please-evict (404), the broker falls back to
    the hard /vram/evict path immediately so behaviour is conservative."""
    try:
        from Core.shared import ipc
    except ImportError:
        _signal_evict(lease)
        return
    try:
        reply = ipc.send(
            lease.service,
            "/vram/please-evict",
            {"lease_id": lease.lease_id, "model": lease.model, "grace_s": grace_s},
            timeout=min(_EVICT_TIMEOUT_S, 2.0),
        )
        ok = getattr(reply, "ok", False)
        if not ok:
            # Service doesn't speak the soft protocol, fall through to hard.
            logger.debug(
                "vram_broker: %s did not accept soft eviction; falling back to hard /vram/evict",
                lease.service,
            )
            _signal_evict(lease)
            return
        logger.info(
            "vram_broker: SOFT evict signalled to %s lease=%s grace=%.1fs",
            lease.service,
            lease.lease_id[:8],
            grace_s,
        )
    except Exception as e:
        logger.debug(
            "vram_broker: soft evict signal to %s failed: %s — falling back to hard evict",
            lease.service,
            e,
        )
        _signal_evict(lease)
