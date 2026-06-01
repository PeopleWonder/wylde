"""rag_graph_stats — node/edge counts in the Memgraph service.

Pulled forward from ``_legacy/core/wylde-rag/tools/graph_stats.py``. The
legacy tool dispatched to ``..graph.stats()`` inside the wylde-rag service.

In the new architecture the graph lives in :mod:`Wylde.Core.harness.memory.memgraph`
(thin client over a named-pipe / HTTP fallback). This tool calls
:func:`memgraph.stats` and surfaces both the result and the reachability
status — the legacy ``enabled``/``reachable`` flags map cleanly onto the
``MemgraphReply.ok`` envelope.

Fail-soft: an unreachable backend returns ``reachable=False`` with zeros,
matching the legacy contract so a planner can branch without try/except.
"""

from __future__ import annotations

from typing import Any, Dict

from .....memory import memgraph as _memgraph


def run_rag_graph_stats(params: Dict[str, Any]) -> Dict[str, Any]:
    del params  # no input

    try:
        reply = _memgraph.stats()
    except Exception as exc:
        return {
            "enabled": True,
            "reachable": False,
            "nodes": 0,
            "edges": 0,
            "error": f"{type(exc).__name__}: {exc}",
        }

    if not reply.ok:
        err = reply.error or {}
        return {
            "enabled": True,
            "reachable": False,
            "nodes": 0,
            "edges": 0,
            "transport": reply.transport,
            "error": {
                "code": err.get("code", "unknown"),
                "message": err.get("message", ""),
            },
        }

    data = reply.data if isinstance(reply.data, dict) else {}
    # Memgraph service returns {entities, chunks, mentions} — surface those
    # plus a derived "edges" count for legacy callers.
    entities = int(data.get("entities", 0) or 0)
    chunks = int(data.get("chunks", 0) or 0)
    mentions = int(data.get("mentions", 0) or 0)

    return {
        "enabled": True,
        "reachable": True,
        "transport": reply.transport,
        "nodes": entities + chunks,
        "edges": mentions,
        "entities": entities,
        "chunks": chunks,
        "mentions": mentions,
    }
