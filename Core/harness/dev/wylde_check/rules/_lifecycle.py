"""Boot / shutdown / service-manifest rules (44-47).

Added at the slice-11 cutover. These enforce the single-source contract:
boot and shutdown are both derived from ONE source of truth (never a
hand-kept roster), every top-level backend service carries a manifest, and
those manifests are schema-valid. The modular-service architecture is the
principle they protect — adding a core service is a one-row addition to the
`DAEMON_MANAGED` table (or, for an out-of-tree sibling, dropping a folder
with a conforming manifest); boot, shutdown, and dispatch all pick it up.

REPOINTED for issue #101 (0.2 stability audit, finding F): rules 44/45
formerly targeted `Core/Lifecycle/launcher.py` / `shutdown.py`, which the
full-Rust cutover DELETED — and, guarded by `if <file>.exists()`, they
skipped their body over the missing file and passed green. A dead gate.
They now target the LIVE Rust single source: the `DAEMON_MANAGED` table in
`rust/crates/wylde-lifecycle/src/daemon_managed.rs`, which drives boot,
shutdown, dispatch, and the kill-image list from one row per service. The
SEMANTIC set-equality gate (boot-set == shutdown-set == dispatch-set,
modulo the two typed exceptions) is the crate unit test
`daemon_managed::tests::boot_shutdown_dispatch_sets_agree`; these static
rules ensure that single source stays STRUCTURALLY in place.
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
    RUST_BOOT_FILE,
    RUST_BOOT_TABLE_TOKEN,
    RUST_DAEMON_MANAGED_FILE,
    RUST_DAEMON_MANAGED_TABLE_TOKEN,
    RUST_HARDCODED_SERVICE_ARRAY_RE,
    RUST_LIFECYCLE_CRATE,
    RUST_SHUTDOWN_FILE,
    RUST_SHUTDOWN_TABLE_TOKEN,
    SERVICE_MANIFEST_EXCLUDED_TOP_LEVEL,
    SERVICE_MANIFEST_NONSERVICE_DIRS,
    SERVICE_MANIFEST_REQUIRED_KEYS,
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


# ── Rule 44: boot is derived from the single DAEMON_MANAGED table ──────


def _require_token(file_rel: str, token: str, rule: str, message: str) -> List[Finding]:
    """Fire unless ``file_rel`` exists AND contains ``token``.

    A **missing file** and a **missing token** both fire — this is the
    fix at the heart of issue #101: the old rules guarded their body with
    ``if <file>.exists()``, so a deleted target file skipped the check and
    the rule passed green (a dead gate). Here, the single source going
    missing is itself the failure.
    """
    path = _pkg.WYLDE_ROOT / file_rel
    text = _read_text(path) if path.exists() else ""
    if token not in text:
        return [
            Finding(
                rule=rule,
                severity="error",
                file=file_rel,
                line=0,
                message=message,
            )
        ]
    return []


def check_launcher_enumerates_services_from_manifests() -> List[Finding]:
    """Boot must be derived from the single ``DAEMON_MANAGED`` table
    (`rust/crates/wylde-lifecycle/src/daemon_managed.rs`), not a
    hand-written run of ``start_<name>()`` calls or a hardcoded roster.

    (Rule key retained for registry/baseline stability; repointed for
    issue #101 from the deleted ``Core/Lifecycle/launcher.py`` to the live
    Rust boot path — the old rule ran over a missing file and passed green.)

    Fires when: the ``DAEMON_MANAGED`` table file is missing / no longer
    declares the table (the single source was removed), or ``daemon.rs`` no
    longer derives boot from it (``boot_sequence()`` gone), or a
    ``const``/``static SERVICES`` array roster reappears in the crate. The
    SEMANTIC boot-set == shutdown-set gate is the crate unit test
    ``daemon_managed::tests::boot_shutdown_dispatch_sets_agree``.
    """
    rule = "launcher_enumerates_services_from_manifests"
    out: List[Finding] = []
    out.extend(
        _require_token(
            RUST_DAEMON_MANAGED_FILE,
            RUST_DAEMON_MANAGED_TABLE_TOKEN,
            rule,
            "the single DAEMON_MANAGED table is missing — boot, shutdown, and "
            "dispatch must all derive from one source of truth in "
            f"{RUST_DAEMON_MANAGED_FILE} (issue #101). Restore the table.",
        )
    )
    out.extend(
        _require_token(
            RUST_BOOT_FILE,
            RUST_BOOT_TABLE_TOKEN,
            rule,
            "boot is no longer derived from the DAEMON_MANAGED table "
            "(`boot_sequence()` call missing in daemon.rs) — boot must iterate "
            "the single source, not a hand-written start_<name>() sequence.",
        )
    )
    out.extend(_scan_rust_for_hardcoded_roster(RUST_LIFECYCLE_CRATE, "boot"))
    return out


def _scan_rust_for_hardcoded_roster(crate_rel: str, surface: str) -> List[Finding]:
    """Flag a ``const``/``static`` SERVICES array in a Rust crate's src —
    a hand-kept roster reintroduced alongside the ``DAEMON_MANAGED`` table
    (the ``DAEMON_MANAGED`` table itself is not a ``SERVICES`` array and is
    intentionally not matched)."""
    out: List[Finding] = []
    rust_src = _pkg.WYLDE_ROOT / crate_rel / "src"
    if not rust_src.exists():
        return out
    rule = (
        "launcher_enumerates_services_from_manifests"
        if surface == "boot"
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
                            f"hardcoded service roster in the Rust {surface} path "
                            "— the core tier is driven by the single DAEMON_MANAGED "
                            "table (one row per service); a SERVICES array is the "
                            "hand-kept-roster anti-pattern issue #101 removed."
                        ),
                        context=line.strip()[:200],
                    )
                )
    return out


# ── Rule 45: shutdown is derived from the same DAEMON_MANAGED table ────


def check_shutdown_enumerates_services_from_manifests() -> List[Finding]:
    """``shutdown_all`` must drain the core tier in the order derived from
    the single ``DAEMON_MANAGED`` table (``state/mod.rs`` iterates
    ``shutdown_sequence()`` in ascending ``shutdown_rank``), not a
    hand-kept ``let steps: [_; N]`` array.

    (Rule key retained for registry/baseline stability; repointed for
    issue #101 from the deleted ``Core/Lifecycle/shutdown.py``.)

    Two-pronged:
    * the Rust drain (``state/mod.rs``) must derive its set + order from
      the table (``shutdown_sequence()``); and
    * the gpui-side ``shutdown.rs`` must *delegate* to the daemon drain
      (it dispatches ``lifecycle.shutdown_all``) rather than enumerate
      services itself. Its ``WYLDE_SERVICE_PROCESSES`` / ``WYLDE_KILL_TARGETS``
      constants are the recognised hard-kill image-name fallback — a last
      resort, not the enumeration — so they are not flagged.

    The SEMANTIC shutdown-set == boot-set gate is the crate unit test
    ``daemon_managed::tests::boot_shutdown_dispatch_sets_agree``.
    """
    rule = "shutdown_enumerates_services_from_manifests"
    out: List[Finding] = []
    out.extend(
        _require_token(
            RUST_SHUTDOWN_FILE,
            RUST_SHUTDOWN_TABLE_TOKEN,
            rule,
            "shutdown is no longer derived from the DAEMON_MANAGED table "
            "(`shutdown_sequence()` call missing in state/mod.rs) — the drain "
            "must iterate the single source, not a hand-kept `let steps: [_; N]` "
            "array.",
        )
    )
    # gpui-side graceful shutdown must delegate to the daemon drain rather
    # than enumerate services itself. Hardened to fire if the file is
    # missing too (no silent pass over a deleted delegate — issue #101).
    out.extend(
        _require_token(
            GPUI_SHUTDOWN_RS,
            GPUI_SHUTDOWN_DELEGATE_TOKEN,
            rule,
            "gpui graceful shutdown no longer delegates to the daemon drain "
            f"({GPUI_SHUTDOWN_DELEGATE_TOKEN!r} dispatch missing) — it must not "
            "enumerate services on its own; route through lifecycle.shutdown_all.",
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
