"""Response normalisation — turn raw backend output into predictable shapes.

Two responsibilities:

* :func:`normalize_tool_calls` — coerce Ollama's tool-call payloads into a
  single ``{id, function:{name, arguments(dict)}}`` shape regardless of which
  variant the model family emitted.
* :class:`ChatResult` and :class:`BackendError` — the canonical shapes any
  non-streaming caller sees. Streaming callers consume
  :class:`HarnessEvent` in :mod:`Wylde.Core.harness.backend.streaming`
  directly and don't touch these.
"""

from __future__ import annotations

import json
import uuid
from dataclasses import dataclass
from typing import Any, Dict, List, Optional


# ─── Errors and result envelopes ───────────────────────────────────────────


class BackendError(Exception):
    """Raised on any backend HTTP/JSON failure.

    Carries ``backend`` (which kind blew up) and ``status`` (HTTP code, when
    relevant) so callers can decide whether to fail open or escalate.
    """

    def __init__(self, message: str, *, backend: str = "", status: int = 0):
        super().__init__(message)
        self.backend = backend
        self.status = status


@dataclass
class ChatResult:
    """Result of a non-streaming chat completion.

    Streaming callers don't use this — they consume
    :class:`Wylde.Core.harness.backend.streaming.HarnessEvent` directly.
    """

    text: str
    prompt_tokens: int = 0
    completion_tokens: int = 0
    backend: str = ""
    model: str = ""
    raw: Optional[Dict[str, Any]] = None


# ─── Tool-call normalisation ───────────────────────────────────────────────


def normalize_tool_calls(raw_calls: List[Dict[str, Any]]) -> List[Dict[str, Any]]:
    """Flatten Ollama tool-call shape variants into a single canonical form.

    Ollama's tool-call payloads vary across model families:

    * ``arguments`` may be a JSON string or a dict.
    * ``id`` is sometimes missing — we fabricate one from the function name
      (or a uuid) so the loop has a stable correlation key.

    The result shape is::

        {"id": str, "function": {"name": str, "arguments": dict}}
    """
    out: List[Dict[str, Any]] = []
    for call in raw_calls or []:
        fn = call.get("function") or {}
        args = fn.get("arguments")
        if isinstance(args, str):
            try:
                args = json.loads(args)
            except Exception:
                args = {}
        out.append(
            {
                "id": call.get("id") or fn.get("name") or str(uuid.uuid4()),
                "function": {
                    "name": fn.get("name", ""),
                    "arguments": args or {},
                },
            }
        )
    return out


__all__ = ["BackendError", "ChatResult", "normalize_tool_calls"]
