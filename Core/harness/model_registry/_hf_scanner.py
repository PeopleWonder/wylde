"""model_registry/_hf_scanner.py — discover models in the HuggingFace cache.

The HF Hub library lays models out as::

    ~/.cache/huggingface/hub/
        models--microsoft--Florence-2-large/
            blobs/...
            refs/main
            snapshots/<sha>/...

This module walks ``models--*/`` directories, parses the repo name back out
of the dasherised folder name, sums file sizes, and emits ``ModelEntry``
records. Cache-misses are cheap (one ``stat`` per scan); cache-hits are
free (just the signature comparison).

Cache invalidation follows the ``tool_registry`` template: snapshot
``(path, mtime, size)`` for every ``models--*`` directory, compare to the
last signature on each call, rebuild only on mismatch. That keeps repeat
``list_models`` calls within a chat turn essentially free even on a TB-scale
HF cache.

Missing-cache handling: if ``HF_HOME`` / ``HUGGINGFACE_HUB_CACHE`` /
``~/.cache/huggingface/hub`` doesn't exist (fresh install, sandbox), we
return an empty list and remember an empty signature so the next call
short-circuits.
"""

from __future__ import annotations

import logging
import os
import threading
from pathlib import Path
from typing import Dict, List, Optional, Tuple

from ._heuristics import infer_kind
from ._types import Kind, ModelEntry

logger = logging.getLogger("wylde.harness.model_registry.hf_scanner")

# Folder-name prefix used by huggingface_hub for model cache dirs.
_MODELS_PREFIX = "models--"

_cache_lock = threading.Lock()
_cached_signature: Optional[Tuple[Tuple[str, float, int], ...]] = None
_cached_entries: List[ModelEntry] = []


def _resolve_hub_dir() -> Path:
    """Resolve the HF Hub cache root in the order huggingface_hub itself uses.

    Order: ``HF_HUB_CACHE`` → ``HUGGINGFACE_HUB_CACHE`` → ``HF_HOME``/hub →
    ``~/.cache/huggingface/hub``. We don't import huggingface_hub here so the
    registry stays usable on systems that haven't installed it yet.
    """
    for env in ("HF_HUB_CACHE", "HUGGINGFACE_HUB_CACHE"):
        v = os.getenv(env)
        if v:
            return Path(v).expanduser()
    hf_home = os.getenv("HF_HOME")
    if hf_home:
        return Path(hf_home).expanduser() / "hub"
    return Path.home() / ".cache" / "huggingface" / "hub"


def _parse_repo_name(folder_name: str) -> Optional[str]:
    """``models--microsoft--Florence-2-large`` → ``microsoft/Florence-2-large``.

    Repo names may legitimately contain a single ``-`` so we can't naively
    join with ``/``; we only convert the *first* ``--`` separator after the
    ``models--`` prefix into the org/name boundary, leaving the rest of the
    repo path intact.
    """
    if not folder_name.startswith(_MODELS_PREFIX):
        return None
    body = folder_name[len(_MODELS_PREFIX) :]
    if not body:
        return None
    # huggingface_hub replaces every ``/`` in the repo name with ``--``. So
    # ``rhasspy/piper-voices`` becomes ``rhasspy--piper-voices``. Convert
    # all ``--`` separators back to ``/``.
    return body.replace("--", "/")


def _dir_size_and_atime(path: Path) -> Tuple[int, Optional[float]]:
    """Sum file sizes and capture the most-recent atime under ``path``.

    Symlinks (which huggingface_hub uses heavily inside ``snapshots/``) are
    followed via ``stat``; missing targets are tolerated. We deliberately
    don't ``rglob`` symlinked directories twice — the snapshot tree points
    at ``blobs/`` so the bytes are counted once via the blobs.
    """
    total = 0
    latest_atime: Optional[float] = None
    blobs = path / "blobs"
    if blobs.is_dir():
        for child in blobs.iterdir():
            try:
                st = child.stat()
            except OSError:
                continue
            total += st.st_size
            if latest_atime is None or st.st_atime > latest_atime:
                latest_atime = st.st_atime
        return total, latest_atime
    # Fallback: no blobs/ tree (e.g. older cache layout). Walk recursively
    # and skip directory symlinks to avoid double-counting.
    for child in path.rglob("*"):
        try:
            if child.is_symlink() or not child.is_file():
                continue
            st = child.stat()
        except OSError:
            continue
        total += st.st_size
        if latest_atime is None or st.st_atime > latest_atime:
            latest_atime = st.st_atime
    return total, latest_atime


def _scan_signature(hub_dir: Path) -> Tuple[Tuple[str, float, int], ...]:
    """(path, mtime, size) per ``models--*`` dir, sorted for stability.

    Missing hub_dir → empty tuple, treated as a valid (and cacheable) state.
    Non-models entries (datasets--…, etc.) are ignored.
    """
    if not hub_dir.is_dir():
        return ()
    sigs: List[Tuple[str, float, int]] = []
    for entry in hub_dir.iterdir():
        if not entry.name.startswith(_MODELS_PREFIX):
            continue
        if not entry.is_dir():
            continue
        try:
            st = entry.stat()
        except OSError:
            continue
        sigs.append((str(entry), st.st_mtime, st.st_size))
    sigs.sort()
    return tuple(sigs)


def _build_entries(
    hub_dir: Path,
    overrides: Optional[Dict[str, Kind]] = None,
    required_by: Optional[Dict[str, List[str]]] = None,
) -> List[ModelEntry]:
    """Walk the hub dir and emit a ``ModelEntry`` per ``models--*`` folder.

    ``overrides`` maps model id → kind from service manifests. When present
    it wins over ``infer_kind``. ``required_by`` maps id → list of services
    that declared the model, surfaced on the entry for the GUI.
    """
    overrides = overrides or {}
    required_by = required_by or {}
    entries: List[ModelEntry] = []
    if not hub_dir.is_dir():
        return entries
    for entry_dir in hub_dir.iterdir():
        if not entry_dir.is_dir() or not entry_dir.name.startswith(_MODELS_PREFIX):
            continue
        repo = _parse_repo_name(entry_dir.name)
        if not repo:
            continue
        size, atime = _dir_size_and_atime(entry_dir)
        kind = overrides.get(repo) or infer_kind(repo)
        from ._types import default_chat_visible

        entries.append(
            ModelEntry(
                id=repo,
                kind=kind,
                path=str(entry_dir),
                size_bytes=size,
                loaded=False,
                provider="huggingface",
                required_by=list(required_by.get(repo, ())),
                profile=None,
                last_accessed=atime,
                chat_visible=default_chat_visible(kind),
            )
        )
    entries.sort(key=lambda e: e.id)
    return entries


def scan_hf_cache(
    overrides: Optional[Dict[str, Kind]] = None,
    required_by: Optional[Dict[str, List[str]]] = None,
    *,
    force: bool = False,
) -> List[ModelEntry]:
    """Return ``ModelEntry``s for every model in the HF cache.

    Cached on the ``(path, mtime, size)`` signature of the hub root's
    ``models--*`` children. ``force=True`` skips the cache (used by
    ``refresh_cache``).

    Note that ``overrides`` and ``required_by`` are applied at *build*
    time, so they don't participate in the cache key. Callers that change
    them between calls must pass ``force=True`` (the package-level
    ``refresh_cache`` does this for you).
    """
    global _cached_signature, _cached_entries
    hub_dir = _resolve_hub_dir()
    sig = _scan_signature(hub_dir)
    with _cache_lock:
        if not force and _cached_signature is not None and sig == _cached_signature:
            return list(_cached_entries)
        try:
            entries = _build_entries(
                hub_dir, overrides=overrides, required_by=required_by
            )
        except Exception as exc:
            logger.warning("hf_scanner: build failed under %s: %s", hub_dir, exc)
            entries = []
        _cached_entries = entries
        _cached_signature = sig
        return list(entries)


def invalidate_cache() -> None:
    """Drop the cached scan so the next ``scan_hf_cache`` call rebuilds."""
    global _cached_signature, _cached_entries
    with _cache_lock:
        _cached_signature = None
        _cached_entries = []


def hub_root() -> Path:
    """Return the resolved HF hub cache root (for diagnostics / tests)."""
    return _resolve_hub_dir()


__all__ = ["scan_hf_cache", "invalidate_cache", "hub_root"]
