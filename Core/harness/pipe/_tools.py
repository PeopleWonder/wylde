"""tools.* action handlers — direct tool inspection and invocation.

Phase 5 strangler-fig (Slice 2)
-------------------------------

``WYLDE_HARNESS_IMPL=rust`` forwards ``tools.run`` to the Rust
``wylde-harness`` pipe at ``\\\\.\\pipe\\wylde-harness`` — the same
forward shape ``chat.run_turn`` uses (``_chat.py``). The Rust
``DefaultHarnessApi.tools_run`` dispatches into the Rust tool registry
(``rust/crates/wylde-harness/src/api.rs``); for the tools already
ported (``memory_search`` / ``memory_update`` / ``memory_delete`` /
``memory_long_term_save`` / ``rag_ask`` / the fs + meta + time tools)
the Rust handler is authoritative.

Two fall-throughs keep this safe:

* **Transport-class failure** (no Rust pipe up, daemon mis-spawn,
  missing binary) → silent fallback to the in-process Python runner so
  a deployment without the built binary can't lose direct tool access.
* **Tool not ported to Rust yet** — the Rust dispatch returns an
  ``ok:false`` envelope with a ``not_found`` / ``phase_*_deferred`` /
  ``not_implemented`` code. ``memory_workspace_save`` is the live
  example (the workspace-memory tier is a later slice). Those codes
  fall back to Python so the Python tool still runs. Genuine tool-level
  failures (the tool ran in Rust and returned ``status:error``) come
  back as ``ok:true`` data and are surfaced verbatim.

Set ``WYLDE_HARNESS_IMPL=python`` to keep every direct ``tools.run`` on
the in-process Python runner.
"""

from __future__ import annotations

import os
from typing import Any, Dict

from ._common import _ActionError, _payload_dict, logger

# Default forward timeout for a single direct tool invocation. Generous
# because retrieval tools (``rag_ask``) can run a multi-hop pipeline; the
# in-process runner has its own retry schedule on top.
_TOOLS_RUN_FORWARD_TIMEOUT_S = 120.0

# Rust reply error codes that mean "Rust didn't actually serve this tool"
# — fall back to the in-process Python runner so a Python-only tool still
# runs. Mirrors the transport-class set ``_chat.py`` uses, plus the
# registry-miss codes. ``phase_*_deferred`` is matched by prefix below.
_TOOL_FALLBACK_CODES = frozenset(
    {
        "not_found",  # tool not in the Rust registry
        "no_action",  # action not registered on the Rust pipe
        "not_implemented",
        "pipe_unavailable",
        "pipe_connect",
        "pipe_timeout",
        "pipe_io",
        "handshake_timeout",
        "handshake_io",
        "handshake_rejected",
    }
)


def _harness_impl() -> str:
    """Read ``WYLDE_HARNESS_IMPL`` (with ``WYLDE_HARNESS_TURN_IMPL`` as a
    one-release fallback) — the same gate ``_chat.py`` reads. Default
    ``rust``; anything other than ``python`` / ``rust`` clamps to the
    default.
    """
    raw = os.environ.get("WYLDE_HARNESS_IMPL")
    if raw is None:
        raw = os.environ.get("WYLDE_HARNESS_TURN_IMPL", "rust")
    val = raw.strip().lower()
    if val in ("python", "rust"):
        return val
    return "rust"


def _tools_list_action(_payload: Any) -> Dict[str, Any]:
    r"""Return the live tool catalog from the in-process registry.

    The GUI used to hit ``\\.\pipe\tool-registry`` for this; the harness
    pipe is the new home so tool inspection lives next to the chat-turn
    driver that uses it. Returns a list of catalog entries, not the
    keyed dict the registry stores internally — easier for callers.

    Not forwarded to Rust: ``tools.list`` reflects the *in-process*
    Python catalog that the in-process Python turn driver actually
    dispatches against, so it must mirror that surface, not the Rust
    registry. Forwarding it is a later slice once the Python driver
    (``turn/``) is retired.
    """
    try:
        from ..tooling.tool_registry import list_canonical_tools
    except ImportError:
        from Core.harness.tooling.tool_registry import list_canonical_tools
    catalog = list_canonical_tools()
    if isinstance(catalog, dict):
        entries = list(catalog.values())
    else:
        entries = list(catalog)
    return {"tools": entries, "count": len(entries)}


def _tools_run_action(payload: Any) -> Dict[str, Any]:
    """Run one tool by id. Returns the runner's envelope verbatim
    (``{ok, data}`` on success, ``{ok: False, error}`` on failure,
    ``{ok: False, confirmation_required: ...}`` for gated tools).

    External callers (mobile, tests, debug tooling) sometimes want to
    invoke a tool directly without going through a turn loop. The
    confirmation gate (Wylde Design Principle #12) still applies — the
    runner returns the gate envelope unless ``confirm: true`` is set.

    Phase 5 strangler-fig (Slice 2): when ``WYLDE_HARNESS_IMPL=rust``
    (the default), forwards to the Rust ``wylde-harness`` pipe and
    surfaces its reply verbatim. Transport failures and not-yet-ported
    tools fall back to the in-process Python runner — see the module
    docstring.
    """
    p = _payload_dict(payload)
    name = p.get("name")
    if not isinstance(name, str) or not name:
        raise _ActionError("bad_request", "name is required")
    args = p.get("args") or {}
    if not isinstance(args, dict):
        raise _ActionError("bad_request", "args must be a map")
    confirm = bool(p.get("confirm", False))

    if _harness_impl() == "rust":
        forwarded = _try_forward_tools_run_to_rust(name, args, confirm, p)
        if forwarded is not None:
            return forwarded
        # Fall-through: Rust unreachable or the tool isn't ported yet → Python.

    try:
        from ..tooling.tool_runner import run_tool
    except ImportError:
        from Core.harness.tooling.tool_runner import run_tool
    return run_tool(name, args, confirm=confirm)


def _try_forward_tools_run_to_rust(
    name: str, args: Dict[str, Any], confirm: bool, p: Dict[str, Any]
) -> Dict[str, Any] | None:
    r"""Forward ``tools.run`` to ``\\.\pipe\wylde-harness``.

    Returns the Rust reply ``data`` on success, ``None`` when the caller
    should fall back to the in-process Python runner: transport failure,
    a non-dict reply, or a Rust ``ok:false`` envelope whose error code
    says the tool isn't Rust-served (``not_found`` / ``phase_*_deferred``
    / ``not_implemented`` — e.g. the still-deferred
    ``memory_workspace_save``). A genuine service-level transport error
    re-raises as ``_ActionError`` to match the Python failure envelope.
    """
    try:
        from Core.shared.ipc import send_action as _ipc_send_action
    except ImportError:  # pragma: no cover — IPC shim always present in prod
        return None

    forward_payload: Dict[str, Any] = {
        "name": name,
        "args": args,
        "confirm": confirm,
    }
    device_tier = p.get("device_tier")
    if isinstance(device_tier, str) and device_tier:
        forward_payload["device_tier"] = device_tier

    try:
        reply = _ipc_send_action(
            "wylde-harness",
            "tools.run",
            forward_payload,
            timeout=_TOOLS_RUN_FORWARD_TIMEOUT_S,
        )
    except Exception:  # noqa: BLE001 — transport failures fall back to Python
        return None

    if not getattr(reply, "ok", False):
        # Transport-layer failure (no Rust pipe, daemon down, binary not
        # built) → silent fallback. A genuine service-level failure is
        # surfaced as if Python had raised, keeping the envelope shape
        # consistent for callers.
        err = getattr(reply, "error", None) or {}
        code = err.get("code") if isinstance(err, dict) else None
        if code in _TOOL_FALLBACK_CODES:
            return None
        message = ""
        if isinstance(err, dict):
            message = str(err.get("message") or err.get("code") or "rust tools.run error")
        raise _ActionError(str(code or "rust_tools_run_error"), message)

    data = getattr(reply, "data", None)
    if not isinstance(data, dict):
        return None

    # Rust served the action, but the tool itself isn't ported to Rust
    # yet — the dispatch returns an ok:false envelope with a registry-miss
    # / deferred code. Fall back so the Python-only tool still runs.
    if data.get("ok") is False:
        err = data.get("error") or {}
        code = err.get("code") if isinstance(err, dict) else None
        if isinstance(code, str) and (
            code in _TOOL_FALLBACK_CODES
            or (code.startswith("phase_") and code.endswith("_deferred"))
        ):
            logger.debug(
                "tools.run: rust has no active handler for %s (%s); "
                "falling back to in-process Python runner",
                name,
                code,
            )
            return None

    # Trust the Rust reply shape — tools.run returns the same
    # {ok, data, ...} envelope the Python runner builds.
    return data
