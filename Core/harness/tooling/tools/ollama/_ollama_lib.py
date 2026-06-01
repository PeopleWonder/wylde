"""Shared HTTP plumbing for tools/ollama/.

Ollama lives outside Wylde, so HTTP is the legitimate transport here for
the pre-migration path. When ``WYLDE_HARNESS_OLLAMA_TRANSPORT=pipe``
(the default), each Ollama call routes through the Rust ``wylde-ollama``
pipe instead, sharing the warm reqwest::Client pool + VRAM lease
bookkeeping with every other harness Ollama caller.

Reads ``OLLAMA_URL`` from env, defaulting to ``http://127.0.0.1:11434``,
for the direct-HTTP fallback path.
"""

from __future__ import annotations

import json
import os
import urllib.request
from typing import Any, Dict

OLLAMA_URL = os.getenv("OLLAMA_URL", "http://127.0.0.1:11434")
DEFAULT_KEEP_ALIVE = "24h"
VRAM_EVICT_THRESHOLD_MB = int(os.getenv("VRAM_EVICT_THRESHOLD_MB", "20000"))


def _use_pipe() -> bool:
    return os.getenv("WYLDE_HARNESS_OLLAMA_TRANSPORT", "pipe").strip().lower() == "pipe"


def _path_to_action(path: str) -> str | None:
    """Map an Ollama HTTP path to the equivalent wylde-ollama action.

    Returns ``None`` for paths that don't have a pipe equivalent (e.g.
    streaming endpoints) so the caller falls through to direct HTTP.
    """
    mapping = {
        "/api/tags": "ollama.list_models",
        "/api/ps": "ollama.list_loaded",
        "/api/show": "ollama.show",
        "/api/delete": "ollama.delete",
    }
    return mapping.get(path)


def post(path: str, body: Dict[str, Any], timeout: int = 60) -> Dict[str, Any]:
    if _use_pipe():
        action = _path_to_action(path)
        if action is not None:
            try:
                from Core.shared import ipc

                reply = ipc.send_action(
                    "wylde-ollama", action, body, timeout=float(timeout)
                )
                if reply.ok:
                    return reply.data  # type: ignore[no-any-return]
            except Exception:  # noqa: BLE001 — fall through to HTTP
                pass
    req = urllib.request.Request(
        OLLAMA_URL + path,
        data=json.dumps(body).encode("utf-8"),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        raw = resp.read().decode("utf-8")
    try:
        result: Dict[str, Any] = json.loads(raw)
        return result
    except json.JSONDecodeError:
        return {"raw": raw}


def get(path: str, timeout: int = 10) -> Dict[str, Any]:
    if _use_pipe():
        action = _path_to_action(path)
        if action is not None:
            try:
                from Core.shared import ipc

                reply = ipc.send_action(
                    "wylde-ollama", action, {}, timeout=float(timeout)
                )
                if reply.ok:
                    return reply.data  # type: ignore[no-any-return]
            except Exception:  # noqa: BLE001
                pass
    req = urllib.request.Request(OLLAMA_URL + path)
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        result: Dict[str, Any] = json.loads(resp.read().decode("utf-8"))
        return result
