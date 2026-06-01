"""Architectural rules: HTTP boundary, manifest paths, import paths,
dead-service references, memory-layer encapsulation, service state
ownership."""

from __future__ import annotations

import re
import sys as _sys
from pathlib import Path
from typing import Dict, List, Tuple

from .. import Finding
from .._config import (
    ACTIVE_ROOTS,
    DEAD_REF_ALLOWLISTED_FILES,
    DEAD_REF_OK_MARKERS,
    DEAD_SERVICE_NAMES,
    HTTP_CLIENT_PATTERNS,
    INTERNAL_HOSTS,
    INTERNAL_PORTS,
    NO_HTTP_EXEMPT_PREFIXES,
)
from .._walkers import _is_excluded, _read_text, _to_rel, _walk

# Resolve the parent ``wylde_check`` package dynamically (see _walkers).
_pkg = _sys.modules[__name__.rsplit(".", 2)[0]]


# ── Rule 1: no internal HTTP between Wylde components ────────────────


def _is_no_http_exempt(rel: str) -> bool:
    """True if file is allowed to do internal HTTP (Gateway, Ollama,
    Memgraph bolt, etc.)."""
    for prefix in NO_HTTP_EXEMPT_PREFIXES:
        if rel.startswith(prefix):
            return True
    return False


def _line_targets_internal(line: str) -> bool:
    """True if the line references a Wylde-internal host:port pair."""
    has_host = any(h in line for h in INTERNAL_HOSTS)
    has_port = any(p in line for p in INTERNAL_PORTS)
    return has_host or has_port


def check_no_internal_http() -> List[Finding]:
    out: List[Finding] = []
    files = _walk((".py", ".svelte", ".js", ".ts"))
    for path in files:
        rel = _to_rel(path)
        if _is_no_http_exempt(rel):
            continue
        # Skip tests — they're allowed to mock internal endpoints.
        if "/tests/" in rel or rel.endswith("_test.py") or rel.startswith("tests/"):
            continue
        text = _read_text(path)
        if not text:
            continue
        for lineno, line in enumerate(text.splitlines(), start=1):
            stripped = line.lstrip()
            if stripped.startswith("#") or stripped.startswith("//"):
                continue
            for pat in HTTP_CLIENT_PATTERNS:
                if pat.search(line) and _line_targets_internal(line):
                    out.append(
                        Finding(
                            rule="no_internal_http",
                            severity="error",
                            file=rel,
                            line=lineno,
                            message=(
                                "Internal HTTP call detected outside Gateway / "
                                "Ollama client / database driver scope.  Use the "
                                "pipe transport for Wylde-internal traffic."
                            ),
                            context=line.strip()[:200],
                        )
                    )
                    break  # one finding per line
    return out


# ── Rule 2: single manifest write path per service ────────────────────


def check_manifest_paths() -> List[Finding]:
    """Services that have a daemon-managed ``_start_X`` should NOT also
    write their own manifest from inside their ``run.py``."""
    daemon_state = _pkg.WYLDE_ROOT / "Core" / "Lifecycle" / "daemon_state.py"
    if not daemon_state.exists():
        return []
    ds_text = _read_text(daemon_state)
    # Find the canonical names of daemon-managed services from the
    # `_write_daemon_manifest("wylde-X", ...)` callsites.
    daemon_managed: List[str] = re.findall(
        r'_write_daemon_manifest\(\s*"(wylde-[a-z0-9_-]+)"', ds_text
    )
    out: List[Finding] = []
    # Map daemon-managed name to the most likely run.py location.
    candidate_runs: Dict[str, Path] = {
        "wylde-voice": _pkg.WYLDE_ROOT / "Voice" / "run.py",
        "wylde-device-gate": _pkg.WYLDE_ROOT / "device_gate" / "run.py",
        "wylde-gateway": _pkg.WYLDE_ROOT / "Gateway" / "run.py",
        "wylde-memgraph": _pkg.WYLDE_ROOT / "Core" / "Memgraph" / "run.py",
        "wylde-vram-broker": _pkg.WYLDE_ROOT / "Core" / "resource_monitor" / "run.py",
    }
    for name in daemon_managed:
        run_path = candidate_runs.get(name)
        if run_path is None or not run_path.exists():
            continue
        text = _read_text(run_path)
        if not text:
            continue
        for lineno, line in enumerate(text.splitlines(), start=1):
            stripped = line.lstrip()
            if stripped.startswith("#") or stripped.startswith('"'):
                continue
            if "write_manifest(" in line:
                out.append(
                    Finding(
                        rule="manifest_paths",
                        severity="warning",
                        file=_to_rel(run_path),
                        line=lineno,
                        message=(
                            f"Service {name!r} is daemon-managed (daemon_state.py "
                            f"writes its manifest); the call here duplicates that "
                            f"write and the registry filters it out."
                        ),
                        context=line.strip()[:200],
                    )
                )
    return out


# ── Rule 5: import path consistency ───────────────────────────────────


_WYLDE_CORE_IMPORT_RE = re.compile(r"\b(?:from|import)\s+Wylde\.Core\b")


def check_import_paths() -> List[Finding]:
    out: List[Finding] = []
    for path in _walk((".py",)):
        rel = _to_rel(path)
        # Tests use try-fallback for both forms — skip.
        if "/tests/" in rel or rel.endswith("_test.py"):
            continue
        text = _read_text(path)
        if not text:
            continue
        for lineno, line in enumerate(text.splitlines(), start=1):
            stripped = line.lstrip()
            if stripped.startswith("#"):
                continue
            if _WYLDE_CORE_IMPORT_RE.search(line):
                out.append(
                    Finding(
                        rule="import_paths",
                        severity="warning",
                        file=rel,
                        line=lineno,
                        message=(
                            "Use bare `Core.*` import path; `Wylde.Core.*` is "
                            "non-canonical in active code (tests use try-fallback)."
                        ),
                        context=line.strip()[:200],
                    )
                )
    return out


# ── Rule 6: dead service references ───────────────────────────────────


def _line_has_dead_ref_marker(line: str) -> bool:
    """True if the line carries an inline ``wylde-check: dead-ref-ok``
    suppression marker."""
    return any(m in line for m in DEAD_REF_OK_MARKERS)


def check_dead_service_refs() -> List[Finding]:
    out: List[Finding] = []
    for path in _walk(
        (".py", ".svelte", ".js", ".ts", ".rs", ".md", ".json"),
        roots=ACTIVE_ROOTS + ("",),
    ):
        rel = _to_rel(path)
        if _is_excluded(path):
            continue
        # Skip the checker package itself — it has the dead names as data.
        if rel.endswith("dev/wylde_check.py") or "/dev/wylde_check/" in rel:
            continue
        # Skip runtime state (manifests, broker state, etc.) — these are
        # written by services at runtime and can legitimately echo names
        # that were live at the time of the snapshot.
        if rel.startswith("data/"):
            continue
        # File-level allowlist (JSON archives / templates can't carry markers).
        if rel in DEAD_REF_ALLOWLISTED_FILES:
            continue
        text = _read_text(path)
        if not text:
            continue
        for lineno, line in enumerate(text.splitlines(), start=1):
            if _line_has_dead_ref_marker(line):
                continue
            for name in DEAD_SERVICE_NAMES:
                if name in line:
                    out.append(
                        Finding(
                            rule="dead_service_refs",
                            severity="warning",
                            file=rel,
                            line=lineno,
                            message=(
                                f"Reference to dead service {name!r}; "
                                f"renamed/removed during refactor."
                            ),
                            context=line.strip()[:200],
                        )
                    )
                    break  # one finding per line
    return out


# ── Rule 22: memory-layer storage paths stay inside the layer ───────


# Map layer-storage path fragment → the file-path prefix that's allowed
# to mention it.  Only memory-layer code reads or writes its layer's
# on-disk state; everything else routes through ``memory.<layer>.*`` pipe
# actions on wylde-harness.
_MEMORY_LAYER_PATHS: Dict[str, Tuple[str, ...]] = {
    "memory/indexes": ("Core/harness/memory/",),
    "memory/workspace_memories": ("Core/harness/memory/",),
    "memory/long_term": ("Core/harness/memory/",),
    "memory/short_term": ("Core/harness/memory/",),
}


def check_memory_layer_boundaries() -> List[Finding]:
    """Layer-storage paths (``memory/indexes/``, ``memory/long_term/``,
    etc.) must only appear in code that lives inside the memory layer
    (``Core/harness/memory/``).  Other callers go through the pipe
    actions on wylde-harness."""
    out: List[Finding] = []
    for path in _walk((".py", ".svelte", ".js", ".ts")):
        rel = _to_rel(path)
        if _is_excluded(path):
            continue
        # The checker itself names these literals as data — skip.
        if "/dev/wylde_check/" in rel:
            continue
        # Tests legitimately fixture these paths in synthetic trees.
        if "/tests/" in rel or rel.endswith("_test.py") or rel.startswith("tests/"):
            continue
        text = _read_text(path)
        if not text:
            continue
        for fragment, allowed_prefixes in _MEMORY_LAYER_PATHS.items():
            if any(rel.startswith(p) for p in allowed_prefixes):
                continue
            for lineno, line in enumerate(text.splitlines(), start=1):
                stripped = line.lstrip()
                if stripped.startswith("#") or stripped.startswith("//"):
                    continue
                if fragment in line:
                    out.append(
                        Finding(
                            rule="memory_layer_boundaries",
                            severity="error",
                            file=rel,
                            line=lineno,
                            message=(
                                f"References memory-layer storage path "
                                f"{fragment!r} from outside the layer.  "
                                f"Route through the corresponding "
                                f"``memory.*`` pipe action on wylde-harness "
                                f"instead of touching the on-disk state."
                            ),
                            context=line.strip()[:200],
                        )
                    )
    return out


# ── Rule 25: services own their state — no cross-service file reads ─


# Map service folder → the on-disk state-path fragments it owns. A
# different service touching one of these is the violation.  Forward
# slashes only, as with rule 10's GUI-bypass list.
_SERVICE_STATE_PATHS: Dict[str, Tuple[str, ...]] = {
    "Core/resource_monitor": ("Core/resource_monitor/data/",),
    "Gateway": ("Gateway/secrets/", "Gateway/logs/"),
    "device_gate": ("device_gate/data/",),
    "Voice": ("Voice/data/",),
    "VPN": ("VPN/data/", "VPN/tunnel/state/"),
    "Trainer": ("Trainer/data/",),
}


def _owning_service(rel: str) -> str:
    """Return the service-folder prefix that owns ``rel``, or '' for
    files that aren't inside a tracked service folder."""
    for svc in _SERVICE_STATE_PATHS:
        if rel == svc or rel.startswith(svc + "/"):
            return svc
    return ""


# Files that legitimately straddle two services as a documented hand-off
# placeholder.  Each entry must carry a punch-list reference in its
# module docstring so the eventual rewire is tracked.
_SERVICE_STATE_FILE_EXEMPTIONS: Tuple[str, ...] = (
    # device_gate API hand-off placeholder — file's module docstring
    # documents this as "punch-list item #9".
    "Gateway/auth/device_gate.py",
)


def check_service_owns_its_state() -> List[Finding]:
    """A service may only read or write paths under its own data
    directory.  Cross-service state access goes through the peer's pipe
    action.  Exception: tests, shared (``Core/shared/``), and the
    Lifecycle daemon (which legitimately knows about every service)
    are unrestricted."""
    out: List[Finding] = []
    for path in _walk((".py", ".rs")):
        rel = _to_rel(path)
        if _is_excluded(path):
            continue
        # Test files and the daemon itself are exempt.
        if "/tests/" in rel or rel.endswith("_test.py") or rel.startswith("tests/"):
            continue
        if rel.startswith("Core/Lifecycle/"):
            continue
        if rel.startswith("Core/shared/"):
            continue
        if "/dev/wylde_check/" in rel:
            continue
        # The GUI's own rule (rule 10) handles the GUI surface — don't
        # double-flag here.
        if rel.startswith("Core/GUI/"):
            continue
        if rel in _SERVICE_STATE_FILE_EXEMPTIONS:
            continue
        owner = _owning_service(rel)
        text = _read_text(path)
        if not text:
            continue
        for svc, fragments in _SERVICE_STATE_PATHS.items():
            if svc == owner:
                continue
            for fragment in fragments:
                for lineno, line in enumerate(text.splitlines(), start=1):
                    stripped = line.lstrip()
                    if stripped.startswith("#") or stripped.startswith("//"):
                        continue
                    if fragment in line:
                        out.append(
                            Finding(
                                rule="service_owns_its_state",
                                severity="error",
                                file=rel,
                                line=lineno,
                                message=(
                                    f"References {svc!r} state path "
                                    f"{fragment!r} from outside that "
                                    f"service.  Cross-service state access "
                                    f"must go through the peer's pipe "
                                    f"action, not the filesystem."
                                ),
                                context=line.strip()[:200],
                            )
                        )
    return out
