"""Benchmark harness , bench_model, scoring, prompt bank.

Runs a fixed prompt bank against a target model (via ``_ollama_chat``, which
itself routes through the multi-backend inference router so vLLM-registered
models are exercised through vLLM rather than Ollama). The result is a per-
capability score dict plus a tok/s estimate.
"""

import json
import logging
import time
from typing import Any, Dict, List, Optional

from . import OLLAMA_URL
from .slots import CAPABILITY_SLOTS

logger = logging.getLogger(__name__)


def _ollama_chat(model: str, messages: list, timeout: int = 60) -> Optional[Dict]:
    """Backend-aware chat call used by the benchmark harness.

    Routes through the multi-backend inference router when available so a
    benchmark of a model registered with backend=vllm actually exercises
    vLLM. Falls back to direct Ollama only if the router import fails (e.g.
    in early bootstrap before sys.path is set up).
    """
    try:
        # Post-Phase-5D path: backend_routing now lives under
        # ``Core/harness/backend/backend_routing.py``. Try the new location
        # first; fall back to the old flat path for pre-rename callers.
        try:
            from ...backend.backend_routing import default_router as _router
        except Exception:
            from harness.backend_routing import default_router as _router  # type: ignore

        router = _router()
        result = router.chat(messages, model, timeout=timeout)
        # Synthesise an Ollama-shaped response so the benchmark code that
        # reads `message.content` and `usage` keeps working unchanged.
        return {
            "message": {"content": result.text, "role": "assistant"},
            "usage": {
                "prompt_tokens": result.prompt_tokens,
                "completion_tokens": result.completion_tokens,
            },
        }
    except Exception as e:
        logger.debug("router unavailable, direct ollama for %s: %s", model, e)

    # Direct fallback path (bootstrap-safe).
    import urllib.request

    payload = json.dumps(
        {
            "model": model,
            "messages": messages,
            "stream": False,
        }
    ).encode()
    try:
        req = urllib.request.Request(
            OLLAMA_URL + "/api/chat",
            data=payload,
            headers={"Content-Type": "application/json"},
        )
        with urllib.request.urlopen(req, timeout=timeout) as r:
            data: Dict = json.loads(r.read())
            return data
    except Exception as e:
        logger.warning("Ollama chat error with %s: %s", model, e)
        return None


_BENCH_PROMPTS = {
    "code": [
        (
            "Write a Python function that finds all prime numbers up to N using the Sieve of Eratosthenes.",
            "system: You are a coding assistant.",
        ),
        (
            "Implement a binary search tree with insert, search, and in-order traversal in Python.",
            "system: You are a coding assistant.",
        ),
    ],
    "reasoning": [
        (
            "A bat and ball cost $1.10 together. The bat costs $1 more than the ball. How much does the ball cost? Show your reasoning step by step.",
            "",
        ),
        (
            "If all Bloops are Razzies and all Razzies are Lazzies, are all Bloops definitely Lazzies? Explain.",
            "",
        ),
    ],
    "extraction": [
        (
            "Extract all dates, organisations, and monetary amounts from: 'On March 15 2024, Acme Corp signed a $4.2M contract with Widget Industries.'",
            "",
        ),
        (
            "Parse this JSON-like text and return valid JSON: name:John Doe, age:thirty-two, city: New York",
            "",
        ),
    ],
    "creative": [
        ("Write a haiku about machine learning.", ""),
        (
            "Continue this story in two sentences: 'The last robot stood alone in the empty server room...'",
            "",
        ),
    ],
    "chat": [
        ("Explain the difference between RAM and storage to a 10-year-old.", ""),
        ("What are three practical tips for improving sleep quality?", ""),
    ],
}


def bench_model(name: str) -> Dict[str, Any]:
    """Run benchmark prompts and return score summary."""
    scores: Dict[str, List[float]] = {cap: [] for cap in CAPABILITY_SLOTS}
    tok_s_gen_samples: List[float] = []

    for cap, prompts in _BENCH_PROMPTS.items():
        for user_msg, sys_msg in prompts:
            messages = []
            if sys_msg:
                messages.append({"role": "system", "content": sys_msg})
            messages.append({"role": "user", "content": user_msg})

            t0 = time.time()
            resp = _ollama_chat(name, messages, timeout=90)
            dur = time.time() - t0

            if resp is None:
                continue

            content = resp.get("message", {}).get("content", "")
            usage = resp.get("usage", {})
            gen_tok = usage.get("completion_tokens", 0)
            if dur > 0 and gen_tok > 0:
                tok_s_gen_samples.append(gen_tok / dur)

            score = _score_response(cap, user_msg, content)
            scores[cap].append(score)

    task_scores = {
        cap: round(sum(v) / len(v), 3) if v else 0.0 for cap, v in scores.items()
    }
    tok_s_gen = (
        round(sum(tok_s_gen_samples) / len(tok_s_gen_samples), 1)
        if tok_s_gen_samples
        else 0
    )

    return {
        "tok_s_gen": tok_s_gen,
        "task_scores": task_scores,
        "primary_capability": max(task_scores, key=lambda k: task_scores[k])
        if task_scores
        else "chat",
    }


def _score_response(capability: str, prompt: str, response: str) -> float:
    """Heuristic response quality scorer (0-1)."""
    if not response:
        return 0.0
    length_score = min(len(response) / 200, 1.0)

    keyword_map = {
        "code": ["def ", "return", "class", "import", "for ", "if "],
        "reasoning": ["because", "therefore", "step", "first", "second", "thus"],
        "extraction": ["{", "}", '"', ":", "date", "organisation", "amount"],
        "creative": [".", "!", "the", "and", "a "],
        "chat": [".", " you", "this", "that", "one", "two", "three"],
    }
    keywords = keyword_map.get(capability, [])
    keyword_score = sum(1 for kw in keywords if kw.lower() in response.lower()) / max(
        len(keywords), 1
    )

    return round(0.4 * length_score + 0.6 * keyword_score, 3)
