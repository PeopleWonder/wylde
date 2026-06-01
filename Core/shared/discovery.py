"""
discovery.py — Unified service discovery across Docker (Consul) and native
Windows (mDNS / zeroconf) services.

Why this exists
---------------
Phase-1 services register themselves with Consul via `consul_client.py`.
As services move to native Windows processes we want them to be
discoverable without each one shipping its own Consul container. mDNS
works zero-config on a single host (and across WSL2 ↔ Windows with the
right firewall posture).

This module is the one place that knows about both. Import it and call
the same three functions regardless of backend:

    from discovery import register_service, install_signal_handlers
    register_service("tool-runner", "tool-runner", 8001, tags=["tools"])
    install_signal_handlers("tool-runner")

Backend selection (env WYLDE_DISCOVERY):
    "consul" . Consul only.
    "mdns"    — mDNS only (requires python-zeroconf).
    "both"   , register + query both.
    "auto"    — default. Probe Consul once; use it if reachable, else
                fall back to mDNS. Queries try whichever backends are
                active, Consul first.

Backwards-compat: `consul_client.py` must still sit next to this file in
every service dir. If it's missing, Consul ops degrade to no-ops and we
log a warning.
"""

import atexit
import logging
import os
import signal
import socket
import sys
import threading
import time
from types import FrameType
from typing import Any, Dict, Iterable, List, Optional

logger = logging.getLogger(__name__)

# ── Backend toggles (resolved lazily in _active_backends) ──────────────
MODE = os.getenv("WYLDE_DISCOVERY", "auto").lower()
MDNS_SERVICE_TYPE = os.getenv("WYLDE_MDNS_TYPE", "_wylde._tcp.local.")

try:
    import consul_client

    _HAS_CONSUL_MODULE = True
except ImportError:
    consul_client = None
    _HAS_CONSUL_MODULE = False

try:
    from zeroconf import (
        IPVersion,
        ServiceBrowser,
        ServiceInfo,
        ServiceListener,
        Zeroconf,
    )

    _HAS_ZEROCONF = True
except ImportError:
    _HAS_ZEROCONF = False


# ── Shared state ──────────────────────────────────────────────────────
_state_lock = threading.Lock()
_zc: Optional["Zeroconf"] = None
_registered_mdns: Dict[str, "ServiceInfo"] = {}  # name -> ServiceInfo
_browser_cache: Dict[str, "ServiceListener"] = {}  # service_type -> listener
_active: Optional[List[str]] = None  # cached result of _active_backends


def _get_zc() -> Optional["Zeroconf"]:
    """Return a lazily-created Zeroconf instance, or None if unavailable."""
    global _zc
    if not _HAS_ZEROCONF:
        return None
    with _state_lock:
        if _zc is None:
            _zc = Zeroconf(ip_version=IPVersion.V4Only)
        return _zc


def _active_backends() -> List[str]:
    """Resolve the mode string into a concrete ordered list of backends.

    Result order is meaningful: earlier backends are preferred for reads.
    """
    global _active
    if _active is not None:
        return _active

    if _HAS_CONSUL_MODULE:
        try:
            consul_ok = consul_client.discover_consul() is not None
        except Exception as e:
            # Network errors, auth failures, malformed config, the downstream
            # mDNS/static fallback is still viable, so log and keep going.
            logger.warning("Consul discovery probe failed: %s", e)
            consul_ok = False
    else:
        consul_ok = False
    mdns_ok = _HAS_ZEROCONF

    if MODE == "consul":
        _active = ["consul"] if consul_ok else []
    elif MODE == "mdns":
        _active = ["mdns"] if mdns_ok else []
    elif MODE == "both":
        _active = [b for b, ok in (("consul", consul_ok), ("mdns", mdns_ok)) if ok]
    else:  # auto
        if consul_ok:
            _active = ["consul"] + (["mdns"] if mdns_ok else [])
        elif mdns_ok:
            _active = ["mdns"]
        else:
            _active = []

    logger.info(
        "discovery: mode=%s active=%s (consul_mod=%s zeroconf=%s)",
        MODE,
        _active,
        _HAS_CONSUL_MODULE,
        _HAS_ZEROCONF,
    )
    return _active


def _host_ip_for(address: str) -> bytes:
    """Resolve `address` to a packed 4-byte IP for zeroconf.

    Docker-style names like 'host.docker.internal' usually fail on a
    native Windows process, so we fall back to the primary local IP.
    """
    try:
        return socket.inet_aton(socket.gethostbyname(address))
    except (socket.gaierror, OSError):
        pass
    # Fall back: UDP-connect to a public-looking address to learn our
    # outbound interface IP without actually sending anything.
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        s.connect(("8.8.8.8", 53))
        ip = s.getsockname()[0]
    except OSError:
        ip = "127.0.0.1"
    finally:
        s.close()
    return socket.inet_aton(ip)


# ── Registration ──────────────────────────────────────────────────────
def register_service(
    name: str,
    address: str,
    port: int,
    tags: Optional[Iterable[str]] = None,
    check_path: str = "/health",
    meta: Optional[Dict[str, str]] = None,
    max_attempts: int = 20,
) -> None:
    """Register `name` with every active backend. Returns immediately;
    each backend does its own retry logic in a daemon thread."""
    tags_list = list(tags or [])
    meta_dict = dict(meta or {})
    backends = _active_backends()

    if "consul" in backends and _HAS_CONSUL_MODULE:
        try:
            consul_client.register_service(
                name=name,
                address=address,
                port=port,
                tags=tags_list,
                check_path=check_path,
                meta=meta_dict,
                max_attempts=max_attempts,
            )
        except Exception as e:  # pragma: no cover — defensive
            logger.warning("consul register_service(%s) raised: %s", name, e)

    if "mdns" in backends:
        _register_mdns(name, address, port, tags_list, check_path, meta_dict)

    if not backends:
        logger.warning(
            "discovery: no active backends for register(%s); service is running but undiscoverable",
            name,
        )


def _register_mdns(
    name: str,
    address: str,
    port: int,
    tags: List[str],
    check_path: str,
    meta: Dict[str, str],
) -> None:
    zc = _get_zc()
    if zc is None:
        return

    props = {
        "check_path": check_path,
        "tags": ",".join(tags),
    }
    for k, v in meta.items():
        # zeroconf flattens property dicts into TXT records; keep small.
        props[f"meta_{k}"] = str(v)

    info = ServiceInfo(
        type_=MDNS_SERVICE_TYPE,
        name=f"{name}.{MDNS_SERVICE_TYPE}",
        addresses=[_host_ip_for(address)],
        port=int(port),
        properties=props,
        server=f"{name}.local.",
    )
    try:
        zc.register_service(info, allow_name_change=True)
        with _state_lock:
            _registered_mdns[name] = info
        logger.info("mdns: registered %s at port %d", name, port)
    except Exception as e:
        logger.warning("mdns register(%s) failed: %s", name, e)


# ── Deregistration / signal handling ──────────────────────────────────
def deregister_service(name: str, timeout: float = 2.0) -> None:
    """Best-effort deregister from every active backend. Safe to call
    from signal handlers / atexit hooks."""
    backends = _active_backends()

    if "consul" in backends and _HAS_CONSUL_MODULE:
        try:
            consul_client.deregister_service(name, timeout=timeout)
        except Exception as e:
            logger.debug("consul deregister(%s): %s", name, e)

    if "mdns" in backends:
        _deregister_mdns(name)


def _deregister_mdns(name: str) -> None:
    with _state_lock:
        info = _registered_mdns.pop(name, None)
        zc = _zc
    if info is None or zc is None:
        return
    try:
        zc.unregister_service(info)
        logger.info("mdns: unregistered %s", name)
    except Exception as e:
        logger.debug("mdns unregister(%s): %s", name, e)


def install_signal_handlers(name: str) -> None:
    """SIGTERM/SIGINT/atexit → deregister from every backend."""

    def _handler(signum: int, _frame: FrameType | None) -> None:
        logger.info("discovery: signal %s -> deregister %s", signum, name)
        deregister_service(name)
        sys.exit(0)

    for sig in (signal.SIGTERM, signal.SIGINT):
        try:
            signal.signal(sig, _handler)
        except (ValueError, OSError):
            pass  # not main thread; skip

    atexit.register(lambda: deregister_service(name))
    atexit.register(_shutdown_zeroconf)


def _shutdown_zeroconf() -> None:
    global _zc
    with _state_lock:
        zc, _zc = _zc, None
    if zc is not None:
        try:
            zc.close()
        except Exception:
            pass


# ── Queries ───────────────────────────────────────────────────────────
def get_service_url(name: str) -> Optional[str]:
    """Return http://address:port for the first healthy instance of
    `name`, trying each active backend in order."""
    for backend in _active_backends():
        if backend == "consul" and _HAS_CONSUL_MODULE:
            url: str | None = consul_client.get_service_url(name)
            if url:
                return url
        elif backend == "mdns":
            inst = _mdns_lookup(name)
            if inst is not None:
                return f"http://{inst['address']}:{inst['port']}"
    return None


def get_healthy_instances(name: str) -> List[Dict]:
    """Merge healthy instances from every active backend.
    De-duplicated on (address, port)."""
    seen = set()
    out: List[Dict] = []

    for backend in _active_backends():
        if backend == "consul" and _HAS_CONSUL_MODULE:
            for inst in consul_client.get_healthy_instances(name):
                key = (inst.get("address"), inst.get("port"))
                if key in seen:
                    continue
                seen.add(key)
                inst.setdefault("source", "consul")
                out.append(inst)
        elif backend == "mdns":
            inst = _mdns_lookup(name)
            if inst is not None:
                key = (inst["address"], inst["port"])
                if key not in seen:
                    seen.add(key)
                    inst["source"] = "mdns"
                    out.append(inst)
    return out


def get_pipe_name(name: str) -> Optional[str]:
    """Return the Windows named-pipe path for `name` if the service announced
    pipe support, else None.

    Preference order:
      1. `meta["pipe"]`, the registered pipe basename (e.g. "wylde-trainer").
      2. `tags` contains "ipc=pipe" — synthesize `wylde-<name>` by convention.
      3. Neither present → None (caller should stick to HTTP).
    """
    for inst in get_healthy_instances(name):
        meta = inst.get("meta") or {}
        tags = inst.get("tags") or []
        pipe_base = meta.get("pipe")
        if pipe_base:
            return rf"\\.\pipe\{pipe_base}"
        if meta.get("ipc") == "pipe" or "ipc=pipe" in tags:
            return rf"\\.\pipe\wylde-{name}"
    return None


def get_catalog_services() -> Dict[str, List[str]]:
    """Return {name: [tags...]} merged across active backends."""
    catalog: Dict[str, List[str]] = {}

    for backend in _active_backends():
        if backend == "consul" and _HAS_CONSUL_MODULE:
            for name, tags in consul_client.get_catalog_services().items():
                catalog.setdefault(name, list(tags))
        elif backend == "mdns":
            for name, tags in _mdns_catalog().items():
                catalog.setdefault(name, tags)

    return catalog


def _mdns_lookup(name: str, timeout_ms: int = 1500) -> Optional[Dict]:
    zc = _get_zc()
    if zc is None:
        return None
    try:
        info = zc.get_service_info(
            MDNS_SERVICE_TYPE,
            f"{name}.{MDNS_SERVICE_TYPE}",
            timeout=timeout_ms,
        )
    except Exception:
        return None
    if info is None or not info.addresses:
        return None

    addr = socket.inet_ntoa(info.addresses[0])
    tags_raw = _decode_txt(info.properties, "tags")
    meta = {
        k[len("meta_") :]: _decode_txt(info.properties, k)
        for k in info.properties
        if isinstance(k, (bytes, str)) and _as_str(k).startswith("meta_")
    }
    return {
        "address": addr,
        "port": int(info.port or 0),
        "tags": [t for t in (tags_raw or "").split(",") if t],
        "meta": meta,
    }


class _CatalogListener(ServiceListener if _HAS_ZEROCONF else object):  # type: ignore[misc]
    """Accumulator for ServiceBrowser callbacks."""

    def __init__(self) -> None:
        self.services: Dict[str, List[str]] = {}

    def add_service(self, zc: Any, type_: str, name: str) -> None:
        info = zc.get_service_info(type_, name, timeout=500)
        short = name.split(".", 1)[0]
        tags_raw = _decode_txt(info.properties, "tags") if info else ""
        self.services[short] = [t for t in (tags_raw or "").split(",") if t]

    def update_service(
        self, zc: Any, type_: str, name: str
    ) -> None:  # pragma: no cover
        self.add_service(zc, type_, name)

    def remove_service(self, zc: Any, type_: str, name: str) -> None:
        short = name.split(".", 1)[0]
        self.services.pop(short, None)


def _mdns_catalog(browse_for_ms: int = 600) -> Dict[str, List[str]]:
    zc = _get_zc()
    if zc is None:
        return {}
    listener = _browser_cache.get(MDNS_SERVICE_TYPE)
    if listener is None:
        listener = _CatalogListener()
        ServiceBrowser(zc, MDNS_SERVICE_TYPE, listener)  # keeps itself alive
        _browser_cache[MDNS_SERVICE_TYPE] = listener
        time.sleep(browse_for_ms / 1000.0)
    assert isinstance(listener, _CatalogListener)
    return dict(listener.services)


# ── tiny helpers ──────────────────────────────────────────────────────
def _as_str(v: Any) -> str:
    if isinstance(v, bytes):
        try:
            return v.decode("utf-8")
        except UnicodeDecodeError:
            return v.decode("latin-1", errors="replace")
    return str(v)


def _decode_txt(props: Dict[Any, Any], key: Any) -> str:
    """zeroconf TXT props are bytes->bytes; find `key` ignoring encoding."""
    if not props:
        return ""
    key_str = _as_str(key)
    for k, v in props.items():
        if _as_str(k) == key_str:
            return _as_str(v) if v is not None else ""
    return ""


# ── Re-exports so existing imports keep working ───────────────────────
# Services that import `from consul_client import register_service` can
# move to `from discovery import register_service` with no other change.
__all__ = [
    "register_service",
    "deregister_service",
    "install_signal_handlers",
    "get_service_url",
    "get_healthy_instances",
    "get_catalog_services",
    "get_pipe_name",
    "MODE",
    "MDNS_SERVICE_TYPE",
]
