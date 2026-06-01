"""model_registry/_routing — LLM-kind capability routing & benchmarking.

Internal package. Public callers use ``Core/harness/model_registry/__init__.py``
which wraps this package (see the package docstring there for the kind taxonomy
and the inference-bar contract).

This used to be a single 688-LOC ``_routing.py`` module merged from
wylde-model-registry. It was split into single-concern submodules so each piece
can be edited, tested, or replaced without re-reading the whole file:

* ``profiles``        , profile schema & storage (get/upsert/list)
* ``slots``           , capability slots, ``CAPABILITY_SLOTS``, ``select_model``
* ``benchmarks``      , bench harness (``bench_model``, scoring)
* ``churn``           , promotion/swap logic, pending-swap state
* ``hf_search``       , HuggingFace API discovery + status
* ``ollama_watcher``  , Ollama polling, auto-bench, background threads

Shared infrastructure (constants, file paths, JSON helpers, the global lock)
lives in this ``__init__`` and is imported by submodules via ``from . import …``.
The public surface is re-exported below so external callers see the same names
they did before the split.

Privacy contract
----------------
  Network discovery is OFF by default.  The package NEVER makes outbound
  HTTP calls to HuggingFace unless the user explicitly enables it:
    - ENV  MODEL_DISCOVERY_ENABLED=false  (default)
    - ENV  MODEL_DISCOVERY_SCHEDULE=weekly (only used when enabled)
  All benchmarks run against locally available Ollama models only.

Internal API (re-exported below)
--------------------------------
  select_model(capability, budget_mode) , Optional[str]
  list_profiles()                        , List[Dict]   (was list_models)
  get_profile(name)                      , Optional[Dict]
  upsert_profile(name, updates)          , Dict
  bench_model(name)                      , Dict
  list_ollama_models()                   , List[str]
  load_pending_swaps()                   , Dict
  clear_swap_prompt(capability)
  promote_model(name, capability, force) , Dict
  hf_search(vram_gb, capability)         , list
  discovery_status()                     , Dict
  start_background_threads()             , call once at startup
"""

import json
import logging
import os
import threading
from pathlib import Path
from typing import Any

logger = logging.getLogger(__name__)

OLLAMA_URL = os.getenv("OLLAMA_URL", "http://127.0.0.1:11434")
DATA_DIR = Path(os.getenv("MODEL_DATA_DIR", "data/model_registry"))
DISCOVERY_ENABLED = os.getenv("MODEL_DISCOVERY_ENABLED", "false").lower() in (
    "1",
    "true",
    "yes",
)
DISCOVERY_SCHEDULE = os.getenv("MODEL_DISCOVERY_SCHEDULE", "weekly")

# Churn prevention constants
_MIN_DELTA_PCT = 0.10  # candidate must beat incumbent by ≥10%
_INCUMBENT_BONUS = 0.05  # 5% bonus for models active > 30 days
_MIN_BENCHMARK_RUNS = 3  # must run benchmark ≥3 times before promotion
_MAX_SWAP_PER_WEEK = 1  # per capability slot
_FALLBACK_DAYS = 14  # keep previous model as fallback for 14 days

DATA_DIR.mkdir(parents=True, exist_ok=True)
_PROFILES_FILE = DATA_DIR / "profiles.json"
_SWAPS_FILE = DATA_DIR / "swaps.json"
_PREFS_FILE = DATA_DIR / "preferences.json"
_DISCOVERY_FILE = DATA_DIR / "discovery.json"
_PENDING_SWAPS_FILE = DATA_DIR / "pending_swaps.json"

_lock = threading.Lock()


def _load_json(path: Path, default: Any = None) -> Any:
    if path.exists():
        try:
            return json.loads(path.read_text(encoding="utf-8"))
        except Exception:
            pass
    return default if default is not None else {}


def _save_json(path: Path, data: Any) -> None:
    path.write_text(json.dumps(data, indent=2, default=str), encoding="utf-8")


# ── Public re-exports ────────────────────────────────────────────────────────
# Submodule imports happen after shared infra is defined so the submodules can
# do ``from . import _PROFILES_FILE, _load_json, …`` without hitting a partially
# initialised package.

from .profiles import get_profile, list_profiles, upsert_profile  # noqa: E402
from .slots import CAPABILITY_SLOTS, select_model  # noqa: E402
from .benchmarks import bench_model  # noqa: E402
from .churn import (  # noqa: E402
    clear_swap_prompt,
    load_pending_swaps,
    promote_model,
)
from .hf_search import discovery_status, hf_search  # noqa: E402
from .ollama_watcher import list_ollama_models, start_background_threads  # noqa: E402

__all__ = [
    "CAPABILITY_SLOTS",
    "bench_model",
    "clear_swap_prompt",
    "discovery_status",
    "get_profile",
    "hf_search",
    "list_ollama_models",
    "list_profiles",
    "load_pending_swaps",
    "promote_model",
    "select_model",
    "start_background_threads",
    "upsert_profile",
]
