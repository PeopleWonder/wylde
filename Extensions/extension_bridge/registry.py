"""Extension registry — runtime state of which extensions are active.

The :mod:`loader` knows what's on disk; the :mod:`registry` knows what's
*currently* available to call. The split mirrors how ``tool_registry``
separates "every manifest under tools/" from "the catalog the LLM sees
right now".

State model
-----------
The registry's source of truth for "is X enabled" is the manifest's
``enabled`` flag. ``enable(name)`` / ``disable(name)`` rewrite that
field on disk so a process restart preserves the flip. Because the
loader's cache invalidates on any manifest mtime change, the next
:func:`discover_extensions` call automatically picks up the edit; we
don't keep a parallel in-memory toggle table.

Tool-catalog merge
------------------
:func:`enabled_tools` returns the list of catalog entries the
extension layer is currently contributing. ``tool_registry.list_tools``
calls into this and unions the result with its own filesystem walk.
The merge order is harness-internal first, then extensions; if an
extension declared a tool with an id that collides with a harness
tool the harness one wins (and we log a warning, same convention as
the in-tools-tree duplicate handler).
"""

from __future__ import annotations

import json
import logging
from pathlib import Path
from typing import Dict, List, Optional

from .contract import Extension
from .loader import discover_extensions, invalidate_loader_cache

logger = logging.getLogger("wylde.extensions.bridge.registry")


def list_extensions() -> Dict[str, Extension]:
    """All extensions found on disk, regardless of enabled state.

    Thin pass-through to :func:`discover_extensions` kept on the
    registry surface so callers don't have to know which submodule
    owns which method.
    """
    return discover_extensions()


def get_extension(name: str) -> Optional[Extension]:
    """Look up a single extension by name. Returns ``None`` if absent."""
    return list_extensions().get(name)


def enabled_extensions() -> Dict[str, Extension]:
    """Subset of :func:`list_extensions` whose ``enabled`` flag is true."""
    return {name: ext for name, ext in list_extensions().items() if ext.enabled}


def enabled_tools() -> List[Dict]:
    """Catalog entries contributed by all currently-enabled extensions.

    Each dict matches the shape returned by
    ``tool_registry._build_catalog`` so the consumer can union the
    two lists without reshaping. ``service`` is always
    ``"extension"`` and ``extension`` carries the source name.
    """
    out: List[Dict] = []
    for ext in enabled_extensions().values():
        for tool in ext.tools:
            out.append(tool.to_catalog_entry(ext.manifest_path))
    return out


def _set_enabled(name: str, enabled: bool) -> Extension:
    """Rewrite the ``enabled`` flag in this extension's manifest.json.

    Internal helper. Loads the file, edits one field, writes it back
    with ``indent=2`` so a human-readable diff lands in git. Re-reads
    the catalog after to confirm the flip.
    """
    ext = get_extension(name)
    if ext is None:
        raise KeyError(f"extension not found: {name!r}")
    if ext.enabled == enabled:
        return ext  # no-op; don't churn the file mtime
    path: Path = ext.manifest_path
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise RuntimeError(f"failed to re-read manifest {path}: {exc}") from exc
    raw["enabled"] = bool(enabled)
    # Preserve a trailing newline; many editors strip it on save and
    # diff tools complain. JSON spec doesn't require one but our
    # convention does.
    path.write_text(json.dumps(raw, indent=2) + "\n", encoding="utf-8")
    invalidate_loader_cache()
    _invalidate_tool_registry()
    refreshed = get_extension(name)
    assert refreshed is not None  # we just wrote a manifest with this name
    logger.info(
        "extension registry: %s %s (manifest=%s)",
        "enabled" if enabled else "disabled",
        name,
        path,
    )
    return refreshed


def enable(name: str) -> Extension:
    """Mark extension ``name`` enabled. Persists to manifest.json."""
    return _set_enabled(name, True)


def disable(name: str) -> Extension:
    """Mark extension ``name`` disabled. Persists to manifest.json."""
    return _set_enabled(name, False)


def refresh() -> None:
    """Drop both the loader cache and the tool-registry cache.

    Call after editing a manifest by hand, or when starting up to
    ensure a fresh read.
    """
    invalidate_loader_cache()
    _invalidate_tool_registry()


# ── Tool-registry cache invalidation ────────────────────────────────────────
#
# Lazy import to avoid pulling tool_registry at bridge import time —
# tool_registry imports our :func:`enabled_tools` to build its merged
# catalog, so eager import in either direction would deadlock the
# module loader. The lazy form keeps the dependency one-way at import
# time and bidirectional only at call time.


def _invalidate_tool_registry() -> None:
    try:
        from Core.harness.tooling.tool_registry import invalidate_cache
    except Exception as exc:
        logger.debug(
            "extension registry: tool_registry not importable yet (%s); "
            "skipping cache invalidation",
            exc,
        )
        return
    invalidate_cache()


__all__ = [
    "list_extensions",
    "get_extension",
    "enabled_extensions",
    "enabled_tools",
    "enable",
    "disable",
    "refresh",
]
