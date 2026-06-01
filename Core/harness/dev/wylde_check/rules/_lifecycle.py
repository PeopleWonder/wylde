"""Launcher / shutdown / service-manifest rules (44-47).

Added at the slice-11 cutover. These enforce the filesystem-as-registry
contract end to end: the launcher and shutdown both build their service
set from manifests (never a hardcoded roster), every top-level backend
service carries a manifest, and those manifests are schema-valid. The
modular-service architecture is the principle they protect — adding a
service folder with a conforming manifest is all it takes for the
launcher to discover it and shutdown to drain it in the right order.
"""

from __future__ import annotations

import json
import sys as _sys
from pathlib import Path
from typing import List

from .. import Finding
from .._config import (
    GPUI_SHUTDOWN_DELEGATE_TOKEN,
    GPUI_SHUTDOWN_RS,
    LAUNCHER_MANIFEST_REFERENCES,
    LAUNCHER_PY,
    PY_HARDCODED_SERVICE_LIST_RE,
    RUST_HARDCODED_SERVICE_ARRAY_RE,
    RUST_LIFECYCLE_CRATE,
    SERVICE_MANIFEST_EXCLUDED_TOP_LEVEL,
    SERVICE_MANIFEST_NONSERVICE_DIRS,
    SERVICE_MANIFEST_REQUIRED_KEYS,
    SHUTDOWN_ENUMERATION_REFERENCES,
    SHUTDOWN_PY,
)
from .._walkers import _is_excluded, _read_text, _to_rel

_pkg = _sys.modules[__name__.rsplit(".", 2)[0]]


def _noncomment_lines(text: str, comment_prefixes: tuple[str, ...]) -> list[tuple[int, str]]:
    """1-based (lineno, line) pairs, skipping whole-line comments."""
    out: list[tuple[int, str]] = []
    for lineno, line in enumerate(text.splitlines(), start=1):
        stripped = line.lstrip()
        if any(stripped.startswith(p) for p in comment_prefixes):
            continue
        out.append((lineno, line))
    return out


# ── Rule 44: launcher enumerates services from manifests ──────────────


def check_launcher_enumerates_services_from_manifests() -> List[Finding]:
    """The launcher must build its service set from the filesystem
    registry (``services.yaml`` + per-service ``manifest.json``), not a
    hardcoded roster.

    Two-pronged on the Python launcher: it must *reference* a manifest /
    registry loader (positive), and it must not assign a module-level
    UPPERCASE ``SERVICES`` list literal (negative). The Rust lifecycle
    crate is held to the negative half only — it spawns tier=core
    services via an explicit, documented ``start_<name>`` sequence (bespoke
    per-service bring-up), which is intentionally NOT a data-driven list;
    a hardcoded ``const SERVICES: [&str; N]`` roster *would* be flagged.
    """
    out: List[Finding] = []

    launcher = _pkg.WYLDE_ROOT / LAUNCHER_PY
    if launcher.exists():
        text = _read_text(launcher)
        if text:
            if not any(ref in text for ref in LAUNCHER_MANIFEST_REFERENCES):
                out.append(
                    Finding(
                        rule="launcher_enumerates_services_from_manifests",
                        severity="error",
                        file=LAUNCHER_PY,
                        line=0,
                        message=(
                            "launcher no longer enumerates services from the "
                            "filesystem registry — expected a call to one of "
                            f"{', '.join(LAUNCHER_MANIFEST_REFERENCES)}."
                        ),
                    )
                )
            for lineno, line in _noncomment_lines(text, ("#",)):
                if PY_HARDCODED_SERVICE_LIST_RE.match(line):
                    out.append(
                        Finding(
                            rule="launcher_enumerates_services_from_manifests",
                            severity="error",
                            file=LAUNCHER_PY,
                            line=lineno,
                            message=(
                                "hardcoded service roster in the launcher — "
                                "build the service list from manifests "
                                "(load_services / load_manifest), not a literal."
                            ),
                            context=line.strip()[:200],
                        )
                    )

    out.extend(_scan_rust_for_hardcoded_roster(RUST_LIFECYCLE_CRATE, "launcher"))
    return out


def _scan_rust_for_hardcoded_roster(crate_rel: str, surface: str) -> List[Finding]:
    """Flag a ``const``/``static`` SERVICES array in a Rust crate's src."""
    out: List[Finding] = []
    rust_src = _pkg.WYLDE_ROOT / crate_rel / "src"
    if not rust_src.exists():
        return out
    rule = (
        "launcher_enumerates_services_from_manifests"
        if surface == "launcher"
        else "shutdown_enumerates_services_from_manifests"
    )
    for path in sorted(rust_src.rglob("*.rs")):
        if _is_excluded(path):
            continue
        text = _read_text(path)
        if not text:
            continue
        for lineno, line in _noncomment_lines(text, ("//",)):
            if RUST_HARDCODED_SERVICE_ARRAY_RE.search(line):
                out.append(
                    Finding(
                        rule=rule,
                        severity="error",
                        file=_to_rel(path),
                        line=lineno,
                        message=(
                            f"hardcoded service roster in the Rust {surface} — "
                            "tier=core bring-up is an explicit start_<name> "
                            "sequence by design, but a SERVICES array is the "
                            "anti-pattern; enumerate from manifests instead."
                        ),
                        context=line.strip()[:200],
                    )
                )
    return out


# ── Rule 45: shutdown enumerates services from manifests ──────────────


def check_shutdown_enumerates_services_from_manifests() -> List[Finding]:
    """``shutdown_all`` must drain the running set in a manifest-driven
    order (reverse-launch by default, ``shutdown_order`` override), not a
    hardcoded service list.

    The Python ``shutdown.py`` is the canonical drain the GUI reaches via
    ``lifecycle.shutdown_all``; it must reference the running-set / manifest
    enumeration and carry no hardcoded roster. The gpui-side
    ``shutdown.rs`` must *delegate* to that drain (it dispatches
    ``lifecycle.shutdown_all``); its ``WYLDE_SERVICE_PROCESSES`` /
    ``WYLDE_KILL_TARGETS`` constants are the recognised hard-kill image-name
    fallback — a last resort, not the enumeration — so they are not flagged.
    """
    out: List[Finding] = []

    shutdown = _pkg.WYLDE_ROOT / SHUTDOWN_PY
    if shutdown.exists():
        text = _read_text(shutdown)
        if text:
            if not any(ref in text for ref in SHUTDOWN_ENUMERATION_REFERENCES):
                out.append(
                    Finding(
                        rule="shutdown_enumerates_services_from_manifests",
                        severity="error",
                        file=SHUTDOWN_PY,
                        line=0,
                        message=(
                            "shutdown no longer enumerates the running service "
                            "set — expected a reference to one of "
                            f"{', '.join(SHUTDOWN_ENUMERATION_REFERENCES)}."
                        ),
                    )
                )
            for lineno, line in _noncomment_lines(text, ("#",)):
                if PY_HARDCODED_SERVICE_LIST_RE.match(line):
                    out.append(
                        Finding(
                            rule="shutdown_enumerates_services_from_manifests",
                            severity="error",
                            file=SHUTDOWN_PY,
                            line=lineno,
                            message=(
                                "hardcoded service roster in shutdown — order "
                                "the drain from the running set + manifest "
                                "shutdown_order, not a literal list."
                            ),
                            context=line.strip()[:200],
                        )
                    )

    # gpui-side graceful shutdown must delegate to the manifest-driven
    # Python drain rather than enumerate services itself.
    gpui_shutdown = _pkg.WYLDE_ROOT / GPUI_SHUTDOWN_RS
    if gpui_shutdown.exists():
        text = _read_text(gpui_shutdown)
        if text and GPUI_SHUTDOWN_DELEGATE_TOKEN not in text:
            out.append(
                Finding(
                    rule="shutdown_enumerates_services_from_manifests",
                    severity="error",
                    file=GPUI_SHUTDOWN_RS,
                    line=0,
                    message=(
                        "gpui graceful shutdown no longer delegates to the "
                        f"manifest-driven drain ({GPUI_SHUTDOWN_DELEGATE_TOKEN!r} "
                        "dispatch missing) — it must not enumerate services on "
                        "its own; route through lifecycle.shutdown_all."
                    ),
                )
            )

    return out


# ── Rule 46: every backend service has a manifest ─────────────────────


def _top_level_service_dirs() -> List[Path]:
    """Top-level WYLDE_ROOT subdirs that count as candidate services —
    every dir that is not in the excluded set and not ``_``/``.``-prefixed.
    Mirrors Core/Lifecycle/_common.list_service_folders."""
    out: List[Path] = []
    root = _pkg.WYLDE_ROOT
    if not root.exists():
        return out
    for p in sorted(root.iterdir(), key=lambda x: x.name):
        if not p.is_dir():
            continue
        if p.name in SERVICE_MANIFEST_EXCLUDED_TOP_LEVEL:
            continue
        if p.name.startswith(("_", ".")):
            continue
        out.append(p)
    return out


def check_every_service_has_manifest() -> List[Finding]:
    """Bidirectional service↔manifest coverage at the top level (the
    launcher's discovery domain):

    * **Forward** — a top-level folder that follows the entry-point
      convention (has a ``run.py``) must carry a ``manifest.json`` so the
      launcher can discover + order it.
    * **Reverse** — a runtime/archive dir (``data``/``logs``/``docs``)
      must NOT carry a service manifest (an auto-gen stub there is the
      bug this catches; ``Core`` is exempt — it has a legit infra rollup).
    """
    out: List[Finding] = []

    # Forward: run.py implies a service → manifest required.
    for folder in _top_level_service_dirs():
        if not (folder / "run.py").exists():
            continue
        if not (folder / "manifest.json").exists():
            out.append(
                Finding(
                    rule="every_service_has_manifest",
                    severity="error",
                    file=f"{_to_rel(folder)}/run.py",
                    line=0,
                    message=(
                        f"service folder {folder.name!r} has a run.py entry "
                        "point but no manifest.json — the launcher can't "
                        "discover, order, or shut it down. Add a manifest."
                    ),
                )
            )

    # Reverse: no service manifest in a runtime/archive dir.
    for name in SERVICE_MANIFEST_NONSERVICE_DIRS:
        mf = _pkg.WYLDE_ROOT / name / "manifest.json"
        if mf.exists():
            out.append(
                Finding(
                    rule="every_service_has_manifest",
                    severity="error",
                    file=f"{name}/manifest.json",
                    line=0,
                    message=(
                        f"{name!r} is a runtime/archive dir, not a service, "
                        "but carries a manifest.json (a stale discovery "
                        "auto-gen). Remove it — the launcher would otherwise "
                        "try to register it as a service."
                    ),
                )
            )

    return out


# ── Rule 47: service manifests are schema-valid ───────────────────────


def check_service_manifest_schema() -> List[Finding]:
    """Every top-level service ``manifest.json`` must declare the required
    keys and use the right types.

    Required: ``name`` (non-empty str), ``entry_point`` (key present; str
    or null — the canonical launch command / binary), ``shutdown_order``
    (int). Optional but type-checked when present: ``depends_on`` (list),
    ``health_check`` (null / str / object), ``tier`` (str).
    """
    out: List[Finding] = []

    for folder in _top_level_service_dirs():
        mf = folder / "manifest.json"
        if not mf.exists():
            continue
        rel = _to_rel(mf)
        try:
            data = json.loads(_read_text(mf))
        except (ValueError, TypeError):
            out.append(
                Finding(
                    rule="service_manifest_schema",
                    severity="error",
                    file=rel,
                    line=0,
                    message="manifest.json is not valid JSON.",
                )
            )
            continue
        if not isinstance(data, dict):
            out.append(
                Finding(
                    rule="service_manifest_schema",
                    severity="error",
                    file=rel,
                    line=0,
                    message="manifest.json must be a JSON object.",
                )
            )
            continue

        for key in SERVICE_MANIFEST_REQUIRED_KEYS:
            if key not in data:
                out.append(
                    Finding(
                        rule="service_manifest_schema",
                        severity="error",
                        file=rel,
                        line=0,
                        message=(
                            f"service manifest missing required key {key!r} "
                            "(required: "
                            f"{', '.join(SERVICE_MANIFEST_REQUIRED_KEYS)})."
                        ),
                    )
                )

        # Type checks on the keys that are present.
        name = data.get("name")
        if "name" in data and (not isinstance(name, str) or not name.strip()):
            out.append(_type_finding(rel, "name", "a non-empty string"))

        if "entry_point" in data and not (
            data["entry_point"] is None or isinstance(data["entry_point"], str)
        ):
            out.append(_type_finding(rel, "entry_point", "a string or null"))

        if "shutdown_order" in data and not isinstance(data["shutdown_order"], int):
            out.append(_type_finding(rel, "shutdown_order", "an integer"))

        if "depends_on" in data and not isinstance(data["depends_on"], list):
            out.append(_type_finding(rel, "depends_on", "a list"))

        if "health_check" in data and not (
            data["health_check"] is None
            or isinstance(data["health_check"], (str, dict))
        ):
            out.append(_type_finding(rel, "health_check", "null, a string, or an object"))

        if "tier" in data and not isinstance(data["tier"], str):
            out.append(_type_finding(rel, "tier", "a string"))

    return out


def _type_finding(rel: str, key: str, expected: str) -> Finding:
    return Finding(
        rule="service_manifest_schema",
        severity="error",
        file=rel,
        line=0,
        message=f"service manifest key {key!r} must be {expected}.",
    )
