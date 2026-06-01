"""System-driven memory extractor — runs after every chat turn.

Called from the turn driver immediately after ``turn_complete``, on a
background thread so the user-facing stream isn't blocked. Reads the
last turn's user/assistant exchange + the conversation's working-memory
breadcrumbs, asks an LLM whether anything's worth promoting to durable
memory, and applies the verdicts.

Per the Wylde user's design (memory rework, Phase 13): the LLM-callable
``memory.*`` write tools are reserved for *explicit* user requests
(``"save this to memory"``). Spontaneous "I think this is worth
remembering" writes go through this extractor instead, so the model
isn't second-guessing the user mid-turn.

Verdict shape (one per line, JSON):

    {"action": "save_long_term",  "body": "...", "importance": 7}
    {"action": "save_workspace",  "body": "...", "importance": 6}
    {"action": "supersede",       "target_id": "abc123", "body": "...", "importance": 8}
    {"action": "noop"}

Only the model's verdicts drive writes; the extractor itself never
decides what to remember.

Module layout
~~~~~~~~~~~~~

* :mod:`._prompt` — system prompt, context builder, dedup guard, LLM
  call wrapper, JSON-per-line verdict parser.
* :mod:`._extract` — :class:`Verdict` / :class:`ExtractionResult`
  dataclasses, :func:`extract_post_turn`, :func:`fire_in_background`.
* :mod:`._persist` — :func:`_apply_verdict` (the verdict →
  memory-store mutation).

This ``__init__`` is the package shim — it re-exports the public surface
so ``from Core.harness.memory import post_turn_extractor`` and
``post_turn_extractor.extract_post_turn(...)`` keep working as before.
"""

from __future__ import annotations

from ._extract import (
    ExtractionResult,
    Verdict,
    extract_post_turn,
    fire_in_background,
)

__all__ = [
    "ExtractionResult",
    "Verdict",
    "extract_post_turn",
    "fire_in_background",
]
