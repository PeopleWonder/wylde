"""Architectural rules: dead-service references, service state
ownership.

Retired 2026-07-20 (dead-rule retirement): ``no_internal_http`` (rule 1),
``manifest_paths`` (rule 2), ``import_paths`` (rule 5) and
``memory_layer_boundaries`` (rule 22).  The first three were Python-only
rules with no production Python left to walk; rule 22's target tree
(``Core/harness/memory/``) was deleted in the Rust cutover."""

from __future__ import annotations

from typing import Dict, List, Tuple

from .. import Finding
from .._config import (
    ACTIVE_ROOTS,
    DEAD_REF_ALLOWLISTED_FILES,
    DEAD_REF_OK_MARKERS,
    DEAD_SERVICE_NAMES,
)
from .._walkers import _is_excluded, _read_text, _to_rel, _walk


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
