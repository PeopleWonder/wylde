"""model_registry/_types.py — shared types for the unified model registry.

The ``ModelEntry`` dataclass is the lingua franca between the HF-cache
scanner, the service-manifest reader, the Ollama probe, and the LLM-routing
profiles. Every consumer (inference bar, voice, caption, embed clients) sees
the same shape regardless of where the model came from.

``Kind`` is the small taxonomy described in the package docstring:

* ``llm``    — chat / instruction-tuned language models (qwen, llama, gemma…)
* ``stt``    — speech-to-text (Whisper variants)
* ``tts``    — text-to-speech (Piper, Kokoro, XTTS)
* ``vision`` — image / video understanding (Florence-2, LLaVA, CLIP, …)
* ``embed``  — text / multimodal embedding models (nomic-embed, BGE, …)

Only models with ``kind == "llm"`` are surfaced to the inference bar — see
the contract in the package docstring.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import List, Literal, Optional

#: The model-kind taxonomy. Adding a kind here is a breaking change for
#: every consumer that filters on ``kind`` — keep this list authoritative.
Kind = Literal["llm", "stt", "tts", "vision", "embed"]

#: Tuple form of ``Kind`` for runtime membership checks (``Literal`` itself
#: doesn't support ``in``).
KIND_VALUES: tuple = ("llm", "stt", "tts", "vision", "embed")


@dataclass
class ModelEntry:
    """One entry in the unified model registry.

    A ``ModelEntry`` is intentionally cheap to construct — the scanner builds
    one per HF cache directory on every refresh. Heavy fields (``profile``,
    ``benchmark_scores``) are pulled lazily from the routing layer when a
    consumer asks for an LLM-kind entry.
    """

    #: Canonical id. For HF-cache-discovered models this is the HF repo
    #: ("microsoft/Florence-2-large"). For Ollama models it's the Ollama tag
    #: ("qwen2.5:14b"). For manifest-declared models it's whatever the
    #: manifest declared.
    id: str

    #: The taxonomy bucket. Manifest declarations win over heuristics.
    kind: Kind

    #: On-disk location, when known. ``None`` for Ollama-only models that
    #: don't materialise as an HF cache directory.
    path: Optional[str] = None

    #: Disk footprint in bytes (sum of files inside ``path``). 0 if unknown.
    size_bytes: int = 0

    #: Whether the model is currently resident in its inference engine.
    #: For LLMs this means "loaded into Ollama"; for non-LLMs it's left as
    #: ``False`` until per-kind probes are wired in.
    loaded: bool = False

    #: Where this entry came from. One of ``"huggingface"``, ``"ollama"``,
    #: ``"local"`` (manifest with a path that's not in the HF cache).
    provider: str = "huggingface"

    #: Names of services declaring this model required, e.g. ["voice"].
    #: Empty for models discovered by the scanner that no service claims.
    required_by: List[str] = field(default_factory=list)

    #: Underlying routing profile, only populated for ``kind == "llm"``
    #: models that the routing layer has profiled. Holds capability slots,
    #: benchmark scores, churn-prevention metadata — the dict shape is
    #: defined in ``_routing.py`` (see the schema comment near
    #: ``upsert_profile``).
    profile: Optional[dict] = None

    #: Last-accessed timestamp (epoch seconds) for eviction policies.
    last_accessed: Optional[float] = None

    #: Whether this entry should appear in the GUI's user-facing chat-model
    #: dropdown. STT / TTS / wake-word entries set this False so the
    #: inference bar never offers Whisper or Kokoro as something to "chat
    #: with". Defaults derive from ``kind`` (only ``llm`` is True), but
    #: service manifests can override with an explicit ``chat_visible``
    #: field — useful for hiding internal LLMs reserved for system tasks.
    chat_visible: bool = True

    def to_dict(self) -> dict:
        """JSON-friendly view used by the inference bar and HTTP routes."""
        return {
            "id": self.id,
            "kind": self.kind,
            "path": self.path,
            "size_bytes": self.size_bytes,
            "loaded": self.loaded,
            "provider": self.provider,
            "required_by": list(self.required_by),
            "profile": self.profile,
            "last_accessed": self.last_accessed,
            "chat_visible": self.chat_visible,
        }


def default_chat_visible(kind: Kind) -> bool:
    """Compute the default ``chat_visible`` for a kind. Only LLMs surface
    in the chat dropdown by default; STT/TTS/vision/embed are hidden."""
    return kind == "llm"


__all__ = ["ModelEntry", "Kind", "KIND_VALUES", "default_chat_visible"]
