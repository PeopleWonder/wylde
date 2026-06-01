"""Phase-9 VPN smoke — static import sanity check.

The VPN package is platform-sensitive: most of the tunnel layer relies
on ``wg`` / ``wg-quick`` / ``iptables`` (Linux only), and pairing
expects ``qrcode`` to be installed (optional dependency). These tests
don't exercise that — they verify that every module under the new
:mod:`Wylde.VPN` layout imports cleanly on a stock Windows-or-Linux
dev box, with the audit's locked-in module shape.

What we DO verify
-----------------
* Every new submodule (``tunnel``, ``nat``, ``discovery``, ``peers``,
  ``monitoring``) imports without raising.
* Submodules that the audit said should be split actually split that
  way (``peers.pairing`` and ``peers.rehydrate`` are separate modules
  exporting the documented surface).
* ``stun_client.py`` and ``stun_prober.py`` are merged into one
  ``nat.stun`` module that exposes both APIs.
* The ``tools/`` package is gone — TOOLS / TOOL_HANDLERS were dead
  code per the audit and are no longer importable.
* ``mobile_proxy.py`` and ``_conversations_store.py`` are gone.
* The Consul auto-synced shim is gone (``consul_client``,
  ``ipc``, ``manifest``, ``errors`` no longer live as VPN-internal
  copies — Core/shared owns those).

Run from a vault-rooted shell::

    py -3 -m pytest "Wylde/VPN/tests/" -v
"""

from __future__ import annotations

import importlib
import sys
from pathlib import Path

import pytest

_HERE = Path(__file__).resolve()
# Point sys.path at the VPN root (sibling of this tests/ dir) so the
# package's top-level "config" / "peers" / "tunnel" / ... imports
# resolve. The VPN code uses bare imports throughout — that mirrors the
# launcher's path setup in run.py / entrypoint.sh.
_VPN_ROOT = _HERE.parent.parent
if str(_VPN_ROOT) not in sys.path:
    sys.path.insert(0, str(_VPN_ROOT))


# ── 1. New layout imports cleanly ──────────────────────────────────────


@pytest.mark.parametrize(
    "module_name",
    [
        "config",
        "tunnel",
        "tunnel.wireguard",
        "tunnel.dns_stub",
        "tunnel.control",
        "nat",
        "nat.stun",
        "nat.turn",
        "nat.hole_puncher",
        "nat.endpoint_updater",
        "discovery",
        "discovery.mdns",
        "discovery.ddns",
        "peers",
        "peers.store",
        "peers.pairing",
        "peers.push",
        "peers.rehydrate",
        "monitoring",
        "monitoring.tunnel_health",
    ],
)
def test_module_imports(module_name: str) -> None:
    """Every audit-mandated module must import without raising."""
    mod = importlib.import_module(module_name)
    assert mod is not None


# ── 2. Module-level surface is the one the audit specified ─────────────


def test_stun_merge_exposes_both_apis() -> None:
    """``stun_client.py`` + ``stun_prober.py`` were merged into
    ``nat.stun`` — the lite ``discover_endpoint`` and the full
    ``classify`` should both be public, with ``classify_nat`` as the
    backwards-compat alias the legacy lite client used."""
    from nat import stun

    assert callable(stun.discover_endpoint)
    assert callable(stun.classify)
    assert callable(stun.classify_nat)
    assert stun.classify_nat is stun.classify
    assert callable(stun.punch_hole)


def test_pairing_split_exports() -> None:
    """``tools/wylde_link.py`` was split into ``peers.pairing`` (token
    issuance, QR, register/connect) and ``peers.rehydrate`` (boot-time
    re-push + last-seen worker)."""
    from peers import pairing, rehydrate

    # Pairing: action dispatch + tool-call wrappers.
    for name in (
        "handle_action",
        "render_qr_for_token",
        "register_peer",
        "generate_pair_token",
        "stun_info",
        "list_peers",
        "remove_peer_action",
        "connect",
        "connection_params",
        "run_link_status",
        "run_link_pair",
    ):
        assert callable(getattr(pairing, name)), name

    # Rehydrate: boot + heartbeat workers.
    assert callable(rehydrate.rehydrate_peers)
    assert callable(rehydrate.start_last_seen_worker)


def test_pairing_drops_hardcoded_service_list() -> None:
    """The audit explicitly removed ``_WYLDE_SERVICES`` /
    ``_list_services`` — Gateway's ``/api/services`` route reads
    ``data/manifests/*.json`` instead. A leftover would re-introduce
    the duplication."""
    from peers import pairing

    assert not hasattr(pairing, "_WYLDE_SERVICES")
    assert not hasattr(pairing, "_list_services")


# ── 3. Deleted-file checks ─────────────────────────────────────────────


@pytest.mark.parametrize(
    "module_name",
    [
        # mobile_proxy.py — superseded by Gateway/routes/*.
        "mobile_proxy",
        # _conversations_store helper — file-backed UI state shouldn't
        # live in the network bridge.
        "gateway.routes._conversations_store",
        # Auto-synced Core/shared copies — VPN now imports from Core.shared
        # at runtime, so the local shims should not exist.
        "consul_client",
        "ipc",
        "errors",
        # tools/ submodules — TOOLS / TOOL_HANDLERS had no live consumers.
        # The bare ``tools`` package name is intentionally not in this list:
        # PEP 420 namespace packages can still resolve ``tools`` against
        # any sibling ``tools/`` directory on sys.path even when this VPN
        # tree no longer has one. The submodules below are the real
        # signal — they would only resolve to live code if a
        # ``Wylde/VPN/tools/<name>.py`` actually existed.
        "tools.vpn_control",
        "tools.wylde_link",
    ],
)
def test_deleted_modules_are_gone(module_name: str) -> None:
    """Modules the audit marked DELETE should no longer be importable
    from inside the VPN root."""
    with pytest.raises(ImportError):
        importlib.import_module(module_name)


# ── 4. Routes left the VPN ─────────────────────────────────────────────


def test_no_gateway_routes_under_vpn() -> None:
    """All ``gateway/routes/*`` blueprints moved to
    ``Wylde.Gateway.routes``. The VPN package should not host any of
    them anymore."""
    with pytest.raises(ImportError):
        importlib.import_module("gateway.routes.chat")
    with pytest.raises(ImportError):
        importlib.import_module("gateway.routes.workflows")


def test_api_module_imports_and_has_flask_app() -> None:
    """The gutted management API still loads and exposes the Flask app
    with the trimmed Phase-9 surface."""
    api = importlib.import_module("api")
    assert api.app is not None
    rule_paths = {rule.rule for rule in api.app.url_map.iter_rules()}
    # VPN-internal control plane stays.
    for path in (
        "/health",
        "/api/vpn/status",
        "/api/vpn/enable",
        "/api/vpn/disable",
        "/api/vpn/keygen",
        "/api/link/status",
        "/api/link/pair",
        "/api/link/register",
        "/api/link/peers",
        "/api/link/connect",
        "/api/link/qr/<token>",
        "/api/link/config",
        "/api/restart",
    ):
        assert path in rule_paths, f"missing {path}"
    # Mobile-proxy + push surface is gone.
    for stripped in (
        "/api/link/mobile/services",
        "/api/link/mobile/command",
        "/api/link/push/subscribe",
        "/api/link/push/pending",
        "/api/link/push/notify",
    ):
        assert stripped not in rule_paths, f"unexpected leftover {stripped}"
