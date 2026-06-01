"""Embedding bridge — turns text into vectors for the vector store.

Wylde used to host its own embedding service (``wylde-rag/embedder.py``); the
harness now owns this responsibility. The backend URL is sourced from
:mod:`Wylde.Core.harness.backend.ollama_client` so every harness module shares
one definition of where Ollama lives. This module remains a thin retry /
shape-validation / Matryoshka-truncation wrapper on top of a direct
``/api/embed`` call — no shared client object yet because the embed surface
is small enough to inline and using `urllib` keeps this module dep-light.

Public surface (matches the legacy embedder):

* :func:`embed`          — embed a list of texts in one call.
* :func:`embed_one`      — convenience wrapper for a single text.
* :exc:`EmbedError`      — generic embedding failure (transient, retried).
* :exc:`EmbedModelMissing` — embedding model not pulled (404 from backend).
* :func:`health_check`   — cheap end-to-end check used by ``/health/deep``.

Matryoshka note: nomic-embed-text and friends pack the most discriminative
information in the leading dimensions, so prefix-slicing to a smaller
``EMBED_DIM`` is a valid quality / cost tradeoff. We re-normalise after the
slice so cosine similarity stays valid.
"""

from __future__ import annotations

import logging
import math
import os
import time
import urllib.error
import urllib.request
from typing import List, Tuple

from ..backend.ollama_client import OLLAMA_URL
from ._common import EMBED_DIM, EMBED_MODEL, EMBED_NATIVE_DIM

logger = logging.getLogger("wylde.harness.memory.embeddings")

# Retry policy for transient backend failures (network blips, model warming up).
# A missing model (HTTP 404) is non-retryable; we surface it immediately so the
# caller can show the pull hint.
_RETRY_ATTEMPTS = 3
_RETRY_BASE_DELAY = 0.5  # seconds; doubles each attempt


class EmbedError(RuntimeError):
    """Embedding failed after all retries."""


class EmbedModelMissing(EmbedError):
    """Backend returned 404 — model not pulled.

    Held distinct from generic :exc:`EmbedError` so the caller can show a
    remediation hint (``ollama pull <model>``) and the health check can
    report an unambiguous reason.
    """


def _truncate_normalize(vec: List[float], dim: int) -> List[float]:
    """Slice the first ``dim`` elements of a Matryoshka embedding and L2-normalise."""
    v = vec[:dim]
    norm = math.sqrt(sum(x * x for x in v))
    if norm > 0.0:
        inv = 1.0 / norm
        v = [x * inv for x in v]
    return v


def _use_pipe() -> bool:
    return os.getenv("WYLDE_HARNESS_OLLAMA_TRANSPORT", "pipe").strip().lower() == "pipe"


def _validate_embeddings(embeddings: object, expected_count: int) -> List[List[float]]:
    """Shared shape validation for both HTTP and pipe paths."""
    if not isinstance(embeddings, list):
        raise EmbedError(
            f"backend response missing 'embeddings' list (got {type(embeddings).__name__})"
        )
    if len(embeddings) != expected_count:
        raise EmbedError(
            f"embedding count mismatch: requested {expected_count}, got {len(embeddings)}"
        )
    if embeddings and len(embeddings[0]) != EMBED_NATIVE_DIM:
        raise EmbedError(
            f"embedding native dim mismatch: expected {EMBED_NATIVE_DIM}d "
            f"from model {EMBED_MODEL!r}, got {len(embeddings[0])}d"
        )
    if EMBED_DIM < EMBED_NATIVE_DIM:
        embeddings = [_truncate_normalize(v, EMBED_DIM) for v in embeddings]
    return embeddings


def _embed_via_pipe(texts: List[str], timeout: int) -> List[List[float]]:
    """Pipe path. Maps service errors to the same EmbedError/EmbedModelMissing
    contract as the HTTP path so callers don't branch."""
    from Core.harness.backend import ollama_pipe
    from Core.harness.backend.ollama_client import (
        OllamaHTTPError,
        OllamaUnreachable,
    )

    try:
        envelope = ollama_pipe.embed_via_pipe(
            EMBED_MODEL, texts, timeout=float(timeout)
        )
    except RuntimeError as exc:
        msg = str(exc)
        if msg.startswith("404"):
            raise EmbedModelMissing(
                f"backend has no model named {EMBED_MODEL!r}. "
                f"Pull it with: ollama pull {EMBED_MODEL} (server response: {msg})"
            ) from exc
        raise EmbedError(f"embed via pipe failed: {msg}") from exc
    except OllamaHTTPError as exc:
        if exc.status == 404:
            raise EmbedModelMissing(
                f"backend has no model named {EMBED_MODEL!r}. "
                f"Pull it with: ollama pull {EMBED_MODEL} (server response: {exc})"
            ) from exc
        raise EmbedError(
            f"backend rejected embed request (HTTP {exc.status}) "
            f"for model {EMBED_MODEL!r}: {exc}"
        ) from exc
    except OllamaUnreachable as exc:
        raise EmbedError(f"backend unreachable: {exc}") from exc

    embeddings = envelope.get("embeddings")
    return _validate_embeddings(embeddings, len(texts))


def embed(texts: List[str], *, timeout: int = 30) -> List[List[float]]:
    """Embed a list of texts. Returns one vector per input.

    Retries on transient failures (connection refused, timeout, 5xx). Fails
    fast on HTTP 404 (model missing) — retrying won't help. Validates shape
    + native dim so a malformed response cannot silently produce a wrong-dim
    vector that LanceDB later rejects.

    Routing: when WYLDE_HARNESS_OLLAMA_TRANSPORT=pipe (default), each
    call goes through the wylde-ollama pipe (which holds a VRAM lease
    for the embed call by default). Otherwise direct HTTP.
    """
    if not texts:
        return []

    if _use_pipe():
        return _embed_via_pipe(texts, timeout)

    url = f"{OLLAMA_URL}/api/embed"
    import json as _json

    body = _json.dumps({"model": EMBED_MODEL, "input": texts}).encode()

    last_exc: Exception | None = None
    for attempt in range(_RETRY_ATTEMPTS):
        try:
            req = urllib.request.Request(
                url,
                data=body,
                headers={"Content-Type": "application/json"},
                method="POST",
            )
            with urllib.request.urlopen(req, timeout=timeout) as resp:
                data = _json.loads(resp.read())

            embeddings = data.get("embeddings")
            if not isinstance(embeddings, list):
                raise EmbedError(
                    f"backend response missing 'embeddings' list: keys={list(data.keys())}"
                )
            if len(embeddings) != len(texts):
                raise EmbedError(
                    f"embedding count mismatch: requested {len(texts)}, got {len(embeddings)}"
                )
            if embeddings and len(embeddings[0]) != EMBED_NATIVE_DIM:
                raise EmbedError(
                    f"embedding native dim mismatch: expected {EMBED_NATIVE_DIM}d "
                    f"from model {EMBED_MODEL!r}, got {len(embeddings[0])}d"
                )
            if EMBED_DIM < EMBED_NATIVE_DIM:
                embeddings = [_truncate_normalize(v, EMBED_DIM) for v in embeddings]
            return embeddings

        except urllib.error.HTTPError as e:
            try:
                err_body = e.read().decode("utf-8", errors="replace")[:500]
            except Exception:
                err_body = ""
            if e.code == 404:
                raise EmbedModelMissing(
                    f"backend has no model named {EMBED_MODEL!r}. "
                    f"Pull it with: ollama pull {EMBED_MODEL} (server response: {err_body})"
                ) from e
            if 400 <= e.code < 500:
                # 4xx that isn't 404 — request-level problem, retrying won't help.
                raise EmbedError(
                    f"backend rejected embed request (HTTP {e.code}) "
                    f"for model {EMBED_MODEL!r}: {err_body}"
                ) from e
            # 5xx — transient, fall through to retry.
            last_exc = e
            logger.warning(
                "embed HTTP %d (attempt %d/%d): %s body=%s",
                e.code,
                attempt + 1,
                _RETRY_ATTEMPTS,
                e,
                err_body,
            )

        except (urllib.error.URLError, TimeoutError, OSError) as e:
            last_exc = e
            logger.warning(
                "embed transient error (attempt %d/%d): %s",
                attempt + 1,
                _RETRY_ATTEMPTS,
                e,
            )

        if attempt < _RETRY_ATTEMPTS - 1:
            time.sleep(_RETRY_BASE_DELAY * (2**attempt))

    raise EmbedError(f"embedding failed after {_RETRY_ATTEMPTS} attempts: {last_exc}")


def embed_one(text: str) -> List[float]:
    """Embed a single text. Convenience wrapper for the common case."""
    return embed([text])[0]


def health_check(timeout: int = 5) -> Tuple[bool, str]:
    """Cheap end-to-end check: embed one short string. Returns (ok, reason).

    Used by ``/health/deep`` (or its harness equivalent) so a missing model
    surfaces loudly instead of silently producing an empty index.
    """
    try:
        vec = embed(["healthcheck"], timeout=timeout)
        if not vec or len(vec[0]) != EMBED_DIM:
            got = len(vec[0]) if vec else 0
            return False, f"degenerate response (got {got} dims, want {EMBED_DIM})"
        return True, "ok"
    except EmbedModelMissing as e:
        return False, str(e)
    except Exception as e:
        return False, f"{type(e).__name__}: {e}"


# TODO(phase 4c): code-specialised embedding channel (legacy ``code_embed``).
# Recommended models when enabled:
#   nomic-embed-text   — current default, strong general baseline.
#   mxbai-embed-large  — 1024d, ~+2 nDCG vs nomic on code.
#   bge-code-v1/large  — code-tuned BGE family.
# When the backend module is wired, expose a ``code_embed`` here that swaps
# the model + native dim for a code-tuned alternative.


__all__ = [
    "EmbedError",
    "EmbedModelMissing",
    "embed",
    "embed_one",
    "health_check",
]
