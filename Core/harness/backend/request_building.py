"""request_building — assemble the request body for a chat backend.

One flat module covering four organisational concerns:

* **Images** — strip dataURL prefixes so Ollama receives raw base64.
* **Messages** — combine system/tools/memory with the conversation history
  into the wire ``messages[]`` array.
* **Options** — Ollama options dict, default settings, ``keep_alive``
  resolution.
* **System prompt** — resolve the active prompt override, append the date
  stamp, fold in the tool catalog and memory blocks for the turn.

The only inter-section private state is the messages section's call into
the images section, which is an in-file reference. No name collisions
across sections.
"""

from __future__ import annotations

import datetime as _dt
import logging
import os
from typing import Any, Dict, List, Optional

import requests

from ..memory.rag import build_memory_block, latest_user_text

logger = logging.getLogger(__name__)


# ───────────────────────────────────────────────────────────────────────────
# Images — Ollama wants raw base64, not data URLs
# ───────────────────────────────────────────────────────────────────────────


def strip_data_url_prefix(image: str) -> str:
    """Return the base64 payload of a ``data:image/...;base64,...`` URL.

    Pass-through for raw base64 strings.
    """
    if not image:
        return image
    return image.split(",", 1)[1] if "," in image else image


def strip_all(images: List[str]) -> List[str]:
    return [strip_data_url_prefix(i) for i in (images or [])]


# ───────────────────────────────────────────────────────────────────────────
# Messages — Ollama wire shape for messages[]
# ───────────────────────────────────────────────────────────────────────────


def format_history_message(msg: Dict[str, Any]) -> Dict[str, Any]:
    """Coerce one conversation entry to Ollama's wire shape."""
    out: Dict[str, Any] = {
        "role": msg.get("role", "user"),
        "content": msg.get("content") or "",
    }
    if msg.get("tool_call_id"):
        out["tool_call_id"] = msg["tool_call_id"]
    if msg.get("tool_calls"):
        out["tool_calls"] = msg["tool_calls"]
    if msg.get("images"):
        out["images"] = strip_all(list(msg["images"]))
    return out


def _has_payload(msg: Dict[str, Any]) -> bool:
    """Drop UI placeholders (empty assistant slots) but keep tool / image / tool_calls rows."""
    if msg.get("content"):
        return True
    if msg.get("role") == "tool":
        return True
    if msg.get("tool_calls"):
        return True
    if msg.get("images"):
        return True
    return False


def build_chat_messages(
    *,
    system_prompt: str,
    history: List[Dict[str, Any]],
    tool_catalog_text: Optional[str] = None,
    memory_block: Optional[str] = None,
) -> List[Dict[str, Any]]:
    """Build the final ``messages[]`` array for ``/api/chat``.

    The system prompt, tool catalog (if present), and memory block (if present)
    are concatenated into a single ``role=system`` entry — many small models
    don't reliably honour multiple system rows.
    """
    parts: List[str] = []
    if system_prompt:
        parts.append(system_prompt)
    if tool_catalog_text:
        parts.append(tool_catalog_text)
    if memory_block:
        parts.append(memory_block)
    full_system = "\n\n".join(p for p in parts if p)

    convo = [format_history_message(m) for m in history if _has_payload(m)]
    return [{"role": "system", "content": full_system}, *convo]


# ───────────────────────────────────────────────────────────────────────────
# Options — Ollama options dict and keep_alive handling
# ───────────────────────────────────────────────────────────────────────────

# Keys that map 1:1 onto Ollama's ``options`` object. ``keep_alive`` is sent at
# the top level of the request body, not inside options.
OLLAMA_OPTION_KEYS = (
    "num_ctx",
    "temperature",
    "top_p",
    "top_k",
    "repeat_penalty",
    "num_predict",
    "seed",
    "min_p",
)

DEFAULT_OLLAMA_SETTINGS: Dict[str, Any] = {
    "num_ctx": 4096,
    "temperature": 0.8,
    "top_p": 0.9,
    "top_k": 40,
    "repeat_penalty": 1.1,
    "num_predict": -1,  # -1 = no limit
    "min_p": 0.0,
    "seed": None,  # None = random
    "keep_alive": "-1",  # -1 = stay loaded forever
}


def build_ollama_options(settings: Optional[Dict[str, Any]]) -> Dict[str, Any]:
    """Return an ``options`` dict suitable for Ollama's chat body.

    Drops null/empty/NaN keys so they don't override Ollama's defaults with
    garbage.
    """
    if not settings:
        return {}
    out: Dict[str, Any] = {}
    for k in OLLAMA_OPTION_KEYS:
        v = settings.get(k)
        if v is None or v == "":
            continue
        if isinstance(v, float) and v != v:  # NaN check
            continue
        out[k] = v
    return out


def resolve_keep_alive(settings: Optional[Dict[str, Any]]) -> Any:
    """Pick a usable keep_alive value.

    Ollama accepts a string like ``"5m"`` or a number of seconds; ``-1`` means
    stay loaded forever, ``0`` unloads immediately. Strings of pure digits
    (``"-1"``, ``"0"``, ``"60"``) round-trip as numbers; anything else is a
    duration string.
    """
    v = (settings or {}).get("keep_alive")
    if v is None or v == "":
        return -1
    if isinstance(v, (int, float)):
        return v
    s = str(v)
    if s.lstrip("-").isdigit():
        return int(s)
    return s


def normalise_settings(patch: Optional[Dict[str, Any]]) -> Dict[str, Any]:
    """Merge ``patch`` over the defaults — used by harness.settings.set action."""
    merged = dict(DEFAULT_OLLAMA_SETTINGS)
    if patch:
        for k, v in patch.items():
            merged[k] = v
    return merged


# ───────────────────────────────────────────────────────────────────────────
# System prompt — resolve override, append date, fold in tools + memory
# ───────────────────────────────────────────────────────────────────────────
#
# The frontend posts the conversation history (user/assistant/tool messages
# only) plus a ``system_prompt_id``; this section composes the final system
# message from:
#
#   1. The catalog default for ``system_prompt_id`` (or the user override
#      stored in ``data/system_prompts.json``).
#   2. The wylde-rag memory block (core context + relevant past notes for  # wylde-check: dead-ref-ok
#      the latest user message).
#   3. The tool catalog text from tool-registry.
#   4. Today's date stamp so the model has fresh temporal context.
#
# The override store and catalog metadata live in
# :mod:`shared.system_prompts` / :mod:`shared.system_prompts_catalog`. CRUD
# goes through the ``harness.prompts.*`` pipe actions in :mod:`harness_api`.

_TOOL_REGISTRY_FALLBACK = os.getenv(
    "TOOL_REGISTRY_URL", "http://127.0.0.1:8011"
).rstrip("/")


def _shared_system_prompts() -> Any:
    """Lazy import — ``Core.shared`` may not yet be on ``sys.path`` at
    module-load time (depending on how the harness was bootstrapped)."""
    try:
        import system_prompts as _sp

        return _sp
    except ImportError:
        try:
            from shared import system_prompts as _sp

            return _sp
        except ImportError:
            return None


def _shared_catalog() -> Any:
    try:
        import system_prompts_catalog as _cat

        return _cat
    except ImportError:
        try:
            from shared import system_prompts_catalog as _cat

            return _cat
        except ImportError:
            return None


def effective_prompt(prompt_id: str) -> str:
    """Return the override (or catalog default) for ``prompt_id``."""
    sp = _shared_system_prompts()
    if sp is not None:
        try:
            return sp.effective_prompt(prompt_id) or ""
        except Exception as exc:
            logger.debug("system_prompts.effective_prompt failed: %s", exc)
    cat = _shared_catalog()
    if cat is not None:
        try:
            return cat.default_for(prompt_id) or ""
        except Exception:
            pass
    return ""


def resolve_system_prompt(prompt_id: str, base_default: str = "") -> str:
    """Return the override (or fallback default) plus today's date stamp.

    Back-compat: graph/loader.py callers expect this signature.
    """
    text = effective_prompt(prompt_id) or base_default
    today = _dt.date.today().isoformat()
    if text:
        return f"{text}\n\nDate: {today}."
    return f"Date: {today}."


def _fetch_tool_catalog_text(
    *,
    wire_tools: Optional[List[Dict[str, Any]]] = None,
    app_schemas: Optional[List[Dict[str, Any]]] = None,
    exclude_auto_managed: bool = True,
) -> str:
    """Return the human-readable in-prompt catalog text.

    If ``wire_tools`` is provided we render directly from it (avoiding a
    network hop when the loop already has the catalog from the same call
    that produced the wire-format tools). Otherwise we ask tool-registry
    via HTTP — best-effort, returns ``""`` on any failure.

    The legacy ``from harness import _consul_url`` lookup is gone; per the
    Phase 4c audit Consul registration is removed and the env-driven fallback
    is the only source of truth.
    """
    if wire_tools:
        return _render_prompt_catalog(wire_tools)

    base = _TOOL_REGISTRY_FALLBACK
    try:
        params: Dict[str, str] = {}
        if exclude_auto_managed:
            params["exclude_auto_managed"] = "1"
        body = {"app_schemas": list(app_schemas or [])}
        resp = requests.post(
            f"{base.rstrip('/')}/api/tools/catalog",
            params=params,
            json=body,
            timeout=4,
        )
        if not resp.ok:
            return ""
        data = resp.json()
        text = data.get("prompt_catalog")
        return str(text or "")
    except Exception as exc:
        logger.debug("tool_registry catalog fetch failed: %s", exc)
        return ""


def _render_prompt_catalog(wire_tools: List[Dict[str, Any]]) -> str:
    """Mirror tool-registry's _build_prompt_catalog so we can render from
    the wire tools the loop already received without a network round trip."""
    if not wire_tools:
        return ""
    lines: List[str] = []
    for t in wire_tools:
        fn = t.get("function") or {}
        params_obj = fn.get("parameters") or {}
        props = params_obj.get("properties") or {}
        required = set(params_obj.get("required") or [])
        param_str = ""
        if props:
            param_str = " | params: " + ", ".join(
                f"{k}{'' if k in required else '?'}: {(v or {}).get('type', 'any')}"
                for k, v in props.items()
            )
        desc = fn.get("description") or "(no description)"
        lines.append(f"- {fn.get('name', '?')}: {desc}{param_str}")
    header = (
        f"--- Available tools ({len(lines)}) ---\n"
        "You invoke tools ONLY through the function-calling mechanism (tool_calls). "
        "NEVER write tool names, brackets, or tool syntax in your text response — "
        "that does nothing. If you want to use a tool, emit a proper function call; "
        "if you want to talk about a tool, just mention it by name in prose.\n"
        "Context retrieval and memory are handled automatically by the system before "
        "each turn — you only need to use the tools listed below."
    )
    return f"{header}\n" + "\n".join(lines)


def assemble_system_prompt(
    prompt_id: str,
    *,
    history: Optional[List[Dict[str, Any]]] = None,
    user_query: str = "",
    wire_tools: Optional[List[Dict[str, Any]]] = None,
    app_schemas: Optional[List[Dict[str, Any]]] = None,
    include_memory: bool = True,
    include_tool_catalog: bool = True,
) -> str:
    """Compose the full system message for a turn.

    Order matches what the legacy InferenceBar produced::

        <prompt with date stamp>

        <tool catalog block>

        <memory block>

    Empty sections are omitted entirely so a model sees no headers without
    bodies.
    """
    parts: List[str] = []

    base = resolve_system_prompt(prompt_id)
    if base.strip():
        parts.append(base)

    if include_tool_catalog:
        catalog_text = _fetch_tool_catalog_text(
            wire_tools=wire_tools, app_schemas=app_schemas
        )
        if catalog_text:
            parts.append(catalog_text)

    if include_memory:
        anchor = (user_query or "").strip()
        if not anchor and history:
            anchor = latest_user_text(history)
        try:
            mem = build_memory_block(anchor)
        except Exception as exc:
            logger.debug("memory injection failed: %s", exc)
            mem = ""
        if mem:
            parts.append(mem)

    return "\n\n".join(parts)


__all__ = [
    # images
    "strip_data_url_prefix",
    "strip_all",
    # messages
    "build_chat_messages",
    "format_history_message",
    # options
    "OLLAMA_OPTION_KEYS",
    "DEFAULT_OLLAMA_SETTINGS",
    "build_ollama_options",
    "resolve_keep_alive",
    "normalise_settings",
    # system prompt
    "effective_prompt",
    "resolve_system_prompt",
    "assemble_system_prompt",
]
