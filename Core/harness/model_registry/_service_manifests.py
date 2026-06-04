"""model_registry/_service_manifests.py — read ``models`` declarations from services.

Each top-level service folder under the Wylde root may ship a
``manifest.json`` with a ``models`` array, e.g.::

    {
      "name": "VoiceAssistant",  # wylde-check: dead-ref-ok
      "models": [
        {"id": "openai/whisper-small", "kind": "stt", "required": true},
        {"id": "rhasspy/piper-voices", "kind": "tts", "required": true}
      ]
    }

This module collects every such declaration and returns:

* an ``overrides`` dict (model id → kind), used by the HF scanner to win
  over the heuristic, and
* a ``required_by`` dict (model id → list of service names) so the GUI
  can show which subsystem owns each model.

Manifests without a ``models`` key are silently skipped — most services
predate this contract and need no migration. The scanner also walks
``Core/harness/tooling/tools/<group>/<id>/manifest.json`` but those are
*tool* manifests, not *service* manifests; they're skipped by virtue of
the search root.
"""

from __future__ import annotations

import json
import logging
import os
import threading
from pathlib import Path
from typing import Dict, List, Optional, Set, Tuple

from ._types import KIND_VALUES, Kind

logger = logging.getLogger("wylde.harness.model_registry.service_manifests")

# model_registry/_service_manifests.py → model_registry → harness → Core → Wylde
_WYLDE_ROOT = Path(__file__).resolve().parents[3]

# Top-level dirs to scan. Skipping ``Core`` (no service manifests there) and
# ``_legacy`` (per the Wylde user's note: stay out of _legacy/). Anything new added
# to the Wylde root that ships a service manifest needs to be added here.
_SERVICE_ROOTS: Tuple[str, ...] = (
    "Voice",
    "VoiceAssistant",  # wylde-check: dead-ref-ok
    "Extensions",
    "Gateway",
    "device_gate",
    "N8N",
)

_lock = threading.Lock()
_cached_signature: Optional[Tuple[Tuple[str, float, int], ...]] = None
_cached_overrides: Dict[str, Kind] = {}
_cached_required_by: Dict[str, List[str]] = {}


def _candidate_manifests() -> List[Path]:
    """Find every service ``manifest.json`` under the recognised roots.

    A service manifest sits at ``<service>/manifest.json`` (top-level of the
    service folder) or at ``<service>/<sub>/manifest.json`` for nested
    services like ``Voice/_wylde_voice``. We allow both; tool manifests
    under ``Core/harness/tooling/tools/`` are not reached because we don't
    scan ``Core``.
    """
    paths: List[Path] = []
    seen: Set[Path] = set()
    for root_name in _SERVICE_ROOTS:
        root = _WYLDE_ROOT / root_name
        if not root.is_dir():
            continue
        # Direct child manifest first.
        direct = root / "manifest.json"
        if direct.is_file() and direct not in seen:
            paths.append(direct)
            seen.add(direct)
        # One level deep for the ``Voice/_wylde_voice``-style services.
        try:
            for child in root.iterdir():
                if not child.is_dir():
                    continue
                if child.name.startswith("__") or child.name.startswith("."):
                    continue
                m = child / "manifest.json"
                if m.is_file() and m not in seen:
                    paths.append(m)
                    seen.add(m)
        except OSError:
            continue
    return paths


def _scan_signature(paths: List[Path]) -> Tuple[Tuple[str, float, int], ...]:
    sigs: List[Tuple[str, float, int]] = []
    for p in paths:
        try:
            st = p.stat()
        except OSError:
            continue
        sigs.append((str(p), st.st_mtime, st.st_size))
    sigs.sort()
    return tuple(sigs)


def _coerce_kind(value: object) -> Optional[Kind]:
    if isinstance(value, str) and value in KIND_VALUES:
        return value  # type: ignore[return-value]
    return None


def _read_one(manifest: Path) -> Tuple[str, List[Tuple[str, Kind]]]:
    """Return (service_name, [(model_id, kind), ...]) for one manifest.

    Service name comes from the manifest's ``name`` field, falling back to
    the parent folder. Bad JSON or missing ``models`` arrays are tolerated
    and produce an empty model list, never an exception.
    """
    try:
        data = json.loads(manifest.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        logger.debug("service_manifests: skip %s (%s)", manifest, exc)
        return manifest.parent.name, []
    if not isinstance(data, dict):
        return manifest.parent.name, []
    service = str(data.get("name") or manifest.parent.name)
    models_raw = data.get("models")
    if not isinstance(models_raw, list):
        return service, []
    out: List[Tuple[str, Kind]] = []
    for spec in models_raw:
        if not isinstance(spec, dict):
            continue
        model_id = str(spec.get("id") or "").strip()
        kind = _coerce_kind(spec.get("kind"))
        if not model_id or kind is None:
            logger.debug(
                "service_manifests: %s: dropped entry %r (need id and recognised kind)",
                manifest,
                spec,
            )
            continue
        out.append((model_id, kind))
    return service, out


def _build() -> Tuple[Dict[str, Kind], Dict[str, List[str]]]:
    overrides: Dict[str, Kind] = {}
    required_by: Dict[str, List[str]] = {}
    for manifest in _candidate_manifests():
        service, decls = _read_one(manifest)
        for model_id, kind in decls:
            existing = overrides.get(model_id)
            if existing is not None and existing != kind:
                logger.warning(
                    "service_manifests: %s declares %s as %s but a prior service "
                    "declared %s; keeping first.",
                    service,
                    model_id,
                    kind,
                    existing,
                )
            else:
                overrides[model_id] = kind
            required_by.setdefault(model_id, []).append(service)
    return overrides, required_by


def load_declarations(
    *, force: bool = False
) -> Tuple[Dict[str, Kind], Dict[str, List[str]]]:
    """Return cached ``(overrides, required_by)`` from all service manifests.

    Cached on the manifests' (path, mtime, size) signature, same template
    as the HF scanner and ``tool_registry``. ``force=True`` rebuilds.
    """
    global _cached_signature, _cached_overrides, _cached_required_by
    paths = _candidate_manifests()
    sig = _scan_signature(paths)
    with _lock:
        if not force and _cached_signature is not None and sig == _cached_signature:
            return dict(_cached_overrides), {
                k: list(v) for k, v in _cached_required_by.items()
            }
        overrides, required_by = _build()
        _cached_signature = sig
        _cached_overrides = overrides
        _cached_required_by = required_by
        return dict(overrides), {k: list(v) for k, v in required_by.items()}


def invalidate_cache() -> None:
    global _cached_signature, _cached_overrides, _cached_required_by
    with _lock:
        _cached_signature = None
        _cached_overrides = {}
        _cached_required_by = {}


def wylde_root() -> Path:
    """For diagnostics / tests — show what path we resolve as the repo root."""
    override = os.getenv("WYLDE_ROOT")
    if override:
        return Path(override).expanduser()
    return _WYLDE_ROOT


__all__ = ["load_declarations", "invalidate_cache", "wylde_root"]
