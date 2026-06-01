"""
consul_client.py — Minimal stdlib-only Consul client for Wylde services.

Usage (in a service's startup):

    from consul_client import register_service, install_signal_handlers
    register_service(
        name="tool-runner",
        address="tool-runner",  # Docker DNS name, or host.docker.internal for host procs
        port=8001,
        tags=["wylde", "tools"],
    )
    install_signal_handlers("tool-runner")
    # ... app.run(...)

The register call spawns a daemon thread that retries with exponential backoff +
jitter until Consul accepts the registration. Flask never blocks on this.

Discovery order (first reachable wins, cached per-process):
    1. CONSUL_HTTP_ADDR env var
    2. http://consul:8500        (Docker DNS, works inside wylde-network)
    3. http://127.0.0.1:8500     (published port, works on host)
    4. http://host.docker.internal:8500  (Docker Desktop fallback)
"""

import atexit
import json
import logging
import os
import random
import signal
import socket
import sys
import threading
import time
import urllib.error
import urllib.request
from types import FrameType
from typing import Any

logger = logging.getLogger(__name__)

CONSUL_CANDIDATES: list[str | None] = [
    os.getenv("CONSUL_HTTP_ADDR"),
    "http://consul:8500",
    "http://127.0.0.1:8500",
    "http://host.docker.internal:8500",
]

_cached_consul_url: str | None = None
_cache_lock = threading.Lock()


def _probe(url: str | None, timeout: float = 0.5) -> bool:
    """Return True if GET <url>/v1/status/leader returns 2xx quickly."""
    if not url:
        return False
    try:
        req = urllib.request.Request(
            url.rstrip("/") + "/v1/status/leader", method="GET"
        )
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return bool(200 <= r.status < 300)
    except (
        urllib.error.URLError,
        urllib.error.HTTPError,
        socket.timeout,
        ConnectionError,
        OSError,
    ):
        return False


def discover_consul() -> str | None:
    """Return the first reachable Consul URL, cached per-process.

    Returns None if nothing is reachable.
    """
    global _cached_consul_url
    with _cache_lock:
        if _cached_consul_url and _probe(_cached_consul_url):
            return _cached_consul_url
        for candidate in CONSUL_CANDIDATES:
            if candidate and _probe(candidate):
                _cached_consul_url = candidate.rstrip("/")
                logger.info(f"Discovered Consul at {_cached_consul_url}")
                return _cached_consul_url
    logger.warning(
        "No reachable Consul endpoint found in candidates: %s",
        [c for c in CONSUL_CANDIDATES if c],
    )
    return None


def _http(
    method: str,
    url: str,
    body: Any = None,
    timeout: float = 3,
) -> tuple[int, str]:
    """Tiny stdlib HTTP helper. Returns (status, text) or raises on network error."""
    headers: dict[str, str] = {}
    data: bytes | None = None
    if body is not None:
        data = json.dumps(body).encode("utf-8")
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read().decode("utf-8")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode("utf-8", errors="replace")


def _build_registration(
    name: str,
    address: str,
    port: int,
    tags: list[str] | None = None,
    check_path: str = "/health",
    check_interval: str = "10s",
    check_timeout: str = "3s",
    deregister_after: str = "10m",
    meta: dict[str, str] | None = None,
) -> dict[str, Any]:
    return {
        "ID": name,
        "Name": name,
        "Address": address,
        "Port": int(port),
        "Tags": list(tags or []),
        "Meta": dict(meta or {}),
        "Check": {
            "Name": f"{name} http {check_path}",
            "HTTP": f"http://{address}:{port}{check_path}",
            "Interval": check_interval,
            "Timeout": check_timeout,
            "DeregisterCriticalServiceAfter": deregister_after,
        },
    }


def _register_once(consul_url: str, registration: dict[str, Any]) -> bool:
    url = f"{consul_url}/v1/agent/service/register"
    status, body = _http("PUT", url, registration, timeout=5)
    if 200 <= status < 300:
        return True
    logger.warning(f"Consul register failed: {status} {body}")
    return False


def _is_registered(consul_url: str, name: str) -> bool:
    """Check whether `name` is currently in the agent's owned-services list.

    Uses /v1/agent/services (not the catalog) so we're asking *this* agent
    what it knows about, which is the authoritative source for re-registration.
    """
    try:
        status, body = _http("GET", f"{consul_url}/v1/agent/services", timeout=3)
        if 200 <= status < 300:
            services = json.loads(body)
            return name in services
    except Exception:
        pass
    return False


def _register_loop(
    name: str,
    address: str,
    port: int,
    tags: list[str] | None,
    check_path: str,
    meta: dict[str, str] | None,
    max_attempts: int,
) -> None:
    """Daemon-thread body: register, then keep the registration fresh.

    Phase 1, initial registration: retry with exponential backoff + jitter
    until Consul accepts or we exhaust max_attempts.

    Phase 2 — persistent heartbeat: once registered, periodically verify we
    are still in the agent's services list. If not (e.g. Consul was restarted
    and lost state, or our service was pruned for some reason), re-register.
    This is what gives us "Consul restart recovery" without operator action.
    """
    registration = _build_registration(
        name, address, port, tags=tags, check_path=check_path, meta=meta
    )

    # Phase 1: initial registration
    delay = 1.0
    registered_url = None
    for attempt in range(1, max_attempts + 1):
        consul_url = discover_consul()
        if consul_url and _register_once(consul_url, registration):
            logger.info(
                f"Registered {name} with Consul at {consul_url} (address={address}, port={port})"
            )
            registered_url = consul_url
            break
        sleep_for = delay + random.uniform(0, delay * 0.5)
        logger.warning(
            f"Consul register attempt {attempt}/{max_attempts} failed; retrying in {sleep_for:.1f}s"
        )
        time.sleep(sleep_for)
        delay = min(delay * 2, 30.0)

    if not registered_url:
        logger.error(
            f"Giving up on Consul registration for {name} after {max_attempts} attempts"
        )
        return

    # Phase 2: heartbeat. Check every HEARTBEAT_INTERVAL seconds that we're
    # still registered. If not, re-register. Uses the cached consul URL but
    # re-discovers on failure (handles Consul URL changes across restarts).
    heartbeat_interval = 30.0
    while True:
        time.sleep(heartbeat_interval)
        try:
            consul_url = discover_consul() or registered_url
            if not _is_registered(consul_url, name):
                logger.info(
                    f"Re-registering {name} with Consul (not found in agent catalog)"
                )
                if _register_once(consul_url, registration):
                    registered_url = consul_url
                    logger.info(f"Re-registered {name} with Consul at {consul_url}")
        except Exception as e:
            logger.debug(f"Heartbeat error for {name}: {e}")


def register_service(
    name: str,
    address: str,
    port: int,
    tags: list[str] | None = None,
    check_path: str = "/health",
    meta: dict[str, str] | None = None,
    max_attempts: int = 20,
) -> threading.Thread:
    """Kick off a daemon thread that registers the service with Consul.

    Returns immediately; actual registration happens in the background so
    Flask can proceed to app.run() without blocking.
    """
    t = threading.Thread(
        target=_register_loop,
        args=(name, address, port, tags, check_path, meta, max_attempts),
        name=f"consul-register-{name}",
        daemon=True,
    )
    t.start()
    return t


def deregister_service(name: str, timeout: float = 2.0) -> bool:
    """Best-effort deregister. Safe to call from signal handlers."""
    consul_url = discover_consul()
    if not consul_url:
        return False
    try:
        url = f"{consul_url}/v1/agent/service/deregister/{name}"
        status, _ = _http("PUT", url, timeout=timeout)
        if 200 <= status < 300:
            logger.info(f"Deregistered {name} from Consul")
            return True
        logger.warning(f"Consul deregister returned {status}")
        return False
    except Exception as e:
        logger.warning(f"Consul deregister failed: {e}")
        return False


def install_signal_handlers(name: str) -> None:
    """SIGTERM/SIGINT → deregister → exit. Also registers an atexit hook.

    Safe to call before register_service(). Idempotent, multiple calls
    replace the prior handler without duplicating work.
    """

    def _handler(signum: int, _frame: FrameType | None) -> None:
        logger.info(f"Signal {signum} received; deregistering {name}")
        deregister_service(name)
        sys.exit(0)

    try:
        signal.signal(signal.SIGTERM, _handler)
    except (ValueError, OSError):
        pass  # not main thread; skip
    try:
        signal.signal(signal.SIGINT, _handler)
    except (ValueError, OSError):
        pass

    atexit.register(lambda: deregister_service(name))


# === Query helpers (for tool-registry's Consul-backed routes) ===


def get_catalog_services(consul_url: str | None = None) -> dict[str, list[str]]:
    """Return dict of {service_name: [tags...]} from Consul's catalog."""
    consul_url = consul_url or discover_consul()
    if not consul_url:
        return {}
    status, body = _http("GET", f"{consul_url}/v1/catalog/services")
    if 200 <= status < 300:
        data: dict[str, list[str]] = json.loads(body)
        return data
    return {}


def get_service_health(
    name: str, consul_url: str | None = None
) -> list[dict[str, Any]]:
    """Return list of health-check entries for a service from Consul.

    Each entry is a dict with keys Node, Service, Checks.
    """
    consul_url = consul_url or discover_consul()
    if not consul_url:
        return []
    status, body = _http("GET", f"{consul_url}/v1/health/service/{name}")
    if 200 <= status < 300:
        data: list[dict[str, Any]] = json.loads(body)
        return data
    return []


def get_healthy_instances(
    name: str, consul_url: str | None = None
) -> list[dict[str, Any]]:
    """Return list of {address, port, tags} for healthy instances of a service."""
    entries = get_service_health(name, consul_url=consul_url)
    out: list[dict[str, Any]] = []
    for entry in entries:
        svc = entry.get("Service", {})
        checks = entry.get("Checks", [])
        if all(c.get("Status") == "passing" for c in checks):
            out.append(
                {
                    "address": svc.get("Address")
                    or entry.get("Node", {}).get("Address"),
                    "port": svc.get("Port"),
                    "tags": svc.get("Tags", []),
                    "meta": svc.get("Meta", {}),
                }
            )
    return out


def get_service_url(name: str, consul_url: str | None = None) -> str | None:
    """Return http://address:port for the first healthy instance, or None."""
    instances = get_healthy_instances(name, consul_url=consul_url)
    if not instances:
        return None
    inst = instances[0]
    return f"http://{inst['address']}:{inst['port']}"
