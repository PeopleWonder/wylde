"""model_registry — unified registry of every model the Wylde harness uses.

the Wylde user's design: all models share a single home in the HuggingFace cache for
deduplication, but consumers see only the slice they care about. The
inference bar shows chat LLMs; the voice subsystem sees only Whisper / Piper
/ Kokoro; the caption subsystem sees only Florence-2 and friends; embedding
clients see only embedding models.

Kind taxonomy
-------------
Every entry carries a ``kind`` from ``ModelEntry``:

* ``llm``    — chat / instruction-tuned LMs (qwen, llama, gemma, mistral, …)
* ``stt``    — speech-to-text (Whisper variants, distil-whisper, wav2vec2)
* ``tts``    — text-to-speech (Piper, Kokoro, XTTS, Bark, SpeechT5)
* ``vision`` — image / video understanding (Florence-2, LLaVA, CLIP, SigLIP)
* ``embed``  — embedding models (nomic-embed, BGE, sentence-transformers)

Discovery sources, in priority order
------------------------------------
1. **Service manifests** — ``Voice/manifest.json``, ``VoiceAssistant/manifest.json``,  # wylde-check: dead-ref-ok
   etc. may carry a ``models: [{id, kind, required}]`` array. These are the
   source of truth for kind labels and surface as ``required_by`` on the entry.
2. **HF cache** — ``~/.cache/huggingface/hub/models--*/`` contributes everything
   downloaded locally. Repo names that no service claimed are kind-tagged by
   the heuristic in ``_heuristics.py`` (default falls through to ``llm``).
3. **Ollama daemon** — ``/api/tags`` enumerates models loaded in the local
   Ollama runtime. These are merged into the unified view as
   ``provider="ollama"`` and treated as ``kind="llm"`` by definition (Ollama
   doesn't host non-LLM kinds).
4. **Routing profiles** — ``_routing.py`` keeps benchmark-driven profiles for
   chat LLMs (capability slots, churn prevention, swap suggestions). Profiles
   attach to ``llm``-kind entries via ``ModelEntry.profile``.

Inference-bar contract
----------------------
**Only** the inference bar (in ``Wylde/Core/GUI``) calls
``list_models(kind="llm")``. The voice / caption / embedding subsystems each
filter on their own kind. This keeps the bar focused on chat models without
accidentally surfacing Whisper or Florence-2 to the user as something to
chat with.

Adding a new model
------------------
1. Drop it in the HF cache (``huggingface_hub`` does this on first
   ``from_pretrained`` / ``snapshot_download``).
2. If the consumer is a Wylde service, add a ``models`` entry to that
   service's ``manifest.json`` so the registry tags it correctly. For
   well-known names (``whisper-*``, ``piper-*``, ``florence-*``, ``*-bge-*``,
   ``nomic-embed-*``) the heuristic also gets it right without a manifest
   change.
3. Call ``refresh_cache()`` from any long-lived process; new HTTP requests
   will pick up the model on the next ``list_models`` call automatically.

Public API
----------
* ``list_models(kind=None)``       — every entry, or only those of one kind
* ``get_model(model_id)``          — one entry by id, ``None`` if unknown
* ``is_loaded(model_id)``          — currently resident in its inference engine
* ``refresh_cache()``              — force a rescan on the next call

The chat-LLM routing layer (``select_model``, ``promote_model``,
``bench_model``, ``upsert_profile``, capability slots, churn prevention,
HF discovery, swap prompts) is re-exported unchanged from ``_routing``.

This file used to be the 668-LOC routing module itself; that logic moved to
``_routing.py`` so this file can stay short and consumer-facing.
"""

from __future__ import annotations

import logging
import threading
from typing import Dict, List, Optional

# Routing layer (the original 668 LOC, kept intact in _routing.py).
from . import _routing
from ._heuristics import infer_kind, iter_patterns
from ._hf_scanner import (
    hub_root,
    invalidate_cache as _invalidate_hf,
    scan_hf_cache,
)
from ._service_manifests import (
    invalidate_cache as _invalidate_manifests,
    load_declarations,
    wylde_root,
)
from ._types import KIND_VALUES, Kind, ModelEntry

logger = logging.getLogger("wylde.harness.model_registry")

_lock = threading.Lock()


# ── Public API ───────────────────────────────────────────────────────────────


def list_models(kind: Optional[Kind] = None) -> List[ModelEntry]:
    """Return every known model, optionally filtered by ``kind``.

    Sources are merged in this order (later wins for duplicate ids on
    overlapping fields):

    1. HF cache scanner — most fields (``path``, ``size_bytes``,
       ``last_accessed``, ``provider="huggingface"``).
    2. Ollama tags — fills the ``loaded`` flag for matching ids and
       contributes Ollama-only ids (e.g. ``qwen2.5:14b``) that aren't in
       the HF cache, with ``provider="ollama"``.
    3. Routing profiles — ``profile`` block attached to ``llm``-kind ids
       that the routing layer has profiled.

    Per the inference-bar contract above, the inference bar must call this
    with ``kind="llm"``; other consumers pick their own kind. Passing
    ``None`` returns the full unified view (used by diagnostics and the
    settings panel).
    """
    if kind is not None and kind not in KIND_VALUES:
        raise ValueError(
            f"Unknown kind {kind!r}; expected one of {sorted(KIND_VALUES)}"
        )

    overrides, required_by = load_declarations()
    by_id: Dict[str, ModelEntry] = {
        entry.id: entry
        for entry in scan_hf_cache(overrides=overrides, required_by=required_by)
    }

    # Merge in Ollama-loaded LLMs so the inference bar sees what's resident.
    try:
        ollama_names = list(_routing.list_ollama_models())
    except Exception as exc:
        logger.debug("model_registry: ollama probe failed (%s); skipping", exc)
        ollama_names = []
    for name in ollama_names:
        existing = by_id.get(name)
        if existing is not None:
            existing.loaded = True
            if existing.provider == "huggingface":
                # Loaded into Ollama as well as cached on disk.
                existing.provider = "ollama"
            continue
        # Pure-Ollama model with no HF cache footprint. Service manifests
        # may still claim it; honour that, otherwise call it an LLM (the
        # only kind Ollama hosts).
        kind_for_ollama: Kind = overrides.get(name) or "llm"
        from ._types import default_chat_visible

        by_id[name] = ModelEntry(
            id=name,
            kind=kind_for_ollama,
            path=None,
            size_bytes=0,
            loaded=True,
            provider="ollama",
            required_by=list(required_by.get(name, ())),
            profile=None,
            last_accessed=None,
            chat_visible=default_chat_visible(kind_for_ollama),
        )

    # Attach routing profiles to LLM-kind entries.
    try:
        profiles_by_name = {p.get("name"): p for p in _routing.list_profiles()}
    except Exception as exc:
        logger.debug("model_registry: profile read failed (%s); skipping", exc)
        profiles_by_name = {}
    for entry in by_id.values():
        if entry.kind != "llm":
            continue
        prof = profiles_by_name.get(entry.id)
        if prof is not None:
            entry.profile = prof

    entries = sorted(by_id.values(), key=lambda e: (e.kind, e.id))
    if kind is not None:
        return [e for e in entries if e.kind == kind]
    return entries


def get_model(model_id: str) -> Optional[ModelEntry]:
    """Look up one entry by id. Returns ``None`` if no source knows it."""
    if not model_id:
        return None
    for entry in list_models():
        if entry.id == model_id:
            return entry
    return None


def is_loaded(model_id: str) -> bool:
    """Whether the model is currently resident in its inference engine.

    For LLMs this means "the local Ollama daemon has it loaded right now".
    Non-LLM kinds always return ``False`` until per-kind probes are wired
    in (open question — see NOTE.md).
    """
    entry = get_model(model_id)
    return bool(entry and entry.loaded)


def refresh_cache() -> None:
    """Force a rescan on the next call.

    Drops the HF-cache mtime cache and the service-manifest cache. The
    routing layer's profile store is unaffected (it's the source of truth
    for benchmarks; we don't want to lose it on a refresh).
    """
    with _lock:
        _invalidate_hf()
        _invalidate_manifests()


# ── Re-exports — back-compat for the existing routing API ────────────────────
#
# Anything that used to import from the old 668-LOC module keeps working:
#   from Wylde.Core.harness.model_registry import select_model, get_profile, ...

select_model = _routing.select_model
list_profiles = _routing.list_profiles
get_profile = _routing.get_profile
upsert_profile = _routing.upsert_profile
bench_model = _routing.bench_model
promote_model = _routing.promote_model
list_ollama_models = _routing.list_ollama_models
load_pending_swaps = _routing.load_pending_swaps
clear_swap_prompt = _routing.clear_swap_prompt
start_background_threads = _routing.start_background_threads
hf_search = _routing.hf_search
discovery_status = _routing.discovery_status
CAPABILITY_SLOTS = _routing.CAPABILITY_SLOTS


__all__ = [
    # New unified API
    "list_models",
    "get_model",
    "is_loaded",
    "refresh_cache",
    "ModelEntry",
    "Kind",
    "KIND_VALUES",
    # Heuristic / scanner / manifest helpers (mostly for tests)
    "infer_kind",
    "iter_patterns",
    "scan_hf_cache",
    "hub_root",
    "load_declarations",
    "wylde_root",
    # LLM-routing API (re-exported from _routing)
    "select_model",
    "list_profiles",
    "get_profile",
    "upsert_profile",
    "bench_model",
    "promote_model",
    "list_ollama_models",
    "load_pending_swaps",
    "clear_swap_prompt",
    "start_background_threads",
    "hf_search",
    "discovery_status",
    "CAPABILITY_SLOTS",
]
