"""tools/meta/tool_search — dynamic tool discovery.

Closes the gap the eval doc called out: agents see only the static tool
catalog provided at workflow-definition time. With this tool, a planner or
delegator stage can ask "find a tool that can do X" mid-execution and route
to it without redefining the workflow.

Backed by :mod:`Wylde.Core.harness.tooling.tool_registry`'s in-process
catalog (filesystem-as-registry: manifests under ``tooling/tools/``). The
HTTP loopback to the legacy tool-registry service was removed during the
locked-in-memory "no HTTP between Wylde components" refactor; the scoring
algorithm is preserved verbatim so results stay deterministic and
cache-friendly across the migration.
"""

from __future__ import annotations

import logging
import re
from typing import Any, Dict, List

from ....tool_registry import list_tools

logger = logging.getLogger(__name__)

_TOKEN_RE = re.compile(r"[A-Za-z][A-Za-z0-9_]+")
_STOPWORDS = {
    "the",
    "a",
    "an",
    "is",
    "are",
    "to",
    "for",
    "of",
    "in",
    "on",
    "at",
    "and",
    "or",
    "with",
    "tool",
    "find",
    "i",
    "need",
    "want",
    "that",
    "does",
    "do",
    "can",
    "use",
    "uses",
    "using",
    "this",
    "any",
}


def _score_match(tool_id: str, tool: Dict[str, Any], query: str) -> float:
    """Identical scoring to MCP bridge, keep the two in lock-step."""
    if not query:
        return 0.0
    q_tokens = {
        t.lower() for t in _TOKEN_RE.findall(query) if t.lower() not in _STOPWORDS
    }
    if not q_tokens:
        return 0.0
    name_text = " ".join(
        [
            tool_id,
            str(tool.get("name", "")),
            str(tool.get("description", "")),
            " ".join(tool.get("tags", []) or []),
        ]
    ).lower()
    name_tokens = set(_TOKEN_RE.findall(name_text))
    overlap = q_tokens & name_tokens
    if not overlap:
        sub = sum(1 for q in q_tokens if q in name_text)
        return round(0.3 * sub / max(len(q_tokens), 1), 3)
    score = len(overlap) / max(len(q_tokens), 1)
    if any(q in tool_id.lower() for q in q_tokens):
        score += 0.25
    return round(min(score, 1.0), 3)


def _fetch_catalog() -> Dict[str, Dict[str, Any]]:
    """Pull the catalog from the in-process tool_registry.

    Replaces the legacy HTTP fetch (``GET /api/tools`` against the standalone
    tool-registry service). The registry already mtime-caches its result, so
    repeat calls in a single turn are essentially free.
    """
    try:
        return list_tools()
    except Exception as exc:  # pragma: no cover — defensive; registry is pure-Python
        logger.debug("tool catalog fetch failed: %s", exc)
        return {}


def run_tool_search(params: Dict[str, Any]) -> Dict[str, Any]:
    """Find tools by natural-language description.

    params:
      query:  free-form description of what the agent needs
      limit:  max results (default 5)
      service: optional service filter
      tag:     optional tag filter
    """
    query = str(params.get("query", "")).strip()
    if not query:
        return {"error": "'query' is required"}
    try:
        limit = int(params.get("limit", 5))
    except (TypeError, ValueError):
        limit = 5
    service_filter = params.get("service")
    tag_filter = params.get("tag")

    catalog = _fetch_catalog()
    if not catalog:
        return {
            "results": [],
            "count": 0,
            "error": "no tool catalog available — is the tools tree populated?",
        }

    candidates: List[Dict[str, Any]] = []
    for tid, tool in catalog.items():
        if not isinstance(tool, dict):
            continue
        if service_filter and tool.get("service") != service_filter:
            continue
        if tag_filter and tag_filter not in (tool.get("tags") or []):
            continue
        score = _score_match(tid, tool, query)
        if score <= 0:
            continue
        candidates.append(
            {
                "tool_id": tid,
                "score": score,
                "service": tool.get("service", ""),
                "description": (tool.get("description") or "")[:300],
                "tags": tool.get("tags", []),
            }
        )
    candidates.sort(key=lambda x: x["score"], reverse=True)
    candidates = candidates[:limit]
    return {
        "query": query,
        "results": candidates,
        "count": len(candidates),
        "scanned": len(catalog),
    }
