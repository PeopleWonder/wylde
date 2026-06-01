"""Profile schema & storage , get_profile, upsert_profile, list_profiles.

Each LLM profile is a dict on disk in ``DATA_DIR/profiles.json``. The schema is
documented inline below; mutations always go through ``upsert_profile`` so the
shared ``_lock`` serialises read-modify-write across threads.
"""

from typing import Any, Dict, List, Optional

from . import _PROFILES_FILE, _load_json, _lock, _save_json


# ── Model profile schema ──────────────────────────────────────────────────────
#
# {
#   "name": "gemma3:27b",
#   "size_gb": 16.0,
#   "quant": "Q4_K_M",
#   "vram_footprint_mb": 14000,
#   "capabilities": ["code", "reasoning"],
#   "benchmark_scores": {
#     "tok_s_prompt": 450,
#     "tok_s_gen":    42,
#     "perplexity":   4.8,
#     "task_scores":  {"code": 0.82, "reasoning": 0.78}
#   },
#   "benchmark_runs":  3,
#   "last_benchmarked": "2025-01-01T00:00:00",
#   "first_active_at":  "2025-01-01T00:00:00",
#   "status": "active",      # active | candidate | retired | fallback
#   "slot":   "code",        # primary capability slot
#   "notes":  ""
# }


def _profiles() -> Dict[str, Dict]:
    data: Dict[str, Dict] = _load_json(_PROFILES_FILE, {})
    return data


def _save_profiles(p: Dict) -> None:
    _save_json(_PROFILES_FILE, p)


def get_profile(name: str) -> Optional[Dict]:
    return _profiles().get(name)


def upsert_profile(name: str, updates: Any) -> Dict:
    """Merge `updates` into the profile for `name` under _lock.

    `updates` may be a dict or a callable that takes the existing profile
    and returns the dict to merge in (atomic read-modify-write).

    Backend fields (added 2026-04-25 with vLLM support):
      backend      , "ollama" | "vllm" | "openai_compat"  (default "ollama")
      backend_url   — endpoint base URL (overrides VLLM_URL/OPENAI_BASE_URL)
      backend_model, model id to send to the backend (when registered name
                       differs from the backend's expected id)
      api_key       — bearer token for openai_compat backends
    """
    with _lock:
        profiles = _profiles()
        if name not in profiles:
            profiles[name] = {
                "name": name,
                "status": "candidate",
                "benchmark_runs": 0,
                "benchmark_scores": {},
                "capabilities": [],
                "vram_footprint_mb": None,
                "size_gb": None,
                "quant": None,
                "first_active_at": None,
                "backend": "ollama",
                "backend_url": "",
                "backend_model": "",
            }
        resolved = updates(profiles[name]) if callable(updates) else updates
        profiles[name].update(resolved)
        _save_profiles(profiles)
        return profiles[name]


def list_profiles() -> List[Dict]:
    """Return every profile dict the routing layer knows about.

    Renamed from ``list_models`` so the package-level ``list_models`` can
    be the unified API across model kinds. Internal callers should use
    this helper directly; external callers read from
    ``model_registry.list_models(kind="llm")`` and translate.
    """
    return list(_profiles().values())
