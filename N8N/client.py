"""Wylde.N8N.client — authenticated REST client for n8n.

This module is the single, in-process surface for talking to a running
n8n instance. It folds together what used to live in three files inside
the now-deleted ``_n8n_service_merge/`` Flask shell:

* ``config.py`` — env vars + a process-global ``requests.Session``
* ``client.py`` — ``_n8n_request`` and the verb helpers
* ``tools/<verb>.py`` — one file per workflow operation

That shape made sense when each tool was an HTTP route inside a Flask
app. Phase 8 collapses the service: the harness imports this module
directly and the seven service-owned tools at ``Wylde/N8N/tools/``
each call one of the public functions below. (Phase 8.5 hoisted those
tools out of the harness ``tools/`` tree into the service folder, per
the new principle that services host the LLM-callable tools they
expose.)

External dependencies: ``requests`` only. n8n itself is an external
service (HTTP allowed per Wylde Design Principle 9 — services we don't
own may use HTTP).

Auth modes
----------
Two modes are supported, picked at import time based on which env vars
are set:

* **API key** (``WYLDE_N8N_API_KEY``): preferred when configured. Sent as
  the ``X-N8N-API-KEY`` header on every request. Stateless.
* **Login session** (``WYLDE_N8N_EMAIL`` + ``WYLDE_N8N_PASSWORD``):
  fallback. The first authenticated request triggers ``_login()`` which
  POSTs ``/rest/login`` and stores the cookie on a module-level
  ``requests.Session``. A 401 mid-session triggers a single
  re-authentication retry.

If neither mode is configured the module still imports cleanly; calls
will fail fast with an error dict ``{"error": "..."}`` rather than
crashing. This is intentional — it keeps the harness catalog buildable
on a machine that hasn't wired n8n yet.

Public API
----------
Every function takes plain Python args, returns a dict, and never
raises on transport errors. Callers (the harness tools) just propagate
the dict back into the tool-runner envelope. On success each function
returns shape-stable fields documented in the docstring; on failure it
returns ``{"error": "...", "detail": "..."}``.
"""

from __future__ import annotations

import logging
import os
import threading
from typing import Any, Dict, Optional

import requests

logger = logging.getLogger("wylde.n8n.client")


# ── Config (read once at import; env wins) ───────────────────────────────


N8N_URL = os.getenv("WYLDE_N8N_URL", os.getenv("N8N_URL", "http://127.0.0.1:5678"))
N8N_EMAIL = os.getenv("WYLDE_N8N_EMAIL", os.getenv("N8N_EMAIL", ""))
N8N_PASSWORD = os.getenv("WYLDE_N8N_PASSWORD", os.getenv("N8N_PASSWORD", ""))
N8N_API_KEY = os.getenv("WYLDE_N8N_API_KEY", os.getenv("N8N_API_KEY", ""))
N8N_BASIC_USER = os.getenv(
    "WYLDE_N8N_BASIC_AUTH_USER", os.getenv("N8N_BASIC_AUTH_USER", "")
)
N8N_BASIC_PASS = os.getenv(
    "WYLDE_N8N_BASIC_AUTH_PASSWORD", os.getenv("N8N_BASIC_AUTH_PASSWORD", "")
)

# Auth is "ready" if at least one credential mode is configured. Calls
# made before auth is wired return a structured error rather than a
# transport exception.
_AUTH_READY = bool(N8N_API_KEY) or (N8N_EMAIL and N8N_PASSWORD)

# Persistent session for cookie-based auth + connection reuse.
_session = requests.Session()
if N8N_BASIC_USER or N8N_BASIC_PASS:
    _session.auth = (N8N_BASIC_USER, N8N_BASIC_PASS)
_session_lock = threading.Lock()


# ── Low-level transport ──────────────────────────────────────────────────


def _login() -> bool:
    """POST /rest/login with email+password; store the session cookie.

    Returns True on success, False otherwise. Transport errors are
    downgraded to warnings (the next request can retry); credential
    rejections (401/403) are logged at ERROR because retrying without a
    config change will keep failing.
    """
    if not (N8N_EMAIL and N8N_PASSWORD):
        return False
    try:
        r = _session.post(
            f"{N8N_URL}/rest/login",
            json={"emailOrLdapLoginId": N8N_EMAIL, "password": N8N_PASSWORD},
            timeout=10,
        )
    except requests.Timeout as e:
        logger.warning("n8n login timed out: %s", e)
        return False
    except requests.ConnectionError as e:
        logger.warning("n8n login connection error: %s", e)
        return False
    except requests.RequestException as e:
        logger.warning("n8n login transport error: %s", e)
        return False

    if r.status_code == 200:
        logger.info("Authenticated with n8n at %s", N8N_URL)
        return True
    if r.status_code in (401, 403):
        logger.error("n8n rejected credentials: %s %s", r.status_code, r.text[:200])
    else:
        logger.warning("n8n login failed: %s %s", r.status_code, r.text[:200])
    return False


def _request(
    method: str, path: str, body: Optional[Dict[str, Any]] = None, timeout: int = 30
) -> requests.Response:
    """Authenticated request against n8n. Retries once on 401 if using session auth."""
    url = f"{N8N_URL}{path}"
    headers: Dict[str, str] = {}
    if body is not None:
        headers["Content-Type"] = "application/json"
    if N8N_API_KEY:
        headers["X-N8N-API-KEY"] = N8N_API_KEY

    with _session_lock:
        r = _session.request(method, url, json=body, headers=headers, timeout=timeout)
        # Session expired? Re-auth once, but only when we're using session
        # mode (an API-key 401 means the key is bad — retrying won't help).
        if r.status_code == 401 and not N8N_API_KEY:
            logger.info("n8n session expired; re-authenticating")
            if _login():
                r = _session.request(
                    method, url, json=body, headers=headers, timeout=timeout
                )
        return r


def _err_envelope(message: str, **extra: Any) -> Dict[str, Any]:
    out: Dict[str, Any] = {"error": message}
    out.update(extra)
    return out


def _check_auth() -> Optional[Dict[str, Any]]:
    """If auth isn't configured, return a structured error; else None."""
    if not _AUTH_READY:
        return _err_envelope(
            "n8n auth not configured (set WYLDE_N8N_API_KEY or "
            "WYLDE_N8N_EMAIL+WYLDE_N8N_PASSWORD)",
            code="auth_not_configured",
        )
    return None


# ── Public API ───────────────────────────────────────────────────────────


def list_workflows() -> Dict[str, Any]:
    """Return ``{"workflows": [{id, name, active, description}, ...], "count": N}``."""
    err = _check_auth()
    if err:
        return err
    try:
        resp = _request("GET", "/rest/workflows", timeout=10)
    except requests.RequestException as exc:
        return _err_envelope(f"transport error: {exc}")

    if resp.status_code != 200:
        return _err_envelope(
            f"n8n returned HTTP {resp.status_code}", detail=resp.text[:500]
        )

    payload = resp.json()
    workflows = payload.get("data", payload) if isinstance(payload, dict) else []
    if not isinstance(workflows, list):
        workflows = []
    return {
        "workflows": [
            {
                "id": str(w.get("id")),
                "name": w.get("name"),
                "active": bool(w.get("active", False)),
                "description": w.get("description", ""),
            }
            for w in workflows
        ],
        "count": len(workflows),
    }


def get_workflow(workflow_id: str) -> Dict[str, Any]:
    """Fetch a workflow definition by ID. Returns the workflow dict or an error."""
    err = _check_auth()
    if err:
        return err
    if workflow_id is None or str(workflow_id) == "":
        return _err_envelope("workflow_id is required")
    try:
        resp = _request("GET", f"/rest/workflows/{workflow_id}", timeout=10)
    except requests.RequestException as exc:
        return _err_envelope(f"transport error: {exc}")

    if resp.status_code == 404:
        return _err_envelope(f"Workflow {workflow_id!r} not found", code="not_found")
    if resp.status_code != 200:
        return _err_envelope(
            f"n8n returned HTTP {resp.status_code}", detail=resp.text[:500]
        )

    body = resp.json()
    workflow = body.get("data", body) if isinstance(body, dict) else body
    return {"workflow": workflow}


def get_execution(execution_id: str) -> Dict[str, Any]:
    """Fetch an execution's status payload by ID. Returns the execution dict or an error.

    Mirrors :func:`get_workflow`: read-only ``GET /rest/executions/<id>``,
    structured error envelope on transport / 404 / non-2xx, otherwise the
    raw execution payload under the ``execution`` key.
    """
    err = _check_auth()
    if err:
        return err
    if execution_id is None or str(execution_id) == "":
        return _err_envelope("execution_id is required")
    try:
        resp = _request("GET", f"/rest/executions/{execution_id}", timeout=10)
    except requests.RequestException as exc:
        return _err_envelope(f"transport error: {exc}")

    if resp.status_code == 404:
        return _err_envelope(f"Execution {execution_id!r} not found", code="not_found")
    if resp.status_code != 200:
        return _err_envelope(
            f"n8n returned HTTP {resp.status_code}", detail=resp.text[:500]
        )

    body = resp.json()
    execution = body.get("data", body) if isinstance(body, dict) else body
    return {"execution": execution}


def execute_workflow(
    workflow_id: str, inputs: Optional[Dict[str, Any]] = None
) -> Dict[str, Any]:
    """Run a workflow by ID. Returns ``{execution_id, status, data}`` or an error.

    ``inputs`` is forwarded as the workflow's run-time data payload (n8n
    wraps it as ``{"data": <inputs>}``).
    """
    err = _check_auth()
    if err:
        return err
    if workflow_id is None or str(workflow_id) == "":
        return _err_envelope("workflow_id is required")

    # n8n workflow IDs are numeric. Fail fast on obvious typos and avoid
    # path-injection surface before the request goes out.
    workflow_id_str = str(workflow_id)
    if not workflow_id_str.isdigit():
        return _err_envelope("workflow_id must be a numeric string")

    body = {"data": inputs or {}}
    try:
        resp = _request(
            "POST", f"/rest/workflows/{workflow_id_str}/run", body=body, timeout=60
        )
    except requests.Timeout:
        return _err_envelope("Workflow execution timed out")
    except requests.RequestException as exc:
        return _err_envelope(f"transport error: {exc}")

    if resp.status_code != 200:
        return _err_envelope(
            f"n8n returned HTTP {resp.status_code}", detail=resp.text[:500]
        )

    payload = resp.json()
    result = payload.get("data", payload) if isinstance(payload, dict) else {}
    return {
        "execution_id": result.get("executionId"),
        "status": result.get("status", "completed"),
        "data": result.get("data"),
    }


def create_workflow(payload: Dict[str, Any]) -> Dict[str, Any]:
    """Create a new workflow. ``payload`` is forwarded mostly verbatim.

    ``payload`` keys: ``name`` (required), ``nodes``, ``connections``,
    ``active``. Anything else is passed through. Returns
    ``{workflow_id, name, active, created_at}`` on success.
    """
    err = _check_auth()
    if err:
        return err
    name = payload.get("name") if isinstance(payload, dict) else None
    if not name:
        return _err_envelope("name is required")

    body = {
        "name": name,
        "nodes": payload.get("nodes", []),
        "connections": payload.get("connections", {}),
        "active": payload.get("active", False),
        "settings": payload.get("settings", {}),
    }

    try:
        resp = _request("POST", "/rest/workflows", body=body, timeout=30)
    except requests.RequestException as exc:
        return _err_envelope(f"transport error: {exc}")

    if resp.status_code not in (200, 201):
        return _err_envelope(
            f"n8n returned HTTP {resp.status_code}", detail=resp.text[:500]
        )

    body_out = resp.json()
    w = body_out.get("data", body_out) if isinstance(body_out, dict) else {}
    return {
        "workflow_id": str(w.get("id")) if w.get("id") is not None else None,
        "name": w.get("name"),
        "active": bool(w.get("active", False)),
        "created_at": w.get("createdAt"),
    }


def edit_workflow(workflow_id: str, payload: Dict[str, Any]) -> Dict[str, Any]:
    """PATCH an existing workflow. Only keys present in ``payload`` are sent.

    Recognised keys: ``name``, ``nodes``, ``connections``, ``active``.
    Returns ``{workflow_id, name, active, updated_at}`` on success.
    """
    err = _check_auth()
    if err:
        return err
    if not workflow_id:
        return _err_envelope("workflow_id is required")
    if not isinstance(payload, dict):
        return _err_envelope("payload must be a dict")

    allowed = {"name", "nodes", "connections", "active"}
    body = {k: v for k, v in payload.items() if k in allowed and v is not None}
    if not body:
        return _err_envelope("No updatable fields provided")

    try:
        resp = _request(
            "PATCH", f"/rest/workflows/{workflow_id}", body=body, timeout=30
        )
    except requests.RequestException as exc:
        return _err_envelope(f"transport error: {exc}")

    if resp.status_code == 404:
        return _err_envelope(f"Workflow {workflow_id!r} not found", code="not_found")
    if resp.status_code != 200:
        return _err_envelope(
            f"n8n returned HTTP {resp.status_code}", detail=resp.text[:500]
        )

    body_out = resp.json()
    w = body_out.get("data", body_out) if isinstance(body_out, dict) else {}
    return {
        "workflow_id": str(w.get("id")) if w.get("id") is not None else workflow_id,
        "name": w.get("name"),
        "active": bool(w.get("active", False)),
        "updated_at": w.get("updatedAt"),
    }


def delete_workflow(workflow_id: str) -> Dict[str, Any]:
    """Permanently delete a workflow. Archives first (n8n requirement).

    Returns ``{"deleted": True, "workflow_id": ...}`` on success.
    """
    err = _check_auth()
    if err:
        return err
    if not workflow_id:
        return _err_envelope("workflow_id is required")

    try:
        # n8n requires archiving before delete. A 404 on archive means the
        # workflow doesn't exist; any other non-2xx aborts before delete.
        archive = _request(
            "POST", f"/rest/workflows/{workflow_id}/archive", body={}, timeout=10
        )
    except requests.RequestException as exc:
        return _err_envelope(f"transport error during archive: {exc}")

    if archive.status_code == 404:
        return _err_envelope(f"Workflow {workflow_id!r} not found", code="not_found")
    if archive.status_code not in (200, 201):
        return _err_envelope(
            f"Failed to archive workflow: HTTP {archive.status_code}",
            detail=archive.text[:500],
        )

    try:
        resp = _request("DELETE", f"/rest/workflows/{workflow_id}", timeout=10)
    except requests.RequestException as exc:
        return _err_envelope(f"transport error during delete: {exc}")

    if resp.status_code == 404:
        return _err_envelope(
            f"Workflow {workflow_id!r} not found after archive", code="not_found"
        )
    if resp.status_code not in (200, 204):
        return _err_envelope(
            f"n8n returned HTTP {resp.status_code}", detail=resp.text[:500]
        )

    return {"deleted": True, "workflow_id": str(workflow_id)}


__all__ = [
    "list_workflows",
    "get_workflow",
    "get_execution",
    "execute_workflow",
    "create_workflow",
    "edit_workflow",
    "delete_workflow",
    # Module-level config exposed for tests / advanced callers
    "N8N_URL",
]
