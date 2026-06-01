"""Query entity extraction — "soft addressing" seeds for graph expansion.

A lightweight take on the SAGE "soft addressing" idea: rather than letting
graph traversal start only from whatever chunks the vector layer happened to
surface, we extract the *named entities the query is about* and hand them to
the graph-expansion stage as explicit seeds. The graph walk then starts from
the query's subject matter directly — closer to how a human would "address"
the relevant region of a knowledge graph.

Two extraction paths, consistent with the HyDE / decompose pattern used
elsewhere in the pipeline:

* **LLM NER** — when a ``chat_fn`` is available, ask the model to list the
  named entities (people, places, projects, technical terms, file paths) in
  the query, one per line. Cheap, and far better at multi-word terms and
  domain jargon than any regex.
* **Regex fallback** — when ``chat_fn`` is missing (or the LLM call fails /
  returns nothing), fall back to a deterministic extractor: capitalised
  words, quoted spans, and file-path-like tokens. Coarse, but enough to give
  the graph stage *some* seeds in a degraded environment.

The result is capped at :data:`MAX_QUERY_ENTITIES` (env-overridable). An
empty result is fine — the graph-expansion stage treats it as "no extra
seeds" and behaves exactly as before.
"""

from __future__ import annotations

import os
import re
from typing import Any, Callable, List, Optional

from ._common import logger


def _env_int(name: str, default: int) -> int:
    raw = os.getenv(name)
    if raw is None or not raw.strip():
        return default
    try:
        return int(raw)
    except ValueError:
        return default


# Cap on entities returned — keeps the graph traverse seed set bounded.
MAX_QUERY_ENTITIES: int = max(1, _env_int("WYLDE_RAG_MAX_ENTITIES", 8))


_NER_SYSTEM = (
    "You extract named entities from a search query. List the named "
    "entities — people, places, projects, technical terms, identifiers, "
    "file paths — that appear in the query below, one per line. Output "
    "nothing else: no numbering, no prose, no commentary. If the query "
    "contains no clear named entities, output nothing at all."
)

# Regex-fallback patterns.
_QUOTED_RE = re.compile(r"[\"'`]([^\"'`]{2,})[\"'`]")
_PATH_RE = re.compile(
    r"[A-Za-z0-9_.\-]+(?:[/\\][A-Za-z0-9_.\-]+)+|[A-Za-z0-9_\-]+\.[A-Za-z]{1,5}\b"
)
_CAPWORD_RE = re.compile(r"\b[A-Z][A-Za-z0-9_]+(?:\s+[A-Z][A-Za-z0-9_]+)*")


def _dedup_cap(values: List[str], cap: int) -> List[str]:
    """Trim, drop empties, dedup case-insensitively, and apply the cap."""
    seen: set[str] = set()
    out: List[str] = []
    for v in values:
        v = v.strip().strip("\"'`.,;:").strip()
        if not v:
            continue
        key = v.lower()
        if key in seen:
            continue
        seen.add(key)
        out.append(v)
        if len(out) >= cap:
            break
    return out


def _regex_entities(query: str, cap: int) -> List[str]:
    """Deterministic fallback extractor — quoted spans, file paths, and
    capitalised words / phrases. Used when no ``chat_fn`` is available."""
    found: List[str] = []
    found.extend(m.group(1) for m in _QUOTED_RE.finditer(query))
    found.extend(m.group(0) for m in _PATH_RE.finditer(query))
    found.extend(m.group(0) for m in _CAPWORD_RE.finditer(query))
    return _dedup_cap(found, cap)


def _parse_ner_lines(text: str, cap: int) -> List[str]:
    """Parse a one-entity-per-line LLM reply into a clean entity list."""
    lines: List[str] = []
    for line in text.splitlines():
        cleaned = line.strip().lstrip("-*0123456789.) \t").strip()
        # Drop obvious non-entity prose (a long sentence is not an entity).
        if cleaned and len(cleaned) <= 80:
            lines.append(cleaned)
    return _dedup_cap(lines, cap)


def extract_entities(
    query: str,
    *,
    chat_fn: Optional[Callable[..., Any]] = None,
    max_entities: Optional[int] = None,
) -> List[str]:
    """Extract up to N named entities from ``query``.

    Uses LLM NER via ``chat_fn`` when available; falls back to the regex
    extractor when ``chat_fn`` is ``None`` or the LLM call fails / yields
    nothing usable. Always returns a list (possibly empty) — never raises.

    ``max_entities`` overrides :data:`MAX_QUERY_ENTITIES` for this call.
    """
    q = (query or "").strip()
    if not q:
        return []
    cap = MAX_QUERY_ENTITIES if max_entities is None else max(1, int(max_entities))

    if chat_fn is None:
        return _regex_entities(q, cap)

    messages = [
        {"role": "system", "content": _NER_SYSTEM},
        {"role": "user", "content": q},
    ]
    try:
        step = chat_fn(messages=messages, tools=[], model=None)
    except Exception as exc:  # noqa: BLE001
        logger.debug("rag_entities: chat_fn failed (%s); regex fallback", exc)
        return _regex_entities(q, cap)

    text = getattr(step, "text", None)
    if not isinstance(text, str) or not text.strip():
        return _regex_entities(q, cap)

    entities = _parse_ner_lines(text, cap)
    # An LLM that legitimately found nothing returns an empty list — but a
    # blank/garbled reply should still leave the graph stage some seeds.
    return entities or _regex_entities(q, cap)


__all__ = ["MAX_QUERY_ENTITIES", "extract_entities"]
