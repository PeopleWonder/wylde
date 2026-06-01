"""backend_routing — multi-backend dispatch (ollama / vllm / openai_compat).

Single entry point for "send these messages to the model named X". Looks up
the model profile in the registry, picks the right backend client, and calls
it. On failure (and ``allow_fallback=True``) we retry against the local
Ollama daemon as the safety net — the eval doc recommends keeping Ollama as
dev/fallback even when vLLM is the production path.

Routing policy:

* ``backend == "ollama"`` — direct call to the local Ollama daemon. Ollama
  runs on the same host as the orchestrator, so there is no network egress
  and the Gateway does not sit in the path.
* ``backend in {"vllm", "openai_compat"}`` — routed through
  :mod:`Core.shared.egress_client`, which is the single place outbound
  internet traffic gets allowlisted, audited, and (optionally) killed. The
  Gateway's destination map (``vllm`` / ``openai_compat``) holds the URL and
  any auth token; this module only knows the logical key.

Returns a :class:`ChatResult`. Streaming callers do not use this router —
they consume :class:`HarnessEvent` from
:mod:`Wylde.Core.harness.backend.streaming` via
:func:`Wylde.Core.harness.backend.ollama_client.stream_chat` directly.
"""

from __future__ import annotations

import logging
import os
from dataclasses import dataclass
from typing import Any, Dict, List, Optional

import requests

from Core.shared.egress_client import (
    GatewayBlocked,
    GatewayDenied,
    GatewayError,
    forward,
)

from .ollama_client import OLLAMA_URL
from .response_normalization import BackendError, ChatResult

logger = logging.getLogger(__name__)


# Logical destination keys understood by the egress gateway. Only remote
# backends appear here — Ollama (local) is intentionally absent and is
# called directly below.
_REMOTE_BACKEND_DEST_KEYS = {
    "vllm": "vllm",
    "openai_compat": "openai_compat",
}


@dataclass
class _ProfileInfo:
    name: str
    backend: str = "ollama"
    backend_model: str = ""


def _lookup_profile(name: str) -> _ProfileInfo:
    """Read backend metadata from the model-registry profile.

    Falls back to defaults if the profile can't be read so existing single-
    Ollama setups keep working without configuration changes.
    """
    prof: dict = {}
    try:
        # Preferred: relative sibling import (we're in harness/backend/).
        from .. import model_registry as _models_mod

        prof = _models_mod.get_profile(name) or {}
    except Exception:
        try:
            # Fallback for callers that import this module with the
            # project root on sys.path but no Wylde package wrapper.
            import model_registry as _models_mod  # type: ignore

            prof = _models_mod.get_profile(name) or {}
        except Exception:
            prof = {}
    backend = str(prof.get("backend") or "ollama").lower()
    return _ProfileInfo(
        name=name,
        backend=backend,
        backend_model=str(prof.get("backend_model") or name),
    )


def _use_pipe_for_chat() -> bool:
    return os.getenv("WYLDE_HARNESS_OLLAMA_TRANSPORT", "pipe").strip().lower() == "pipe"


def _call_ollama(
    model: str,
    messages: List[Dict[str, Any]],
    *,
    fmt: Optional[str],
    temperature: float,
    timeout: int,
) -> ChatResult:
    """Call the local Ollama daemon — gated through the Rust
    ``wylde-ollama`` pipe when ``WYLDE_HARNESS_OLLAMA_TRANSPORT=pipe``
    (default), or direct HTTP when ``...=http``."""
    payload: Dict[str, Any] = {
        "model": model,
        "messages": messages,
        "stream": False,
        "options": {"temperature": temperature},
        "keep_alive": "24h",
    }
    if fmt:
        payload["format"] = fmt

    if _use_pipe_for_chat():
        try:
            from . import ollama_pipe

            data = ollama_pipe.chat_via_pipe(payload, timeout=float(timeout))
        except Exception as exc:  # noqa: BLE001 — convert to BackendError
            raise BackendError(f"ollama pipe failure: {exc}", backend="ollama") from exc
    else:
        try:
            resp = requests.post(
                f"{OLLAMA_URL}/api/chat", json=payload, timeout=timeout
            )
        except requests.RequestException as exc:
            raise BackendError(f"ollama unreachable: {exc}", backend="ollama") from exc
        if not resp.ok:
            raise BackendError(
                f"ollama returned {resp.status_code}: {resp.text[:300]}",
                backend="ollama",
                status=resp.status_code,
            )
        data = resp.json()

    content = data.get("message", {}).get("content", "")
    usage = data.get("usage", {}) or {}
    return ChatResult(
        text=content,
        prompt_tokens=int(usage.get("prompt_tokens", 0)),
        completion_tokens=int(usage.get("completion_tokens", 0)),
        backend="ollama",
        model=model,
        raw=data,
    )


def _call_remote_openai_compatible(
    dest: str,
    model: str,
    messages: List[Dict[str, Any]],
    *,
    fmt: Optional[str],
    temperature: float,
    timeout: int,
    backend_kind: str,
) -> ChatResult:
    """OpenAI-compatible chat completion through the Gateway.

    Used for vLLM, OpenRouter, LiteLLM — all remote endpoints. ``fmt='json'``
    translates to ``response_format={'type': 'json_object'}`` which most
    OAI-compatibles accept; servers without structured output silently
    ignore the field, callers must validate output. Auth headers are
    injected by the Gateway from the destination's configured env var.
    """
    payload: Dict[str, Any] = {
        "model": model,
        "messages": messages,
        "stream": False,
        "temperature": temperature,
    }
    if fmt == "json":
        payload["response_format"] = {"type": "json_object"}
    try:
        resp = forward(
            dest=dest,
            method="POST",
            path="/v1/chat/completions",
            body=payload,
            timeout=timeout,
        )
    except (GatewayBlocked, GatewayDenied) as exc:
        raise BackendError(f"egress {dest}: {exc}", backend=backend_kind) from exc
    except GatewayError as exc:
        raise BackendError(
            f"{backend_kind} unreachable via gateway: {exc}", backend=backend_kind
        ) from exc
    if not resp.ok:
        raise BackendError(
            f"{backend_kind} returned {resp.status}: {str(resp.body)[:300]}",
            backend=backend_kind,
            status=resp.status,
        )
    data = resp.body
    if not isinstance(data, dict):
        raise BackendError(
            f"{backend_kind} returned non-JSON body: {type(data).__name__}",
            backend=backend_kind,
        )
    choices = data.get("choices") or []
    if not choices:
        raise BackendError(f"{backend_kind} returned no choices", backend=backend_kind)
    content = choices[0].get("message", {}).get("content", "")
    usage = data.get("usage", {}) or {}
    return ChatResult(
        text=content,
        prompt_tokens=int(usage.get("prompt_tokens", 0)),
        completion_tokens=int(usage.get("completion_tokens", 0)),
        backend=backend_kind,
        model=model,
        raw=data,
    )


class InferenceRouter:
    """Resolve a model name to a backend and dispatch the chat call."""

    def __init__(self, allow_fallback: bool = True):
        self.allow_fallback = allow_fallback

    def chat(
        self,
        messages: List[Dict[str, Any]],
        model: str,
        *,
        fmt: Optional[str] = None,
        temperature: float = 0.7,
        timeout: int = 120,
    ) -> ChatResult:
        prof = _lookup_profile(model)
        backend = prof.backend
        dest = _REMOTE_BACKEND_DEST_KEYS.get(backend)

        if backend in ("vllm", "openai_compat") and dest is None:
            logger.warning(
                "Model %s asks for backend=%s but no gateway destination configured; "
                "falling back to ollama",
                model,
                backend,
            )
            backend = "ollama"
            prof = _ProfileInfo(name=model, backend="ollama")

        backend_model = prof.backend_model or model

        try:
            if backend == "ollama":
                return _call_ollama(
                    backend_model,
                    messages,
                    fmt=fmt,
                    temperature=temperature,
                    timeout=timeout,
                )
            assert dest is not None  # narrowed by the ollama fallback branch above
            return _call_remote_openai_compatible(
                dest,
                backend_model,
                messages,
                fmt=fmt,
                temperature=temperature,
                timeout=timeout,
                backend_kind=backend,
            )
        except BackendError as exc:
            if self.allow_fallback and exc.backend != "ollama":
                logger.warning(
                    "Backend %s failed for %s (%s); retrying via ollama",
                    exc.backend,
                    model,
                    exc,
                )
                return _call_ollama(
                    model,
                    messages,
                    fmt=fmt,
                    temperature=temperature,
                    timeout=timeout,
                )
            raise


_default_router: Optional[InferenceRouter] = None


def default_router() -> InferenceRouter:
    """Process-wide router singleton."""
    global _default_router
    if _default_router is None:
        _default_router = InferenceRouter(
            allow_fallback=os.getenv("INFERENCE_FALLBACK", "true").lower() == "true",
        )
    return _default_router


__all__ = ["InferenceRouter", "default_router"]
