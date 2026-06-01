"""Extension loader — discovers and validates extensions on disk.

Filesystem-as-registry, same convention as ``tool_registry``: every
extension is just a folder under ``Wylde/Extensions/`` with a
``manifest.json`` at the top. Drop a folder in, it's catalogued; remove
it, it's gone. Two folders are reserved and ignored:

* ``extension_bridge/`` — the bridge itself.
* names beginning with ``_`` — staging / legacy folders
  (e.g. ``_old_service``, ``_helpers``).

Per-tool manifests
------------------
The top-level extension manifest is the inventory: it lists every tool
the extension contributes. Each tool *may* additionally have its own
``Extensions/<name>/tools/<tool_id>/manifest.json`` mirroring the
harness tool-shape (same keys ``id``, ``description``, ``parameters``,
etc.). When present these overlays take precedence — the description,
parameters, and tags read from the per-tool manifest replace what the
extension manifest declared. This lets a tool be inspected in isolation
the same way harness tools can, without forcing every author to
duplicate the inline-list approach.

Cache
-----
The loader caches its result keyed on the (path, mtime, size) signature
of every manifest.json under ``Extensions/`` — same trick the
``tool_registry`` uses, so repeat calls within a chat turn are cheap.
The signature includes per-tool manifests too, so editing a tool's
overlay invalidates the cache.
"""

from __future__ import annotations

import json
import logging
import threading
from pathlib import Path
from typing import Dict, List, Optional, Tuple

from .contract import Extension, ExtensionTool, ManifestError

logger = logging.getLogger("wylde.extensions.bridge.loader")

# loader.py → extension_bridge/ → Extensions/
_EXTENSIONS_DIR: Path = Path(__file__).resolve().parent.parent

# Folders inside Extensions/ that are not extensions themselves.
_RESERVED_NAMES = {"extension_bridge"}


_cache_lock = threading.Lock()
_cached_signature: Optional[Tuple[Tuple[str, float, int], ...]] = None
_cached_extensions: Dict[str, Extension] = {}


def _is_extension_folder(folder: Path) -> bool:
    if not folder.is_dir():
        return False
    if folder.name in _RESERVED_NAMES:
        return False
    if folder.name.startswith("_"):
        return False
    if folder.name.startswith("."):
        return False
    return True


def _scan_signature() -> Tuple[Tuple[str, float, int], ...]:
    """(path, mtime, size) for every extension + per-tool manifest, sorted."""
    if not _EXTENSIONS_DIR.is_dir():
        return ()
    sigs: List[Tuple[str, float, int]] = []
    for child in sorted(_EXTENSIONS_DIR.iterdir(), key=lambda p: p.name.lower()):
        if not _is_extension_folder(child):
            continue
        manifest = child / "manifest.json"
        if not manifest.is_file():
            continue
        try:
            st = manifest.stat()
        except OSError:
            continue
        sigs.append((str(manifest), st.st_mtime, st.st_size))
        # Per-tool overlay manifests under <ext>/tools/<tool_id>/manifest.json
        tools_dir = child / "tools"
        if tools_dir.is_dir():
            for tm in tools_dir.rglob("manifest.json"):
                try:
                    st = tm.stat()
                except OSError:
                    continue
                sigs.append((str(tm), st.st_mtime, st.st_size))
    sigs.sort()
    return tuple(sigs)


def _load_one(manifest_path: Path) -> Optional[Extension]:
    try:
        raw = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        logger.warning("extension loader: failed to read %s: %s", manifest_path, exc)
        return None
    try:
        ext = Extension.from_manifest(manifest_path, raw)
    except ManifestError as exc:
        logger.warning(
            "extension loader: invalid manifest at %s: %s", manifest_path, exc
        )
        return None
    return _apply_tool_overlays(ext)


def _apply_tool_overlays(ext: Extension) -> Extension:
    """Replace tool entries with per-tool manifests where present.

    Per-tool manifests live at
    ``Extensions/<ext>/tools/<tool_id>/manifest.json`` and mirror the
    harness tool-shape. When found they take precedence over the
    inline declaration in the extension manifest — description,
    parameters, tags, version, and group all come from the overlay.
    Endpoint stays from the inline declaration so the dispatcher
    routing isn't surprised by a renamed endpoint.
    """
    tools_dir = ext.folder / "tools"
    if not tools_dir.is_dir():
        return ext
    overlays: Dict[str, Dict] = {}
    for tm in tools_dir.rglob("manifest.json"):
        try:
            data = json.loads(tm.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            logger.warning("extension loader: bad per-tool manifest %s: %s", tm, exc)
            continue
        if not isinstance(data, dict):
            continue
        # Tool id resolution: explicit "id" field or the parent folder name.
        tool_id = str(data.get("id") or tm.parent.name).strip()
        if not tool_id:
            continue
        overlays[tool_id] = {"data": data, "path": str(tm)}
    if not overlays:
        return ext

    new_tools: List[ExtensionTool] = []
    for tool in ext.tools:
        ov = overlays.get(tool.tool_id)
        if ov is None:
            new_tools.append(tool)
            continue
        data = ov["data"]
        params_raw = data.get("parameters") or list(tool.parameters)
        if not isinstance(params_raw, list):
            logger.warning(
                "extension loader: per-tool manifest %s 'parameters' must "
                "be a list; ignoring overlay",
                ov["path"],
            )
            new_tools.append(tool)
            continue
        new_tools.append(
            ExtensionTool(
                tool_id=tool.tool_id,
                description=str(data.get("description") or tool.description),
                extension_name=tool.extension_name,
                group=str(data.get("group") or tool.group),
                endpoint=tool.endpoint,  # endpoint stays inline-defined
                parameters=tuple(dict(p) for p in params_raw if isinstance(p, dict)),
                tags=tuple(str(t) for t in (data.get("tags") or tool.tags)),
                version=str(data.get("version") or tool.version),
            )
        )

    # Warn about overlay manifests that don't match any declared tool —
    # likely a typo in the per-tool manifest's id or a stale folder.
    declared = {t.tool_id for t in ext.tools}
    for orphan_id, ov in overlays.items():
        if orphan_id not in declared:
            logger.warning(
                "extension loader: per-tool manifest %s declares id %r "
                "but extension %r doesn't list it; overlay ignored",
                ov["path"],
                orphan_id,
                ext.name,
            )

    # Extension is frozen; build a new one with the merged tools.
    return Extension(
        name=ext.name,
        description=ext.description,
        version=ext.version,
        enabled=ext.enabled,
        transport=ext.transport,
        handler_module=ext.handler_module,
        capabilities=ext.capabilities,
        tools=tuple(new_tools),
        folder=ext.folder,
        manifest_path=ext.manifest_path,
        browser_extension_path=ext.browser_extension_path,
        raw=ext.raw,
    )


def _build() -> Dict[str, Extension]:
    if not _EXTENSIONS_DIR.is_dir():
        return {}
    out: Dict[str, Extension] = {}
    for child in sorted(_EXTENSIONS_DIR.iterdir(), key=lambda p: p.name.lower()):
        if not _is_extension_folder(child):
            continue
        manifest = child / "manifest.json"
        if not manifest.is_file():
            continue
        ext = _load_one(manifest)
        if ext is None:
            continue
        if ext.name in out:
            logger.warning(
                "extension loader: duplicate extension name %r at %s "
                "(keeping first occurrence)",
                ext.name,
                manifest,
            )
            continue
        out[ext.name] = ext
    return out


def discover_extensions() -> Dict[str, Extension]:
    """Walk ``Wylde/Extensions/`` once, return a name→Extension dict.

    Result is cached; the cache is invalidated automatically when any
    manifest's mtime / size changes. The returned dict is the cached
    object — callers must not mutate it.
    """
    global _cached_signature, _cached_extensions
    sig = _scan_signature()
    with _cache_lock:
        if sig == _cached_signature and _cached_extensions:
            return _cached_extensions
        _cached_extensions = _build()
        _cached_signature = sig
        return _cached_extensions


def invalidate_loader_cache() -> None:
    """Force the next :func:`discover_extensions` call to re-scan from disk.

    Called by :mod:`registry` after ``enable`` / ``disable`` flips an
    extension's manifest. Mostly an internal hook — tests can use it to
    re-read a manifest they edited mid-run without sleeping for the
    filesystem mtime to advance.
    """
    global _cached_signature, _cached_extensions
    with _cache_lock:
        _cached_signature = None
        _cached_extensions = {}


__all__ = [
    "discover_extensions",
    "invalidate_loader_cache",
]
