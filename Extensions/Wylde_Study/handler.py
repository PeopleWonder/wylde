"""Wylde_Study handler — Wylde-side request router for the browser extension.

Each ``run_*`` function (or, in this extension's manifest, the
``index_page`` / ``query`` / ``summarize`` / ``explain`` / ``flashcards``
endpoints) receives a JSON dict from the extension_bridge dispatcher
and returns a JSON dict result. The bridge wraps any raised exception
in :class:`DispatchError`; this module only needs to validate input
and route to the right harness module.

Routing
-------
* index_page → ``Wylde.Core.harness.memory.rag.add_episodic`` (writes a
  memory row tagged with the source URL).
* query      → ``Wylde.Core.harness.memory.rag.search``.
* summarize / explain / flashcards →
  ``Wylde.Core.harness.backend.backend_routing.default_router().chat``
  with a focused prompt template per task. The ``model`` parameter
  defaults to the value of ``WYLDE_DEFAULT_MODEL`` env var, or
  ``"llama3"`` if unset.

Egress
------
The browser-side requests come *into* the harness via the Gateway —
the handler itself is purely in-process and either reads/writes the
memory graph (no egress) or calls the LLM router (which routes
remote-backend traffic via ``Core.shared.egress_client.forward`` already).
No code in this file does HTTP-out directly.
"""

from __future__ import annotations

import json
import logging
import os
import re
from typing import Any, Dict, List, Optional

logger = logging.getLogger("wylde.extensions.wylde_study.handler")


# ── Lazy harness imports ────────────────────────────────────────────────────
#
# Imported lazily so a smoke-test environment without LanceDB / ollama
# installed can still load the handler module to inspect its surface.


def _rag() -> Any:
    from Core.harness.memory import rag

    return rag


def _router() -> Any:
    from Core.harness.backend.backend_routing import default_router

    return default_router()


def _default_model() -> str:
    return os.getenv("WYLDE_DEFAULT_MODEL", "llama3")


# ── Helpers ─────────────────────────────────────────────────────────────────


def _err(code: str, message: str, **extra: Any) -> Dict[str, Any]:
    out: Dict[str, Any] = {"status": "error", "code": code, "error": message}
    out.update(extra)
    return out


def _ok(**fields: Any) -> Dict[str, Any]:
    out: Dict[str, Any] = {"status": "ok"}
    out.update(fields)
    return out


def _llm_call(
    *,
    system: str,
    user: str,
    model: Optional[str],
    fmt: Optional[str] = None,
    temperature: float = 0.4,
    timeout: int = 120,
) -> Dict[str, Any]:
    """Single chat round-trip via the harness inference router.

    Returns ``{ok: bool, text, prompt_tokens, completion_tokens, model}``.
    Catches BackendError so a missing Ollama daemon doesn't crash the
    handler — surfaces the error in the dict instead.
    """
    name = (model or _default_model()).strip()
    messages = [
        {"role": "system", "content": system},
        {"role": "user", "content": user},
    ]
    try:
        result = _router().chat(
            messages, name, fmt=fmt, temperature=temperature, timeout=timeout
        )
    except Exception as exc:
        return {
            "ok": False,
            "error": f"{type(exc).__name__}: {exc}",
            "model": name,
        }
    return {
        "ok": True,
        "text": result.text,
        "prompt_tokens": result.prompt_tokens,
        "completion_tokens": result.completion_tokens,
        "model": result.model,
        "backend": result.backend,
    }


# ── Endpoints ───────────────────────────────────────────────────────────────


def index_page(params: Dict[str, Any]) -> Dict[str, Any]:
    """Index a browser page into the Wylde episodic memory tier.

    The page text is treated as one episodic chunk; chunking +
    entity extraction + graph upsert happen later on consolidation,
    same flow as any other ingest source. The URL is stored in
    ``source_path`` so a later RAG hit can be traced back to the
    page it came from.
    """
    url = str(params.get("url") or "").strip()
    text = str(params.get("text") or "").strip()
    title = str(params.get("title") or "").strip()
    session_id = str(params.get("session_id") or "").strip()
    if not url:
        return _err("INVALID_PARAMS", "'url' is required")
    if not text:
        return _err("INVALID_PARAMS", "'text' is required")

    # Prepend title for retrieval surface — chunks that match the title
    # phrasing should rank higher when the user asks about the page.
    body = f"{title}\n\n{text}" if title else text

    try:
        row_id = _rag().add_episodic(body, source_path=url, session_id=session_id)
    except Exception as exc:
        return _err("INGEST_ERROR", f"{type(exc).__name__}: {exc}", url=url)

    return _ok(
        url=url,
        title=title,
        memory_id=row_id,
        chars=len(body),
    )


def query(params: Dict[str, Any]) -> Dict[str, Any]:
    """Answer a question against the indexed corpus (RAG hits)."""
    q = str(params.get("q") or "").strip()
    if not q:
        return _err("INVALID_PARAMS", "'q' is required and must be non-empty")
    try:
        limit = max(1, min(50, int(params.get("limit", 8))))
    except (TypeError, ValueError):
        limit = 8
    try:
        hits = _rag().search(q, limit=limit)
    except Exception as exc:
        return _err("SEARCH_ERROR", f"{type(exc).__name__}: {exc}")
    if not hits:
        return _ok(
            query=q,
            hits=[],
            count=0,
            note="no matches in the indexed corpus; index more pages first",
            insufficient_context=True,
        )
    return _ok(query=q, hits=hits, count=len(hits))


_SUMMARIZE_SYS = (
    "You are a concise study assistant. Summarise the user's text in plain "
    "language. Keep the summary tight; produce a JSON object of the form "
    '{"summary": "...", "key_points": ["...", "..."]} and nothing else.'
)


def summarize(params: Dict[str, Any]) -> Dict[str, Any]:
    text = str(params.get("text") or "").strip()
    if not text:
        return _err("INVALID_PARAMS", "'text' is required")
    try:
        max_words = max(20, min(800, int(params.get("max_words", 150))))
    except (TypeError, ValueError):
        max_words = 150
    user = (
        f"Summarise the following in at most {max_words} words. Reply in "
        f"JSON with keys 'summary' and 'key_points'.\n\n{text}"
    )
    res = _llm_call(
        system=_SUMMARIZE_SYS,
        user=user,
        model=params.get("model"),
        fmt="json",
    )
    if not res.get("ok"):
        return _err("LLM_ERROR", str(res.get("error")), model=res.get("model"))
    parsed = _try_parse_json(res["text"])
    return _ok(
        summary=parsed.get("summary") if isinstance(parsed, dict) else None,
        key_points=parsed.get("key_points") if isinstance(parsed, dict) else None,
        raw=res["text"],
        model=res["model"],
        backend=res["backend"],
    )


_EXPLAIN_SYS = (
    "You are a study tutor. Explain the user's concept or excerpt in plain "
    "language for the requested audience. Be precise but accessible. Reply "
    'in JSON: {"explanation": "...", "analogy": "..."}.'
)


def explain(params: Dict[str, Any]) -> Dict[str, Any]:
    text = str(params.get("text") or "").strip()
    if not text:
        return _err("INVALID_PARAMS", "'text' is required")
    audience = str(params.get("audience") or "general").strip()
    user = (
        f"Audience: {audience}\nExplain the following:\n\n{text}\n\n"
        "Reply in JSON with keys 'explanation' and 'analogy'."
    )
    res = _llm_call(
        system=_EXPLAIN_SYS,
        user=user,
        model=params.get("model"),
        fmt="json",
    )
    if not res.get("ok"):
        return _err("LLM_ERROR", str(res.get("error")), model=res.get("model"))
    parsed = _try_parse_json(res["text"])
    return _ok(
        explanation=parsed.get("explanation") if isinstance(parsed, dict) else None,
        analogy=parsed.get("analogy") if isinstance(parsed, dict) else None,
        raw=res["text"],
        model=res["model"],
        backend=res["backend"],
    )


_FLASHCARDS_SYS = (
    "You generate study flashcards. Each card is a JSON object with "
    "'front' (a question) and 'back' (a concise answer). Reply in JSON: "
    '{"cards": [{"front": "...", "back": "..."}, ...]}.'
)


def flashcards(params: Dict[str, Any]) -> Dict[str, Any]:
    text = str(params.get("text") or "").strip()
    if not text:
        return _err("INVALID_PARAMS", "'text' is required")
    try:
        count = max(1, min(50, int(params.get("count", 8))))
    except (TypeError, ValueError):
        count = 8
    user = (
        f"Generate {count} study flashcards from the following text. "
        f"Reply in JSON.\n\n{text}"
    )
    res = _llm_call(
        system=_FLASHCARDS_SYS,
        user=user,
        model=params.get("model"),
        fmt="json",
    )
    if not res.get("ok"):
        return _err("LLM_ERROR", str(res.get("error")), model=res.get("model"))
    parsed = _try_parse_json(res["text"])
    cards: List[Dict[str, Any]] = []
    if isinstance(parsed, dict) and isinstance(parsed.get("cards"), list):
        for c in parsed["cards"]:
            if isinstance(c, dict) and c.get("front") and c.get("back"):
                cards.append({"front": str(c["front"]), "back": str(c["back"])})
    return _ok(
        cards=cards,
        count=len(cards),
        raw=res["text"],
        model=res["model"],
        backend=res["backend"],
    )


# ── Tolerant JSON parsing ───────────────────────────────────────────────────


_JSON_BLOCK_RE = re.compile(r"\{.*\}", re.DOTALL)


def _try_parse_json(text: str) -> Any:
    """Best-effort JSON parse, lenient with markdown code fences.

    Some local models like to wrap JSON in ```json``` fences or add
    a leading explanation paragraph. We try strict parse first, then
    fall back to the longest brace-delimited substring.
    """
    text = text or ""
    try:
        return json.loads(text)
    except Exception:
        pass
    m = _JSON_BLOCK_RE.search(text)
    if m:
        try:
            return json.loads(m.group(0))
        except Exception:
            return None
    return None


__all__ = [
    "index_page",
    "query",
    "summarize",
    "explain",
    "flashcards",
]
