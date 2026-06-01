"""Reads services.yaml, spawns enabled services as subprocesses.

For each enabled service, the launcher:
    1. Loads the service's manifest.json for the entry_point command.
    2. Builds an env that includes WYLDE_<NAME>_PORT and WYLDE_<NAME>_ENDPOINT
       for every registered service (so children can find each other).
    3. Spawns the entry_point as a subprocess in the service's folder.
    4. Tracks the running process for graceful shutdown later.

Persistent state: services.yaml's `enabled: true|false` flag persists across
runs. Last session's enabled set resumes on next start. The GUI flips this
flag to start/stop services interactively.
"""

from __future__ import annotations

import os
import shlex
import subprocess
import sys
import time
from typing import Any

from . import manifest as manifest_mod
from ._common import (
    WYLDE_ROOT,
    load_services,
    logger,
    save_services,
)


# How long to wait for a service's manifest `health_check` to pass before
# giving up and spawning the next one anyway. A slow service shouldn't
# wedge the whole bring-up, so the gate is best-effort: on timeout we log
# and continue rather than abort.
HEALTH_TIMEOUT: float = 10.0


# Names of running subprocesses are tracked here for shutdown.py to find.
# Process state is also written into services.yaml so a fresh launcher
# instance can recover after a crash. The in-memory dict is the fast path.
_running: dict[str, subprocess.Popen] = {}


def launch_all() -> dict[str, subprocess.Popen]:
    """Start every enabled service. Returns the dict of running processes.

    The service set is built entirely from the filesystem-as-registry —
    ``Network/services.yaml`` (the discovered roster + port/endpoint/
    enabled state) joined with each service's ``manifest.json`` (the
    launch command, tier, and ``depends_on``). There is no hardcoded
    service list; adding a folder with a manifest is all it takes for a
    new service to be launched. The ``wylde_check`` rule
    ``launcher_enumerates_services_from_manifests`` guards that property.
    """
    # Apply Wylde/Core/Config/*.yaml to os.environ before we build the
    # subprocess env overlay, so config values flow into every child.
    # Existing env vars win over file values (override=False).
    try:
        from Core.Config import load_all_to_env

        load_all_to_env()
    except ImportError:
        # If Core.Config isn't on sys.path yet (early bootstrap), fall back
        # to a direct path-based load so the launcher still works.
        _load_config_via_path()

    services = load_services()

    if not services:
        logger.info("services.yaml is empty — nothing to launch")
        return {}

    env_overlay = _build_env_overlay(services)

    # Honor depends_on by topologically sorting before launch
    ordered = _topological_order(services)

    for name in ordered:
        svc = next((s for s in services if s.get("name") == name), None)
        if svc is None:
            continue
        if not svc.get("enabled"):
            continue
        try:
            proc = _spawn(svc, env_overlay)
            if proc is not None:
                _running[name] = proc
                svc["status"] = "running"
                # Gate the next spawn on this service's readiness probe.
                # Dormant unless the manifest declares `health_check`;
                # with none declared it returns immediately, so live
                # bring-up behaviour is unchanged. A dependent spawned
                # next can then assume its dependency's pipe/port is up.
                _wait_for_health(svc)
        except Exception:  # noqa: BLE001
            logger.exception("failed to launch %s", name)
            svc["status"] = "crashed"

    save_services(services)
    return _running


def _spawn(svc: dict[str, Any], env_overlay: dict[str, str]) -> subprocess.Popen | None:
    """Launch a single service. Returns the Popen, or None if skipped."""
    name = svc["name"]
    folder = WYLDE_ROOT / name

    if not folder.exists():
        logger.warning("service folder missing for %s, skipping", name)
        return None

    mf = manifest_mod.load_manifest(folder)
    if mf is None:
        logger.warning("no manifest for %s, skipping", name)
        return None

    entry_point = mf.get("entry_point")
    if not entry_point:
        # Library/internal service (no long-running process). Common for
        # things like Network/ which is just a YAML file.
        logger.debug("%s has no entry_point — library service, skipping launch", name)
        return None

    # Tier=core services are spawned by the Lifecycle daemon directly
    # (Memgraph, Voice). The launcher must skip them or we'd double-spawn —
    # the daemon's _start_<name> already owns the Popen and shutdown path.
    if str(mf.get("tier") or "").lower() == "core":
        logger.debug("%s is tier=core — daemon-managed, skipping launcher spawn", name)
        return None

    env = os.environ.copy()
    env.update(env_overlay)

    cmd = (
        shlex.split(entry_point) if isinstance(entry_point, str) else list(entry_point)
    )
    logger.info("launching %s: %s", name, " ".join(cmd))

    proc = subprocess.Popen(
        cmd,
        cwd=folder,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        # On Windows, CREATE_NEW_PROCESS_GROUP lets us send Ctrl-Break
        # for graceful shutdown without taking down the parent.
        creationflags=subprocess.CREATE_NEW_PROCESS_GROUP
        if sys.platform == "win32"
        else 0,
    )
    return proc


def _build_env_overlay(services: list[dict[str, Any]]) -> dict[str, str]:
    """Produce the WYLDE_<NAME>_PORT / _ENDPOINT vars for every service.

    Sets vars for ALL registered services (not just enabled ones), so an
    enabled service can still reach a disabled one when it gets re-enabled
    mid-session — the env was set at launch time.

    Also prepends the Wylde namespace root (``parent of WYLDE_ROOT``) to
    ``PYTHONPATH`` so manifests with ``Wylde.X.run``-style entry points
    (e.g. ``Gateway/manifest.json``: ``"py -3 -m Wylde.Gateway.run"``)
    resolve when the launcher spawns them with the service folder as
    cwd. Without this overlay, ``-m Wylde.Gateway.run`` would fail —
    cwd is ``Wylde/Gateway/`` which doesn't have a ``Wylde`` subdir.
    """
    overlay: dict[str, str] = {}
    for svc in services:
        port = svc.get("port")
        if not isinstance(port, int):
            continue
        slug = _envvar_slug(svc.get("name", ""))
        if not slug:
            continue
        overlay[f"WYLDE_{slug}_PORT"] = str(port)
        overlay[f"WYLDE_{slug}_ENDPOINT"] = (
            svc.get("endpoint") or f"http://localhost:{port}"
        )

    namespace_root = str(WYLDE_ROOT.parent)
    existing = os.environ.get("PYTHONPATH", "")
    overlay["PYTHONPATH"] = (
        namespace_root + os.pathsep + existing if existing else namespace_root
    )
    return overlay


def _envvar_slug(name: str) -> str:
    """Make an env-var-safe slug from a folder name.

    "resource_monitor" → "RESOURCE_MONITOR"
    "Wylde_Study"     → "WYLDE_STUDY"
    "extension_bridge" → "EXTENSION_BRIDGE"
    """
    return "".join(c if c.isalnum() else "_" for c in name).strip("_").upper()


def _topological_order(services: list[dict[str, Any]]) -> list[str]:
    """Return service names sorted such that dependencies come before dependents.

    Cycles are broken arbitrarily (with a warning logged). Services with no
    declared deps preserve their order in services.yaml.
    """
    name_to_deps: dict[str, list[str]] = {}
    for svc in services:
        n = svc.get("name")
        if not n:
            continue
        deps = svc.get("depends_on") or []
        # Also pick up depends_on from the manifest if present
        mf = manifest_mod.load_manifest(WYLDE_ROOT / n)
        if mf and mf.get("depends_on"):
            deps = list({*deps, *mf["depends_on"]})
        name_to_deps[n] = [
            d
            for d in deps
            if d in name_to_deps or any(s.get("name") == d for s in services)
        ]

    ordered: list[str] = []
    visiting: set[str] = set()
    visited: set[str] = set()

    def visit(node: str) -> None:
        if node in visited:
            return
        if node in visiting:
            logger.warning("dependency cycle involving %s — breaking", node)
            return
        visiting.add(node)
        for dep in name_to_deps.get(node, []):
            visit(dep)
        visiting.discard(node)
        visited.add(node)
        ordered.append(node)

    for n in name_to_deps:
        visit(n)
    return ordered


def _wait_for_health(svc: dict[str, Any], *, timeout: float = HEALTH_TIMEOUT) -> bool:
    """Block until the service's manifest ``health_check`` passes.

    Returns immediately (``True``) when the manifest declares no probe —
    the common case today, so this is a no-op on the live bring-up path.
    On timeout, logs and returns ``False`` rather than aborting: a slow
    service shouldn't wedge the whole stack. Manifest-driven and modular
    by design — a new service opts into gating purely by adding a
    ``health_check`` to its manifest.
    """
    mf = manifest_mod.load_manifest(WYLDE_ROOT / svc.get("name", ""))
    probe = (mf or {}).get("health_check")
    if not probe or not isinstance(probe, str):
        return True  # no gate declared — proceed immediately

    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if _health_probe_ok(probe):
            return True
        time.sleep(0.25)
    logger.warning(
        "health_check %r for %s did not pass within %ss — continuing anyway",
        probe,
        svc.get("name"),
        timeout,
    )
    return False


def _health_probe_ok(probe: str) -> bool:
    """Evaluate a single manifest ``health_check`` probe once.

    Supported shapes:
      * ``"pipe:wylde-<name>"`` — true once the named pipe exists.
      * ``"http://…"`` / ``"https://…"`` — true on any HTTP response.
      * anything else — treated as "ready" (unknown shape never blocks).

    Imports live inside the branch so the launcher's import cost is
    unchanged when no probe is configured.
    """
    if probe.startswith("pipe:"):
        pipe = probe[len("pipe:") :]
        bare = pipe[len("wylde-") :] if pipe.startswith("wylde-") else pipe
        return os.path.exists(rf"\\.\pipe\wylde-{bare}")
    if probe.startswith(("http://", "https://")):
        import urllib.request

        try:
            with urllib.request.urlopen(probe, timeout=1.0) as resp:  # noqa: S310
                return 200 <= resp.status < 500
        except Exception:  # noqa: BLE001 — any failure means "not ready yet"
            return False
    return True


def get_running() -> dict[str, subprocess.Popen]:
    """Return the in-memory dict of running processes."""
    return _running


def _load_config_via_path() -> None:
    """Fallback: load Core/Config without importing it as a package.

    Used during early bootstrap when sys.path may not be set up yet.
    Mirrors what `Core.Config.load_all_to_env(override=False)` does.
    """
    config_dir = WYLDE_ROOT / "Core" / "Config"
    if not config_dir.is_dir():
        return
    try:
        import yaml
    except ImportError:
        logger.warning("PyYAML not installed; skipping Core/Config load")
        return
    for path in sorted(config_dir.glob("*.yaml")):
        try:
            data = yaml.safe_load(path.read_text(encoding="utf-8")) or {}
        except yaml.YAMLError:
            continue
        if not isinstance(data, dict):
            continue
        for key, value in data.items():
            if not isinstance(key, str) or key in os.environ:
                continue
            os.environ[key] = "" if value is None else str(value)


def main() -> int:
    from Core.shared.logging_setup import configure_logging

    configure_logging()
    launch_all()
    return 0


if __name__ == "__main__":
    sys.exit(main())
