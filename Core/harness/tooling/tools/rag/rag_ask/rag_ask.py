"""rag_ask — semantic Q&A retrieval over a workspace index, with chunk citations.

Pulled forward from ``_legacy/core/wylde-rag/tools/ask.py``. The legacy tool
ran a HyDE → hybrid retrieval → cross-encoder rerank → forced-citation
*generation* pipeline inside the ``wylde-rag`` Flask service.

This tool now drives the in-process port of that pipeline,
:func:`Wylde.Core.harness.memory.rag_pipeline.ask` — query decomposition,
HyDE-expanded hybrid search (vector + BM25 + graph) with multi-hop
follow-ups, cross-encoder rerank, score gating, and a semantic result cache.

It deliberately stops short of generation. Per the harness design there is
no separate generation model: the tool returns ranked candidate chunks, each
stamped with an ``[N]`` citation label plus a ready-to-inject
``citation_block``. The chat-turn LLM — already loaded for this turn — does
the generation and cites the ``[N]`` labels in its reply.

The internal LLM helpers (HyDE, decomposition, multi-hop follow-up
synthesis) reuse that same chat model: the tool surfaces the harness's
production ``chat_fn`` to the pipeline so no extra model is loaded. When the
chat backend is unreachable every helper degrades to the query as-is and the
pipeline still returns ranked hits.

``status="insufficient_context"`` is returned when retrieval finds nothing
or the confidence gate fires, so a planner can widen retrieval or fall back
to a non-memory tool.
"""

from __future__ import annotations

from typing import Any, Callable, Dict, Optional

from .....memory import rag_pipeline as _rag_pipeline


def _resolve_workspace_id(params: Dict[str, Any]) -> str:
    """Pick the workspace: explicit param, else the active turn's workspace,
    else ``"default"``."""
    explicit = str(params.get("workspace") or "").strip()
    if explicit:
        return explicit
    try:
        from ....._tool_context import current_tool_context

        ctx = current_tool_context()
        if ctx is not None and ctx.workspace_id:
            return str(ctx.workspace_id)
    except Exception:  # noqa: BLE001
        pass
    return "default"


def _resolve_chat_fn() -> Optional[Callable[..., Any]]:
    """Return the harness's production chat step so the pipeline's LLM
    helpers (HyDE / decompose / multi-hop) reuse the loaded chat model.

    The tool runner surfaces per-turn context through the ``turn`` package's
    thread-local :class:`ToolContext`, but that context carries no callable;
    the production ``chat_fn`` (``turn._streaming._default_chat_fn``) is a
    stateless Ollama-streaming step, so importing and passing it directly is
    equivalent to "the model currently loaded for this turn". Returns
    ``None`` when the turn package can't be imported — the pipeline then
    runs degraded (no HyDE / decompose / multi-hop)."""
    try:
        from .....turn._streaming import _default_chat_fn

        return _default_chat_fn
    except Exception:  # noqa: BLE001
        return None


def run_rag_ask(params: Dict[str, Any]) -> Dict[str, Any]:
    q = str(params.get("q") or "").strip()
    if not q:
        return {
            "status": "error",
            "error": "'q' parameter required and must be non-empty",
        }

    try:
        limit = max(1, min(50, int(params.get("limit", 8))))
    except (TypeError, ValueError):
        limit = 8

    workspace_id = _resolve_workspace_id(params)
    chat_fn = _resolve_chat_fn()

    try:
        return _rag_pipeline.ask(
            q,
            workspace_id=workspace_id,
            chat_fn=chat_fn,
            limit=limit,
        )
    except Exception as exc:  # noqa: BLE001
        return {"status": "error", "error": f"{type(exc).__name__}: {exc}"}
