"""Prompt-building, dedup heuristics, and verdict parsing.

This submodule owns the *text* side of the extractor: how the user
prompt is built from the just-completed turn, the system prompt the
LLM sees, the cheap token-overlap guard that catches near-duplicate
verdicts, and the JSON-per-line parser that turns the LLM's reply
back into structured ``Verdict`` objects.

The runtime construction of ``Verdict`` in :func:`_parse_verdicts`
imports it lazily from ``._extract`` to break the circular dependency
(``_extract`` imports from ``_prompt``).
"""

from __future__ import annotations

import json
import logging
import re
from typing import Any, Callable, Dict, List, Optional, TYPE_CHECKING

from .. import conversation as _conversation

if TYPE_CHECKING:
    from ._extract import Verdict

logger = logging.getLogger("wylde.harness.memory.post_turn_extractor")


# How many recent working-memory entries to include as "what just
# happened" context. Bigger → more signal, slower / pricier extraction.
_RECENT_WORKING_MEMORY = 8
# Max verdicts the model can emit per turn — cap so a runaway model
# doesn't paste the conversation back as one save per sentence.
_MAX_VERDICTS = 6
# Hard upper bound on how long the extractor blocks before bailing.
# This is a per-call timeout the chat_fn implementations should respect;
# the extractor itself doesn't enforce it (the chat_fn does).
_DEFAULT_TIMEOUT_S = 60.0
# Token-Jaccard threshold above which a verdict is considered a near-
# duplicate of an already-saved body. Defense-in-depth — the model is
# also told to dedupe via the prompt, but a pure-text guard catches
# misses on small models that ignore the instruction.
#
# 0.5 means "more than half the meaningful tokens overlap." The
# similarity calc strips a small stopword list and normalises trivial
# inflection (trailing s / es / ed) before comparing, so "I prefer
# kebab-case" vs "the user prefers kebab-case" matches even though
# the surface forms differ ("prefer" vs "prefers").
_DEDUP_SIMILARITY_THRESHOLD = 0.5

# Tokens too common to count as content overlap. Tiny list — we don't
# need a real stemmer's stopword set, just enough to keep "I"/"the"/
# pronouns from inflating Jaccard scores between unrelated bodies.
_DEDUP_STOPWORDS = frozenset(
    {
        "a",
        "an",
        "and",
        "are",
        "as",
        "at",
        "be",
        "by",
        "do",
        "does",
        "for",
        "from",
        "he",
        "her",
        "his",
        "i",
        "in",
        "is",
        "it",
        "its",
        "my",
        "of",
        "on",
        "or",
        "she",
        "that",
        "the",
        "their",
        "they",
        "this",
        "to",
        "user",
        "was",
        "we",
        "were",
        "with",
        "you",
        "your",
    }
)


_EXTRACTION_SYSTEM = (
    "You are a memory consolidator running after a conversation turn. "
    "Review the user's message and the assistant's response below. "
    "Identify any DURABLE facts, preferences, decisions, or learned "
    "context worth remembering across future conversations or in this "
    "workspace.\n\n"
    "Return ONE JSON object per line — no surrounding prose, no list "
    "markers. Each line is one verdict. Choose action from:\n"
    "  - save_long_term: a durable global fact / preference / identity "
    "(applies across conversations + workspaces).\n"
    "  - save_workspace: a fact specific to the current workspace / "
    "project (only meaningful while that workspace is active).\n"
    "  - supersede: a refinement of an EXISTING memory; pass target_id.\n"
    "  - noop: nothing in this turn worth promoting.\n\n"
    "Schema per line:\n"
    '  {"action": "save_long_term", "body": "<one-paragraph '
    'fact>", "importance": <1-10>}\n'
    '  {"action": "save_workspace", "body": "<one-paragraph fact>", '
    '"importance": <1-10>}\n'
    '  {"action": "supersede", "target_id": "<id>", "body": '
    '"<refined fact>", "importance": <1-10>}\n'
    '  {"action": "noop"}\n\n'
    "Be conservative — most turns produce noop. Promote only stable "
    "preferences, identity, or decisions; do NOT promote ephemeral "
    "task state. Importance: 9-10 is hard user identity, 7-8 is a "
    "stable preference, 5-6 is project context, 1-4 is rarely worth "
    "promoting at all. Reply with one or more lines; emit noop alone "
    "when nothing qualifies."
)


# ── Context builder ───────────────────────────────────────────────────


def _build_context(
    conversation_id: str,
    turn_id: str,
    *,
    already_saved: Optional[List[Dict[str, Any]]] = None,
) -> Optional[str]:
    """Build the user-prompt content the extractor sees.

    Pulls the last user message + the assistant's final reply from the
    conversation, plus the most recent working-memory entries. When
    ``already_saved`` is non-empty, the prompt includes the bodies the
    LLM saved during this turn so the model knows not to re-save them.
    Returns None if the conversation file isn't readable yet (the
    chat-turn save and this function race; the chat side wins almost
    always).
    """
    try:
        doc = _conversation.read_conversation(conversation_id)
    except Exception:  # noqa: BLE001
        return None

    msgs = doc.get("messages") or []
    if not isinstance(msgs, list):
        return None
    # Take the trailing user → assistant pair. There may be multiple
    # tool / tool-result messages between them; we skip those for the
    # extractor's prompt (the working_memory section captures the gist).
    last_user = ""
    last_assistant = ""
    for m in reversed(msgs):
        if not isinstance(m, dict):
            continue
        role = m.get("role")
        content = m.get("content") or ""
        if not isinstance(content, str):
            continue
        if role == "assistant" and not last_assistant and content.strip():
            last_assistant = content.strip()
        elif role == "user" and not last_user and content.strip():
            last_user = content.strip()
            if last_assistant:
                break
    if not last_user and not last_assistant:
        return None

    working = doc.get("working_memory") or []
    if not isinstance(working, list):
        working = []
    recent = [
        e
        for e in working[-_RECENT_WORKING_MEMORY:]
        if isinstance(e, dict) and not e.get("superseded_by")
    ]

    parts: List[str] = []
    parts.append(f"User: {last_user}" if last_user else "User: (no message)")
    parts.append(
        f"Assistant: {last_assistant}" if last_assistant else "Assistant: (no reply)"
    )

    if recent:
        bits: List[str] = []
        for e in recent:
            kind = e.get("kind") or "raw"
            data = e.get("data")
            if isinstance(data, dict):
                if kind == "tool":
                    bits.append(f"[tool] ran {data.get('name', '?')}")
                else:
                    body = ", ".join(
                        f"{k}={str(v)[:60]}" for k, v in list(data.items())[:3]
                    )
                    bits.append(f"[{kind}] {body}")
            else:
                bits.append(f"[{kind}] {str(data)[:120]}")
        parts.append("Recent activity:\n" + "\n".join(bits))

    if already_saved:
        bullet_lines: List[str] = []
        for s in already_saved:
            if not isinstance(s, dict):
                continue
            body = str(s.get("body") or "").strip()
            if not body:
                continue
            mid = str(s.get("memory_id") or "")
            tag = f" (id={mid[:8]})" if mid else ""
            bullet_lines.append(f"- {body[:200]}{tag}")
        if bullet_lines:
            parts.append(
                "Already saved by an explicit user-directed tool call "
                "during this turn — do NOT save substantially-similar "
                "content; only return verdicts for NEW facts not already "
                "covered:\n" + "\n".join(bullet_lines)
            )

    parts.append(f"(conversation_id={conversation_id} turn_id={turn_id})")
    return "\n\n".join(parts)


# ── Near-duplicate guard ──────────────────────────────────────────────


_TOKEN_RE = re.compile(r"[a-z0-9]+")


def _stem(tok: str) -> str:
    """Trivial inflection-stripping: chop the most common English
    plural / past-tense endings so "prefer" and "prefers" collapse to
    the same token. Not a real stemmer — we just need enough to make
    near-duplicate matching robust without a heavy dependency."""
    if len(tok) > 4:
        if tok.endswith("ies"):
            return tok[:-3] + "y"
        if tok.endswith(("sses", "shes", "ches")):
            return tok[:-2]
        if tok.endswith("es") and not tok.endswith(("ses", "ies")):
            return tok[:-2]
        if tok.endswith("ed") and len(tok) > 5:
            return tok[:-2]
        if tok.endswith("ing") and len(tok) > 6:
            return tok[:-3]
        if tok.endswith("s") and not tok.endswith("ss"):
            return tok[:-1]
    return tok


def _tokens(text: str) -> set:
    raw = _TOKEN_RE.findall((text or "").lower())
    return {_stem(t) for t in raw if t and t not in _DEDUP_STOPWORDS}


def _token_jaccard(a: str, b: str) -> float:
    """Cheap text similarity. Strips punctuation, lowercases, splits on
    word characters, computes |A ∩ B| / |A ∪ B|. Returns 0.0 when
    either side is empty so empty strings never look "similar"."""
    ta = _tokens(a)
    tb = _tokens(b)
    if not ta or not tb:
        return 0.0
    return len(ta & tb) / len(ta | tb)


def _is_near_duplicate(candidate: str, existing_bodies: List[str]) -> bool:
    """True if ``candidate`` overlaps any entry in ``existing_bodies``
    above the dedup threshold. Hot path on every verdict; kept O(n*m)
    in token counts which is fine — n is tiny (verdicts per turn ≤6)
    and m is bounded by the LLM-saves cap (also small)."""
    if not candidate or not existing_bodies:
        return False
    cand_tokens = _tokens(candidate)
    if not cand_tokens:
        return False
    for body in existing_bodies:
        score = _token_jaccard(candidate, body)
        if score >= _DEDUP_SIMILARITY_THRESHOLD:
            return True
    return False


# ── LLM call + verdict parsing ────────────────────────────────────────


def _ask(
    chat_fn: Callable[..., Any],
    user_prompt: str,
    model: Optional[str],
) -> str:
    messages = [
        {"role": "system", "content": _EXTRACTION_SYSTEM},
        {"role": "user", "content": user_prompt},
    ]
    try:
        step = chat_fn(messages=messages, tools=[], model=model)
    except Exception as exc:  # noqa: BLE001
        logger.warning("post_turn_extractor: chat_fn raised: %s", exc)
        return ""
    text = getattr(step, "text", "")
    return (text or "").strip()


def _parse_verdicts(text: str) -> List["Verdict"]:
    """Pull JSON-per-line verdicts out of the model's output.

    Tolerant: ignores blank lines, non-JSON narration, and trailing
    commentary. Only lines that parse as a dict with an ``action`` key
    matching one of the known actions are accepted.
    """
    # Lazy import to break the _prompt ↔ _extract cycle: _extract
    # imports _parse_verdicts at module top, so importing Verdict at
    # module top here would loop. Runtime resolution is fine because
    # _extract has finished initialising by the time _parse_verdicts
    # is ever called.
    from ._extract import Verdict

    out: List[Verdict] = []
    # Some models prefix with code fences; strip them.
    cleaned = re.sub(r"^```[a-z]*\n?|```$", "", text.strip(), flags=re.MULTILINE)
    for raw_line in cleaned.splitlines():
        line = raw_line.strip()
        if not line:
            continue
        if not line.startswith("{"):
            continue
        try:
            obj = json.loads(line)
        except (ValueError, TypeError):
            continue
        if not isinstance(obj, dict):
            continue
        action = str(obj.get("action") or "").strip()
        if action not in {"save_long_term", "save_workspace", "supersede", "noop"}:
            continue
        body = str(obj.get("body") or "").strip()
        if action != "noop" and not body:
            continue
        try:
            importance = int(obj.get("importance") or 5)
        except (TypeError, ValueError):
            importance = 5
        importance = max(1, min(10, importance))
        target_id = str(obj.get("target_id") or "").strip()
        out.append(
            Verdict(
                action=action,
                body=body,
                importance=importance,
                target_id=target_id,
                raw=obj,
            )
        )
    return out
