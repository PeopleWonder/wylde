"""Streaming — normalised events emitted by the chat parser.

Distinct from the SSE *transport* used by every workflow channel. The harness
streaming layer turns Ollama's NDJSON wire format into structured Python
events; how those events get republished to a remote subscriber is the
workflow's concern.

Public surface (single Union + five dataclasses) lives flat in this module.
The legacy ``parser.accumulate`` helper is dead per the Phase 4c audit and is
not pulled forward — callers that want to drain a stream into a single result
should switch to the non-streaming router or accumulate inline.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Dict, Union


@dataclass
class AssistantToken:
    """A streamed chunk of assistant content."""

    text: str


@dataclass
class ThinkingToken:
    """A streamed chunk of model "thinking" (only with think_enabled=True)."""

    text: str


@dataclass
class ToolCallDelta:
    """One tool-call payload received from the stream.

    Ollama emits each tool-call as a complete object today (no partial-arg
    streaming), but we treat it as a delta so the callsite can accumulate.
    """

    call: Dict[str, Any] = field(default_factory=dict)


@dataclass
class Done:
    """Stream finished cleanly. ``raw`` is the final ``done`` object Ollama sent."""

    raw: Dict[str, Any] = field(default_factory=dict)


@dataclass
class StreamError:
    """Stream emitted an explicit ``{"error": ...}`` line."""

    message: str


HarnessEvent = Union[AssistantToken, ThinkingToken, ToolCallDelta, Done, StreamError]


__all__ = [
    "HarnessEvent",
    "AssistantToken",
    "ThinkingToken",
    "ToolCallDelta",
    "Done",
    "StreamError",
]
