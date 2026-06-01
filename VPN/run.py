#!/usr/bin/env python3
"""VPN service entry point — hosts the WyldeLink management API on
``\\\\.\\pipe\\wylde-vpn``.

VPN is the peer-to-peer tunnel layer (WireGuard + TURN relay) that
lets mobile devices reach Gateway from outside the LAN.  This module
is the Windows process wrapper: it reads ``config.yaml``, copies
values into the environment, configures logging, writes the service
manifest, starts a heartbeat, installs SIGINT/SIGTERM handlers that
mark the manifest stopped, and hands off to :func:`api.main` which
serves the Flask app over named-pipe IPC (with HTTP fallback when
ipc isn't importable).

Daemon-launched in production by direct script path
(``py -3 "VPN/run.py"``); standalone-runnable for dev the same way.
The Linux/Docker path uses ``entrypoint.sh`` instead — that script
also brings up wg-quick / iptables.  On Windows the management API
serves but tunnel-control endpoints return errors because wg-quick /
iptables / boringtun aren't present.

File layout — ``main()`` is defined first so the canonical
startup-sequence rule (Core/harness/dev/wylde_check rule 18) matches
its calls in source order; helper functions come after.
"""

from __future__ import annotations

import logging
import os
import signal
import sys
from pathlib import Path

import yaml

HERE = Path(__file__).parent.resolve()
SERVICE_NAME = "wylde-vpn"
logger = logging.getLogger("wylde.vpn.run")


def main() -> None:
    cfg = _load_config()
    _apply_config_to_env(cfg)

    sys.path.insert(0, str(HERE))

    # configure_logging / manifest primitives are imported lazily because
    # VPN is launched by direct script path; when invoked standalone
    # Core.shared may not be on sys.path. Best-effort wrap so the rest
    # of the boot sequence still runs (api.main still hosts the pipe).
    try:
        from Core.shared.logging_setup import configure_logging
        from Core.shared.manifest import (
            mark_stopped,
            start_heartbeat,
            write_manifest,
        )
    except ImportError:

        def configure_logging(*args: object, **kwargs: object) -> None:  # type: ignore[misc]
            return None

        def write_manifest(*args: object, **kwargs: object) -> None:  # type: ignore[misc]
            return None

        def start_heartbeat(*args: object, **kwargs: object) -> None:  # type: ignore[misc]
            return None

        def mark_stopped(*args: object, **kwargs: object) -> None:  # type: ignore[misc]
            return None

    if configure_logging is not None:
        configure_logging(service=SERVICE_NAME)
    if write_manifest is not None:
        write_manifest(
            service_name=SERVICE_NAME,
            port=int(os.environ.get("PORT", 8020)),
            category="network",
            description=(
                "WyldeLink — peer-to-peer VPN with WireGuard tunnels and TURN relay "
                "for remote access"
            ),
            contributes={
                "dashboard": {
                    "label": "WyldeLink",
                    "icon": "wifi",
                    "color": "orange",
                },
                "tools": [],
                "commands": [],
                "settings": [],
            },
            entry_point="python:VPN.run",
        )
    if start_heartbeat is not None:
        start_heartbeat(SERVICE_NAME)
    _install_signal_handlers()

    from api import main as svc_main  # noqa: E402

    try:
        svc_main()
    finally:
        if mark_stopped is not None:
            mark_stopped(SERVICE_NAME)


def _set_env(name: str, value: object) -> None:
    if value is None or name in os.environ:
        return
    os.environ[name] = str(value)


def _resolve_path(value: object, default: Path) -> Path:
    p = Path(str(value)).expanduser() if value else default
    p.mkdir(parents=True, exist_ok=True)
    return p


def _load_config() -> dict:
    cfg_path = HERE / "config.yaml"
    if not cfg_path.exists():
        return {}
    with open(cfg_path, "r", encoding="utf-8") as f:
        return yaml.safe_load(f) or {}


def _apply_config_to_env(cfg: dict) -> None:
    service = cfg.get("service", {})
    vpn = cfg.get("vpn", {})
    link = cfg.get("link", {})
    relay = link.get("relay", {})
    dns = cfg.get("dns_stub", {})

    _set_env("PORT", service.get("port", 8020))

    _set_env("VPN_ENABLED", str(bool(vpn.get("enabled", False))).lower())
    _set_env("VPN_ENDPOINT", vpn.get("endpoint", ""))
    _set_env("VPN_PEER_PUBKEY", vpn.get("peer_pubkey", ""))
    _set_env("VPN_PRIVATE_KEY", vpn.get("private_key", ""))
    _set_env("VPN_TUNNEL_ADDR", vpn.get("tunnel_addr", "10.8.0.2/24"))
    _set_env("VPN_DNS", vpn.get("dns", "1.1.1.1"))
    _set_env("VPN_ALLOWED_IPS", vpn.get("allowed_ips", "0.0.0.0/0, ::/0"))

    _set_env("LINK_ENABLED", str(bool(link.get("enabled", False))).lower())
    _set_env("LINK_PRIVATE_KEY", link.get("private_key", ""))
    _set_env("LINK_TUNNEL_ADDR", link.get("tunnel_addr", "192.0.2.1/24"))
    _set_env("LINK_LISTEN_PORT", link.get("listen_port", 51821))
    _set_env(
        "LINK_STUN_SERVERS",
        link.get("stun_servers", "stun.l.google.com:19302,stun1.l.google.com:19302"),
    )
    link_data = _resolve_path(link.get("data_dir"), HERE / "data" / "wylde-link")
    _set_env("LINK_DATA_DIR", str(link_data))
    _set_env("LINK_TOKEN_TTL", link.get("token_ttl", 300))
    _set_env("LINK_PAIR_RATE_MAX", link.get("pair_rate_max", 5))
    _set_env("LINK_PAIR_RATE_WIN", link.get("pair_rate_win", 60))
    _set_env("LINK_PEER_SUBNET", link.get("peer_subnet", "192.0.2.0/24"))
    _set_env("LINK_PUBLIC_HOST", link.get("public_host", ""))

    _set_env("LINK_RELAY_HOST", relay.get("host", ""))
    _set_env("LINK_RELAY_PORT", relay.get("port", 3478))
    _set_env("LINK_RELAY_USER", relay.get("user", "wylde"))
    _set_env("LINK_RELAY_PASS", relay.get("password", ""))
    _set_env("LINK_RELAY_REALM", relay.get("realm", "wylde.local"))
    _set_env("LINK_RELAY_LIFETIME", relay.get("lifetime", 600))

    _set_env("DNS_STUB_HOST", dns.get("host", "0.0.0.0"))
    _set_env("DNS_STUB_PORT", dns.get("port", 5300))


def _write_manifest_if_available() -> None:
    """Best-effort manifest write — uses Core/shared/manifest if importable."""
    try:
        from Core.shared.manifest import write_manifest, start_heartbeat
    except ImportError:
        return
    write_manifest(
        service_name=SERVICE_NAME,
        port=int(os.environ.get("PORT", 8020)),
        category="network",
        description=(
            "WyldeLink — peer-to-peer VPN with WireGuard tunnels and TURN relay "
            "for remote access"
        ),
        contributes={
            "dashboard": {"label": "WyldeLink", "icon": "wifi", "color": "orange"},
            "tools": [],
            "commands": [],
            "settings": [],
        },
        entry_point="python:VPN.run",
    )
    start_heartbeat(SERVICE_NAME)


def _mark_stopped_if_available() -> None:
    """Best-effort manifest-stopped flip — used by shutdown signal handler."""
    try:
        from Core.shared.manifest import mark_stopped
    except ImportError:
        return
    mark_stopped(SERVICE_NAME)


def _install_signal_handlers() -> None:
    def _handler(signum: int, _frame: object) -> None:
        logger.info("vpn: signal %s, shutting down", signum)
        _mark_stopped_if_available()
        # Re-raise via default handler so the Flask/api shutdown path
        # also unwinds — otherwise the signal is swallowed and the
        # serve loop keeps spinning.
        signal.signal(signum, signal.SIG_DFL)
        signal.raise_signal(signum)

    for sig_name in ("SIGINT", "SIGTERM"):
        sig = getattr(signal, sig_name, None)
        if sig is None:
            continue
        try:
            signal.signal(sig, _handler)
        except (ValueError, OSError):
            pass


if __name__ == "__main__":
    main()
