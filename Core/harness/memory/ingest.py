"""Indexing pipeline — N8N trigger client.

Per the Wylde N8N principle ("all workflows go through N8N"), the actual
ingestion pipeline (file discovery, chunking, entity extraction, embedding,
LanceDB write, graph upsert) is an N8N workflow. This module is the thin
client that triggers the workflow and reports its outcome.

If you need to debug or evolve the ingestion logic, edit the N8N workflow —
not this file. This file should only ever change when the trigger contract
itself changes (input fields, response shape, webhook URL).

Public surface:

* :func:`trigger_ingest`       — kick off an ingest workflow run.
* :func:`get_ingest_status`    — poll execution status by id.

Both functions fail open: an unreachable N8N returns a structured error
envelope rather than raising, so a missing N8N installation doesn't crash
the harness.
"""

from __future__ import annotations

import logging
import os
from typing import Any, Dict, List, Optional

import requests

logger = logging.getLogger("wylde.harness.memory.ingest")


# OPEN QUESTION (see MEMORY_MIGRATION_MAP.md §4.7): confirm webhook URL pattern.
# Defaults below assume the local N8N convention.
_N8N_BASE_URL = os.getenv("WYLDE_N8N_BASE_URL", "http://127.0.0.1:5678").rstrip("/")
_INGEST_WEBHOOK = (
    os.getenv("WYLDE_N8N_INGEST_WEBHOOK", "/webhook/wylde-ingest").lstrip("/")
    or "webhook/wylde-ingest"
)
_DEFAULT_TIMEOUT_S = 10.0


def trigger_ingest(
    *,
    target_path: str,
    workspace_id: str = "default",
    paths: Optional[List[str]] = None,
    options: Optional[Dict[str, Any]] = None,
    timeout: float = _DEFAULT_TIMEOUT_S,
) -> Dict[str, Any]:
    """Trigger the ingest workflow in N8N. Returns ``{ok, execution_id, ...}``.

    ``target_path``  — root path the workflow indexes.
    ``workspace_id`` — logical workspace bucket (controls graph + chunk filters).
    ``paths``        — optional explicit subset (skip discovery).
    ``options``      — pass-through dict for workflow-specific knobs (e.g.
                       ``{"force_reindex": True}``).
    """
    payload: Dict[str, Any] = {
        "target_path": target_path,
        "workspace_id": workspace_id,
    }
    if paths:
        payload["paths"] = list(paths)
    if options:
        payload["options"] = dict(options)

    url = f"{_N8N_BASE_URL}/{_INGEST_WEBHOOK}"
    try:
        resp = requests.post(url, json=payload, timeout=timeout)
    except requests.RequestException as exc:
        logger.warning("ingest: N8N webhook unreachable (%s): %s", url, exc)
        return {"ok": False, "error": "unreachable", "detail": str(exc)}

    if not resp.ok:
        return {
            "ok": False,
            "error": f"http_{resp.status_code}",
            "detail": resp.text[:500],
        }

    try:
        body = resp.json()
    except ValueError:
        # N8N sometimes returns plain text for trivial workflows.
        return {"ok": True, "raw": resp.text}
    return (
        {"ok": True, **body} if isinstance(body, dict) else {"ok": True, "result": body}
    )


def get_ingest_status(
    execution_id: str, *, timeout: float = _DEFAULT_TIMEOUT_S
) -> Dict[str, Any]:
    """Poll N8N for the status of a running or finished execution.

    Uses the standard N8N REST API path ``/rest/executions/{id}``. If the
    deployment uses a different status surface, override ``WYLDE_N8N_BASE_URL``
    or call the trigger workflow with a webhook that returns status inline.
    """
    if not execution_id:
        return {"ok": False, "error": "missing_execution_id"}
    url = f"{_N8N_BASE_URL}/rest/executions/{execution_id}"
    try:
        resp = requests.get(url, timeout=timeout)
    except requests.RequestException as exc:
        return {"ok": False, "error": "unreachable", "detail": str(exc)}
    if not resp.ok:
        return {
            "ok": False,
            "error": f"http_{resp.status_code}",
            "detail": resp.text[:500],
        }
    try:
        return {"ok": True, **resp.json()}
    except ValueError:
        return {"ok": True, "raw": resp.text}


__all__ = ["trigger_ingest", "get_ingest_status"]
