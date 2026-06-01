# AUTO-GENERATED, edit core/shared/manifest.py and run python core/shared/sync.py
"""
manifest.py — Service manifest writer.

Services write a JSON manifest to data/manifests/{service}.json on startup.
The Fletch GUI reads these files directly (zero IPC) to render service cards,
tool lists, and settings panels.

Write-on-startup:
    write_manifest(service_name, port, category, description, contributes)

Heartbeat (daemon thread, optional, call after write_manifest if needed):
    start_heartbeat(service_name, interval=30)

Update dynamic fields (e.g. device probe results) without racing the heartbeat:
    update_contributes(service_name, contributes)

The heartbeat thread keeps the manifest cached in memory so each tick performs
one short atomic write — no JSON re-parse, no re-read from disk. That keeps
GIL-holding Python work per tick small enough that the heartbeat survives
neighbouring threads doing heavy ML work (training tokenisation, model loads).
"""

from __future__ import annotations

import datetime
import json
import logging
import os
import threading
from pathlib import Path
from typing import Any

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


def _cache_entry(service_name: str) -> tuple[dict, threading.Lock]:
    entry = _manifest_cache.get(service_name)
    if entry is None:
        raise RuntimeError(
            f"manifest: no cached entry for {service_name!r}; "
            "call write_manifest() before start_heartbeat() / update_contributes()"
        )
    return entry


def write_manifest(
    service_name: str,
    port: int,
    category: str,
    description: str,
    contributes: dict[str, Any] | None = None,
) -> None:
    """Write (or overwrite) the service manifest. Safe to call multiple times.

    The manifest dict is cached in memory after writing; subsequent
    start_heartbeat / update_contributes calls operate on the cached copy
    so no JSON re-parse is needed per heartbeat tick.
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

    pipe_suffix = service_name.removeprefix("wylde-")
    manifest: dict[str, Any] = {
        "service": service_name,
        "version": "1.0.0",
        "pipe": rf"\\.\pipe\wylde-{pipe_suffix}",
        "port": port,
        "category": category,
        "description": description,
        "contributes": contributes or {},
        "status": {
            "pid": os.getpid(),
            "started_at": started_at,
            "heartbeat": _now_iso(),
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


def start_heartbeat(service_name: str, interval: float = 30.0) -> None:
    """Start a daemon thread that updates status.heartbeat every `interval` s.

    Must be called after write_manifest() — the heartbeat loop reuses the
    in-memory manifest dict cached there, so each tick is just a timestamp
    bump + atomic write (no read/parse).

    Default 30 s matches docs/protocols/MANIFEST.md §4: a service can miss one
    tick and remain `active` (the GUI's ACTIVE_MS = 15 s threshold). Override
    to a shorter interval only when sub-30s liveness detection is critical.
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
    logger.info(
        "manifest: heartbeat started for %s (interval=%ss)", service_name, interval
    )


def stop_heartbeat(service_name: str) -> None:
    """Signal the heartbeat thread to exit."""
    stop = _heartbeat_stops.pop(service_name, None)
    if stop:
        stop.set()


__all__ = [
    "write_manifest",
    "update_contributes",
    "start_heartbeat",
    "stop_heartbeat",
]
