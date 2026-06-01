"""Extension dispatcher — routes calls between Wylde-Core and extensions.

Two entry points:

* :func:`dispatch`           — Wylde-Core → extension. Used when the LLM
                               picks an extension-provided tool out of
                               the catalog and the runner needs to
                               actually execute it. Resolves the
                               extension by tool id, loads its handler
                               module, calls the matching function,
                               returns the result.
* :func:`dispatch_external`  — Outside world → extension. Used by the
                               Gateway when it receives an HTTP request
                               at ``/extensions/<name>/<endpoint>``.
                               Same handler-resolution path; the
                               difference is the entry name (extension
                               + endpoint, not extension + tool_id).

Handler module convention
-------------------------
Each extension's manifest declares ``handler`` (default ``"handler"``).
The dispatcher imports that module from inside the extension's folder
and looks up a function whose name is the tool's ``endpoint`` (or
``tool_id`` when ``endpoint`` is empty). The function takes a single
``params`` dict and returns a JSON-serialisable dict.

Egress
------
If the handler itself wants to call out to the public web (the
Webcrawler does), it must use ``Core.shared.egress_client.forward`` rather
than driving its own HTTP client. The dispatcher doesn't enforce that
mechanically — extensions are trusted code the Wylde user wrote — but the
Webcrawler handler we ship demonstrates the pattern.

Errors
------
Three exception classes let callers react meaningfully:

* :class:`ExtensionNotFound`  — no extension by that name on disk.
* :class:`ExtensionNotEnabled` — found but disabled; calling code
                                 should probably surface that to the
                                 user (offer to enable).
* :class:`DispatchError`      — anything else: missing handler module,
                                 missing function, raised exception
                                 inside the handler.
"""

from __future__ import annotations

import importlib.util
import logging
import sys
from types import ModuleType
from typing import Any, Callable, Dict, Optional, Tuple, cast

from .contract import Extension, ExtensionTool
from .registry import get_extension, list_extensions

logger = logging.getLogger("wylde.extensions.bridge.dispatcher")


# ── Errors ──────────────────────────────────────────────────────────────────


class DispatchError(RuntimeError):
    """Generic dispatch failure (missing handler module, raised exception)."""


class ExtensionNotFound(DispatchError):
    """No extension by that name exists on disk."""


class ExtensionNotEnabled(DispatchError):
    """Extension exists but is currently disabled."""


# ── Handler-module loading ──────────────────────────────────────────────────
#
# Cached by absolute handler path so repeat dispatches inside one chat
# turn don't re-parse Python every time. Cache invalidates implicitly
# when an extension folder moves (path changes).


_handler_cache: Dict[str, ModuleType] = {}


def _load_handler(extension: Extension) -> ModuleType:
    """Import the extension's handler module from disk via importlib.

    Extensions live under ``Wylde/Extensions/<Name>/`` — the folder
    name often has mixed case but is otherwise import-friendly. We
    still go through importlib so the handler module is registered
    under a stable qualified name (``wylde_extension.<name>.<handler>``)
    and so we don't depend on the parent ``Extensions/`` package being
    fully importable in every test setup.
    """
    handler_file = extension.folder / f"{extension.handler_module}.py"
    cache_key = str(handler_file)
    cached = _handler_cache.get(cache_key)
    if cached is not None:
        return cached
    if not handler_file.is_file():
        raise DispatchError(
            f"extension {extension.name!r}: handler module "
            f"{extension.handler_module!r} not found at {handler_file}"
        )
    qual = f"wylde_extension.{extension.name}.{extension.handler_module}"
    spec = importlib.util.spec_from_file_location(qual, handler_file)
    if spec is None or spec.loader is None:
        raise DispatchError(
            f"extension {extension.name!r}: could not create import spec "
            f"for {handler_file}"
        )
    module = importlib.util.module_from_spec(spec)
    sys.modules[qual] = module
    try:
        spec.loader.exec_module(module)
    except Exception as exc:
        sys.modules.pop(qual, None)
        raise DispatchError(
            f"extension {extension.name!r}: handler import raised: "
            f"{type(exc).__name__}: {exc}"
        ) from exc
    _handler_cache[cache_key] = module
    return module


def _resolve_function(
    extension: Extension, function_name: str
) -> Callable[[Dict[str, Any]], Dict[str, Any]]:
    """Find the handler function and check it's callable."""
    module = _load_handler(extension)
    fn = getattr(module, function_name, None)
    if fn is None:
        raise DispatchError(
            f"extension {extension.name!r}: handler module exposes no "
            f"function named {function_name!r}"
        )
    if not callable(fn):
        raise DispatchError(
            f"extension {extension.name!r}: {function_name!r} is not callable"
        )
    return cast(Callable[[Dict[str, Any]], Dict[str, Any]], fn)


# ── Routing ─────────────────────────────────────────────────────────────────


def _find_by_tool_id(tool_id: str) -> Tuple[Extension, ExtensionTool]:
    """Locate which extension owns a given tool_id."""
    for ext in list_extensions().values():
        for tool in ext.tools:
            if tool.tool_id == tool_id:
                return ext, tool
    raise ExtensionNotFound(f"no extension provides tool_id {tool_id!r}")


def _find_by_endpoint(
    extension_name: str, endpoint: str
) -> Tuple[Extension, ExtensionTool]:
    """Used by the Gateway path: extension name + endpoint segment."""
    ext = get_extension(extension_name)
    if ext is None:
        raise ExtensionNotFound(f"unknown extension: {extension_name!r}")
    for tool in ext.tools:
        if (tool.endpoint or tool.tool_id) == endpoint:
            return ext, tool
    raise ExtensionNotFound(
        f"extension {extension_name!r} has no endpoint {endpoint!r}"
    )


def _check_enabled(ext: Extension) -> None:
    if not ext.enabled:
        raise ExtensionNotEnabled(
            f"extension {ext.name!r} is disabled — enable it first via "
            f"the extension_bridge registry"
        )


def dispatch(tool_id: str, params: Optional[Dict[str, Any]] = None) -> Dict[str, Any]:
    """Wylde-Core → extension call by tool id.

    Resolves the extension that owns ``tool_id``, loads its handler
    module, and calls the function named after the tool's
    ``endpoint`` (defaulting to ``tool_id``). Raises one of the
    typed errors above on failure; otherwise returns whatever the
    handler returned.
    """
    params = dict(params or {})
    ext, tool = _find_by_tool_id(tool_id)
    _check_enabled(ext)
    fn_name = tool.endpoint or tool.tool_id
    fn = _resolve_function(ext, fn_name)
    logger.debug(
        "dispatching tool %r → extension %r handler %s.%s",
        tool_id,
        ext.name,
        ext.handler_module,
        fn_name,
    )
    try:
        return fn(params)
    except Exception as exc:
        raise DispatchError(
            f"extension {ext.name!r} handler {fn_name!r} raised: "
            f"{type(exc).__name__}: {exc}"
        ) from exc


def dispatch_external(
    extension_name: str,
    endpoint: str,
    params: Optional[Dict[str, Any]] = None,
) -> Dict[str, Any]:
    """External (e.g. browser via Gateway) → extension call.

    Same shape as :func:`dispatch` but addressed by ``(extension,
    endpoint)`` instead of ``tool_id`` because the browser side of an
    extension can register endpoints that aren't catalog tools (e.g.
    ``/extensions/wylde_study/index_page`` — used by the chrome
    extension but not exposed to the LLM as a tool).
    """
    params = dict(params or {})
    ext, tool = _find_by_endpoint(extension_name, endpoint)
    _check_enabled(ext)
    fn = _resolve_function(ext, tool.endpoint or tool.tool_id)
    logger.debug(
        "dispatching external request → extension %r endpoint %r",
        ext.name,
        endpoint,
    )
    try:
        return fn(params)
    except Exception as exc:
        raise DispatchError(
            f"extension {ext.name!r} endpoint {endpoint!r} raised: "
            f"{type(exc).__name__}: {exc}"
        ) from exc


__all__ = [
    "DispatchError",
    "ExtensionNotEnabled",
    "ExtensionNotFound",
    "dispatch",
    "dispatch_external",
]
