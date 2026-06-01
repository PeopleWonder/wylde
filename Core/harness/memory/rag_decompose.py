"""Query decomposition — split a compound question into standalone sub-queries.

Ported from ``_legacy/core/wylde-rag/hyde.py::decompose_query``. The legacy
module called a dedicated small "orchestrator" model over HTTP; the harness
port instead reuses the active chat turn's ``chat_fn`` — there is no separate
generation/orchestrator model in the new architecture (the Wylde user's directive).

The contract is deliberately small and resilient:

* :func:`decompose_query` asks the LLM to break a compound query into 1-N
  standalone sub-queries and returns them as a ``List[str]``.
* Every failure mode collapses to ``[query]`` — no ``chat_fn`` wired, the
  LLM call raised, an empty / unparseable reply. A degraded environment
  still produces a usable single-element list, mirroring the degrade-in-
  place philosophy of :mod:`retrieval`.
* The result is capped at :data:`DECOMPOSE_MAX_SUBQ` (env-overridable) so a
  chatty model can't blow up the downstream retrieval fan-out.

The pipeline orchestrator (:mod:`rag_pipeline`) calls this once per query;
each returned sub-query is retrieved independently and the candidate sets
are merged before the final rerank.
"""

from __future__ import annotations

import json
import os
import re
from typing import Any, Callable, List, Optional

from ._common import logger


def _env_int(name: str, default: int) -> int:
    """Read an int env var, falling back to ``default`` on unset / garbage."""
    raw = os.getenv(name)
    if raw is None or not raw.strip():
        return default
    try:
        return int(raw)
    except ValueError:
        return default


# Hard cap on the sub-query fan-out. Legacy default was 3; kept here so the
# retrieval cost of decomposition stays bounded regardless of model output.
DECOMPOSE_MAX_SUBQ: int = max(1, _env_int("DECOMPOSE_MAX_SUBQ", 3))


_DECOMPOSE_SYSTEM = (
    "You break a compound question into a short list of standalone "
    "sub-queries. Each sub-query must be answerable on its own, without "
    "the others for context. If the question is already simple (not "
    "compound), return it unchanged as the single list element. Reply "
    "with ONLY a JSON array of strings — no prose, no preamble, no "
    "code fences. At most {max_n} items."
)

_ARRAY_RE = re.compile(r"\[.*?\]", re.DOTALL)


def _coerce_str_list(value: Any) -> List[str]:
    """Turn a parsed JSON value into a list of non-empty strings, or []."""
    if isinstance(value, list):
        return [str(x).strip() for x in value if str(x).strip()]
    if isinstance(value, dict):
        # Some models wrap the array under a key ("queries", "subqueries", …).
        for key in ("queries", "subqueries", "sub_queries", "items", "results"):
            inner = value.get(key)
            if isinstance(inner, list):
                return [str(x).strip() for x in inner if str(x).strip()]
    return []


def _parse_query_list(text: str) -> List[str]:
    """Best-effort extraction of a string list from a model reply.

    Tries, in order: direct JSON parse, first ``[...]`` block, then a
    bullet/numbered-line fallback. Models wrap their output in prose or
    fences often enough that the raw ``json.loads`` alone is not reliable.
    """
    if not text:
        return []

    try:
        parsed = _coerce_str_list(json.loads(text))
        if parsed:
            return parsed
    except ValueError:
        pass

    match = _ARRAY_RE.search(text)
    if match:
        try:
            parsed = _coerce_str_list(json.loads(match.group(0)))
            if parsed:
                return parsed
        except ValueError:
            pass

    # Last resort: treat each non-structural line as a sub-query, stripping
    # bullet / numbering prefixes.
    out: List[str] = []
    for line in text.splitlines():
        stripped = line.strip()
        if not stripped or stripped[0] in "{}[]":
            continue
        out.append(stripped.lstrip("-*0123456789. \t").strip())
    return [s for s in out if s]


def decompose_query(
    query: str,
    *,
    chat_fn: Optional[Callable[..., Any]] = None,
    max_subq: Optional[int] = None,
) -> List[str]:
    """Split ``query`` into 1-N standalone sub-queries.

    ``chat_fn`` matches the harness driver's ``ChatFn`` shape — keyword
    ``messages`` / ``tools`` / ``model`` in, an object with a ``.text``
    attribute out. When it is ``None`` or the call fails, the original
    query is returned as a single-element list (degraded but functional).

    ``max_subq`` overrides :data:`DECOMPOSE_MAX_SUBQ` for this call.
    """
    q = (query or "").strip()
    if not q:
        return []
    cap = DECOMPOSE_MAX_SUBQ if max_subq is None else max(1, int(max_subq))

    if chat_fn is None:
        return [q]

    messages = [
        {"role": "system", "content": _DECOMPOSE_SYSTEM.format(max_n=cap)},
        {"role": "user", "content": q},
    ]
    try:
        step = chat_fn(messages=messages, tools=[], model=None)
    except Exception as exc:  # noqa: BLE001
        logger.debug("rag_decompose: chat_fn failed (%s); single sub-query", exc)
        return [q]

    text = getattr(step, "text", None)
    if not isinstance(text, str) or not text.strip():
        return [q]

    subs = _parse_query_list(text)

    # Dedup case-insensitively and apply the cap. Keep first occurrence so
    # the most prominent sub-query (models tend to list it first) survives.
    seen: set[str] = set()
    deduped: List[str] = []
    for s in subs:
        s = s.strip()
        if not s:
            continue
        key = s.lower()
        if key in seen:
            continue
        seen.add(key)
        deduped.append(s)
        if len(deduped) >= cap:
            break

    return deduped or [q]


__all__ = ["DECOMPOSE_MAX_SUBQ", "decompose_query"]
