"""tools.* action handlers — direct tool inspection and invocation."""

from __future__ import annotations

from typing import Any, Dict

from ._common import _ActionError, _payload_dict


def _tools_list_action(_payload: Any) -> Dict[str, Any]:
    r"""Return the live tool catalog from the in-process registry.

    The GUI used to hit ``\\.\pipe\tool-registry`` for this; the harness
    pipe is the new home so tool inspection lives next to the chat-turn
    driver that uses it. Returns a list of catalog entries, not the
    keyed dict the registry stores internally — easier for callers.
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
    """
    p = _payload_dict(payload)
    name = p.get("name")
    if not isinstance(name, str) or not name:
        raise _ActionError("bad_request", "name is required")
    args = p.get("args") or {}
    if not isinstance(args, dict):
        raise _ActionError("bad_request", "args must be a map")
    confirm = bool(p.get("confirm", False))
    try:
        from ..tooling.tool_runner import run_tool
    except ImportError:
        from Core.harness.tooling.tool_runner import run_tool
    return run_tool(name, args, confirm=confirm)
