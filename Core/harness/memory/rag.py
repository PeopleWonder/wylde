"""RAG query interface — in-process replacement for the legacy ``wylde-rag`` service.

The harness used to call out over HTTP to a separate Flask service for every
memory read / write. That service is now dissolved into this module: same
tier model, same API shape, but everything runs in-process so a chat turn
doesn't pay a network round-trip per memory call.

Tier model (same as legacy ``memory_tier.MemoryManager``):

* **core**       — always-in-context snippets (user identity, hard preferences).
* **episodic**   — timestamped events / past interactions, searched semantically.
* **semantic**   — abstracted facts derived from episodic clusters.
* **procedural** — verified executable lessons (Voyager-style skill library).

Public surface:

* :func:`add_core` / :func:`add_episodic` / :func:`add_semantic` / :func:`add_procedural`
* :func:`core_context`  — full text concatenated, for prompt injection.
* :func:`search`        — tier-aware semantic search.
* :func:`find_procedural` — surface skills whose ``when`` clause matches a task.
* :func:`save_episodic_turn` — bundle (user, assistant) into one episodic memory.
* :func:`save_tiered_memory` — explicit "remember that ..." promotion.
* :func:`build_memory_block` — prompt-block composition (core + relevant).
* :func:`fetch_node_memories` — engine-side helper, keeps legacy snippet shape.
* :func:`stats` — counts by tier.
* :data:`MEMORY_AUGMENT_PATTERNS` — node-id substrings that auto-receive memory.
"""

from __future__ import annotations

import json
from concurrent.futures import ThreadPoolExecutor, TimeoutError as FuturesTimeoutError
from dataclasses import dataclass
from typing import Any, Callable, Dict, List, Optional, Sequence

from . import vector_store
from ._common import logger

# The consolidation LLM caller is wired in at orchestrator startup via
# :func:`set_llm_caller`. The backend module now exists
# (:mod:`Wylde.Core.harness.backend.backend_routing` with ``InferenceRouter``)
# but routing requires a configured profile registry — leaving this as a
# runtime injection point keeps test setups and Ollama-only deployments simple.
# When the caller is None, semantic consolidation falls back to a
# deterministic longest-common-prefix summary.
_llm_caller: Optional[Callable[[str], str]] = None


# ─── Tier vocabulary ────────────────────────────────────────────────────────

TIER_CORE = "core"
TIER_EPISODIC = "episodic"
TIER_SEMANTIC = "semantic"
TIER_PROCEDURAL = "procedural"
ALL_TIERS = (TIER_CORE, TIER_EPISODIC, TIER_SEMANTIC, TIER_PROCEDURAL)

# Legacy memory_type → tier (read-compatible). New writes use the explicit
# tier names above.
_LEGACY_TYPE_MAP = {
    "user": TIER_CORE,
    "feedback": TIER_CORE,
    "project": TIER_CORE,
    "reference": TIER_CORE,
    "lesson": TIER_EPISODIC,
    "custom": TIER_EPISODIC,
}


# ─── Engine integration constants ───────────────────────────────────────────
#
# Pulled forward from ``_legacy/core/wylde-orchestrator/graph/engine.py``.
# Node IDs containing these substrings auto-receive a memory block in the
# engine's planner / architect path.
MEMORY_AUGMENT_PATTERNS = frozenset(
    {"plan", "architect", "design", "spec", "blueprint", "strategy"}
)


# ─── Procedural skill signature ─────────────────────────────────────────────


@dataclass
class SkillSignature:
    """JSON-serialised metadata for a procedural skill.

    Stored in ``source_path`` (which is unused for procedural rows) so we
    don't have to migrate the LanceDB schema.
    """

    name: str
    description: str
    inputs: List[str]
    when: str = ""
    source_run: str = ""

    def to_json(self) -> str:
        return json.dumps(
            {
                "skill": self.name,
                "description": self.description,
                "inputs": list(self.inputs),
                "when": self.when,
                "source_run": self.source_run,
            }
        )

    @classmethod
    def from_json(cls, raw: str) -> Optional["SkillSignature"]:
        try:
            d = json.loads(raw)
        except Exception:
            return None
        if not d.get("skill"):
            return None
        return cls(
            name=str(d["skill"]),
            description=str(d.get("description", "")),
            inputs=list(d.get("inputs", []) or []),
            when=str(d.get("when", "")),
            source_run=str(d.get("source_run", "")),
        )


def set_llm_caller(caller: Optional[Callable[[str], str]]) -> None:
    """Inject the LLM caller used by :func:`consolidate_episodic_to_semantic`.

    Called from harness bootstrap once the backend module is wired up.
    """
    global _llm_caller
    _llm_caller = caller


# ─── Writes ─────────────────────────────────────────────────────────────────


def add_core(content: str, *, score: float = 1.0, session_id: str = "") -> str:
    return vector_store.add_row(
        content=content,
        memory_type=TIER_CORE,
        score=score,
        session_id=session_id,
        source_path="core",
    )


def add_episodic(
    content: str,
    *,
    score: float = 0.5,
    session_id: str = "",
    source_path: str = "",
) -> str:
    return vector_store.add_row(
        content=content,
        memory_type=TIER_EPISODIC,
        score=score,
        session_id=session_id,
        source_path=source_path,
    )


def add_semantic(
    content: str,
    *,
    source_ids: Sequence[str] = (),
    score: float = 0.7,
    session_id: str = "",
) -> str:
    src = (",".join(source_ids))[:200] if source_ids else ""
    return vector_store.add_row(
        content=content,
        memory_type=TIER_SEMANTIC,
        score=score,
        session_id=session_id,
        source_path=f"semantic:{src}" if src else "semantic",
    )


def add_procedural(
    content: str, signature: SkillSignature, *, score: float = 0.85
) -> str:
    return vector_store.add_row(
        content=content,
        memory_type=TIER_PROCEDURAL,
        score=score,
        session_id="",
        source_path=signature.to_json(),
    )


# ─── Reads ──────────────────────────────────────────────────────────────────


def core_context(limit: int = 10, max_chars: int = 4000) -> str:
    """Concatenated core memory text for prompt injection.

    Highest-score first, oldest as tiebreaker (stability). Capped at
    ``max_chars`` so a runaway core list never blows the prompt.
    """
    rows = vector_store.list_rows(memory_type=TIER_CORE, limit=max(limit * 4, limit))
    rows.sort(
        key=lambda r: (-float(r.get("score", 0.0)), float(r.get("created_at", 0)))
    )
    rows = rows[:limit]
    if not rows:
        return ""
    out: List[str] = []
    used = 0
    for r in rows:
        text = str(r.get("content", "")).strip()
        if not text:
            continue
        chunk = f"- {text}"
        if used + len(chunk) > max_chars:
            break
        out.append(chunk)
        used += len(chunk) + 1
    return "\n".join(out)


def search(
    query: str, *, tier: Optional[str] = None, limit: int = 10
) -> List[Dict[str, Any]]:
    """Tier-aware semantic search. ``tier=None`` searches all rows.

    Every call is logged via :func:`miss_log.log_query` so the
    rag_feedback / rag_misses / rag_chunk_usage tools have a query
    history to operate on. The log write is best-effort — if disk
    isn't writable the search still returns its hits.
    """
    if tier is not None and tier not in ALL_TIERS:
        raise ValueError(f"unknown tier '{tier}'; must be one of {ALL_TIERS}")
    from .embeddings import embed_one

    def _log(hits: List[Dict[str, Any]]) -> None:
        try:
            from . import miss_log

            miss_log.log_query(query, tier=tier, hits=hits)
        except Exception:  # noqa: BLE001
            logger.debug("rag.search: miss_log write failed", exc_info=True)

    try:
        qvec = embed_one(query)
    except Exception as exc:
        logger.warning("rag.search: embed failed: %s", exc)
        _log([])
        return []
    hits = vector_store.search_vectors(qvec, memory_type=tier, limit=limit)
    _log(hits)
    return hits


def find_procedural(task: str, limit: int = 5) -> List[Dict[str, Any]]:
    """Surface procedural skills whose signature ``when`` clause matches.

    Two-pass: vector search widens recall; then a token-overlap on the
    parsed signature's ``when`` field tightens the result.
    """
    rows = search(task, tier=TIER_PROCEDURAL, limit=limit * 2)
    scored: List[tuple[float, Dict[str, Any]]] = []
    task_lower = task.lower()
    for r in rows:
        sig = SkillSignature.from_json(str(r.get("source_path", "")))
        if sig is None:
            continue
        score = 0.5
        if sig.when:
            w = sig.when.lower()
            hits = sum(1 for tok in w.split() if len(tok) > 3 and tok in task_lower)
            score += min(0.4, hits * 0.1)
        scored.append((score, {**r, "signature": sig.__dict__}))
    scored.sort(key=lambda x: x[0], reverse=True)
    return [r for _, r in scored[:limit]]


# ─── Episodic write helpers (turn-loop callers) ─────────────────────────────

_EPISODIC_MAX_CHARS = 4000


def save_episodic_turn(
    *,
    session_id: str,
    user_text: str,
    assistant_text: str,
    score: float = 0.5,
) -> Optional[str]:
    """Persist (user, assistant) as one episodic memory; return id or None."""
    u = (user_text or "").strip()
    a = (assistant_text or "").strip()
    if not u and not a:
        return None
    parts: List[str] = []
    if u:
        parts.append(f"User: {u}")
    if a:
        parts.append(f"Assistant: {a}")
    content = "\n".join(parts)
    if len(content) > _EPISODIC_MAX_CHARS:
        content = content[:_EPISODIC_MAX_CHARS] + " … [truncated]"
    try:
        return add_episodic(content, score=score, session_id=session_id or "")
    except Exception as exc:
        logger.debug("save_episodic_turn failed: %s", exc)
        return None


def save_tiered_memory(
    *,
    tier: str = TIER_CORE,
    content: str,
    score: Optional[float] = None,
    session_id: str = "",
) -> Optional[str]:
    """Promote content into a specific tier (``core``/``episodic``/``semantic``/``procedural``)."""
    c = (content or "").strip()
    if not c:
        return None
    try:
        if tier == TIER_CORE:
            return add_core(
                c, score=score if score is not None else 1.0, session_id=session_id
            )
        if tier == TIER_EPISODIC:
            return add_episodic(
                c, score=score if score is not None else 0.5, session_id=session_id
            )
        if tier == TIER_SEMANTIC:
            return add_semantic(
                c, score=score if score is not None else 0.7, session_id=session_id
            )
        if tier == TIER_PROCEDURAL:
            # Procedural needs a signature; fall back to a thin one.
            sig = SkillSignature(
                name=session_id or "ad_hoc", description=c[:120], inputs=[]
            )
            return add_procedural(c, sig, score=score if score is not None else 0.85)
    except Exception as exc:
        logger.debug("save_tiered_memory failed (tier=%s): %s", tier, exc)
        return None
    raise ValueError(f"unknown tier '{tier}'")


# ─── Prompt-block composition ───────────────────────────────────────────────

_SEARCH_LIMIT = 5
_SEARCH_MAX_CHARS = 1500
_CORE_LIMIT = 8
_CORE_MAX_CHARS = 2000


def build_memory_block(
    user_query: str,
    *,
    workspace_id: str = "",
    deadline_s: float = 3.0,
) -> str:
    """Compose the core + relevant memory block prepended to system prompt.

    Three retrievals run in parallel via a
    ``ThreadPoolExecutor(max_workers=3)``:

    1. ``core_context`` — long-term core memories (always-remember tier).
    2. ``search(user_query, ...)`` — tier-aware semantic search across
       all memory tiers.
    3. ``workspace_memory.search(workspace_id, user_query, ...)`` —
       workspace-scoped memory, only when ``workspace_id`` is set.

    Each branch is wrapped in its own try/except — one slow or failing
    retrieval doesn't kill the others. ``deadline_s`` caps the total
    wait so a stuck retriever can't hold up turn-start; pending
    futures past the deadline are skipped (their threads keep running
    but their results are dropped).

    Returns empty string when all branches are empty so callers can
    skip inserting any memory section without checking length.
    """
    core = ""
    rag_hits: List[Dict[str, Any]] = []
    workspace_hits: List[Dict[str, Any]] = []
    q = (user_query or "").strip()

    def _safe_core() -> str:
        try:
            return core_context(limit=_CORE_LIMIT, max_chars=_CORE_MAX_CHARS) or ""
        except Exception as exc:
            logger.warning("build_memory_block: core fetch failed: %s", exc)
            return ""

    def _safe_search() -> List[Dict[str, Any]]:
        if not q:
            return []
        try:
            return search(q, limit=_SEARCH_LIMIT) or []
        except Exception as exc:
            logger.warning("build_memory_block: search failed: %s", exc)
            return []

    def _safe_workspace() -> List[Dict[str, Any]]:
        if not workspace_id or not q:
            return []
        try:
            from . import workspace_memory as _wm

            return _wm.search(workspace_id, q, limit=_SEARCH_LIMIT) or []
        except Exception as exc:
            logger.warning(
                "build_memory_block: workspace_memory.search failed: %s",
                exc,
            )
            return []

    with ThreadPoolExecutor(max_workers=3, thread_name_prefix="memblock") as ex:
        core_fut = ex.submit(_safe_core)
        rag_fut = ex.submit(_safe_search)
        workspace_fut = ex.submit(_safe_workspace)

        try:
            core = core_fut.result(timeout=deadline_s)
        except FuturesTimeoutError:
            logger.warning(
                "build_memory_block: core fetch hit %ss deadline", deadline_s
            )
        except Exception as exc:
            logger.warning("build_memory_block: core branch raised: %s", exc)

        try:
            rag_hits = rag_fut.result(timeout=deadline_s)
        except FuturesTimeoutError:
            logger.warning(
                "build_memory_block: rag search hit %ss deadline", deadline_s
            )
        except Exception as exc:
            logger.warning("build_memory_block: rag branch raised: %s", exc)

        try:
            workspace_hits = workspace_fut.result(timeout=deadline_s)
        except FuturesTimeoutError:
            logger.warning(
                "build_memory_block: workspace search hit %ss deadline",
                deadline_s,
            )
        except Exception as exc:
            logger.warning("build_memory_block: workspace branch raised: %s", exc)

    sections: List[str] = []
    if core:
        sections.append(f"--- Persistent context (always remember) ---\n{core}")
    if rag_hits:
        used = 0
        lines: List[str] = []
        for m in rag_hits:
            tier = m.get("memory_type") or "memory"
            content = str(m.get("content") or "").strip()
            if not content:
                continue
            line = f"[{tier}] {content}"
            if used + len(line) > _SEARCH_MAX_CHARS:
                break
            lines.append(line)
            used += len(line) + 1
        if lines:
            sections.append("--- Relevant past notes ---\n" + "\n".join(lines))
    if workspace_hits:
        ws_lines: List[str] = []
        used = 0
        for h in workspace_hits:
            body = str(h.get("body") or h.get("content") or "").strip()
            if not body:
                continue
            line = f"- {body}"
            if used + len(line) > _SEARCH_MAX_CHARS:
                break
            ws_lines.append(line)
            used += len(line) + 1
        if ws_lines:
            sections.append("--- Workspace memory ---\n" + "\n".join(ws_lines))
    return "\n\n".join(sections) if sections else ""


def latest_user_text(history: List[Dict[str, Any]]) -> str:
    """Find the most recent user message in ``history``.

    Used to anchor the relevant-memories search — tool results and assistant
    turns introduce vocabulary the user never typed and make poor anchors.
    """
    for msg in reversed(history or []):
        if msg.get("role") == "user" and msg.get("content"):
            return str(msg["content"])
    return ""


# ─── Engine integration ─────────────────────────────────────────────────────


def fetch_node_memories(query: str, limit: int = 3) -> str:
    """Pulled forward from ``_legacy/.../engine.py`` ``_fetch_node_memories``.

    Returns a snippet block to inject into a planner / architect node prompt.
    Fails open (returns "") so a temporarily broken memory layer never
    blocks engine execution.
    """
    try:
        results = search(query, limit=limit)
    except Exception as exc:
        logger.debug("fetch_node_memories: search failed: %s", exc)
        return ""
    if not results:
        return ""
    snippets = "\n\n".join(f"[Past decision] {r.get('content', '')}" for r in results)
    return f"\n\nRelevant past decisions from memory:\n{snippets}"


# ─── Consolidation (LLM-driven, episodic → semantic) ────────────────────────


def consolidate_episodic_to_semantic(
    max_clusters: int = 8,
    cluster_size: int = 3,
    archive_originals: bool = True,
) -> List[str]:
    """Greedy-cluster recent episodic rows and summarise each cluster as one
    semantic abstraction. Returns the new semantic ids.

    Falls back to a deterministic "longest-common-prefix" summary when no
    LLM caller is registered (Phase 4c will wire one in via
    :func:`set_llm_caller`).
    """
    raw = vector_store.list_rows(memory_type=TIER_EPISODIC, limit=200)
    if len(raw) < cluster_size:
        return []

    clusters: List[List[Dict[str, Any]]] = []
    used: set[str] = set()
    rows = sorted(raw, key=lambda r: float(r.get("created_at", 0)))
    for seed in rows:
        sid = str(seed.get("id", ""))
        if sid in used or len(clusters) >= max_clusters:
            continue
        seed_text = str(seed.get("content", ""))
        if not seed_text:
            continue
        try:
            neighbours = search(seed_text, tier=TIER_EPISODIC, limit=cluster_size + 2)
        except Exception:
            neighbours = []
        cluster = [seed]
        for n in neighbours:
            nid = str(n.get("id", ""))
            if nid in used or nid == sid:
                continue
            cluster.append(n)
            if len(cluster) >= cluster_size:
                break
        if len(cluster) >= cluster_size:
            for c in cluster:
                used.add(str(c.get("id", "")))
            clusters.append(cluster)

    new_ids: List[str] = []
    for cluster in clusters:
        summary = _summarise_cluster(cluster)
        if not summary:
            continue
        sid = add_semantic(summary, source_ids=[str(c.get("id", "")) for c in cluster])
        new_ids.append(sid)
        if archive_originals:
            ids = [str(c["id"]) for c in cluster if c.get("id")]
            if ids:
                vector_store.delete_rows(ids)
    logger.info(
        "rag: consolidated %d clusters into %d semantic memories",
        len(clusters),
        len(new_ids),
    )
    return new_ids


def _summarise_cluster(cluster: List[Dict[str, Any]]) -> str:
    bullets = "\n".join(f"- {c.get('content', '')}" for c in cluster)
    prompt = (
        "Below are several related observations from an agent's session log. "
        "Summarise them as ONE short, durable, generalised fact (one sentence, "
        "imperative or declarative — not a list). Avoid speculative claims, "
        "drop session-specific details, retain the underlying pattern.\n\n"
        f"Observations:\n{bullets}\n\nSummary:"
    )
    if _llm_caller is not None:
        try:
            return _llm_caller(prompt).strip().splitlines()[0]
        except Exception as exc:
            logger.debug("rag: llm consolidator failed: %s", exc)
    contents = [str(c.get("content", "")).strip() for c in cluster]
    if not contents:
        return ""
    prefix = contents[0]
    for c in contents[1:]:
        i = 0
        while i < len(prefix) and i < len(c) and prefix[i] == c[i]:
            i += 1
        prefix = prefix[:i]
    prefix = prefix.strip()
    if len(prefix) >= 20:
        return prefix
    return contents[0]


# ─── Stats ──────────────────────────────────────────────────────────────────


def stats() -> Dict[str, int]:
    out: Dict[str, int] = {}
    for tier in ALL_TIERS:
        out[tier] = len(vector_store.list_rows(memory_type=tier, limit=10000))
    out["legacy"] = len(
        [
            r
            for r in vector_store.list_rows(limit=10000)
            if r.get("memory_type") not in ALL_TIERS
        ]
    )
    out["total"] = vector_store.count_rows()
    return out


__all__ = [
    "TIER_CORE",
    "TIER_EPISODIC",
    "TIER_SEMANTIC",
    "TIER_PROCEDURAL",
    "ALL_TIERS",
    "MEMORY_AUGMENT_PATTERNS",
    "SkillSignature",
    "set_llm_caller",
    "add_core",
    "add_episodic",
    "add_semantic",
    "add_procedural",
    "core_context",
    "search",
    "find_procedural",
    "save_episodic_turn",
    "save_tiered_memory",
    "build_memory_block",
    "latest_user_text",
    "fetch_node_memories",
    "consolidate_episodic_to_semantic",
    "stats",
]
