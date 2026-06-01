"""ollama_client — raw HTTP transport against the (local) Ollama daemon.

Pure I/O: no prompt building, no streaming semantics, no agent state. Each
function is a thin wrapper around one Ollama endpoint, returning the parsed
JSON (or normalised list/bool) and swallowing transport errors as empty
results so callers don't need to wrap every call in try/except.

Ollama runs on the same host as the orchestrator, so calls go straight to
``OLLAMA_URL`` (default ``http://127.0.0.1:11434``). The Gateway egress
client (``Core.shared.egress_client``) is reserved for traffic that actually
leaves the machine — see :mod:`Core.harness.backend.backend_routing` for the
dispatcher that picks between the local path and the remote-via-gateway
path.

The streaming chat path is also here: :func:`stream_chat` POSTs the assembled
body to ``/api/chat`` and yields :class:`HarnessEvent` instances from
:mod:`Wylde.Core.harness.backend.streaming`.
"""

from __future__ import annotations

import json
import logging
import os
import re
import time
from typing import Any, Callable, Dict, Iterator, List, Optional

import requests

from .streaming import (
    AssistantToken,
    Done,
    HarnessEvent,
    StreamError,
    ThinkingToken,
    ToolCallDelta,
)

logger = logging.getLogger(__name__)


# ─── Configuration ─────────────────────────────────────────────────────────

OLLAMA_URL: str = os.getenv("OLLAMA_URL", "http://127.0.0.1:11434").rstrip("/")
"""Single source of truth for the Ollama base URL across every harness module."""


# ─── Strangler-fig switch ──────────────────────────────────────────────────
#
# When ``WYLDE_HARNESS_OLLAMA_TRANSPORT=pipe`` (default ``pipe`` once the
# Rust wylde-ollama service is stable; ``http`` keeps the legacy direct
# path), the eight unary functions below route through
# :mod:`Wylde.Core.harness.backend.ollama_pipe` instead of direct HTTP
# to ``127.0.0.1:11434``. Streaming functions (``stream_chat``,
# ``pull_model``) stay on direct HTTP regardless until the Python
# streaming-IPC client lands (master plan Q-S6).


def _use_pipe() -> bool:
    return os.getenv("WYLDE_HARNESS_OLLAMA_TRANSPORT", "pipe").strip().lower() == "pipe"


# ─── Connection liveness ───────────────────────────────────────────────────


def check_health(timeout: float = 3.0) -> bool:
    """GET ``/`` for liveness. Returns False on any transport failure."""
    if _use_pipe():
        from . import ollama_pipe

        try:
            return ollama_pipe.check_health(timeout=timeout)
        except Exception:  # noqa: BLE001 — pipe shim raised; fall through to HTTP
            logger.debug(
                "ollama_pipe check_health failed; falling through", exc_info=True
            )
    try:
        resp = requests.get(OLLAMA_URL + "/", timeout=timeout)
        return resp.ok
    except requests.RequestException:
        return False


# ─── Pull-name normalisation ───────────────────────────────────────────────


def normalize_pull_name(name: str) -> str:
    """Resolve user-facing model names to the form Ollama's ``/api/pull`` expects.

    Ollama defaults to its own registry (registry.ollama.ai). HuggingFace GGUFs
    only resolve when the name carries the ``hf.co/`` scheme — otherwise the
    daemon routes to the Ollama registry and 404s. Browse results expose HF
    repos as ``<author>/<repo>:<quant>`` with no scheme, so those need tagging.

    Pass-throughs:
      - already ``hf.co/...``    → keep as-is
      - ``library/foo:tag``      → Ollama's own default namespace
      - no ``/`` at all          → plain registry tag like ``qwen3:0.6b``
    """
    if not name:
        return name
    if name.startswith("hf.co/"):
        return name
    if name.startswith("library/"):
        return name
    if "/" not in name:
        return name
    return "hf.co/" + name


# ─── Model-management endpoints ────────────────────────────────────────────


def list_models() -> List[str]:
    """Return the names of every locally-installed model (``/api/tags``)."""
    if _use_pipe():
        from . import ollama_pipe

        try:
            return ollama_pipe.list_models()
        except Exception:  # noqa: BLE001
            logger.debug(
                "ollama_pipe list_models failed; falling through", exc_info=True
            )
    try:
        resp = requests.get(OLLAMA_URL + "/api/tags", timeout=10)
        if not resp.ok:
            return []
        data = resp.json() or {}
        return [m.get("name", "") for m in (data.get("models") or []) if m.get("name")]
    except requests.RequestException:
        return []


def list_models_detailed() -> List[Dict[str, Any]]:
    """Return the full ``/api/tags`` model list including size, digest, etc."""
    if _use_pipe():
        from . import ollama_pipe

        try:
            return ollama_pipe.list_models_detailed()
        except Exception:  # noqa: BLE001
            logger.debug(
                "ollama_pipe list_models_detailed failed; falling through",
                exc_info=True,
            )
    try:
        resp = requests.get(OLLAMA_URL + "/api/tags", timeout=10)
        if not resp.ok:
            return []
        data = resp.json() or {}
        return list(data.get("models") or [])
    except requests.RequestException:
        return []


def list_loaded_models() -> List[str]:
    """Return the names of models currently held in memory (``/api/ps``)."""
    if _use_pipe():
        from . import ollama_pipe

        try:
            return ollama_pipe.list_loaded_models()
        except Exception:  # noqa: BLE001
            logger.debug(
                "ollama_pipe list_loaded_models failed; falling through", exc_info=True
            )
    try:
        resp = requests.get(OLLAMA_URL + "/api/ps", timeout=3)
        if not resp.ok:
            return []
        data = resp.json() or {}
        return [m.get("name", "") for m in (data.get("models") or []) if m.get("name")]
    except requests.RequestException:
        return []


def list_loaded_models_detailed() -> List[Dict[str, Any]]:
    """Return the full ``/api/ps`` payload for models currently in VRAM.

    Each entry includes ``name``, ``size``, ``size_vram``, ``digest``,
    ``expires_at`` etc. Used by the consolidated ``harness.status`` action so
    the InferenceBar can show VRAM usage without a second call.
    """
    if _use_pipe():
        from . import ollama_pipe

        try:
            return ollama_pipe.list_loaded_models_detailed()
        except Exception:  # noqa: BLE001
            logger.debug(
                "ollama_pipe list_loaded_models_detailed failed; falling through",
                exc_info=True,
            )
    try:
        resp = requests.get(OLLAMA_URL + "/api/ps", timeout=3)
        if not resp.ok:
            return []
        data = resp.json() or {}
        return list(data.get("models") or [])
    except requests.RequestException:
        return []


def show_model(name: str) -> Optional[Dict[str, Any]]:
    """Fetch detailed metadata for a locally-installed model.

    Returns the ``/api/show`` payload (details, model_info, capabilities,
    parameters, template, license, ...) or ``None`` on failure.
    """
    if not name:
        return None
    if _use_pipe():
        from . import ollama_pipe

        try:
            return ollama_pipe.show_model(name)
        except Exception:  # noqa: BLE001
            logger.debug(
                "ollama_pipe show_model failed; falling through", exc_info=True
            )
    try:
        resp = requests.post(
            OLLAMA_URL + "/api/show",
            json={"model": name},
            timeout=8,
        )
        if not resp.ok:
            return None
        data: Dict[str, Any] = resp.json()
        return data
    except requests.RequestException:
        return None


def delete_model(name: str) -> bool:
    if not name:
        return False
    if _use_pipe():
        from . import ollama_pipe

        try:
            return ollama_pipe.delete_model(name)
        except Exception:  # noqa: BLE001
            logger.debug(
                "ollama_pipe delete_model failed; falling through", exc_info=True
            )
    try:
        resp = requests.delete(
            OLLAMA_URL + "/api/delete",
            json={"name": name},
            timeout=10,
        )
        return resp.ok
    except requests.RequestException:
        return False


def unload_model(name: str) -> bool:
    """Tell Ollama to evict ``name`` from VRAM.

    The documented eviction trick is an empty-prompt ``/api/generate`` call
    with ``keep_alive=0``. Returns True on 200.
    """
    if not name:
        return False
    if _use_pipe():
        from . import ollama_pipe

        try:
            return ollama_pipe.unload_model(name)
        except Exception:  # noqa: BLE001
            logger.debug(
                "ollama_pipe unload_model failed; falling through", exc_info=True
            )
    try:
        resp = requests.post(
            OLLAMA_URL + "/api/generate",
            json={"model": name, "keep_alive": 0},
            timeout=8,
        )
        return resp.ok
    except requests.RequestException:
        return False


# ─── Pull (NDJSON streamer with transient-error retry) ─────────────────────

# Errors that mean "the upstream registry hiccupped, but Ollama's blob cache
# still has whatever we already pulled". Retrying the same /api/pull request
# resumes from the cached chunks instead of starting over, so retries are
# effectively free. "context deadline exceeded" comes from Ollama's Go HTTP
# client when HuggingFace is slow to respond — common on multi-GB pulls.
_TRANSIENT_PULL_ERROR_RE = (
    "context deadline exceeded|deadline exceeded|EOF|connection reset|"
    "socket hang up|ECONNRESET|ETIMEDOUT|stream ended before reporting success|"
    "network|fetch failed"
)
_MAX_PULL_ATTEMPTS = 6
_RETRY_BASE_DELAY_S = 3


def _is_transient(msg: str) -> bool:
    return bool(re.search(_TRANSIENT_PULL_ERROR_RE, msg, re.IGNORECASE))


def pull_model(
    name: str,
    on_progress: Optional[Callable[[Dict[str, Any]], None]] = None,
) -> Iterator[Dict[str, Any]]:
    """Pull a model from the appropriate registry, streaming progress.

    Generator: yields each NDJSON status line so callers can either consume
    them iteratively (HTTP streaming endpoint) or via ``on_progress`` callback
    (synchronous in-process calls).

    Retries on transient registry errors; raises on the final attempt.
    """
    last_err: Optional[Exception] = None
    for attempt in range(1, _MAX_PULL_ATTEMPTS + 1):
        try:
            yield from _pull_model_once(name, on_progress)
            return
        except Exception as exc:  # noqa: BLE001
            last_err = exc
            msg = str(exc)
            if not _is_transient(msg) or attempt >= _MAX_PULL_ATTEMPTS:
                raise
            delay = _RETRY_BASE_DELAY_S * attempt
            evt = {
                "status": (
                    f"retry {attempt}/{_MAX_PULL_ATTEMPTS - 1}: {msg} — "
                    f"resuming in {delay}s"
                )
            }
            if on_progress:
                try:
                    on_progress(evt)
                except Exception:
                    logger.exception("pull_model on_progress callback raised")
            yield evt
            time.sleep(delay)
    if last_err is not None:
        raise last_err


def _pull_model_once(
    name: str,
    on_progress: Optional[Callable[[Dict[str, Any]], None]],
) -> Iterator[Dict[str, Any]]:
    resolved = normalize_pull_name(name)
    url = OLLAMA_URL + "/api/pull"
    try:
        resp = requests.post(
            url,
            json={"name": resolved, "stream": True},
            stream=True,
            timeout=(10, None),
        )
    except requests.RequestException as exc:
        raise RuntimeError(f"Cannot reach Ollama at {OLLAMA_URL}: {exc}") from exc

    if not resp.ok:
        text = ""
        try:
            text = resp.text[:300]
        except Exception:
            pass
        raise RuntimeError(f"{resp.status_code} {text} [{url}] [model: {resolved}]")

    saw_success = False
    saw_any_line = False
    try:
        for raw_line in resp.iter_lines(decode_unicode=True):
            if not raw_line:
                continue
            try:
                obj = json.loads(raw_line)
            except json.JSONDecodeError:
                continue
            saw_any_line = True
            if obj.get("error"):
                raise RuntimeError(obj["error"])
            if obj.get("status") == "success":
                saw_success = True
            if on_progress:
                try:
                    on_progress(obj)
                except Exception:
                    logger.exception("pull_model on_progress callback raised")
            yield obj
    finally:
        try:
            resp.close()
        except Exception:
            pass

    if not saw_success:
        detail = (
            "Ollama stream ended before reporting success — "
            "the pull may have been interrupted."
            if saw_any_line
            else "Ollama returned an empty response — pull did not start."
        )
        raise RuntimeError(detail)


# ─── Streaming /api/chat ───────────────────────────────────────────────────


class OllamaUnreachable(RuntimeError):
    """Raised when the Ollama daemon cannot be reached at all."""


class OllamaHTTPError(RuntimeError):
    """Raised when Ollama responds with a non-2xx status."""

    def __init__(self, message: str, status: int = 0):
        super().__init__(message)
        self.status = status


def stream_chat(
    body: Dict[str, Any],
    *,
    abort_check: Optional[Callable[[], bool]] = None,
    connect_timeout: float = 10.0,
) -> Iterator[HarnessEvent]:
    """POST ``body`` to ``/api/chat`` and yield normalised events as they arrive.

    The caller controls request shaping — assemble the body with the
    helpers in :mod:`Wylde.Core.harness.backend.request_building`
    (``build_chat_messages`` + ``build_ollama_options`` +
    ``resolve_keep_alive``).

    ``abort_check`` is an optional zero-arg callable returning truthy when the
    caller wants to stop reading mid-stream (e.g. user cancelled the turn).
    """
    try:
        resp = requests.post(
            f"{OLLAMA_URL}/api/chat",
            json=body,
            stream=True,
            timeout=(connect_timeout, None),
        )
    except requests.RequestException as exc:
        raise OllamaUnreachable(str(exc)) from exc

    if not resp.ok:
        text = ""
        try:
            text = resp.text[:300]
        except Exception:
            pass
        raise OllamaHTTPError(
            text or f"Ollama error {resp.status_code}", status=resp.status_code
        )

    try:
        for raw_line in resp.iter_lines(decode_unicode=True):
            if abort_check is not None and abort_check():
                return
            if not raw_line:
                continue
            try:
                obj = json.loads(raw_line)
            except json.JSONDecodeError:
                continue

            if obj.get("error"):
                yield StreamError(message=str(obj["error"]))
                return

            msg = obj.get("message") or {}
            if msg.get("thinking"):
                yield ThinkingToken(text=msg["thinking"])
            if msg.get("content"):
                yield AssistantToken(text=msg["content"])
            if msg.get("tool_calls"):
                for call in msg["tool_calls"]:
                    yield ToolCallDelta(call=call)

            if obj.get("done"):
                yield Done(raw=obj)
                return
    finally:
        try:
            resp.close()
        except Exception:
            pass


__all__ = [
    "OLLAMA_URL",
    "check_health",
    "list_models",
    "list_models_detailed",
    "list_loaded_models",
    "list_loaded_models_detailed",
    "show_model",
    "delete_model",
    "unload_model",
    "pull_model",
    "normalize_pull_name",
    "stream_chat",
    "OllamaUnreachable",
    "OllamaHTTPError",
]
