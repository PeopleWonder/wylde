"""Per-call tool dispatch — tier gating, tool runner invocation, event
emission, and per-call summary bookkeeping.

This module owns one tool call from the moment the driver picks it off
``step.tool_calls`` to the moment the result / error event lands on
``state.tool_events``. The driver loop (in :mod:`._driver`) iterates
calls and delegates to :func:`_run_one_tool` here.

Also houses the per-tier constants (``TIER_READ_ONLY`` etc.) and the
canonical-id helpers (:func:`_canonicalise_tool_id`,
:func:`_resolve_tool_alias_map`) because they live alongside the gate
that consumes them.
"""

from __future__ import annotations

import logging
import time
from typing import Any, Callable, Dict, List, Optional, Protocol

from ._state import (
    ToolCall,
    TurnState,
    _emit_tool,
)

logger = logging.getLogger("wylde.harness.turn")


# Tool ids (canonical, snake_case) that are write surfaces against the
# memory layer. The dispatcher canonicalizes whatever the LLM called
# (dotted or snake) before consulting this set, so an LLM call to
# ``memory.long_term.save`` matches.
_MEMORY_WRITE_TOOL_IDS = frozenset(
    {
        "memory_long_term_save",
        "memory_workspace_save",
        "memory_update",
    }
)


# Device-permission tiers — string constants AND the rank table the
# tier-gate uses for "is this tier at-or-above that one?" comparisons.
# Mirrors ``device_gate/store.py``'s definitions but kept local to the
# turn driver so the chat-loop doesn't depend on the device_gate
# package (its folder name has a space and isn't a valid module path).
TIER_READ_ONLY = "read_only"
TIER_TOOL_USE = "tool_use"
TIER_DESTRUCTIVE = "destructive_tool_access"
_VALID_TIERS = frozenset({TIER_READ_ONLY, TIER_TOOL_USE, TIER_DESTRUCTIVE})
_DEFAULT_TIER = TIER_TOOL_USE


def _normalise_device_tier(tier: Optional[str]) -> str:
    """Pick the tier the turn runs under. Empty / unknown / None all
    fall back to :data:`_DEFAULT_TIER` (``tool_use``) — in-process
    callers (Voice service, desktop GUI via the local pipe) don't
    carry a Bearer token and aren't expected to thread one through;
    they're already inside the trust boundary."""
    if isinstance(tier, str) and tier in _VALID_TIERS:
        return tier
    return _DEFAULT_TIER


def _canonicalise_tool_id(name: str) -> str:
    """Resolve a tool name (dotted or snake) to its canonical id by
    consulting the registry. Returns the input unchanged when the
    catalog isn't reachable."""
    if not isinstance(name, str) or not name:
        return ""
    try:
        from ..tooling.tool_registry import list_tools
    except ImportError:
        try:
            from Core.harness.tooling.tool_registry import list_tools
        except ImportError:
            return name
    catalog = list_tools()
    entry = catalog.get(name)
    if entry is None:
        return name
    return str(entry.get("id") or name)


def _resolve_tool_alias_map(
    tools_catalog: Optional[List[Dict[str, Any]]] = None,
) -> Dict[str, str]:
    """Build a ``{name → canonical-id}`` map for the salvage parser.

    Primary source: the registry's :func:`list_tools` output, which
    already keys both canonical ids and dotted/snake aliases against
    their entry dicts.  Each value is flattened to the entry's ``id``
    string so the salvage helper can use ``alias_map.get(name)``
    directly without unwrapping.

    Secondary overlay: any per-turn ``tools_catalog`` (e.g. the
    LLM-facing catalog produced by ``list_tools_fn``) — covers tests
    with synthetic catalogs and extension surfaces that aren't in the
    registry.  Both dotted and snake-cased aliases are synthesised for
    canonical ids that contain ``_`` or ``.``.

    Returns ``{}`` when neither source is reachable so the parser
    degrades to "every recovered call is unrecognised" — that fires a
    ``tool_error`` rather than a silent dispatch.
    """
    alias_map: Dict[str, str] = {}

    _list_tools: Optional[Callable[[], Any]] = None
    try:
        from ..tooling.tool_registry import list_tools as _list_tools
    except ImportError:
        try:
            from Core.harness.tooling.tool_registry import list_tools as _list_tools
        except ImportError:
            _list_tools = None
    if _list_tools is not None:
        try:
            registry_catalog = _list_tools()
        except Exception:  # noqa: BLE001
            registry_catalog = None
        if isinstance(registry_catalog, dict):
            for name, entry in registry_catalog.items():
                if not isinstance(entry, dict):
                    continue
                canonical = str(entry.get("id") or name)
                if name and canonical:
                    alias_map[name] = canonical

    for tool in tools_catalog or []:
        if not isinstance(tool, dict):
            continue
        canonical_raw = tool.get("id") or tool.get("tool_id") or tool.get("name")
        if not isinstance(canonical_raw, str) or not canonical_raw:
            continue
        canonical = canonical_raw
        alias_map.setdefault(canonical, canonical)
        name_field = tool.get("name")
        if isinstance(name_field, str) and name_field:
            alias_map.setdefault(name_field, canonical)
        if "_" in canonical:
            alias_map.setdefault(canonical.replace("_", "."), canonical)
        if "." in canonical:
            alias_map.setdefault(canonical.replace(".", "_"), canonical)

    return alias_map


def emit_memory_written(
    state: "TurnState",
    *,
    source: str,
    scope: str,
    memory_id: str,
    body: str,
    importance: int,
    extra: Optional[Dict[str, Any]] = None,
) -> None:
    """Fire a ``memory_written`` event on ``state.tool_events``.

    Two callers:

    * The chat-turn driver, after a successful memory-write tool call
      (``source="llm_tool"``) — the LLM explicitly asked for the save.
    * The post-turn extractor, when its verdict yields a save / supersede
      against the live store (``source="auto"``).

    The event lands on the tool-activity stream so GUI surfaces watching
    ``chat.stream_tools`` see it alongside ``tool_dispatched`` /
    ``tool_result``. ``ToolActivity.svelte`` renders these distinctly so
    the user can tell auto-writes from explicit ones.

    ``body`` is truncated to 200 chars in the payload — the full body
    lives on the persisted record at ``memory_id``.
    """
    preview = body if len(body) <= 200 else body[:200].rstrip() + "…"
    payload: Dict[str, Any] = {
        "kind": "memory_write",
        "turn_id": state.turn_id,
        "source": source,
        "scope": scope,
        "memory_id": memory_id,
        "body": preview,
        "importance": int(importance),
        "timestamp_ms": int(time.time() * 1000),
    }
    if extra:
        for k, v in extra.items():
            if k not in payload:
                payload[k] = v
    _emit_tool(state, "memory_written", payload)


# ── Tool runner protocol + short-term recording ───────────────────────


class ToolRunFn(Protocol):
    def __call__(self, name: str, args: Dict[str, Any]) -> Dict[str, Any]: ...


def _record_short_term(state: TurnState, call: "ToolCall") -> None:
    """Append a tool-dispatch entry to the conversation's working memory.

    Best-effort: if the conversation store is unavailable, the turn
    keeps going. The stored entry shape mirrors the design's
    ``{"kind": "tool", "at": <ts>, "data": {...}}`` convention.
    """
    try:
        from ..memory import conversation as _conv
    except ImportError:
        try:
            from Core.harness.memory import conversation as _conv
        except ImportError:
            return
    try:
        _conv.append_working_memory(
            state.conversation_id,
            {
                "kind": "tool",
                "data": {
                    "name": call.name,
                    "args": call.args,
                    "call_id": call.id,
                    "turn_id": state.turn_id,
                },
            },
        )
    except Exception:  # noqa: BLE001
        pass


# ── Per-call dispatch ──────────────────────────────────────────────────


def _check_tier_gate(
    state: TurnState,
    call: ToolCall,
) -> Optional[Dict[str, Any]]:
    """Decide whether this turn's device tier is allowed to invoke the
    requested tool. Returns None when the call is allowed; otherwise
    a ``{error, reason}`` dict the caller folds into a tool_error
    event.

    The destructive-tool signal is the manifest's
    ``requires_confirmation`` flag (Wylde Design Principle #12 — same
    flag the user-facing confirmation gate already uses). Tools whose
    manifest is missing or unreadable get treated as non-destructive
    by default; a missing manifest already means the tool itself
    can't run, so a stricter default would be redundant.
    """
    tier = state.device_tier or _DEFAULT_TIER

    if tier == TIER_READ_ONLY:
        return {
            "reason": "tier_read_only",
            "error": (
                f"tool {call.name!r} blocked: device tier is "
                f"'read_only', no tools may run on this turn"
            ),
        }

    if tier == TIER_DESTRUCTIVE:
        # Full surface — nothing to gate.
        return None

    # tier == "tool_use" → block destructive tools.
    try:
        from ..tooling.tool_registry import list_tools
    except ImportError:
        try:
            from Core.harness.tooling.tool_registry import list_tools
        except ImportError:
            return None  # registry unreachable — let the runner decide
    catalog = list_tools()
    entry = catalog.get(call.name) if isinstance(catalog, dict) else None
    if not isinstance(entry, dict):
        return None  # unknown tool: let the runner produce its own error
    if bool(entry.get("requires_confirmation", False)):
        return {
            "reason": "tier_tool_use_blocked_destructive",
            "error": (
                f"tool {call.name!r} blocked: device tier is "
                f"'tool_use'; this tool requires "
                f"'destructive_tool_access' (manifest "
                f"requires_confirmation=true)"
            ),
        }
    return None


def _run_one_tool(
    state: TurnState,
    call: ToolCall,
    tool_run: ToolRunFn,
    messages: List[Dict[str, Any]],
) -> None:
    """Run a single tool call, emit the result/error event, append a tool
    message to the LLM history.

    The runner returns an envelope (``{ok: True, data}`` or
    ``{ok: False, error}``). We unwrap to a single ``output`` payload for
    the event stream; tests injecting a synthetic runner can pass back
    either the envelope shape or a raw dict — both are accepted.
    """
    _emit_tool(
        state,
        "tool_dispatched",
        {
            "turn_id": state.turn_id,
            "call_id": call.id,
            "name": call.name,
            "args": call.args,
        },
    )

    # Tier-gate. Runs BEFORE the tool runner so a blocked call never
    # touches the dispatcher or filesystem — we want a clean 0ms-ish
    # rejection event with a structured reason. Three rules:
    #   * read_only  →  blocks every tool. Reason: tier_read_only.
    #   * tool_use   →  blocks tools whose manifest sets
    #                   requires_confirmation: true (the existing
    #                   "destructive" signal). Reason:
    #                   tier_tool_use_blocked_destructive.
    #   * destructive_tool_access → runs everything.
    block = _check_tier_gate(state, call)
    if block is not None:
        elapsed_ms = 0
        _emit_tool(
            state,
            "tool_error",
            {
                "turn_id": state.turn_id,
                "call_id": call.id,
                "name": call.name,
                "error": block["error"],
                "reason": block["reason"],
                "duration_ms": elapsed_ms,
            },
        )
        state.tool_calls_summary.append(
            {
                "call_id": call.id,
                "name": call.name,
                "ok": False,
                "duration_ms": elapsed_ms,
                "error": block["error"],
                "reason": block["reason"],
            }
        )
        # Tell the LLM why so it can adjust its plan / explain to the
        # user instead of looping. Same shape as the regular tool-error
        # path so the chat-history append stays uniform.
        messages.append(
            {
                "role": "tool",
                "tool_call_id": call.id,
                "name": call.name,
                "content": f"[tier_blocked] {block['error']}",
            }
        )
        return

    started = time.monotonic()
    try:
        envelope = tool_run(call.name, call.args)
    except Exception as exc:  # noqa: BLE001
        elapsed_ms = int((time.monotonic() - started) * 1000)
        err = f"{type(exc).__name__}: {exc}"
        _emit_tool(
            state,
            "tool_error",
            {
                "turn_id": state.turn_id,
                "call_id": call.id,
                "name": call.name,
                "error": err,
                "duration_ms": elapsed_ms,
            },
        )
        state.tool_calls_summary.append(
            {
                "call_id": call.id,
                "name": call.name,
                "ok": False,
                "duration_ms": elapsed_ms,
                "error": err,
            }
        )
        messages.append(
            {
                "role": "tool",
                "tool_call_id": call.id,
                "name": call.name,
                "content": f"[error] {err}",
            }
        )
        return

    elapsed_ms = int((time.monotonic() - started) * 1000)
    ok, payload, err_msg = _unwrap_runner_envelope(envelope)
    if ok:
        _emit_tool(
            state,
            "tool_result",
            {
                "turn_id": state.turn_id,
                "call_id": call.id,
                "name": call.name,
                "output": payload,
                "duration_ms": elapsed_ms,
            },
        )
        # If this was a memory-write tool, fire the structured
        # memory_written event so GUI surfaces can render auto-writes
        # vs LLM-driven writes distinctly. We canonicalize the name
        # because the LLM may have called ``memory.long_term.save``
        # which the registry aliases to ``memory_long_term_save``.
        canonical_id = _canonicalise_tool_id(call.name)
        if canonical_id in _MEMORY_WRITE_TOOL_IDS and isinstance(payload, dict):
            mem_record = payload.get("memory")
            if isinstance(mem_record, dict) and mem_record.get("id"):
                if canonical_id == "memory_workspace_save":
                    scope = "workspace"
                elif canonical_id == "memory_update":
                    # Update preserves the original scope on the new
                    # record; the entrypoint copies it through args.
                    scope = str(call.args.get("scope") or "long_term")
                else:
                    scope = "long_term"
                emit_memory_written(
                    state,
                    source="llm_tool",
                    scope=scope,
                    memory_id=str(mem_record.get("id") or ""),
                    body=str(mem_record.get("body") or ""),
                    importance=int(mem_record.get("importance") or 0),
                    extra={"call_id": call.id, "tool": canonical_id},
                )
        state.tool_calls_summary.append(
            {
                "call_id": call.id,
                "name": call.name,
                "ok": True,
                "duration_ms": elapsed_ms,
            }
        )
        messages.append(
            {
                "role": "tool",
                "tool_call_id": call.id,
                "name": call.name,
                "content": _stringify(payload),
            }
        )
    else:
        _emit_tool(
            state,
            "tool_error",
            {
                "turn_id": state.turn_id,
                "call_id": call.id,
                "name": call.name,
                "error": err_msg,
                "duration_ms": elapsed_ms,
            },
        )
        state.tool_calls_summary.append(
            {
                "call_id": call.id,
                "name": call.name,
                "ok": False,
                "duration_ms": elapsed_ms,
                "error": err_msg,
            }
        )
        messages.append(
            {
                "role": "tool",
                "tool_call_id": call.id,
                "name": call.name,
                "content": f"[error] {err_msg}",
            }
        )


def _unwrap_runner_envelope(envelope: Any) -> tuple:
    """Pull (ok, payload, error_message) out of a runner envelope.

    Accepts either the runner's standard ``{ok: bool, data?, error?}``
    shape or a bare value (which is treated as a successful payload).
    """
    if isinstance(envelope, dict) and "ok" in envelope:
        ok = bool(envelope.get("ok"))
        if ok:
            return True, envelope.get("data"), ""
        err = envelope.get("error") or {}
        if isinstance(err, dict):
            msg = err.get("message") or err.get("code") or "tool failed"
        else:
            msg = str(err)
        return False, None, str(msg)
    # Bare value — pass it through as a success payload. Used by the
    # synthetic tool runner in tests.
    return True, envelope, ""


class CancelledError(Exception):
    """Raised inside the driver loop when ``state.cancel_event`` is set."""


def _check_cancelled(state: TurnState) -> None:
    if state.cancel_event.is_set():
        raise CancelledError()


def _stringify(value: Any) -> str:
    if isinstance(value, str):
        return value
    try:
        import json

        return json.dumps(value, default=str)
    except (TypeError, ValueError):
        return str(value)
