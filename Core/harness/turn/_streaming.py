"""LLM chat backend wiring + assistant-content tool-call salvage.

Three concerns live here because they all touch the LLM wire shape:

* :func:`_default_chat_fn` — the production chat_fn (streams from
  Ollama, accumulates ``ChatStep``).
* The salvage parser (:func:`_extract_tool_calls_from_content`,
  :func:`_find_balanced_braces`, :func:`_parse_one_call`,
  :func:`_call_hash`) — recovers tool calls a model emitted as plain
  text instead of the structured ``tool_calls`` field.
* :func:`_default_tool_run` / :func:`_default_list_tools` — the
  production tool-runner / catalog shims.
"""

from __future__ import annotations

import hashlib
import json as _json
import logging
import re
from typing import Any, Callable, Dict, Iterator, List, Optional, Tuple

from ._state import ChatStep, ToolCall
from ._tool_round import ToolRunFn

logger = logging.getLogger("wylde.harness.turn")


# ── Default chat function (production path) ────────────────────────────


def _default_chat_fn(
    *,
    messages: List[Dict[str, Any]],
    tools: List[Dict[str, Any]],
    model: Optional[str],
    on_token: Optional[Callable[[str], None]] = None,
    on_thinking: Optional[Callable[[str], None]] = None,
) -> ChatStep:
    """Production LLM step — streams from the Ollama daemon.

    Pushes assistant tokens to ``on_token`` and thinking tokens (if any)
    to ``on_thinking`` as they arrive. Accumulates the full text and any
    tool calls; returns a single :class:`ChatStep` at end-of-stream.

    Tool-call extraction follows Ollama's wire shape: each
    ``ToolCallDelta`` event carries one complete ``{"function":
    {"name", "arguments"}}`` object — Ollama emits them whole, so the
    accumulation here is just a list append.
    """
    try:
        from ..backend.ollama_client import stream_chat
        from ..backend import streaming as _stream_evt
    except ImportError:
        # Bare-namespace import path (cwd = Wylde/, no parent on sys.path).
        from Core.harness.backend.ollama_client import stream_chat
        from Core.harness.backend import streaming as _stream_evt

    body: Dict[str, Any] = {
        "model": model or _select_default_model(),
        "messages": messages,
        "stream": True,
        "keep_alive": "24h",
    }
    # Pass tools through Ollama's tool-calling interface when the
    # backend supports it. Ollama happily accepts the field on tool-
    # capable models and ignores it on others — no harm either way.
    if tools:
        body["tools"] = _tools_to_wire(tools)

    text_parts: List[str] = []
    thinking_parts: List[str] = []
    tool_calls: List[ToolCall] = []

    for event in stream_chat(body):
        if isinstance(event, _stream_evt.AssistantToken):
            text_parts.append(event.text)
            if on_token is not None and event.text:
                try:
                    on_token(event.text)
                except Exception:  # noqa: BLE001
                    logger.exception("turn: on_token callback raised")
        elif isinstance(event, _stream_evt.ThinkingToken):
            thinking_parts.append(event.text)
            if on_thinking is not None and event.text:
                try:
                    on_thinking(event.text)
                except Exception:  # noqa: BLE001
                    logger.exception("turn: on_thinking callback raised")
        elif isinstance(event, _stream_evt.ToolCallDelta):
            call_dict = event.call or {}
            fn = call_dict.get("function") or {}
            name = fn.get("name") or call_dict.get("name") or ""
            args = fn.get("arguments") or call_dict.get("arguments") or {}
            if isinstance(args, str):
                import json as _json

                try:
                    args = _json.loads(args)
                except (ValueError, TypeError):
                    args = {"_raw": args}
            if not isinstance(args, dict):
                args = {"_raw": args}
            if name:
                tool_calls.append(
                    ToolCall(
                        id=call_dict.get("id") or f"call_{len(tool_calls)}",
                        name=name,
                        args=args,
                    )
                )
        elif isinstance(event, _stream_evt.StreamError):
            # Surface the upstream error as a stream-shaped exception so
            # the driver's error handler runs the standard turn_aborted
            # flow.
            raise RuntimeError(f"stream_error: {event.message}")
        elif isinstance(event, _stream_evt.Done):
            break

    return ChatStep(
        text="".join(text_parts),
        thinking="".join(thinking_parts),
        tool_calls=tool_calls,
    )


def _tools_to_wire(tools_catalog: List[Dict[str, Any]]) -> List[Dict[str, Any]]:
    """Convert harness tool registry entries to Ollama's tool-call wire
    shape. Each entry becomes ``{type: "function", function: {name,
    description, parameters}}``. Bounded to 60 tools to keep the prompt
    size sensible — the same cap the inline system prompt uses."""
    wire: List[Dict[str, Any]] = []
    for tool in tools_catalog[:60]:
        if not isinstance(tool, dict):
            continue
        name = tool.get("tool_id") or tool.get("name")
        if not name:
            continue
        params_schema = _params_to_json_schema(tool.get("parameters") or [])
        wire.append(
            {
                "type": "function",
                "function": {
                    "name": name,
                    "description": tool.get("description") or "",
                    "parameters": params_schema,
                },
            }
        )
    return wire


# ── Tool-call salvage from assistant content ──────────────────────────
#
# Architectural rule: tool calls live on ``state.tool_events``
# (chat.stream_tools); user-visible content lives on ``state.turn_events``
# (chat.stream_turn).  A model that emits its tool call as plain text in
# the assistant ``content`` field — rather than the structured
# ``tool_calls`` field — violates that split.
#
# The salvage parser below detects three common shapes the model drifts
# into, extracts them as :class:`ToolCall` objects, and scrubs them from
# the content so the chat bubble never renders raw call JSON.  Detection
# priority (highest first) is fenced JSON → tag-wrapped → bare JSON.
# Bare JSON requires a ``"name":`` substring to keep prose-shaped JSON
# from being scrubbed.


_FENCED_JSON_RE = re.compile(r"```(?:json)?\s*(\{.*?\})\s*```", re.DOTALL)
_TOOL_TAG_PATTERNS: Tuple[re.Pattern[str], ...] = (
    re.compile(r"<tool_call>\s*(.*?)\s*</tool_call>", re.DOTALL),
    re.compile(r"<function_call>\s*(.*?)\s*</function_call>", re.DOTALL),
    re.compile(r"<tool_use>\s*(.*?)\s*</tool_use>", re.DOTALL),
)


def _find_balanced_braces(text: str) -> Iterator[Tuple[int, int]]:
    """Yield ``(start, end)`` half-open spans for every top-level balanced
    ``{...}`` object in ``text``.

    Respects double-quoted strings (no brace counting inside ``"..."``)
    and backslash escapes within strings.  Skips fragments whose braces
    don't balance.  Used by the salvage parser to find candidate JSON
    objects without committing to a full json.loads first.
    """
    i, n = 0, len(text)
    while i < n:
        if text[i] != "{":
            i += 1
            continue
        depth = 0
        in_str = False
        esc = False
        j = i
        found_end = False
        while j < n:
            c = text[j]
            if in_str:
                if esc:
                    esc = False
                elif c == "\\":
                    esc = True
                elif c == '"':
                    in_str = False
            else:
                if c == '"':
                    in_str = True
                elif c == "{":
                    depth += 1
                elif c == "}":
                    depth -= 1
                    if depth == 0:
                        yield (i, j + 1)
                        i = j + 1
                        found_end = True
                        break
            j += 1
        if not found_end:
            return


def _parse_one_call(obj: Any) -> Optional[Dict[str, Any]]:
    """Coerce one parsed JSON object into ``{name, args}`` if it looks
    like a tool call, else return ``None``.

    Accepts both ``{"name": ..., "arguments": ...}`` (Ollama/Qwen) and
    ``{"name": ..., "parameters": ...}`` (Llama) and the nested
    ``{"function": {"name": ..., "arguments": ...}}`` form.  ``arguments``
    that came through as a string get re-parsed as JSON (the same
    fallback :func:`_chat_step` already runs on structured deltas).
    """
    if not isinstance(obj, dict):
        return None
    name = obj.get("name")
    if not name and isinstance(obj.get("function"), dict):
        name = obj["function"].get("name")
    if not isinstance(name, str) or not name:
        return None
    args = (
        obj.get("arguments")
        or obj.get("parameters")
        or (obj.get("function") or {}).get("arguments")
        or {}
    )
    if isinstance(args, str):
        try:
            args = _json.loads(args)
        except (ValueError, TypeError):
            args = {"_raw": args}
    if not isinstance(args, dict):
        args = {"_raw": args}
    return {"name": name, "args": args}


def _extract_tool_calls_from_content(
    text: str,
    alias_map: Optional[Dict[str, str]] = None,
) -> Tuple[str, List[Dict[str, Any]], List[Dict[str, Any]]]:
    """Find and excise tool-call shapes from assistant content.

    Returns ``(cleaned_text, recovered_calls, unrecognised_calls)``.

    * ``recovered_calls`` resolved to a known tool in ``alias_map`` —
      each entry carries ``{id, name, args, raw_name}`` where ``name``
      is the canonical id and ``raw_name`` is what the model wrote.
      Caller folds these into ``step.tool_calls`` so the standard
      dispatch loop runs them.
    * ``unrecognised_calls`` parsed cleanly but the name didn't resolve
      — caller emits a ``tool_error`` with
      ``reason="tool_call_text_unrecognised"``.  The raw JSON is still
      scrubbed from ``cleaned_text`` so the bubble doesn't render it.
    * ``cleaned_text`` has every matched span removed and trailing
      whitespace stripped.  May be empty.

    Detection priority is fenced JSON → tag-wrapped → bare JSON.  Bare
    JSON requires a ``"name":`` substring to avoid false positives on
    prose JSON like ``{"weather": "sunny"}``.
    """
    if not isinstance(text, str) or not text:
        return text or "", [], []
    alias_map = alias_map or {}
    recovered: List[Dict[str, Any]] = []
    unrecognised: List[Dict[str, Any]] = []
    seq = [0]

    def _consume(parsed: Any) -> bool:
        info = _parse_one_call(parsed)
        if info is None:
            return False
        seq[0] += 1
        call_id = f"call_text_{seq[0]}"
        canonical = alias_map.get(info["name"])
        if canonical:
            recovered.append(
                {
                    "id": call_id,
                    "name": canonical,
                    "args": info["args"],
                    "raw_name": info["name"],
                }
            )
        else:
            unrecognised.append(
                {
                    "id": call_id,
                    "name": info["name"],
                    "args": info["args"],
                }
            )
        return True

    # Pass 1 — fenced ```json ...``` blocks.  Replace with empty string
    # if the body parses as a tool call; leave intact otherwise (so a
    # user reading about JSON examples doesn't lose their fenced block).
    def _fenced_sub(m: re.Match[str]) -> str:
        body = m.group(1)
        try:
            obj = _json.loads(body)
        except (ValueError, TypeError):
            return m.group(0)
        return "" if _consume(obj) else m.group(0)

    text = _FENCED_JSON_RE.sub(_fenced_sub, text)

    # Pass 2 — explicit tool-call tags.
    def _tag_sub(m: re.Match[str]) -> str:
        body = m.group(1).strip()
        try:
            obj = _json.loads(body)
        except (ValueError, TypeError):
            return m.group(0)
        return "" if _consume(obj) else m.group(0)

    for pat in _TOOL_TAG_PATTERNS:
        text = pat.sub(_tag_sub, text)

    # Pass 3 — bare balanced-brace JSON.  We require a ``"name":``
    # substring in the span as a cheap guard against prose JSON.
    spans_to_remove: List[Tuple[int, int]] = []
    for start, end in _find_balanced_braces(text):
        span = text[start:end]
        if '"name"' not in span:
            continue
        try:
            obj = _json.loads(span)
        except (ValueError, TypeError):
            continue
        if not isinstance(obj, dict) or "name" not in obj:
            continue
        if _consume(obj):
            spans_to_remove.append((start, end))

    # Strip right-to-left so earlier offsets stay valid.
    for start, end in sorted(spans_to_remove, reverse=True):
        text = text[:start] + text[end:]

    return text.strip(), recovered, unrecognised


def _call_hash(name: str, args: Dict[str, Any]) -> str:
    """Stable per-turn dedupe key over ``(name, args)``.

    Args dict is json-canonicalised with sorted keys so two equivalent
    arg payloads hash the same regardless of dict iteration order.
    Non-serialisable values fall back to ``repr`` — the hash is still
    stable within a process, which is all the dedupe set needs.
    """
    try:
        args_canonical = _json.dumps(args, sort_keys=True, default=str)
    except (TypeError, ValueError):
        args_canonical = repr(args)
    blob = f"{name}\x00{args_canonical}".encode("utf-8")
    return hashlib.sha256(blob).hexdigest()


def _params_to_json_schema(params: List[Dict[str, Any]]) -> Dict[str, Any]:
    """Best-effort JSON-schema from the registry's parameter list."""
    properties: Dict[str, Any] = {}
    required: List[str] = []
    for p in params or []:
        if not isinstance(p, dict):
            continue
        pname = p.get("name")
        if not pname:
            continue
        properties[pname] = {
            "type": p.get("type") or "string",
            "description": p.get("description") or "",
        }
        if p.get("required"):
            required.append(pname)
    schema: Dict[str, Any] = {"type": "object", "properties": properties}
    if required:
        schema["required"] = required
    return schema


def _select_default_model() -> str:
    """Best-effort default model when the caller doesn't specify one."""
    import os

    return os.getenv("WYLDE_DEFAULT_MODEL", "qwen2.5:7b")


# ── Default tool runner (production path) ──────────────────────────────


def _default_tool_run(name: str, args: Dict[str, Any]) -> Dict[str, Any]:
    """Production tool-runner shim. Returns the runner's envelope verbatim
    (``{ok, data}`` on success, ``{ok: False, error}`` on failure)."""
    try:
        from ..tooling.tool_runner import run_tool
    except ImportError:
        from Core.harness.tooling.tool_runner import run_tool
    return run_tool(name, args)


def _default_list_tools() -> List[Dict[str, Any]]:
    """Production tool-catalog shim. ``list_tools()`` returns a dict keyed
    by tool id; the driver wants a list of entry dicts, so we values-out
    here. Extensions whose registry enable flag is off are already filtered
    by the underlying registry."""
    try:
        from ..tooling.tool_registry import list_canonical_tools
    except ImportError:
        from Core.harness.tooling.tool_registry import list_canonical_tools
    catalog = list_canonical_tools()
    if isinstance(catalog, dict):
        return list(catalog.values())
    return list(catalog)


# Re-export ToolRunFn so consumers importing from _streaming have it.
__all__ = [
    "ChatStep",
    "ToolCall",
    "ToolRunFn",
    "_call_hash",
    "_default_chat_fn",
    "_default_list_tools",
    "_default_tool_run",
    "_extract_tool_calls_from_content",
    "_find_balanced_braces",
    "_params_to_json_schema",
    "_parse_one_call",
    "_select_default_model",
    "_tools_to_wire",
]
