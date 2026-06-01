"""Auth-middleware + tier-gating integration tests.

We don't spin up the full Gateway here — we mount FastAPI routes that
take the device-auth dependencies and assert their behaviour with
``starlette.testclient.TestClient``. The device_gate pipe call is
monkey-patched to return canned responses, so we exercise the
dependency logic without needing a live ``\\\\.\\pipe\\wylde-device-gate``.

What we cover (per spec):

* Valid token → request proceeds, ``request.state.device_auth`` set.
* Invalid token → 401.
* Missing token → 401.
* Destructive tool with read_only tier → 403.
* Destructive tool with destructive_tool_access tier → 200.
* Tool-use tier can call non-destructive tools.
"""

from __future__ import annotations

import sys
from pathlib import Path
from typing import Any, Dict, Tuple

import pytest

# Vault root must be importable so the shared package path
# (``from Core.shared.gateway_auth import ...``) resolves cleanly.
_HERE = Path(__file__).resolve()
_VAULT_ROOT = _HERE.parents[3]
if str(_VAULT_ROOT) not in sys.path:
    sys.path.insert(0, str(_VAULT_ROOT))


fastapi = pytest.importorskip("fastapi")
TestClient = pytest.importorskip("starlette.testclient").TestClient


from fastapi import Depends, FastAPI  # noqa: E402
from starlette.testclient import TestClient as _TestClient  # noqa: E402

from Core.shared.gateway_auth import (  # noqa: E402
    DeviceAuth,
    require_device,
    require_tier,
    require_tool_access,
)
from Core.shared.gateway_auth import device_gate as svc  # noqa: E402


@pytest.fixture
def app(monkeypatch: pytest.MonkeyPatch) -> _TestClient:
    """Mount minimal routes that exercise the auth dependencies.
    Each route is a thin wrapper around the dep so we can assert the
    HTTP-level behaviour without dragging in the rest of the Gateway."""

    # Tokens we recognise:
    valid = {
        "token-readonly": {"device_id": "dev_a", "tier": "read_only"},
        "token-toolers": {"device_id": "dev_b", "tier": "tool_use"},
        "token-everything": {"device_id": "dev_c", "tier": "destructive_tool_access"},
    }

    def _verify_stub(token: str) -> Tuple[int, Dict[str, Any]]:
        if token in valid:
            return 200, dict(valid[token])
        return 400, {
            "ok": False,
            "error": {"code": "invalid_token", "message": "no match"},
        }

    monkeypatch.setattr(svc, "verify", _verify_stub)

    # Stub the destructive-flag lookup so the tool-access dep is
    # deterministic. Two known tools: "fs.read" (non-destructive) and
    # "fs.write" (destructive).
    from Core.shared.gateway_auth import device as auth_device

    def _is_destructive_stub(tool_id: str) -> bool:
        return tool_id == "fs.write"

    monkeypatch.setattr(auth_device, "is_destructive_tool", _is_destructive_stub)

    app = FastAPI()

    @app.get("/whoami")
    def whoami(auth: DeviceAuth = Depends(require_device)) -> Dict[str, Any]:
        return {"device_id": auth.device_id, "tier": auth.tier}

    @app.get("/needs-tool-use")
    def needs_tool_use(
        auth: DeviceAuth = Depends(require_tier("tool_use")),
    ) -> Dict[str, Any]:
        return {"ok": True, "tier": auth.tier}

    @app.get("/run-fs-read")
    def run_fs_read(
        auth: DeviceAuth = Depends(require_tool_access("fs.read")),
    ) -> Dict[str, Any]:
        return {"ok": True, "tool": "fs.read", "tier": auth.tier}

    @app.get("/run-fs-write")
    def run_fs_write(
        auth: DeviceAuth = Depends(require_tool_access("fs.write")),
    ) -> Dict[str, Any]:
        return {"ok": True, "tool": "fs.write", "tier": auth.tier}

    return _TestClient(app)


# ── Auth middleware ──────────────────────────────────────────────────


def test_valid_token_proceeds_with_state(app: _TestClient) -> None:
    r = app.get(
        "/whoami",
        headers={"Authorization": "Bearer token-readonly"},
    )
    assert r.status_code == 200, r.text
    body = r.json()
    assert body == {"device_id": "dev_a", "tier": "read_only"}


def test_invalid_token_401(app: _TestClient) -> None:
    r = app.get("/whoami", headers={"Authorization": "Bearer bogus"})
    assert r.status_code == 401, r.text
    assert r.json()["detail"]["error"]["code"] == "invalid_token"


def test_missing_token_401(app: _TestClient) -> None:
    r = app.get("/whoami")
    assert r.status_code == 401
    assert r.json()["detail"]["error"]["code"] == "missing_token"


def test_malformed_authorization_header_401(app: _TestClient) -> None:
    # Wrong scheme.
    r = app.get("/whoami", headers={"Authorization": "Basic abc"})
    assert r.status_code == 401
    # Missing token component.
    r = app.get("/whoami", headers={"Authorization": "Bearer"})
    assert r.status_code == 401


# ── Tier gating ──────────────────────────────────────────────────────


def test_tier_below_required_403(app: _TestClient) -> None:
    """Read-only device hitting a tool-use route → 403."""
    r = app.get(
        "/needs-tool-use",
        headers={"Authorization": "Bearer token-readonly"},
    )
    assert r.status_code == 403
    assert r.json()["detail"]["error"]["code"] == "tier_insufficient"


def test_tier_at_or_above_required_proceeds(app: _TestClient) -> None:
    r = app.get(
        "/needs-tool-use",
        headers={"Authorization": "Bearer token-toolers"},
    )
    assert r.status_code == 200
    r = app.get(
        "/needs-tool-use",
        headers={"Authorization": "Bearer token-everything"},
    )
    assert r.status_code == 200


# ── Tool-access gating (destructive flag) ───────────────────────────


def test_destructive_tool_with_read_only_403(app: _TestClient) -> None:
    r = app.get(
        "/run-fs-write",
        headers={"Authorization": "Bearer token-readonly"},
    )
    assert r.status_code == 403


def test_destructive_tool_with_tool_use_403(app: _TestClient) -> None:
    """tool_use tier still can't run destructive tools — that's the
    whole point of the third tier."""
    r = app.get(
        "/run-fs-write",
        headers={"Authorization": "Bearer token-toolers"},
    )
    assert r.status_code == 403


def test_destructive_tool_with_destructive_tier_200(app: _TestClient) -> None:
    r = app.get(
        "/run-fs-write",
        headers={"Authorization": "Bearer token-everything"},
    )
    assert r.status_code == 200
    assert r.json()["tool"] == "fs.write"


def test_non_destructive_tool_with_tool_use_200(app: _TestClient) -> None:
    r = app.get(
        "/run-fs-read",
        headers={"Authorization": "Bearer token-toolers"},
    )
    assert r.status_code == 200
    assert r.json()["tool"] == "fs.read"


def test_non_destructive_tool_with_read_only_403(app: _TestClient) -> None:
    """Read-only is chat-view-only — even non-destructive tools need
    the ``tool_use`` tier."""
    r = app.get(
        "/run-fs-read",
        headers={"Authorization": "Bearer token-readonly"},
    )
    assert r.status_code == 403
