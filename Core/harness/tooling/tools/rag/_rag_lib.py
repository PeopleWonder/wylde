"""Shared helpers for tools/rag/.

The legacy ``wylde-rag`` tools all returned a ``ToolResult`` envelope
(``status``, ``data``, ``error``, etc.) because the runner expected it. The
Phase 6 contract is "tools return plain dicts; the runner wraps the
envelope" — so we don't need any of that here. What we DO need is a
consistent way to surface errors from the in-process memory modules.

Public helpers:

* :func:`error`  — uniform error response so the runner doesn't have to
  special-case raises that originate inside the memory layer.
"""

from __future__ import annotations

from typing import Any, Dict


def error(code: str, message: str, **details: Any) -> Dict[str, Any]:
    """Uniform error shape for memory-layer failures surfaced to the LLM."""
    out: Dict[str, Any] = {
        "status": "error",
        "error": {"code": code, "message": message},
    }
    if details:
        out["error"]["details"] = details
    return out
