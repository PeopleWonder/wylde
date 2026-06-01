"""model_registry/_heuristics.py — repo-name → kind inference fallback.

Service manifests are the source of truth for kinds (see ``_service_manifests``).
Anything in the HF cache that no manifest claims falls through to this module's
``infer_kind`` function, which uses repo-name patterns to guess.

Patterns are intentionally narrow: a wrong guess is better than mis-routing a
chat model to the voice subsystem, so when in doubt we return ``"llm"`` and
let the user (or a manifest update) override.

Add a pattern here only when you have a real repo whose canonical name embeds
the kind unambiguously. For experimental forks, prefer to declare the kind in
the consuming service's manifest.
"""

from __future__ import annotations

import re
from typing import Iterable, Tuple

from ._types import Kind

# Order matters: the first pattern that matches wins. Place the
# discriminative tokens (whisper, piper, florence) ahead of broader ones.
_PATTERNS: Tuple[Tuple[str, Kind], ...] = (
    # speech-to-text
    (r"whisper", "stt"),
    (r"\bwav2vec", "stt"),
    (r"\bdistil[-_]?whisper", "stt"),
    # text-to-speech
    (r"\bpiper\b", "tts"),
    (r"\bkokoro\b", "tts"),
    (r"\bxtts\b", "tts"),
    (r"\bbark\b", "tts"),
    (r"speecht5", "tts"),
    # vision (image / video understanding)
    (r"florence", "vision"),
    (r"\bllava\b", "vision"),
    (r"\bclip\b", "vision"),
    (r"\bsiglip\b", "vision"),
    (r"\bblip\b", "vision"),
    (r"\bqwen2\.5-vl\b", "vision"),
    (r"\bqwen-vl\b", "vision"),
    # embeddings
    (r"\bnomic[-_]?embed", "embed"),
    (r"-bge-", "embed"),
    (r"\bbge[-_]", "embed"),
    (r"\bembed\b", "embed"),
    (r"sentence[-_]?transformers", "embed"),
    (r"e5[-_](small|base|large)", "embed"),
)

_COMPILED: Tuple[Tuple[re.Pattern, Kind], ...] = tuple(
    (re.compile(pat, re.IGNORECASE), kind) for pat, kind in _PATTERNS
)


def infer_kind(repo_or_name: str) -> Kind:
    """Heuristic kind inference from a model id or HF repo name.

    The default — when nothing matches — is ``"llm"``. That's deliberate:
    most users will be looking at chat models in their HF cache, and a
    false ``"llm"`` reading is harmless (the inference bar shows it; the
    voice subsystem ignores it). False ``"stt"`` / ``"tts"`` would route a
    chat model into a subsystem that can't run it.
    """
    if not repo_or_name:
        return "llm"
    haystack = repo_or_name.lower()
    for pattern, kind in _COMPILED:
        if pattern.search(haystack):
            return kind
    return "llm"


def iter_patterns() -> Iterable[Tuple[str, Kind]]:
    """Expose the (pattern, kind) list for tests and tooling."""
    return iter(_PATTERNS)


__all__ = ["infer_kind", "iter_patterns"]
