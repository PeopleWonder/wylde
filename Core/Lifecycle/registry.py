"""Service registry — walks manifest.json files, probes liveness, returns
a unified view.

Two manifest sources are walked at each call:

1. **Declarative folder manifests** — ``<folder>/manifest.json`` for every
   service folder.

   * Top-level service folders (``Voice/``, ``device_gate/``, ``Gateway/``,
     ``VPN/``, ``N8N/``, ``Extensions/``) come from
     :func:`_common.list_service_folders`.
   * ``Wylde/Core/`` is added explicitly as a single logical service (its
     internal pipes — lifecycle, harness, memgraph, memory-scheduler —
     are no longer surfaced individually). Core's running state is
     derived from ``constituent_pipes`` in ``Core/manifest.json``: active
     iff every listed pipe is live on ``\\\\.\\pipe\\``.

2. **Runtime/heartbeat manifests** — JSON files under
   ``data/manifests/<name>.json`` written by services at boot. The
   daemon writes ``core.json`` for Core; Memgraph (and other top-level
   services) write their own. Provides the live ``status.pid`` /
   ``status.heartbeat`` info.

Each entry is then probed for liveness:

* If the manifest declares ``constituent_pipes`` (Core) → check ALL
  pipes exist in ``os.listdir(r'\\\\.\\pipe\\\\')``.
* Else if it declares a ``pipe`` → single-pipe check.
* Otherwise if it declares a ``port`` → TCP probe
  ``127.0.0.1:<port>``.
* Otherwise → fall back to the manifest ``enabled`` flag.

Runtime-only manifests that match a Core constituent pipe are filtered
out (transitional concern: pre-restart daemons may still be writing
the old per-pipe manifest files; the runtime-only loop must not surface
them as peer services).

The result is a list of :class:`ServiceInfo` dicts that
:func:`Core.Lifecycle.control.list_services_action` shapes into the
GUI's expected response.
"""

from __future__ import annotations

import json
import os
import socket
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

from . import manifest as manifest_mod
from ._common import WYLDE_ROOT, list_service_folders, logger


# ── Core/ as a single logical service ────────────────────────────────
#
# ``list_service_folders()`` excludes ``Core/`` by design (it's
# infrastructure, not a launchable top-level service). Core is added
# back here as ONE entry — driven by ``Wylde/Core/manifest.json`` —
# so the GUI dashboard sees a single Core service rather than per-
# subpipe rows. The manifest's ``constituent_pipes`` list drives the
# liveness probe.

_CORE_FOLDER_NAME = "Core"
_CORE_PATH: Path = WYLDE_ROOT / "Core"

# Runtime / heartbeat manifests live here.
_RUNTIME_MANIFEST_DIR: Path = WYLDE_ROOT / "data" / "manifests"

# Pipe-existence probe uses os.listdir on the named-pipe filesystem on
# Windows. Off-Windows it's a no-op (returns []).
_PIPE_LISTDIR_PATH = r"\\.\pipe\\"

# TCP probe timeout. Short — anything slower than this is "not really
# listening" for dashboard purposes.
_PROBE_TIMEOUT_S = 0.25


@dataclass
class ServiceInfo:
    """Per-service registry entry. The shape `list_services_action` reshapes
    for the GUI."""

    name: str
    description: str = ""
    version: str = ""
    kind: str = "standard"  # "core" | "optional" | "standard"
    enabled: bool = False
    pipe: Optional[str] = None
    port: Optional[int] = None
    constituent_pipes: List[str] = field(default_factory=list)
    running: bool = False
    source: str = "manifest"  # "manifest" | "runtime"
    contributes: Dict[str, Any] = field(default_factory=dict)
    # Runtime fields (filled from data/manifests/<name>.json if present)
    pid: Optional[int] = None
    started_at: Optional[str] = None
    heartbeat: Optional[str] = None
    manifest_path: Optional[str] = None


# ── Probes ────────────────────────────────────────────────────────────


def _pipe_alive(pipe_name: Optional[str]) -> bool:
    """Is ``\\\\.\\pipe\\<pipe_name>`` in the live named-pipe namespace?

    Strip any leading ``\\\\.\\pipe\\`` so callers can pass either the
    short name (``wylde-harness``) or the full path
    (``\\\\.\\pipe\\wylde-harness``).
    """
    if os.name != "nt" or not pipe_name:
        return False
    short = pipe_name
    if "\\" in short:
        short = short.rsplit("\\", 1)[-1]
    try:
        pipes = os.listdir(_PIPE_LISTDIR_PATH)
    except OSError:
        return False
    return short in pipes


def _port_alive(port: Optional[int]) -> bool:
    """TCP-connect to 127.0.0.1:<port>. True iff the port accepts."""
    if not isinstance(port, int) or port <= 0:
        return False
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        sock.settimeout(_PROBE_TIMEOUT_S)
        return sock.connect_ex(("127.0.0.1", port)) == 0
    except OSError:
        return False
    finally:
        try:
            sock.close()
        except OSError:
            pass


def _is_running(info: ServiceInfo) -> bool:
    """Probe order: constituent pipes (all-must-be-alive) → pipe → port → False."""
    if info.constituent_pipes:
        return all(_pipe_alive(p) for p in info.constituent_pipes)
    if info.pipe and _pipe_alive(info.pipe):
        return True
    if info.port and _port_alive(info.port):
        return True
    return False


# ── Manifest reading ──────────────────────────────────────────────────


def _read_runtime_manifests() -> Dict[str, Dict[str, Any]]:
    """Map service-name → runtime manifest dict from data/manifests/."""
    out: Dict[str, Dict[str, Any]] = {}
    if not _RUNTIME_MANIFEST_DIR.exists():
        return out
    for path in sorted(_RUNTIME_MANIFEST_DIR.glob("*.json")):
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            continue
        if not isinstance(data, dict):
            continue
        name = data.get("service") or data.get("name")
        if isinstance(name, str) and name:
            out.setdefault(name, data)
    return out


def _load_folder_manifest(folder: Path) -> Optional[Dict[str, Any]]:
    """Read ``<folder>/manifest.json``. Returns None if missing/malformed."""
    return manifest_mod.load_manifest(folder)


def _service_folders() -> List[Tuple[str, Path]]:
    """All folders that contribute a declarative manifest.

    Returns ``(declared_name, folder_path)`` tuples. The declared_name
    is the folder name; the manifest's ``name`` field may override
    (e.g. ``Voice/manifest.json`` declares ``name: "Voice"`` but we
    keep the folder name for unique-key purposes).
    """
    out: List[Tuple[str, Path]] = [(p.name, p) for p in list_service_folders()]
    if (_CORE_PATH / "manifest.json").exists():
        out.append((_CORE_FOLDER_NAME, _CORE_PATH))
    return out


# ── Merge: declarative + runtime → ServiceInfo ────────────────────────


def _name_with_wylde_prefix(name: str) -> str:
    """The runtime manifest uses ``wylde-<name>``; the folder manifest
    often uses the folder name (e.g. ``Voice`` → ``wylde-voice``).
    Normalise so a lookup against the runtime map hits."""
    candidate = name.lower().replace(" ", "-")
    if candidate.startswith("wylde-"):
        return candidate
    return f"wylde-{candidate}"


def _build_info(
    folder_name: str,
    folder_manifest: Optional[Dict[str, Any]],
    runtime: Dict[str, Dict[str, Any]],
) -> Optional[ServiceInfo]:
    """Merge declarative + runtime for one service folder."""
    if folder_manifest is None:
        return None

    declared_name = folder_manifest.get("name") or folder_name
    runtime_key = _name_with_wylde_prefix(declared_name)
    runtime_doc = runtime.get(runtime_key) or runtime.get(declared_name)

    pipe = folder_manifest.get("pipe")
    if isinstance(pipe, str) and pipe and not pipe.startswith("\\\\"):
        # Folder manifest uses the short form ("wylde-voice"); normalise
        # to the full pipe path for the probe + GUI surface.
        pipe = rf"\\.\pipe\{pipe}"
    elif not pipe and runtime_doc:
        pipe = runtime_doc.get("pipe")

    port = folder_manifest.get("port")
    if port is None and runtime_doc:
        port = runtime_doc.get("port")

    constituent_pipes = folder_manifest.get("constituent_pipes") or []
    if not isinstance(constituent_pipes, list):
        constituent_pipes = []
    constituent_pipes = [p for p in constituent_pipes if isinstance(p, str) and p]

    tier = (folder_manifest.get("tier") or "standard").lower()
    if tier == "core":
        kind = "core"
    elif tier == "optional":
        kind = "optional"
    else:
        kind = "standard"

    info = ServiceInfo(
        name=runtime_key,
        description=folder_manifest.get("description", "")
        or (runtime_doc.get("description", "") if runtime_doc else ""),
        version=folder_manifest.get("version", "")
        or (runtime_doc.get("version", "") if runtime_doc else ""),
        kind=kind,
        enabled=bool(folder_manifest.get("enabled", False)),
        pipe=pipe if isinstance(pipe, str) else None,
        port=port if isinstance(port, int) else None,
        constituent_pipes=constituent_pipes,
        source="manifest" if runtime_doc is None else "runtime",
        contributes=(runtime_doc.get("contributes") if runtime_doc else None)
        or folder_manifest.get("contributes")
        or {},
    )

    if runtime_doc:
        status = runtime_doc.get("status") or {}
        info.pid = status.get("pid") if isinstance(status, dict) else None
        info.started_at = status.get("started_at") if isinstance(status, dict) else None
        info.heartbeat = status.get("heartbeat") if isinstance(status, dict) else None

    info.running = _is_running(info)
    return info


def _runtime_only_info(name: str, runtime_doc: Dict[str, Any]) -> ServiceInfo:
    """Build a ServiceInfo from a runtime manifest that has no
    declarative counterpart (e.g. an extension publishing a runtime
    manifest from a tools/ folder)."""
    pipe = runtime_doc.get("pipe")
    port = runtime_doc.get("port") if isinstance(runtime_doc.get("port"), int) else None
    kind = (runtime_doc.get("kind") or "standard").lower()
    info = ServiceInfo(
        name=name,
        description=runtime_doc.get("description", ""),
        version=runtime_doc.get("version", ""),
        kind="core" if kind == "daemon-managed" else "standard",
        enabled=True,
        pipe=pipe if isinstance(pipe, str) else None,
        port=port,
        source="runtime",
        contributes=runtime_doc.get("contributes") or {},
    )
    status = runtime_doc.get("status") or {}
    if isinstance(status, dict):
        info.pid = status.get("pid")
        info.started_at = status.get("started_at")
        info.heartbeat = status.get("heartbeat")
    info.running = _is_running(info)
    return info


def _short_pipe_name(pipe: Any) -> str:
    """Strip the ``\\\\.\\pipe\\`` prefix off a runtime manifest's pipe field.

    Returns ``""`` for non-strings, empty strings, or strings that don't
    look like a Windows named pipe path. Used to compare runtime entries
    against ``constituent_pipes`` (which list short pipe names).
    """
    if not isinstance(pipe, str) or not pipe:
        return ""
    if "\\" in pipe:
        return pipe.rsplit("\\", 1)[-1]
    return pipe


def _collect_constituent_pipe_names(infos: Dict[str, ServiceInfo]) -> set[str]:
    """Names that have been claimed by another service's constituent_pipes.

    Used to filter runtime-only manifests so a Core sub-pipe (e.g.
    ``wylde-memgraph``) — which Memgraph's own ``run.py`` still writes
    a manifest for, and which a pre-restart daemon may still be writing
    manifests for — doesn't show up as a peer of Core in the dashboard.
    """
    claimed: set[str] = set()
    for info in infos.values():
        for pipe in info.constituent_pipes:
            # constituent_pipes are pipe names (``wylde-memgraph``);
            # runtime manifests are keyed by service name which is the
            # same string by convention.
            claimed.add(pipe)
    return claimed


# ── Public API ────────────────────────────────────────────────────────


def list_services() -> List[ServiceInfo]:
    """Return the canonical service inventory.

    Order is deterministic — sorted by service name — so the GUI
    dashboard renders stably across refreshes.
    """
    runtime = _read_runtime_manifests()
    by_name: Dict[str, ServiceInfo] = {}

    # 1. Walk declarative manifests (folder-rooted).
    for folder_name, folder_path in _service_folders():
        folder_manifest = _load_folder_manifest(folder_path)
        info = _build_info(folder_name, folder_manifest, runtime)
        if info is None:
            continue
        info.manifest_path = str(folder_path / "manifest.json")
        by_name[info.name] = info

    # 2. Any runtime manifest without a declarative counterpart still
    #    surfaces — e.g. an extension that ships only a runtime
    #    heartbeat file. EXCEPT entries already claimed as a constituent
    #    pipe of another service (Core absorbs lifecycle/harness/memgraph/
    #    vram-broker rather than letting them appear as peers). Filter
    #    matches by EITHER the runtime manifest's ``service`` field OR its
    #    short pipe name — needed because e.g. ``vram-broker``'s service
    #    field has no ``wylde-`` prefix even though its pipe does.
    constituent_names = _collect_constituent_pipe_names(by_name)
    for rt_name, rt_doc in runtime.items():
        if rt_name in by_name:
            continue
        if rt_name in constituent_names:
            continue
        if _short_pipe_name(rt_doc.get("pipe")) in constituent_names:
            continue
        info = _runtime_only_info(rt_name, rt_doc)
        info.manifest_path = str(_RUNTIME_MANIFEST_DIR / f"{rt_name}.json")
        by_name[rt_name] = info

    out = sorted(by_name.values(), key=lambda x: x.name)
    if logger.isEnabledFor(10):  # DEBUG
        logger.debug(
            "registry.list_services: %d entries (%d running)",
            len(out),
            sum(1 for s in out if s.running),
        )
    return out


__all__ = ["ServiceInfo", "list_services"]
