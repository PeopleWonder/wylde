"""
manifest.py — Service manifest writer.

Services write a JSON manifest to data/manifests/{service}.json on startup.
The Fletch GUI reads these files directly (zero IPC) to render service cards,
tool lists, and settings panels.

Ownership model (post manifest-ownership refactor):

* The **service** owns the manifest. Its run.py calls write_manifest() at
  startup, start_heartbeat() to keep status.heartbeat fresh, and mark_stopped()
  from its SIGTERM/SIGINT handler. Lifecycle daemon no longer writes
  per-service manifests for the subprocesses it spawns — it records the
  spawn intent in its own state, supervises the process, and acts as an
  orphan-detection safety net via mark_orphan_dead().

* Write-on-startup:
      write_manifest(service_name, port, category, description, contributes)

* Heartbeat (daemon thread, call after write_manifest):
      start_heartbeat(service_name, interval=60)

* Graceful shutdown — flip the manifest to a stopped state:
      mark_stopped(service_name)

* Orphan safety net (Lifecycle daemon only — service should never call this):
      mark_orphan_dead(service_name)

* Update dynamic fields (e.g. device probe results) without racing the
  heartbeat:
      update_contributes(service_name, contributes)

The heartbeat thread keeps the manifest cached in memory so each tick performs
one short atomic write — no JSON re-parse, no re-read from disk. That keeps
GIL-holding Python work per tick small enough that the heartbeat survives
neighbouring threads doing heavy ML work (training tokenisation, model loads).

status.state values
-------------------
``alive``        — service has written its manifest and is heartbeating.
``stopped``      — service called mark_stopped() during graceful shutdown.
``dead-orphan``  — Lifecycle daemon detected an ``alive`` manifest whose pid
                   is no longer running (ungraceful kill). The classifier
                   treats this as terminal until the service rewrites its
                   manifest on the next start.
"""

from __future__ import annotations

import datetime
import json
import logging
import os
import sys
import threading
from pathlib import Path
from typing import Any, Optional

logger = logging.getLogger(__name__)

# Root of the Wylde repo — services may override with WYLDE_ROOT env var.
_WYLDE_ROOT = Path(os.getenv("WYLDE_ROOT", Path(__file__).parent.parent.parent))
_MANIFEST_DIR = _WYLDE_ROOT / "data" / "manifests"


def _manifest_path(service_name: str) -> Path:
    return _MANIFEST_DIR / f"{service_name}.json"


def _now_iso() -> str:
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _atomic_write(path: Path, data: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    # Per-service tmp suffix so two services writing concurrently can't clobber
    # each other's in-flight write.
    tmp = path.with_name(f"{path.stem}.{os.getpid()}.tmp")
    try:
        tmp.write_text(json.dumps(data, indent=2), encoding="utf-8")
        os.replace(tmp, path)
    except Exception as e:
        logger.error("manifest: atomic write failed for %s: %s", path, e)
        try:
            tmp.unlink(missing_ok=True)
        except Exception:
            pass
        raise


# In-memory cache: service_name → (manifest dict, threading.Lock).
# Heartbeat ticks mutate this dict + write it; update_contributes() also
# mutates it, holding the same lock so the two never race.
_manifest_cache: dict[str, tuple[dict, threading.Lock]] = {}


# Module-level startup-phase log.  Phases that fire BEFORE write_manifest
# is called are buffered here and then seeded into the manifest's
# ``startup_sequence`` field when the manifest is first written.  Phases
# that fire AFTER write_manifest land in the on-disk manifest directly
# via :func:`_append_phase`.  This is how Wylde services self-attest the
# startup convention — ``wylde_check`` reads ``startup_sequence`` rather
# than AST-walking ``run.py``.
_PHASES_FIRED_PRE_WRITE: list[str] = []
_phases_lock = threading.Lock()


def attest_phase(phase: str) -> None:
    """Record that a named startup phase has fired this process.

    Called by the run.py startup helpers (configure_logging,
    write_manifest, start_heartbeat, ipc.serve) so the runtime
    self-attests its progress through the startup convention.

    Idempotent on adjacent duplicates so callers can attest without
    worrying about double-record (e.g. a logging-setup call that fires
    twice across re-entrant configure_logging).
    """
    with _phases_lock:
        if not _PHASES_FIRED_PRE_WRITE or _PHASES_FIRED_PRE_WRITE[-1] != phase:
            _PHASES_FIRED_PRE_WRITE.append(phase)


def _append_phase(service_name: str, phase: str) -> None:
    """Append ``phase`` to the cached manifest's startup_sequence and persist.

    Best-effort — failure to flush is logged, not raised, because
    attestation must never break a service's startup path.  No-op when
    the service has no cached manifest entry yet (in which case the
    caller should use :func:`attest_phase`, which buffers until
    :func:`write_manifest` flushes the buffer).
    """
    entry = _manifest_cache.get(service_name)
    if entry is None:
        attest_phase(phase)
        return
    cached, lock = entry
    with lock:
        seq = cached.setdefault("startup_sequence", [])
        if not seq or seq[-1] != phase:
            seq.append(phase)
        try:
            _atomic_write(_manifest_path(service_name), cached)
        except Exception as e:  # noqa: BLE001
            logger.warning(
                "manifest: attest_phase %s failed for %s: %s",
                phase,
                service_name,
                e,
            )


def mark_serve_loop_entered(service_name: str) -> None:
    """Attest that the service has entered its serve loop.

    Called by :func:`Core.shared.ipc.serve` at the top of the accept
    loop so ``startup_sequence`` records all four expected phases.
    Services that don't go through ``ipc.serve`` should call this
    explicitly at the top of their serve loop.
    """
    _append_phase(service_name, "serve_loop")


def _cache_entry(service_name: str) -> tuple[dict, threading.Lock]:
    entry = _manifest_cache.get(service_name)
    if entry is None:
        raise RuntimeError(
            f"manifest: no cached entry for {service_name!r}; "
            "call write_manifest() before start_heartbeat() / update_contributes()"
        )
    return entry


def _default_entry_point() -> str:
    """Best-effort fallback for ``entry_point`` when the caller doesn't pass one.

    Reads ``sys.argv[0]`` (the script that started the interpreter),
    strips the ``.py`` suffix, and prefixes ``python:``. Future Rust
    services pass an explicit ``rust:<crate-bin>`` value, so this
    fallback only ever fires for Python entry points.
    """
    argv0 = sys.argv[0] if sys.argv else ""
    stem = Path(argv0).name.removesuffix(".py") if argv0 else ""
    return f"python:{stem or 'unknown'}"


def write_manifest(
    service_name: str,
    port: int,
    category: str,
    description: str,
    contributes: dict[str, Any] | None = None,
    entry_point: Optional[str] = None,
) -> None:
    """Write (or overwrite) the service manifest. Safe to call multiple times.

    The manifest dict is cached in memory after writing; subsequent
    start_heartbeat / update_contributes calls operate on the cached copy
    so no JSON re-parse is needed per heartbeat tick.

    ``entry_point`` is the language-prefixed identifier the daemon uses
    to start this service. Python services pass ``"python:<module>"``
    (e.g. ``"python:Voice.run"``), future Rust services pass
    ``"rust:<crate-bin>"``. Falls back to ``_default_entry_point()``
    when omitted so existing callers keep writing valid manifests
    while the entry_point field rolls out.
    """
    path = _manifest_path(service_name)

    # Preserve started_at from an existing manifest so a refresh mid-run
    # doesn't reset the original start time.
    existing: dict[str, Any] = {}
    try:
        if path.exists():
            existing = json.loads(path.read_text(encoding="utf-8"))
    except Exception:
        pass

    prev_status = existing.get("status", {})
    started_at = prev_status.get("started_at") or _now_iso()

    # Seed startup_sequence from phases that fired before this write.
    # The write itself is the second canonical phase.
    with _phases_lock:
        startup_sequence = list(_PHASES_FIRED_PRE_WRITE)
    if not startup_sequence or startup_sequence[-1] != "write_manifest":
        startup_sequence.append("write_manifest")

    pipe_suffix = service_name.removeprefix("wylde-")
    manifest: dict[str, Any] = {
        "service": service_name,
        "version": "1.0.0",
        "pipe": rf"\\.\pipe\wylde-{pipe_suffix}",
        "port": port,
        "category": category,
        "description": description,
        "entry_point": entry_point or _default_entry_point(),
        "contributes": contributes or {},
        "startup_sequence": startup_sequence,
        "shutdown_attested": False,
        "status": {
            "pid": os.getpid(),
            "started_at": started_at,
            "heartbeat": _now_iso(),
            "state": "alive",
        },
    }

    entry = _manifest_cache.get(service_name)
    if entry is None:
        entry = (manifest, threading.Lock())
        _manifest_cache[service_name] = entry
    else:
        # Replace cached dict contents in-place so the heartbeat thread (which
        # holds a reference to the same dict) keeps writing the right content.
        cached, lock = entry
        with lock:
            cached.clear()
            cached.update(manifest)

    cached, lock = _manifest_cache[service_name]
    with lock:
        _atomic_write(path, cached)
    logger.info("manifest: wrote %s", path)


def update_contributes(service_name: str, contributes: dict[str, Any]) -> None:
    """Replace the manifest's `contributes` block.

    Use this when dynamic fields (device probe results, tool counts, etc.)
    aren't known at write_manifest() time. Thread-safe with the heartbeat
    loop — they share a lock and operate on the same cached dict.
    """
    cached, lock = _cache_entry(service_name)
    with lock:
        cached["contributes"] = contributes
        cached.setdefault("status", {})["heartbeat"] = _now_iso()
        _atomic_write(_manifest_path(service_name), cached)


def _heartbeat_loop(service_name: str, interval: float, stop: threading.Event) -> None:
    path = _manifest_path(service_name)
    cached, lock = _cache_entry(service_name)
    while not stop.wait(timeout=interval):
        try:
            with lock:
                cached.setdefault("status", {})["heartbeat"] = _now_iso()
                _atomic_write(path, cached)
        except Exception as e:
            logger.warning(
                "manifest: heartbeat write failed for %s: %s", service_name, e
            )


_heartbeat_stops: dict[str, threading.Event] = {}


def start_heartbeat(service_name: str, interval: float = 60.0) -> None:
    """Start a daemon thread that updates status.heartbeat every `interval` s.

    Must be called after write_manifest() — the heartbeat loop reuses the
    in-memory manifest dict cached there, so each tick is just a timestamp
    bump + atomic write (no read/parse).

    Default 60 s is the unified Wylde tick (also used by
    :func:`daemon_state._start_daemon_heartbeat`); the registry
    classifier treats services as ``active`` while their last heartbeat is
    ≤90 s old.  Override only when sub-minute liveness detection is critical.
    """
    # Fail fast if the cache wasn't seeded — same exception path as the loop.
    _cache_entry(service_name)
    stop = threading.Event()
    _heartbeat_stops[service_name] = stop
    t = threading.Thread(
        target=_heartbeat_loop,
        args=(service_name, interval, stop),
        name=f"manifest-hb-{service_name}",
        daemon=True,
    )
    t.start()
    _append_phase(service_name, "start_heartbeat")
    logger.info(
        "manifest: heartbeat started for %s (interval=%ss)", service_name, interval
    )


def stop_heartbeat(service_name: str) -> None:
    """Signal the heartbeat thread to exit."""
    stop = _heartbeat_stops.pop(service_name, None)
    if stop:
        stop.set()


def mark_stopped(service_name: str) -> None:
    """Flip the service's manifest to a stopped state and halt its heartbeat.

    Call this from a SIGTERM / SIGINT / atexit handler so the dashboard
    sees the service as cleanly stopped rather than as a stale heartbeat.

    If the heartbeat thread is running it is stopped first so it cannot
    race the state write back to ``alive``. The manifest update is
    best-effort — failures (file missing, disk full) are logged but do
    not raise, because shutdown paths should not raise.

    Idempotent: safe to call when no manifest exists or when the
    service was never registered in this process's cache.
    """
    stop_heartbeat(service_name)
    path = _manifest_path(service_name)
    entry = _manifest_cache.get(service_name)
    try:
        if entry is not None:
            cached, lock = entry
            with lock:
                cached.setdefault("status", {})["state"] = "stopped"
                cached["status"]["stop_time"] = _now_iso()
                cached["status"]["heartbeat"] = _now_iso()
                cached["shutdown_attested"] = True
                _atomic_write(path, cached)
            return
        # No cache entry — service didn't go through write_manifest() in
        # this process. Read whatever is on disk, mutate, atomic-replace.
        if not path.exists():
            return
        data = json.loads(path.read_text(encoding="utf-8"))
        status = data.setdefault("status", {})
        status["state"] = "stopped"
        status["stop_time"] = _now_iso()
        status["heartbeat"] = _now_iso()
        data["shutdown_attested"] = True
        _atomic_write(path, data)
    except Exception as e:  # noqa: BLE001
        logger.warning("manifest: mark_stopped failed for %s: %s", service_name, e)


def mark_orphan_dead(service_name: str) -> None:
    """Mark an alive-but-orphaned manifest as ``dead-orphan``.

    Used exclusively by the Lifecycle daemon's orphan-detection sweep
    when a manifest claims ``alive`` but the recorded pid is no longer
    running (ungraceful kill, segfault, OS-level termination). Sets
    ``status.state = "dead-orphan"`` and stamps ``last_seen`` so the
    classifier can show how long ago the orphan was detected.

    Does NOT touch ``heartbeat`` — leaving the original stale timestamp
    in place preserves the forensic timeline of when the process
    actually disappeared.

    Best-effort: returns silently when the manifest is missing or
    unreadable. Idempotent: re-marking an already-orphaned manifest
    just refreshes ``last_seen``.
    """
    path = _manifest_path(service_name)
    try:
        if not path.exists():
            return
        data = json.loads(path.read_text(encoding="utf-8"))
        status = data.setdefault("status", {})
        status["state"] = "dead-orphan"
        status["last_seen"] = _now_iso()
        _atomic_write(path, data)
    except Exception as e:  # noqa: BLE001
        logger.warning("manifest: mark_orphan_dead failed for %s: %s", service_name, e)


__all__ = [
    "write_manifest",
    "update_contributes",
    "start_heartbeat",
    "stop_heartbeat",
    "mark_stopped",
    "mark_orphan_dead",
    "attest_phase",
    "mark_serve_loop_entered",
]
