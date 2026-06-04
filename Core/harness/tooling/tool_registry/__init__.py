"""tool_registry — in-process catalog of Wylde tools.

Phase 1 left this folder as an empty scaffold; this minimal listing API was
added during the tool_search refactor (HTTP loopback -> in-process). Phase 7
extended the catalog to also include tools contributed by enabled
*extensions* (anything that talks to the outside world -- see
``Wylde/Extensions/extension_bridge/``). Phase 8 onwards will populate the
rest -- execution routing, auth scopes, analytics, etc.

Three discovery locations
-------------------------
The catalog is a union over three trees, walked in this order. On id
collision the earlier source wins -- harness > extension > service -- and
a warning is logged.

1. **Harness tools** -- ``Wylde/Core/harness/tooling/tools/<group>/<tool>/manifest.json``.
   The original location, used for cross-cutting tools that don't belong
   to any single service (meta, code, fs, git, search, diff, test,
   ollama, rag, visual, ...). ``group`` is the immediate parent folder.

2. **Extensions** -- ``Wylde/Extensions/<extension>/`` declares its tools
   through the extension_bridge registry. Tools from disabled extensions
   are excluded; toggling enable/disable busts the cache through the
   per-extension ``enabled`` flag captured in the signature.

3. **Services** -- ``Wylde/<Service>/tools/<tool>/manifest.json`` (and
   one level deeper, ``Wylde/<Service>/<SubService>/tools/<tool>/manifest.json``,
   for service-of-services layouts). For each
   surviving top-level Wylde subdirectory, the walker globs both its
   own ``tools/**/manifest.json`` AND any one-level-nested
   ``<sub>/tools/**/manifest.json``. Service-rooted tools have their
   ``group`` defaulted to the (immediate) parent folder name and their
   ``module`` defaulted to the canonical absolute import path derived
   from the manifest's filesystem location, so the runner can dispatch
   without knowing whether the tool is direct or nested.

Filesystem-as-registry convention
---------------------------------
Across all three locations: each tool folder ships a ``manifest.json``
describing the tool, and discovery is a glob over ``manifest.json`` -- no
service registration step, no mutable state. Drop a folder in, it's
catalogued; delete a folder, it's gone.

Cache model
-----------
The cache key is the union of three signatures:

* every harness ``manifest.json`` under ``tools/`` -- (path, mtime, size).
* every extension manifest under ``Extensions/<name>/`` -- same triples,
  plus a per-extension ``enabled`` flag captured into the signature so a
  flip alone busts the cache.
* every service-rooted ``manifest.json`` under
  ``Wylde/<Service>/tools/`` and ``Wylde/<Service>/<SubService>/tools/``
  -- same (path, mtime, size) triples.

If the combined signature matches the previous one, the cached catalog is
returned untouched. Otherwise the catalog is rebuilt from disk.
"""

from __future__ import annotations

import json
import logging
import threading
from pathlib import Path
from typing import Any, Dict, Optional, Tuple

logger = logging.getLogger("wylde.harness.tooling.tool_registry")

# tool_registry/__init__.py -> tooling/ -> harness/ -> Core/ -> Wylde/
_TOOLS_DIR: Path = Path(__file__).resolve().parent.parent / "tools"

# Wylde/ root -- five hops up from this file (tool_registry/ -> tooling/ ->
# harness/ -> Core/ -> Wylde/). Used for the service-folder walk; the
# resolution is robust because the on-disk layout under Wylde/ is fixed.
_WYLDE_ROOT: Path = Path(__file__).resolve().parents[4]

# Top-level folders under Wylde/ that the service walker must NOT treat
# as services. Core/ is the harness itself (already covered by
# ``_TOOLS_DIR``); Extensions/ is covered by the bridge. Folders starting
# with ``_`` (legacy, scratch, smoke logs) or ``.`` (vcs/system) are also
# skipped without listing them here.
_SKIP_TOPLEVEL_NAMES = frozenset({"core", "extensions"})

_cache_lock = threading.Lock()
# Composite signature: (harness_sig, extension_sig, service_sig). Any
# component may be empty; the union is what we compare against to
# decide rebuild.
_cached_signature: Optional[
    Tuple[Tuple[Any, ...], Tuple[Any, ...], Tuple[Any, ...]]
] = None
_cached_catalog: Dict[str, Dict[str, Any]] = {}


def _scan_signature() -> Tuple[Tuple[str, float, int], ...]:
    """Snapshot of (path, mtime, size) for every harness manifest under tools/.

    Sorted so the tuple is stable across runs and comparable by equality.
    Missing tools dir -> empty tuple (not an error; just no tools yet).
    """
    if not _TOOLS_DIR.is_dir():
        return ()
    sigs: list[Tuple[str, float, int]] = []
    for manifest in _TOOLS_DIR.rglob("manifest.json"):
        try:
            st = manifest.stat()
        except OSError:
            continue
        sigs.append((str(manifest), st.st_mtime, st.st_size))
    sigs.sort()
    return tuple(sigs)


def _extension_signature() -> Tuple[Tuple[str, float, int, int], ...]:
    """Signature contribution from the extension_bridge.

    Lazy import: the bridge lives outside Core/ and bringing it in at
    module load time would create a circular dependency (the bridge's
    registry calls back into ``invalidate_cache`` here when a flip
    happens). At call time the dependency is one-way per call site.

    We capture (path, mtime, size, enabled_flag) per extension so a
    pure ``enable``/``disable`` flip -- which only changes the manifest
    by one boolean -- still busts our cache via mtime *and* via the
    explicit flag (belt-and-braces in case mtime resolution loses the
    edit).
    """
    try:
        from Wylde.Extensions import extension_bridge
    except Exception:  # pragma: no cover - bridge may be absent in some setups
        return ()
    if extension_bridge is None:
        return ()
    try:
        exts = extension_bridge.list_extensions()
    except Exception as exc:
        logger.debug(
            "tool_registry: extension bridge listing failed (%s); skipping",
            exc,
        )
        return ()
    sigs: list[Tuple[str, float, int, int]] = []
    for ext in exts.values():
        try:
            st = ext.manifest_path.stat()
        except OSError:
            continue
        sigs.append((str(ext.manifest_path), st.st_mtime, st.st_size, int(ext.enabled)))
    sigs.sort()
    return tuple(sigs)


def _service_tool_dirs() -> list[Path]:
    """Top-level ``Wylde/<Service>/tools/`` folders that may host
    service-owned tools, plus one-level-nested
    ``Wylde/<Service>/<SubService>/tools/`` for service-of-services
    layouts.

    A "service" here is any first-party top-level folder under ``Wylde/``
    that ships its own LLM-callable tools (e.g. ``Wylde/N8N/``). The
    walker enumerates ``Wylde/`` once and filters out folders that are
    not services: the harness root (``Core/``), the extension surface
    (``Extensions/``), anything starting with ``_`` (legacy/scratch) or
    ``.`` (vcs/system). For each surviving top-level folder, the walker
    captures both ``<Service>/tools`` AND any
    ``<Service>/<SubService>/tools`` (one level deep), mirroring the
    convention model_registry uses for service manifests so a
    "service-of-services" can host its sub-services' tool catalogs from
    their own folders.

    Returns the list of ``tools`` directories that actually exist on
    disk. Missing ``tools/`` subdir -> that level contributes nothing.
    """
    if not _WYLDE_ROOT.is_dir():
        return []
    out: list[Path] = []
    try:
        children = list(_WYLDE_ROOT.iterdir())
    except OSError:
        return []
    for child in children:
        if not child.is_dir():
            continue
        name = child.name
        if not name or name[0] in {"_", "."}:
            continue
        if name.lower() in _SKIP_TOPLEVEL_NAMES:
            continue
        # Direct: Wylde/<Service>/tools/
        tools_dir = child / "tools"
        if tools_dir.is_dir():
            out.append(tools_dir)
        # Nested: Wylde/<Service>/<SubService>/tools/. Only one level
        # deep -- sub-sub-services would imply too much depth and any
        # genuine case can be flattened.
        try:
            sub_children = list(child.iterdir())
        except OSError:
            continue
        for sub in sub_children:
            if not sub.is_dir():
                continue
            sname = sub.name
            if not sname or sname[0] in {"_", "."}:
                continue
            sub_tools = sub / "tools"
            if sub_tools.is_dir():
                out.append(sub_tools)
    return out


def _service_signature() -> Tuple[Tuple[str, float, int], ...]:
    """Snapshot of (path, mtime, size) for every service-rooted manifest.

    Walks every service tools dir resolved by ``_service_tool_dirs``
    (direct + one-level-nested). Sorted so the tuple is stable across
    runs and comparable by equality. Same shape as ``_scan_signature``
    so the union check stays uniform.
    """
    sigs: list[Tuple[str, float, int]] = []
    for tools_dir in _service_tool_dirs():
        for manifest in tools_dir.rglob("manifest.json"):
            try:
                st = manifest.stat()
            except OSError:
                continue
            sigs.append((str(manifest), st.st_mtime, st.st_size))
    sigs.sort()
    return tuple(sigs)


def _load_manifest(path: Path) -> Optional[Dict[str, Any]]:
    try:
        data: Dict[str, Any] = json.loads(path.read_text(encoding="utf-8"))
        return data
    except (OSError, json.JSONDecodeError) as exc:
        logger.warning("tool_registry: failed to load %s: %s", path, exc)
        return None


def _build_catalog() -> Dict[str, Dict[str, Any]]:
    """Walk tools/ once and turn every manifest.json into a catalog entry,
    then union in tools contributed by enabled extensions.

    Tool id resolution: ``manifest["id"]`` if present, otherwise the parent
    folder name (filesystem-as-id convention). Harness tools win on id
    collision with extension tools -- extensions are an additive surface,
    not an override one.
    """
    catalog: Dict[str, Dict[str, Any]] = {}
    if _TOOLS_DIR.is_dir():
        for manifest in _TOOLS_DIR.rglob("manifest.json"):
            data = _load_manifest(manifest)
            if not isinstance(data, dict):
                continue
            tool_dir = manifest.parent
            tool_id = str(data.get("id") or tool_dir.name).strip()
            if not tool_id:
                continue
            # Group is the immediate parent of the tool folder (e.g. "meta",
            # "git", "web"). Useful for filtering and for human readability.
            group = tool_dir.parent.name if tool_dir.parent != _TOOLS_DIR else ""
            entry: Dict[str, Any] = {
                "id": tool_id,
                "name": str(data.get("name") or tool_id),
                "description": str(data.get("description") or ""),
                "tags": list(data.get("tags") or []),
                "parameters": list(data.get("parameters") or []),
                "service": str(data.get("service") or ""),
                "group": group,
                "module": str(data.get("module") or ""),
                "entrypoint": str(data.get("entrypoint") or ""),
                "version": str(data.get("version") or "1.0"),
                # Confirmation gate (Wylde Design Principle #12). Tools that
                # create/modify/delete meaningful state set this to true in
                # their manifest; the runner reads it to decide whether to
                # short-circuit with a confirmation_required envelope.
                "requires_confirmation": bool(data.get("requires_confirmation", False)),
                # Human-readable description of the expected effect, surfaced
                # to the user when the gate fires. Empty for non-gated tools.
                "expected_effect": str(data.get("expected_effect") or ""),
                "manifest_path": str(manifest),
            }
            if tool_id in catalog:
                logger.warning(
                    "tool_registry: duplicate tool id %r (overwriting %s with %s)",
                    tool_id,
                    catalog[tool_id]["manifest_path"],
                    manifest,
                )
            catalog[tool_id] = entry

    # Union enabled extension tools. Lazy import -- the bridge's registry
    # calls back into ``invalidate_cache`` here when an extension is
    # toggled, so eager import would deadlock the loader.
    for ext_entry in _enabled_extension_entries():
        tool_id = str(ext_entry.get("id") or "").strip()
        if not tool_id:
            continue
        if tool_id in catalog:
            logger.warning(
                "tool_registry: extension tool %r collides with harness "
                "tool at %s; harness wins (extension manifest=%s)",
                tool_id,
                catalog[tool_id]["manifest_path"],
                ext_entry.get("manifest_path"),
            )
            continue
        catalog[tool_id] = ext_entry

    # Union service-rooted tools. Walk each Wylde/<Service>/tools/ and
    # ``Wylde/<Service>/<SubService>/tools/`` folder; turn every
    # manifest into a catalog entry, defaulting ``group`` to the tools
    # folder's parent name and ``module`` to the canonical absolute
    # import path so the runner can dispatch without knowing the
    # physical layout. Harness and extension tools win on collision
    # (services are the newest and least authoritative source -- drop
    # in to extend, don't drop in to override).
    for tools_dir in _service_tool_dirs():
        service_name = tools_dir.parent.name  # e.g. "N8N", "Voice", "Caption"
        for manifest in tools_dir.rglob("manifest.json"):
            data = _load_manifest(manifest)
            if not isinstance(data, dict):
                continue
            tool_dir = manifest.parent
            tool_id = str(data.get("id") or tool_dir.name).strip()
            if not tool_id:
                continue
            # Group: respect the manifest if set, else fall back to the
            # service folder name. (Existing manifests under N8N/tools/
            # already declare ``group: "n8n"`` -- that wins.)
            group = str(data.get("group") or service_name).strip()
            # Module: respect the manifest if set, else derive the
            # canonical absolute import path FROM THE FILESYSTEM. Tool
            # folders ship ``__init__.py`` re-exporting ``run_<id>`` and
            # a ``<id>.py`` carrying the entrypoint, so importing the
            # inner module is the safe default. Computing the dotted
            # path from the relative location under Wylde/ is the only
            # form that survives nested-service layouts (e.g.
            # ``Trainer/Caption/tools/<tool>/`` ->
            # ``Wylde.Trainer.Caption.tools.<tool>.<tool>``).
            declared_module = str(data.get("module") or "").strip()
            if not declared_module:
                try:
                    rel = tool_dir.relative_to(_WYLDE_ROOT)
                    declared_module = "Wylde." + ".".join(rel.parts) + f".{tool_id}"
                except ValueError:
                    # Tool dir somehow not under Wylde/ -- keep the legacy
                    # default so we don't regress N8N-style services.
                    declared_module = f"Wylde.{service_name}.tools.{tool_id}.{tool_id}"
            entry = {
                "id": tool_id,
                "name": str(data.get("name") or tool_id),
                "description": str(data.get("description") or ""),
                "tags": list(data.get("tags") or []),
                "parameters": list(data.get("parameters") or []),
                "service": str(data.get("service") or service_name),
                "group": group,
                "module": declared_module,
                "entrypoint": str(data.get("entrypoint") or ""),
                "version": str(data.get("version") or "1.0"),
                "requires_confirmation": bool(data.get("requires_confirmation", False)),
                "expected_effect": str(data.get("expected_effect") or ""),
                "manifest_path": str(manifest),
            }
            if tool_id in catalog:
                logger.warning(
                    "tool_registry: service tool %r (from %s) collides "
                    "with existing entry at %s; existing wins",
                    tool_id,
                    manifest,
                    catalog[tool_id]["manifest_path"],
                )
                continue
            catalog[tool_id] = entry
    return catalog


def _enabled_extension_entries() -> list:
    """Pull catalog entries from currently-enabled extensions.

    Returns an empty list if the bridge isn't importable yet (clean
    failure mode -- the harness still works without extensions wired).
    """
    try:
        from Wylde.Extensions import extension_bridge
    except Exception:
        return []
    if extension_bridge is None:
        return []
    try:
        return list(extension_bridge.enabled_tools())
    except Exception as exc:
        logger.warning(
            "tool_registry: extension bridge failed to list enabled tools: %s",
            exc,
        )
        return []


def _alias_keys_for(entry: Dict[str, Any]) -> Tuple[str, ...]:
    """Every key shape this entry should be findable under.

    The catalog is canonically keyed by ``id`` (snake_case folder name),
    but LLMs naturally generate dotted forms like ``memory.long_term.save``
    when the manifest's ``name`` field uses dots. Aliasing lets the
    runner resolve ``catalog.get("memory.long_term.save")`` AND
    ``catalog.get("memory_long_term_save")`` to the same entry without
    the LLM caring which it picked. Empty strings are filtered.
    """
    seen: Dict[str, None] = {}
    for k in (
        entry.get("id") or "",
        entry.get("name") or "",
        # Dot-form derived from snake-form: "memory_long_term_save" → "memory.long_term.save".
        # Best-effort — we only synthesize when the id has underscores and
        # the manifest didn't give us an explicit dotted name already.
        (entry.get("id") or "").replace("_", "."),
        # Snake-form derived from dot-form: "memory.long_term.save" → "memory_long_term_save".
        (entry.get("name") or "").replace(".", "_"),
    ):
        k = (k or "").strip()
        if k:
            seen[k] = None
    return tuple(seen.keys())


def _apply_aliases(catalog: Dict[str, Dict[str, Any]]) -> Dict[str, Dict[str, Any]]:
    """Return ``catalog`` with name-form aliases pointing at the same
    entry dicts. Mutates the dict in place AND returns it.

    Aliases never overwrite a canonical id — if ``memory.long_term.save``
    happened to be another tool's canonical id, we leave that mapping
    alone and skip the alias. The original lookup by ``id`` always wins.
    """
    canonical_ids = set(catalog.keys())
    for tool_id in list(canonical_ids):
        entry = catalog[tool_id]
        for alias in _alias_keys_for(entry):
            if alias in canonical_ids:
                # alias collides with another canonical id — never override
                continue
            catalog[alias] = entry
    return catalog


def list_tools() -> Dict[str, Dict[str, Any]]:
    """Return the full tool catalog, keyed by tool id (with name aliases).

    Walks three discovery roots and unions the results:

    * ``Wylde/Core/harness/tooling/tools/**/manifest.json`` (harness)
    * Enabled extensions via the extension_bridge registry
    * ``Wylde/<Service>/tools/**/manifest.json`` AND
      ``Wylde/<Service>/<SubService>/tools/**/manifest.json`` for each
      top-level service folder (skipping ``Core/``, ``Extensions/``,
      ``_*``, ``.*``)

    Result is cached and only rebuilt when the combined on-disk
    signature (paths + mtimes + sizes for harness and service trees,
    plus per-extension enabled flags) changes -- repeat calls within a
    chat turn are essentially free.

    Each entry is reachable under multiple keys: the canonical
    ``id`` (snake_case folder name), the manifest's ``name`` (often
    dotted), and the inverse-form derivations of each. This means an
    LLM tool call that comes back as ``memory.long_term.save`` resolves
    to the same entry as ``memory_long_term_save``. The same entry dict
    is shared across aliases — there's no copy.

    The returned dict is the cached object; callers must not mutate it.
    Standard fields per entry: ``id``, ``name``, ``description``, ``tags``,
    ``parameters``, ``service``, ``group``, ``module``, ``entrypoint``,
    ``version``, ``requires_confirmation``, ``expected_effect``,
    ``manifest_path``. Extension tools additionally carry an ``extension``
    field naming the source extension.
    """
    global _cached_signature, _cached_catalog
    sig = (_scan_signature(), _extension_signature(), _service_signature())
    with _cache_lock:
        if sig == _cached_signature and _cached_catalog:
            return _cached_catalog
        _cached_catalog = _apply_aliases(_build_catalog())
        _cached_signature = sig
        return _cached_catalog


def list_canonical_tools() -> Dict[str, Dict[str, Any]]:
    """Return the catalog with only canonical (``id``-keyed) entries.

    Use this anywhere the alias-keyed view would double-count tools —
    LLM-facing tool advertisements, GUI catalogs, count badges. The
    runner uses :func:`list_tools` for resolution; everything else
    should prefer this.
    """
    full = list_tools()
    # An entry's canonical key is the value of its ``id`` field. Anything
    # else is an alias pointing at the same entry.
    return {k: v for k, v in full.items() if v.get("id") == k}


def get_catalog() -> Dict[str, Dict[str, Any]]:
    """Alias for :func:`list_tools` -- kept for callers that prefer the noun form."""
    return list_tools()


def invalidate_cache() -> None:
    """Force the next :func:`list_tools` call to re-scan from disk.

    Mostly useful in tests; production callers can rely on the mtime-based
    invalidation in :func:`list_tools` itself.
    """
    global _cached_signature, _cached_catalog
    with _cache_lock:
        _cached_signature = None
        _cached_catalog = {}


__all__ = [
    "list_tools",
    "list_canonical_tools",
    "get_catalog",
    "invalidate_cache",
]
