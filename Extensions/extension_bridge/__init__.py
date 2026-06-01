r"""extension_bridge — runtime wiring between Wylde core and extensions.

An *extension* (the Wylde user's Phase 7 definition) is anything that leaves the
system: talks to the public web, drives a browser, or shells out to a
native service. *Tools* stay internal — they're just functions inside the
harness. Extensions sit at the boundary; the bridge is the pipe that lets
Wylde-Core call out to them and lets them call back in.

What lives here
---------------
* :mod:`contract`   — schema for ``manifest.json`` plus the validated
                      ``Extension`` dataclass and ``LeavesSystem`` enum.
* :mod:`loader`     — discovers extensions on disk
                      (``Wylde/Extensions/<name>/manifest.json``),
                      validates each, returns a list of ``Extension``\ s.
                      mtime-cached the same way as ``tool_registry``.
* :mod:`registry`   — runtime state: which extensions are enabled, which
                      tools they currently expose. ``enable`` /
                      ``disable`` flip state and invalidate the tool
                      catalog so the next ``list_tools()`` call sees the
                      change.
* :mod:`dispatcher` — routes calls. Wylde-Core → extension when an LLM
                      tool call lands on an extension-provided tool;
                      Gateway → extension when the browser side of an
                      extension hits ``/extensions/<name>/<endpoint>``.
                      All external HTTP egress goes via
                      ``Core.shared.egress_client.forward``.

Public surface kept deliberately small. Most callers should reach for
the named submodules; this module re-exports the most common helpers
(``list_extensions`` / ``enabled_tools`` / ``dispatch``) so the typical
tool-registry merge call stays a one-liner.
"""

from __future__ import annotations

from .contract import (
    Extension,
    ExtensionTool,
    LeavesSystem,
    ManifestError,
)
from .dispatcher import (
    DispatchError,
    ExtensionNotEnabled,
    ExtensionNotFound,
    dispatch,
    dispatch_external,
)
from .loader import discover_extensions, invalidate_loader_cache
from .registry import (
    disable,
    enable,
    enabled_extensions,
    enabled_tools,
    get_extension,
    list_extensions,
    refresh,
)

__all__ = [
    # contract
    "Extension",
    "ExtensionTool",
    "LeavesSystem",
    "ManifestError",
    # loader
    "discover_extensions",
    "invalidate_loader_cache",
    # registry
    "list_extensions",
    "enabled_extensions",
    "enabled_tools",
    "enable",
    "disable",
    "get_extension",
    "refresh",
    # dispatcher
    "dispatch",
    "dispatch_external",
    "DispatchError",
    "ExtensionNotEnabled",
    "ExtensionNotFound",
]
