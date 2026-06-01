r"""
vram_broker.py — Client library for the shared VRAM allocation broker.

Call site (typical):

    import vram_broker as vram
    with vram.reserved("wylde-caption", "florence-2", bytes=4 * 1024**3,
                      priority=vram.Priority.CAPTION):
        model = load_model(...)
        ...  # lease auto-heartbeats; released on exit

Or the manual form:

    lease = vram.reserve("wylde-trainer", "qwen-7b-lora",
                         bytes=10 * 1024**3, priority=vram.Priority.TRAINER)
    try:
        train()
    finally:
        vram.release(lease.lease_id)

Contract:
  - Every GPU model load in a Wylde service must hold a lease before loading.
  - The broker tracks declared bytes; nvml is the source of truth for actual
    usage. Divergence between the two exposes un-brokered loads.
  - A service that crashes between reserve() and release() leaks a lease only
    until TTL expires, the broker reaps it.

Transport:
  - Calls route through ``Core/shared/ipc`` as action invocations
    (``vram.reserve``, ``vram.release``, ...) on the broker service over
    ``\\.\pipe\wylde-vram-broker``. The Python broker
    (``Core/resource_monitor/broker/service.py``) and the Rust port
    (``rust/crates/wylde-vram-broker``) both expose the SAME action
    surface, so flipping ``WYLDE_WYLDE_VRAM_BROKER_IMPL=rust`` keeps
    this client working without changes.
  - If ipc or the broker is unreachable, behavior depends on
    WYLDE_VRAM_REQUIRED: "1" (default) raises VramError("broker_unreachable");
    "0" logs and returns a synthetic always-granted lease so single-service
    dev runs aren't blocked by a down broker.
"""

from __future__ import annotations

import logging
import os
import threading
import time
import uuid
from contextlib import contextmanager
from dataclasses import dataclass
from typing import Any, Dict, Iterator, List, Optional

logger = logging.getLogger(__name__)


# ── Priority tiers ────────────────────────────────────────────────────
# Higher number wins. These are the policy tiers the roadmap prescribes:
# active inference must not be preempted by background work; training is
# the first thing evicted when anything else needs the GPU.
class Priority:
    INFERENCE = 100  # Ollama, primary chat/generation path
    VOICE = 80  # wylde-voice / wylde-voice-assistant
    RAG = 60  # wylde-rag reranker, optional embeddings  # wylde-check: dead-ref-ok
    CAPTION = 40  # wylde-caption (Florence-2, Qwen-VL, JoyCaption)
    TRAINER = 20  # wylde-trainer (background LoRA jobs)

    @classmethod
    def for_service(cls, service: Optional[str]) -> int:
        s = (service or "").lower()
        if "ollama" in s or "inference" in s:
            return cls.INFERENCE
        if "voice" in s:
            return cls.VOICE
        if "rag" in s:
            return cls.RAG
        if "caption" in s:
            return cls.CAPTION
        if "trainer" in s or "training" in s:
            return cls.TRAINER
        return cls.CAPTION  # sensible default for anything else


# ── Public data types ─────────────────────────────────────────────────
@dataclass
class Lease:
    lease_id: str
    service: str
    model: str
    bytes: int
    priority: int
    granted_at: float
    expires_at: float
    synthetic: bool = False  # true for broker-fabricated leases (e.g. Ollama)


class VramError(Exception):
    def __init__(
        self, code: str, message: str, details: Optional[Dict[str, Any]] = None
    ):
        self.code = code
        self.message = message
        self.details = details or {}
        super().__init__(f"{code}: {message}")


class VramUnavailable(VramError):
    """Reservation cannot currently be satisfied. `details['blockers']` lists
    the leases that would need to release for the request to fit."""


# ── Env + config ──────────────────────────────────────────────────────
_BROKER_SERVICE = os.getenv("WYLDE_VRAM_BROKER", "wylde-vram-broker")
_REQUIRED = os.getenv("WYLDE_VRAM_REQUIRED", "1").lower() in ("1", "true", "yes")
_DEFAULT_TTL = float(os.getenv("WYLDE_VRAM_TTL", "60"))
_HEARTBEAT_EVERY = float(os.getenv("WYLDE_VRAM_HEARTBEAT", "20"))
_CALL_TIMEOUT = float(os.getenv("WYLDE_VRAM_TIMEOUT", "10"))

_leases_lock = threading.Lock()
_heartbeat_stops: Dict[str, threading.Event] = {}


# ── IPC shim ──────────────────────────────────────────────────────────
# Import lazily so this module is still importable in environments where
# ipc isn't on sys.path (e.g. unit tests that only want the Priority enum).
def _call(action: str, data: Any = None, timeout: float = _CALL_TIMEOUT) -> Any:
    """Invoke a broker action by name (``vram.reserve``, ``vram.state``, ...).

    Routed via :func:`Core.shared.ipc.send_action`, which both the
    Python and Rust broker implementations dispatch from the same pipe
    path — so this client doesn't need to know which impl is currently
    serving requests.
    """
    try:
        from Core.shared import ipc
    except ImportError as e:
        raise VramError("no_ipc", f"ipc module not importable: {e}")
    reply = ipc.send_action(_BROKER_SERVICE, action, data, timeout=timeout)
    if reply.ok:
        return reply.data
    err = reply.error or {}
    code = err.get("code", "unknown")
    msg = err.get("message", "broker call failed")
    # The broker signals "can't fit" with a dedicated code so callers can
    # catch it without scraping message strings.
    if code in ("insufficient_vram", "would_exceed_total"):
        raise VramUnavailable(code, msg, err.get("details"))
    if code in (
        "transport",
        "pipe_connect",
        "pipe_unavailable",
        "pipe_io",
        "pipe_timeout",
        # Older builds returned ``no_action`` (action surface not yet
        # registered) or HTTP 404/501 (Flask routes missing) when the
        # broker was up but the contract hadn't loaded. Treat them as
        # unreachable so WYLDE_VRAM_REQUIRED=0 fallback still kicks in.
        "no_action",
        "http_404",
        "http_501",
    ):
        raise VramError("broker_unreachable", msg, err)
    raise VramError(code, msg, err.get("details"))


# ── Public API ────────────────────────────────────────────────────────
def reserve(
    service: str,
    model: str,
    bytes: int,
    priority: Optional[int] = None,
    ttl: float = _DEFAULT_TTL,
    preempt: bool = False,
    timeout: float = _CALL_TIMEOUT,
) -> Lease:
    """Request a VRAM lease. Raises VramUnavailable if the request can't fit
    and preempt=False (or preemption wouldn't help because all blockers are
    higher-priority).

    Args:
      service:  caller identifier (usually the wylde service name).
      model:    human-readable tag — shows up in the dashboard/manifest.
      bytes:    bytes of VRAM the load will consume.
      priority: see Priority.*. Defaults to Priority.for_service(service).
      ttl:      seconds the broker will honour the lease without a heartbeat.
      preempt:  if True, broker may evict lower-priority leases to make room.
    """
    if bytes <= 0:
        raise VramError("invalid_request", f"bytes must be positive, got {bytes}")
    if priority is None:
        priority = Priority.for_service(service)

    payload = {
        "service": service,
        "model": model,
        "bytes": int(bytes),
        "priority": int(priority),
        "ttl": float(ttl),
        "preempt": bool(preempt),
        "pid": os.getpid(),
        # Client-generated nonce lets the broker dedupe if a retry lands
        # after the first request already succeeded but the response got lost.
        "client_nonce": uuid.uuid4().hex,
    }

    try:
        data = _call("vram.reserve", payload, timeout=timeout)
    except VramUnavailable:
        raise
    except VramError as e:
        # "Can't reach the broker at all" is the only case we may soften.
        # A structured rejection (VramUnavailable) always bubbles up.
        if e.code != "broker_unreachable" or _REQUIRED:
            raise
        logger.warning(
            "vram_broker: broker unreachable, proceeding without lease "
            "(WYLDE_VRAM_REQUIRED=0): service=%s model=%s",
            service,
            model,
        )
        return _synthetic_lease(service, model, int(bytes), int(priority), ttl)

    lease = _lease_from_reply(data)
    _start_heartbeat(lease)
    return lease


def release(lease_id: Optional[str]) -> None:
    """Release a lease. Safe to call multiple times, the broker is idempotent
    and a synthetic lease (from a broker-unreachable grant) returns silently."""
    if not lease_id:
        return
    _stop_heartbeat(lease_id)
    if lease_id.startswith("synthetic:"):
        return
    try:
        _call("vram.release", {"lease_id": lease_id})
    except VramError as e:
        # Release must not throw on a dying service — log and move on.
        logger.warning("vram_broker: release(%s) failed: %s", lease_id, e)


def heartbeat(lease_id: str) -> None:
    """Renew a lease's TTL. Normally the background thread started by
    reserve() does this automatically."""
    if not lease_id or lease_id.startswith("synthetic:"):
        return
    _call("vram.heartbeat", {"lease_id": lease_id})


def get_state() -> Dict[str, Any]:
    """Fetch the broker's full state: leases, totals, Ollama reflection.
    Returns an empty dict if the broker isn't reachable."""
    try:
        state: Dict[str, Any] = _call("vram.state", timeout=min(_CALL_TIMEOUT, 5.0))
        return state
    except VramError as e:
        logger.debug("vram_broker: get_state unreachable: %s", e)
        return {}


@contextmanager
def reserved(
    service: str,
    model: str,
    bytes: int,
    priority: Optional[int] = None,
    ttl: float = _DEFAULT_TTL,
    preempt: bool = False,
    timeout: float = _CALL_TIMEOUT,
) -> Iterator[Lease]:
    lease = reserve(
        service,
        model,
        bytes,
        priority=priority,
        ttl=ttl,
        preempt=preempt,
        timeout=timeout,
    )
    try:
        yield lease
    finally:
        release(lease.lease_id)


# ── Heartbeat management ──────────────────────────────────────────────
def _start_heartbeat(lease: Lease) -> None:
    if lease.synthetic:
        return
    stop = threading.Event()

    def _loop() -> None:
        while not stop.wait(timeout=_HEARTBEAT_EVERY):
            try:
                _call(
                    "vram.heartbeat",
                    {"lease_id": lease.lease_id},
                    timeout=min(_CALL_TIMEOUT, 5.0),
                )
            except VramError as e:
                # A failed heartbeat just means the broker will eventually
                # reap us. Don't spam logs, debug level is enough.
                logger.debug("vram_broker: heartbeat(%s) failed: %s", lease.lease_id, e)

    with _leases_lock:
        _heartbeat_stops[lease.lease_id] = stop
    threading.Thread(
        target=_loop,
        name=f"vram-heartbeat-{lease.lease_id[:8]}",
        daemon=True,
    ).start()


def _stop_heartbeat(lease_id: str) -> None:
    with _leases_lock:
        stop = _heartbeat_stops.pop(lease_id, None)
    if stop is not None:
        stop.set()


# ── Helpers ───────────────────────────────────────────────────────────
def _lease_from_reply(data: Dict[str, Any]) -> Lease:
    return Lease(
        lease_id=str(data.get("lease_id")),
        service=str(data.get("service", "")),
        model=str(data.get("model", "")),
        bytes=int(data.get("bytes", 0)),
        priority=int(data.get("priority", 0)),
        granted_at=float(data.get("granted_at", time.time())),
        expires_at=float(data.get("expires_at", time.time() + _DEFAULT_TTL)),
        synthetic=bool(data.get("synthetic", False)),
    )


def _synthetic_lease(
    service: str, model: str, bytes_: int, priority: int, ttl: float
) -> Lease:
    now = time.time()
    return Lease(
        lease_id=f"synthetic:{uuid.uuid4().hex}",
        service=service,
        model=model,
        bytes=bytes_,
        priority=priority,
        granted_at=now,
        expires_at=now + ttl,
        synthetic=True,
    )


def format_blockers(err: VramUnavailable) -> str:
    """Pretty-printer for a VramUnavailable error — useful for logs and GUI."""
    det = err.details or {}
    blockers: List[Dict[str, Any]] = det.get("blockers") or []
    free_mb = (det.get("free_bytes") or 0) / (1024 * 1024)
    need_mb = (det.get("requested_bytes") or 0) / (1024 * 1024)
    lines = [f"VRAM unavailable: need {need_mb:.0f} MiB, free {free_mb:.0f} MiB"]
    for b in blockers:
        lines.append(
            f"  - {b.get('service', '?')}/{b.get('model', '?')}: "
            f"{(b.get('bytes') or 0) / (1024 * 1024):.0f} MiB "
            f"(priority {b.get('priority', '?')})"
        )
    return "\n".join(lines)


__all__ = [
    "Priority",
    "Lease",
    "VramError",
    "VramUnavailable",
    "reserve",
    "release",
    "heartbeat",
    "reserved",
    "get_state",
    "format_blockers",
]
