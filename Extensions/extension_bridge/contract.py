"""Extension contract — schema for ``manifest.json`` and the parsed model.

Every extension is a folder under ``Wylde/Extensions/`` with a
``manifest.json`` at the top. The bridge's :mod:`loader` validates
each manifest against the rules in this file and turns it into an
``Extension`` instance the rest of the bridge can pass around.

Manifest schema (canonical, JSON)::

    {
      "name":         "webcrawler",                 // unique, snake_case
      "description":  "Scrapes and fetches web ...",
      "version":      "1.0",                        // optional, default "1.0"
      "enabled":      false,                        // default false
      "transport":    "http",                       // "http" only for now
      "handler":      "handler",                    // python module under
                                                    // the extension folder
      "capabilities": ["egress.web"],               // LeavesSystem flags
      "tools": [
        {
          "tool_id":     "scrape",
          "group":       "web",                     // catalog grouping
          "description": "...",
          "endpoint":    "scrape",                  // optional, for HTTP
                                                    // ingress; defaults to
                                                    // tool_id
          "parameters":  [ ... ]                    // same shape as the
                                                    // existing tool catalog
        },
        ...
      ]
    }

For browser-side extensions the manifest may also declare
``browser_extension_path`` pointing at the chrome extension folder; the
bridge doesn't load that itself, but Gateway / setup tooling can.

Errors during validation raise :class:`ManifestError` with a path
prefix describing where in the document the problem is — that prefix
makes loader log lines self-explanatory when there are several
extensions on disk.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple


# ── Public errors ───────────────────────────────────────────────────────────


class ManifestError(ValueError):
    """Raised when an extension manifest fails validation."""


# ── Capability vocabulary ───────────────────────────────────────────────────


class LeavesSystem(str, Enum):
    """Declares the kind of egress an extension performs.

    Used by the Gateway to decide whether the extension is allowed to
    talk to the outside at all (kill switch, allowlist scopes). The set
    is intentionally small and additive; adding a flag is cheap, removing
    one is a breaking change.
    """

    EGRESS_WEB = "egress.web"
    """Talks to the public internet (HTTP/S)."""

    EGRESS_BROWSER = "egress.browser"
    """Drives a local web browser (DOM-aware automation)."""

    EGRESS_NATIVE = "egress.native"
    """Shells out to a native local service or app."""

    INGRESS_HTTP = "ingress.http"
    """Receives HTTP requests from outside (e.g. browser extensions)."""

    INGRESS_BROWSER = "ingress.browser"
    """Receives content from a browser extension running locally."""

    @classmethod
    def parse_set(cls, raw: Any, where: str) -> Tuple["LeavesSystem", ...]:
        if not isinstance(raw, list):
            raise ManifestError(f"{where}: 'capabilities' must be a list")
        out: List["LeavesSystem"] = []
        for i, item in enumerate(raw):
            if not isinstance(item, str):
                raise ManifestError(
                    f"{where}.capabilities[{i}]: each capability must be a string"
                )
            try:
                out.append(cls(item))
            except ValueError as exc:
                allowed = ", ".join(sorted(c.value for c in cls))
                raise ManifestError(
                    f"{where}.capabilities[{i}]: unknown capability "
                    f"{item!r}; allowed: {allowed}"
                ) from exc
        return tuple(out)


# ── Tool ─────────────────────────────────────────────────────────────────────


@dataclass(frozen=True)
class ExtensionTool:
    """One tool exposed by an extension.

    Maps cleanly onto the existing tool-catalog entry shape so
    ``tool_registry.list_tools`` can merge extension tools alongside
    the harness-internal ones with no special-casing on the consumer
    side.
    """

    tool_id: str
    description: str
    extension_name: str
    group: str = ""
    endpoint: str = ""
    """HTTP path segment under ``/extensions/<ext>/<endpoint>``. Empty
    string means dispatch by ``tool_id``."""
    parameters: Tuple[Dict[str, Any], ...] = field(default_factory=tuple)
    tags: Tuple[str, ...] = field(default_factory=tuple)
    version: str = "1.0"

    def to_catalog_entry(self, manifest_path: Path) -> Dict[str, Any]:
        """Render as a tool-catalog dict, matching the harness convention.

        The returned dict has the same keys as entries built by
        ``tool_registry._build_catalog`` so callers can't tell the
        origin apart by shape alone — only by ``service`` (``"extension"``)
        and the ``extension`` field.
        """
        return {
            "id": self.tool_id,
            "name": self.tool_id,
            "description": self.description,
            "tags": list(self.tags),
            "parameters": [dict(p) for p in self.parameters],
            "service": "extension",
            "group": self.group or self.extension_name,
            "module": "",  # extensions are dispatched, not imported by id
            "entrypoint": self.endpoint or self.tool_id,
            "version": self.version,
            "manifest_path": str(manifest_path),
            "extension": self.extension_name,
        }

    @classmethod
    def from_dict(
        cls,
        raw: Any,
        *,
        extension_name: str,
        where: str,
    ) -> "ExtensionTool":
        if not isinstance(raw, dict):
            raise ManifestError(f"{where}: each tool must be an object")
        tool_id = raw.get("tool_id") or raw.get("id")
        if not isinstance(tool_id, str) or not tool_id.strip():
            raise ManifestError(
                f"{where}: 'tool_id' is required and must be a non-empty string"
            )
        tool_id = tool_id.strip()
        description = str(raw.get("description") or "").strip()
        group = str(raw.get("group") or "").strip()
        endpoint = str(raw.get("endpoint") or "").strip()
        version = str(raw.get("version") or "1.0").strip()
        params_raw = raw.get("parameters") or []
        if not isinstance(params_raw, list):
            raise ManifestError(f"{where}.parameters: must be a list")
        params: List[Dict[str, Any]] = []
        for i, p in enumerate(params_raw):
            if not isinstance(p, dict):
                raise ManifestError(
                    f"{where}.parameters[{i}]: each parameter must be an object"
                )
            params.append(dict(p))
        tags_raw = raw.get("tags") or []
        if not isinstance(tags_raw, list):
            raise ManifestError(f"{where}.tags: must be a list of strings")
        tags = tuple(str(t) for t in tags_raw)
        return cls(
            tool_id=tool_id,
            description=description,
            extension_name=extension_name,
            group=group,
            endpoint=endpoint,
            parameters=tuple(params),
            tags=tags,
            version=version,
        )


# ── Extension ───────────────────────────────────────────────────────────────


@dataclass(frozen=True)
class Extension:
    """A validated extension definition, parsed from ``manifest.json``."""

    name: str
    description: str
    version: str
    enabled: bool
    transport: str
    """Currently always ``"http"`` — locked decision per Phase 7."""
    handler_module: str
    """Dotted name of the python module under the extension folder
    that exposes the dispatch surface (callable functions named after
    each tool's ``endpoint``). Default ``"handler"``."""
    capabilities: Tuple[LeavesSystem, ...]
    tools: Tuple[ExtensionTool, ...]
    folder: Path
    manifest_path: Path
    browser_extension_path: Optional[Path] = None
    """If this extension ships a browser side (chrome MV3), the path to
    that subfolder. Optional — Webcrawler does not have one."""
    raw: Dict[str, Any] = field(default_factory=dict)

    @property
    def tool_ids(self) -> Tuple[str, ...]:
        return tuple(t.tool_id for t in self.tools)

    def has_capability(self, cap: LeavesSystem) -> bool:
        return cap in self.capabilities

    @classmethod
    def from_manifest(cls, manifest_path: Path, raw: Any) -> "Extension":
        """Validate ``raw`` against the manifest schema, return an Extension.

        The on-disk folder name is the source-of-truth for ``name`` —
        the manifest's ``name`` field, if present, must match.
        """
        if not isinstance(raw, dict):
            raise ManifestError("manifest must be a JSON object at the top level")
        folder = manifest_path.parent
        folder_name = folder.name
        manifest_name = raw.get("name")
        if manifest_name is None:
            name = folder_name
        else:
            if not isinstance(manifest_name, str):
                raise ManifestError("'name': must be a string")
            if manifest_name.strip() != folder_name:
                raise ManifestError(
                    f"'name' ({manifest_name!r}) must match folder name "
                    f"({folder_name!r})"
                )
            name = manifest_name.strip()

        description = str(raw.get("description") or "").strip()
        version = str(raw.get("version") or "1.0").strip()

        enabled_raw = raw.get("enabled", False)
        if not isinstance(enabled_raw, bool):
            raise ManifestError("'enabled': must be a boolean")
        enabled = enabled_raw

        transport = str(raw.get("transport") or "http").strip().lower()
        if transport != "http":
            raise ManifestError(
                f"'transport': only 'http' is supported in Phase 7 "
                f"(got {transport!r}); WebSockets were explicitly excluded"
            )

        handler_module = str(raw.get("handler") or "handler").strip()
        if not handler_module:
            raise ManifestError("'handler': must be a non-empty string")

        caps = LeavesSystem.parse_set(raw.get("capabilities") or [], where="manifest")

        tools_raw = raw.get("tools") or []
        if not isinstance(tools_raw, list):
            raise ManifestError("'tools': must be a list")
        seen: set[str] = set()
        tools: List[ExtensionTool] = []
        for i, t in enumerate(tools_raw):
            tool = ExtensionTool.from_dict(t, extension_name=name, where=f"tools[{i}]")
            if tool.tool_id in seen:
                raise ManifestError(
                    f"tools[{i}]: duplicate tool_id {tool.tool_id!r} "
                    f"within this extension"
                )
            seen.add(tool.tool_id)
            tools.append(tool)

        browser_path: Optional[Path] = None
        if "browser_extension_path" in raw:
            bp = raw["browser_extension_path"]
            if not isinstance(bp, str) or not bp.strip():
                raise ManifestError(
                    "'browser_extension_path': must be a non-empty string"
                )
            candidate = folder / bp.strip()
            # Don't insist the folder exists yet; flag without erroring.
            browser_path = candidate

        return cls(
            name=name,
            description=description,
            version=version,
            enabled=enabled,
            transport=transport,
            handler_module=handler_module,
            capabilities=caps,
            tools=tuple(tools),
            folder=folder,
            manifest_path=manifest_path,
            browser_extension_path=browser_path,
            raw=dict(raw),
        )


__all__ = [
    "Extension",
    "ExtensionTool",
    "LeavesSystem",
    "ManifestError",
]
