"""tool_runner — in-process dispatcher for Wylde tools.

Phase 6 replacement for the legacy ``core/tool-runner/`` Flask service. The
legacy runner shipped a single ``TOOLS_CONFIG`` dict, a ``/api/<tool>``
HTTP endpoint, and a subprocess.CompletedProcess-shaped result contract.
Everything from that era — Flask app, /api routes, register_tools.py self-
registration, n8n pipeline loopback — is gone. The new runner is a plain
function: import a tool, call it, return the dict.

Dispatch path::

    LLM → harness.tooling.tool_runner.run_tool(tool_id, params, context)
        → tool_registry.list_tools()                    # filesystem catalog
        → importlib.import_module(<manifest.module>)    # dynamic import
        → getattr(module, <manifest.entrypoint>)(params)
        → result envelope
        → LLM context

No HTTP, no service registration, no subprocess wrapper. Tools are pure
Python functions of shape ``run_<tool_id>(params: dict) -> dict``. They
return their own data shapes; the runner only wraps errors.

Result envelope::

    {"ok": True,  "tool": "<id>", "data": <tool's return value>, "elapsed_ms": <int>}
    {"ok": False, "tool": "<id>", "error": {"code": "...", "message": "...", "details": {...}}, "elapsed_ms": <int>}

Codes match the 7-code taxonomy in ``Core/shared/errors.py``. Retry logic
also reuses the shared ``retry_with_backoff`` helper — we don't redefine the
schedule. Most tool failures (auth, parse, not_found) are not retryable and
short-circuit; only ``connection_refused``, ``timeout``, and
``resource_exhausted`` go through the exponential schedule.

Caller convenience::

    from Core.harness.tooling.tool_runner import run_tool

    out = run_tool("git_status", {"path": "."})
    if not out["ok"]:
        ...
"""

from __future__ import annotations

import importlib
import logging
import os
import time
from typing import Any, Callable, Dict, Optional

from ....shared.errors import IpcError, classify, retry_with_backoff
from ..tool_registry import list_tools

logger = logging.getLogger("wylde.harness.tooling.tool_runner")

# Codes that warrant the §5 retry schedule. Mirrors the default in
# shared.errors.retry_with_backoff but spelled out here so the policy is
# inspectable from the runner side.
_RETRYABLE_CODES = frozenset({"connection_refused", "timeout", "resource_exhausted"})

# ── Confirmation gate (Wylde Design Principle #12) ────────────────────────
#
# Tools whose manifests declare ``requires_confirmation: true`` cannot be
# dispatched directly. The runner intercepts them and returns a
# ``confirmation_required`` envelope; the caller (LLM agent loop) is
# expected to surface it to the user, gather a yes/no, and either re-call
# this runner with ``confirm=True`` or abort.
#
# The gate is bypassed when the WYLDE_AUTO_MODE env var is truthy. This
# is the auto-mode escape hatch — set by Core/Config/auto_mode.yaml and
# overridable per-process by setting the env var directly.

_AUTO_MODE_ENV = "WYLDE_AUTO_MODE"


def _auto_mode_active() -> bool:
    """True if the WYLDE_AUTO_MODE env var is set to a truthy value."""
    raw = os.environ.get(_AUTO_MODE_ENV, "")
    return raw.strip().lower() in {"1", "true", "yes", "on"}


# tools/ root, derived from this module's actual import name so the runner
# works regardless of whether the project is rooted at ``Wylde.Core`` or
# ``Core`` or anywhere else. ``__name__`` here is the runner's package
# (``...tooling.tool_runner``); strip the last segment, append ``.tools``.
_TOOLS_PACKAGE_PREFIX = ".".join(__name__.split(".")[:-1] + ["tools"])


# ── Resolution ────────────────────────────────────────────────────────────


def _resolve_entry(tool_id: str) -> Dict[str, Any]:
    """Find the manifest entry for ``tool_id`` or raise ``not_found``.

    Looks up the in-process catalog (``tool_registry.list_tools``). The
    catalog is mtime-cached; repeat calls in a chat turn are essentially
    free.
    """
    catalog = list_tools()
    entry = catalog.get(tool_id)
    if entry is None:
        raise IpcError(
            "not_found",
            f"unknown tool: {tool_id!r}",
            details={"available": sorted(catalog.keys())[:50]},
        )
    return entry


def _load_extension_bridge() -> Any:
    """Return the extension_bridge module or ``None`` if unreachable.

    The bridge folder is ``Wylde/Extensions/extension_bridge/`` — space
    in the name — so it's registered in :data:`sys.modules` under the
    qualified name ``Wylde.Extensions.extension_bridge`` by the
    ``Wylde/Extensions/__init__.py`` shim. We try both the namespaced
    import (when sys.path contains Wylde's parent) and the bare path
    (when sys.path contains Wylde itself, the typical pytest layout).
    """
    try:
        from Wylde.Extensions import extension_bridge

        return extension_bridge
    except ImportError:
        pass
    try:
        from Extensions import extension_bridge

        return extension_bridge
    except ImportError:
        return None


def _resolve_callable(entry: Dict[str, Any]) -> Callable[[Dict[str, Any]], Any]:
    """Import the tool module and return its entrypoint.

    Resolution order:

    1. If the manifest carries ``service: "extension"``, dispatch via
       the extension_bridge's :func:`bridge.dispatch` rather than
       importing from the harness tools/ tree. Extension tools live
       under ``Wylde/Extensions/<name>/`` and the bridge owns handler
       resolution + enable-flag checks; the runner just hands off
       ``(tool_id, params)``.
    2. If ``manifest.module`` is set, try that import path verbatim. This
       lets a tool ship from anywhere on sys.path (e.g. an extension that
       registers a manifest into the tools/ tree but lives elsewhere).
    3. Otherwise, derive the path from ``manifest.group`` + ``manifest.id``
       relative to this runner's package — ``<tooling>/tools/<group>/<id>/<id>``.
       Falls back to the package itself (``<tooling>/tools/<group>/<id>``)
       if the inner module is missing, since each tool folder's
       ``__init__.py`` re-exports ``run_<id>`` by convention.

    Entrypoint defaults to ``run_<tool_id>`` when the manifest doesn't
    specify one.  Extension tools ignore ``entrypoint`` here — the
    bridge looks up the function name from the extension manifest's
    ``endpoint`` field.
    """
    tool_id = str(entry.get("id") or "")
    declared_module = str(entry.get("module") or "").strip()
    entrypoint = str(entry.get("entrypoint") or f"run_{tool_id}").strip()
    group = str(entry.get("group") or "").strip()

    # Extension-routed tools — dispatch via extension_bridge.  The bridge
    # owns enable-flag checks, handler-module loading, and exception
    # wrapping; we just return a thin callable that forwards params.
    if entry.get("service") == "extension":
        bridge = _load_extension_bridge()
        if bridge is None:
            raise IpcError(
                "internal_error",
                f"tool {tool_id!r}: service='extension' but "
                "Wylde.Extensions.extension_bridge is not importable",
            )

        def _extension_call(params: Dict[str, Any]) -> Any:
            # The bridge's typed errors are runtime-only — translate them
            # into IpcError so the existing envelope code path handles
            # the failure shape uniformly.
            try:
                return bridge.dispatch(tool_id, params)
            except bridge.ExtensionNotEnabled as exc:
                raise IpcError("not_found", str(exc)) from exc
            except bridge.ExtensionNotFound as exc:
                raise IpcError("not_found", str(exc)) from exc
            except bridge.DispatchError as exc:
                raise IpcError("internal_error", str(exc)) from exc

        return _extension_call

    candidates: list[str] = []
    if declared_module:
        candidates.append(declared_module)
        # Service-rooted tools have their module auto-stamped as
        # ``Wylde.<Service>.tools.<id>.<id>`` by the registry. That import
        # path resolves when sys.path contains Wylde/'s parent, but fails
        # when sys.path contains Wylde/ itself (the typical layout when
        # running pytest from the repo root). Retry without the prefix so
        # the resolver succeeds either way.
        if declared_module.startswith("Wylde."):
            candidates.append(declared_module[len("Wylde.") :])
    if group and tool_id:
        # Inner-module convention: tools/<group>/<tool_id>/<tool_id>.py
        candidates.append(f"{_TOOLS_PACKAGE_PREFIX}.{group}.{tool_id}.{tool_id}")
        # Package convention: tools/<group>/<tool_id>/__init__.py re-exports.
        candidates.append(f"{_TOOLS_PACKAGE_PREFIX}.{group}.{tool_id}")
    if not candidates:
        raise IpcError(
            "internal_error",
            f"tool {tool_id!r}: manifest has neither 'module' nor 'group'",
        )

    last_err: Optional[Exception] = None
    mod = None
    for path in candidates:
        try:
            mod = importlib.import_module(path)
            break
        except ImportError as exc:
            last_err = exc
            continue

    if mod is None:
        raise IpcError(
            "internal_error",
            f"could not import tool {tool_id!r}: {last_err}",
            details={"candidates": candidates},
        )

    fn: Optional[Callable[[Dict[str, Any]], Any]] = getattr(mod, entrypoint, None)
    if not callable(fn):
        raise IpcError(
            "internal_error",
            f"tool {tool_id!r}: module {mod.__name__!r} has no callable {entrypoint!r}",
            details={"module": mod.__name__, "entrypoint": entrypoint},
        )
    return fn


# ── Invocation ────────────────────────────────────────────────────────────


def _invoke(fn: Callable[[Dict[str, Any]], Any], params: Dict[str, Any]) -> Any:
    """Call the tool. Translate raw exceptions into ``IpcError`` so retry
    machinery and the envelope wrapper see a consistent shape.

    Tools that raise ``IpcError`` themselves (with a proper code) bubble
    through unchanged. Anything else gets ``classify``'d.
    """
    try:
        return fn(params or {})
    except IpcError:
        raise
    except Exception as exc:
        code = classify(exc)
        raise IpcError(code, f"{type(exc).__name__}: {exc}") from exc


def _envelope_ok(tool_id: str, data: Any, elapsed_ms: int) -> Dict[str, Any]:
    return {"ok": True, "tool": tool_id, "data": data, "elapsed_ms": elapsed_ms}


def _envelope_error(tool_id: str, exc: IpcError, elapsed_ms: int) -> Dict[str, Any]:
    return {
        "ok": False,
        "tool": tool_id,
        "error": {"code": exc.code, "message": exc.message, "details": exc.details},
        "elapsed_ms": elapsed_ms,
    }


def _envelope_confirmation_required(
    tool_id: str,
    entry: Dict[str, Any],
    params: Dict[str, Any],
    elapsed_ms: int,
) -> Dict[str, Any]:
    """Envelope returned when a gated tool is dispatched without ``confirm``.

    Shape mirrors the ``ok=False`` error envelope so callers that only
    branch on ``ok`` keep working, while adding the gate-specific fields
    a caller needs to ask the user and re-dispatch:

    * ``confirmation_required`` — always ``True`` for this envelope
    * ``params``                — the params the runner *would* have used
    * ``description``           — the tool's catalog description
    * ``expected_effect``       — manifest's ``expected_effect`` (free text)
    """
    return {
        "ok": False,
        "tool": tool_id,
        "confirmation_required": True,
        "params": dict(params or {}),
        "description": entry.get("description", ""),
        "expected_effect": entry.get("expected_effect", ""),
        "elapsed_ms": elapsed_ms,
    }


# ── Public API ────────────────────────────────────────────────────────────


def run_tool(
    tool_id: str,
    params: Optional[Dict[str, Any]] = None,
    *,
    context: Optional[Dict[str, Any]] = None,
    retry: bool = True,
    confirm: bool = False,
) -> Dict[str, Any]:
    """Dispatch a tool by id and return its result envelope.

    Parameters
    ----------
    tool_id:
        Catalog id (e.g. ``"git_status"``). Must match a manifest under
        ``tooling/tools/**/manifest.json``.
    params:
        Dict passed verbatim to the tool's entrypoint. Defaults to ``{}``.
    context:
        Reserved for future use (per-turn provenance, auth scopes, …).
        Currently unused; tools that need session context can read from
        the env or from a thread-local set up by the caller.
    retry:
        When ``True`` (the default), retryable failures
        (``connection_refused``/``timeout``/``resource_exhausted``) go
        through the shared exponential schedule (§5: 1, 2, 4, 8, 30s; max
        5 attempts). Set ``False`` for tests or for tools the caller has
        already wrapped in their own retry policy.
    confirm:
        Bypass for the confirmation gate. When the resolved tool has
        ``requires_confirmation: true`` in its manifest, the runner
        returns a ``confirmation_required`` envelope *unless* either
        (a) ``confirm=True`` is passed here (the caller has obtained
        explicit user approval and is re-dispatching), or (b) the
        ``WYLDE_AUTO_MODE`` env var is truthy. Has no effect on tools
        that don't require confirmation.

    Returns
    -------
    Always a dict. ``ok=True`` envelopes carry a ``data`` field with the
    tool's raw return value; ``ok=False`` envelopes carry either a
    structured ``error`` block or — for gated tools awaiting approval —
    a ``confirmation_required`` block. The runner never raises.
    """
    del context  # reserved; see docstring
    started = time.perf_counter()

    try:
        entry = _resolve_entry(tool_id)
        fn = _resolve_callable(entry)
    except IpcError as exc:
        elapsed_ms = int((time.perf_counter() - started) * 1000)
        return _envelope_error(tool_id, exc, elapsed_ms)

    params = dict(params or {})

    # Confirmation gate (Wylde Design Principle #12). Read the tool's
    # manifest flag from the resolved catalog entry. If the tool requires
    # confirmation and the caller hasn't already obtained it (and auto-
    # mode is off), short-circuit with the confirmation envelope. The
    # caller surfaces it to the user and re-dispatches with confirm=True.
    if entry.get("requires_confirmation") and not confirm and not _auto_mode_active():
        elapsed_ms = int((time.perf_counter() - started) * 1000)
        logger.info(
            "tool_runner: %s requires confirmation; returning gate envelope",
            tool_id,
        )
        return _envelope_confirmation_required(tool_id, entry, params, elapsed_ms)

    def _call() -> Any:
        return _invoke(fn, params)

    try:
        if retry:
            data = retry_with_backoff(_call, on_codes=_RETRYABLE_CODES)
        else:
            data = _call()
    except IpcError as exc:
        elapsed_ms = int((time.perf_counter() - started) * 1000)
        logger.info("tool_runner: %s failed (%s): %s", tool_id, exc.code, exc.message)
        return _envelope_error(tool_id, exc, elapsed_ms)

    elapsed_ms = int((time.perf_counter() - started) * 1000)
    return _envelope_ok(tool_id, data, elapsed_ms)


__all__ = ["run_tool"]
