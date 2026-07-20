"""Runtime / lifecycle rules: pipe-name convention, manifest orphan reap.

Retired 2026-07-20 (dead-rule retirement): ``logging_setup_only``
(rule 13), ``no_external_subprocess`` (rule 14), ``spawn_paths_exist``
(rule 15), ``run_py_entry_point`` (rule 16), ``run_py_startup_sequence``
(rule 18) and ``shutdown_handler_marks_stopped`` (rule 19).  Rules 13/14
were Python-only with no production Python left to walk; 15/16/18/19
keyed on ``Core/Lifecycle/daemon_state.py`` and per-service ``run.py``
entry points, all deleted in the Rust cutover."""

from __future__ import annotations

import re
import sys as _sys
from typing import List

from .. import Finding
from .._config import (
    PIPE_NAME_GOOD_RE,
    PIPE_NAME_REF_RE,
    PIPE_NAME_TYPO_RE,
)
from .._walkers import _read_text, _to_rel, _walk

_pkg = _sys.modules[__name__.rsplit(".", 2)[0]]


# ── Rule 17: named-pipe naming convention ────────────────────────────


def check_pipe_name_convention() -> List[Finding]:
    """Every ``wylde-<name>`` named-pipe literal in active code must be
    lowercase, dash-separated, and start with ``wylde-``."""
    out: List[Finding] = []
    seen: set = set()
    for path in _walk((".py", ".js", ".svelte", ".rs", ".md", ".json")):
        rel = _to_rel(path)
        # The checker itself uses ``wylde-X`` / ``wylde-foo`` as
        # placeholders in docstrings and rule messages — skip it.
        if rel.endswith("dev/wylde_check.py") or "/dev/wylde_check/" in rel:
            continue
        # The wylde_check test package uses bad-form pipe names as
        # synthetic data; skip it wholesale.
        if "/dev/tests/wylde_check/" in rel:
            continue
        text = _read_text(path)
        if not text:
            continue
        for lineno, line in enumerate(text.splitlines(), start=1):
            # Pass 1: canonical dash form with bad casing / trailing noise.
            for m in PIPE_NAME_REF_RE.finditer(line):
                name = m.group(0)
                if PIPE_NAME_GOOD_RE.match(name):
                    continue
                if (name, rel, lineno) in seen:
                    continue
                seen.add((name, rel, lineno))
                out.append(
                    Finding(
                        rule="pipe_name_convention",
                        severity="error",
                        file=rel,
                        line=lineno,
                        message=(
                            f"Pipe name {name!r} does not match the "
                            f"convention `^wylde-[a-z][a-z0-9-]*$`.  Use "
                            f"lowercase, dash-separated form."
                        ),
                        context=line.strip()[:200],
                    )
                )
            # Pass 2: typo'd underscore form, only inside quoted strings.
            for m in PIPE_NAME_TYPO_RE.finditer(line):
                name = m.group(1)
                if (name, rel, lineno) in seen:
                    continue
                seen.add((name, rel, lineno))
                out.append(
                    Finding(
                        rule="pipe_name_convention",
                        severity="error",
                        file=rel,
                        line=lineno,
                        message=(
                            f"Pipe name {name!r} uses underscores; the "
                            f"convention is dash-separated "
                            f"(``wylde-{name[len('wylde_') :].replace('_', '-')}``)."
                        ),
                        context=line.strip()[:200],
                    )
                )
    return out


# ── Rule 31: daemon reaps manifest orphans ───────────────────────────


# The Rust lifecycle daemon's boot path, which owns the manifest
# orphan sweep, and the module that defines the sweep itself.
#
# (Rule key retained for registry/baseline stability; repointed for
# issue #116 from the deleted ``Core/Lifecycle/daemon_state/__init__.py``
# to the live Rust lifecycle crate.  The guarantee did not disappear in
# the Rust cutover — it *moved*: teardown no longer reaps, boot sweeps.
# See the rule docstring for why that relocation is the same safety net.)
_ORPHAN_DAEMON_FILE = "rust/crates/wylde-lifecycle/src/daemon.rs"
_ORPHAN_SWEEP_FILE = "rust/crates/wylde-lifecycle/src/state/orphan_sweep.rs"

# The sweep entry point the daemon must invoke on the boot path, and
# the definition that must back it.  Both are pattern-bound rather than
# literal so the implementation can be renamed (``boot_manifest_sweep``,
# ``sweep_boot_orphans``, …) without churn here, but cannot be silently
# replaced by an unrelated call.
_ORPHAN_SWEEP_CALL_RE = re.compile(r"\b([a-z_]*orphan[a-z_]*sweep[a-z_]*|[a-z_]*sweep[a-z_]*orphan[a-z_]*)\s*\(")
_ORPHAN_SWEEP_DEF_RE = re.compile(
    r"^\s*pub\s+fn\s+([a-z_]*orphan[a-z_]*sweep[a-z_]*|[a-z_]*sweep[a-z_]*orphan[a-z_]*)\s*\(",
    re.MULTILINE,
)

# The boot path's first service launch.  The sweep must precede it.
_SERVICE_START_RE = re.compile(r"\bstart_[a-z_]+\s*\(")

# Calls that halt the recurring sweep rather than perform one.  These
# satisfy the *name* pattern but not the guarantee, so they are excluded
# before the ordering check — otherwise ``stop_orphan_sweep()`` inside
# teardown would count as a boot-time reap.
_ORPHAN_SWEEP_NEGATIVE_RE = re.compile(r"\b(stop|halt|cancel|abort)_[a-z_]*orphan[a-z_]*sweep[a-z_]*\s*\(")


def _rust_code_lines(text: str) -> List[tuple]:
    """1-based ``(lineno, line)`` pairs with ``//`` / ``//!`` comment
    lines and trailing line comments stripped.

    A doc comment that merely *mentions* the sweep must not satisfy the
    rule — that is the rules-44/45 residue this rule refuses to repeat.
    """
    out: List[tuple] = []
    for lineno, line in enumerate(text.splitlines(), start=1):
        stripped = line.lstrip()
        if stripped.startswith("//"):
            continue
        code = line.split("//", 1)[0]
        if not code.strip():
            continue
        out.append((lineno, code))
    return out


def check_shutdown_reaps_manifest_orphans() -> List[Finding]:
    """The lifecycle daemon must reap manifest orphans before it starts
    any service.

    Why: a manifest left behind by an ungraceful prior exit (Ctrl-C,
    ``taskkill``, SIGKILL) still marks its service "alive" with a pid
    that is now dead.  Nothing in the new daemon's in-memory state knows
    about it, and the recurring 60s sweep only fires *after* the boot
    spawns — so without a one-shot sweep on the boot path the stale
    manifest survives a lifecycle restart and the affected service stays
    dark.  That is the harness / extension_bridge / ollama outage of
    2026-05-31, recorded in ``daemon.rs`` Phase 2b-sweep.

    Where the guarantee lives now: under the Python daemon this was a
    reap step inside ``stop_all_daemon_managed`` (teardown).  The Rust
    cutover moved it to the *boot* path — ``stop_all_daemon_managed`` in
    ``state/mod.rs`` now only calls ``stop_orphan_sweep()``, which halts
    the recurring sweep so an in-flight tick cannot rewrite a manifest
    mid-teardown.  It performs no reap.  Checking teardown for a reaper
    would therefore be checking for something the system deliberately no
    longer does; the rule follows the guarantee to its new home.

    Fires when: either target file is missing (the #101 inversion — a
    deleted target is the failure, never a pass), the sweep has no
    ``pub fn`` definition, the daemon never calls it, or it is called
    only *after* the first ``start_<service>()`` on the boot path.
    """
    out: List[Finding] = []

    sweep_rel = _ORPHAN_SWEEP_FILE
    daemon_rel = _ORPHAN_DAEMON_FILE
    sweep_path = _pkg.WYLDE_ROOT / sweep_rel
    daemon_path = _pkg.WYLDE_ROOT / daemon_rel

    # ── Inverted guard (#101): a missing target fires, never passes ──
    sweep_text = _read_text(sweep_path) if sweep_path.exists() else ""
    daemon_text = _read_text(daemon_path) if daemon_path.exists() else ""

    if not sweep_text:
        out.append(
            Finding(
                rule="shutdown_reaps_manifest_orphans",
                severity="error",
                file=sweep_rel,
                line=0,
                message=(
                    f"Expected the manifest orphan sweep at {sweep_rel!r}; "
                    f"file missing or empty.  The rule cannot verify that "
                    f"orphaned services are reaped, which is a failure, not "
                    f"a pass.  If the sweep moved, repoint the rule."
                ),
            )
        )

    if not daemon_text:
        out.append(
            Finding(
                rule="shutdown_reaps_manifest_orphans",
                severity="error",
                file=daemon_rel,
                line=0,
                message=(
                    f"Expected the lifecycle daemon boot path at "
                    f"{daemon_rel!r}; file missing or empty.  If the daemon "
                    f"moved, repoint the rule."
                ),
            )
        )

    if out:
        return out

    # ── The sweep must actually be defined ──────────────────────────
    def_match = _ORPHAN_SWEEP_DEF_RE.search(sweep_text)
    if def_match is None:
        out.append(
            Finding(
                rule="shutdown_reaps_manifest_orphans",
                severity="error",
                file=sweep_rel,
                line=0,
                message=(
                    f"{sweep_rel!r} declares no public orphan-sweep function "
                    f"(expected a ``pub fn`` whose name matches "
                    f"``*orphan*sweep*``, e.g. ``boot_orphan_sweep``).  "
                    f"Without it the daemon has no manifest-walking reap."
                ),
            )
        )
        return out

    # ── The daemon must call it, on real code, before any start_ ────
    sweep_lineno = None
    first_start_lineno = None
    for lineno, code in _rust_code_lines(daemon_text):
        if (
            sweep_lineno is None
            and _ORPHAN_SWEEP_CALL_RE.search(code)
            and not _ORPHAN_SWEEP_NEGATIVE_RE.search(code)
        ):
            sweep_lineno = lineno
        if first_start_lineno is None and _SERVICE_START_RE.search(code):
            first_start_lineno = lineno

    if sweep_lineno is None:
        out.append(
            Finding(
                rule="shutdown_reaps_manifest_orphans",
                severity="error",
                file=daemon_rel,
                line=0,
                message=(
                    f"{daemon_rel!r} never calls a manifest orphan sweep.  "
                    f"Without it, a service orphaned by an ungraceful prior "
                    f"exit keeps an alive-marked manifest with a dead pid and "
                    f"stays dark across every restart.  Call the "
                    f"``*orphan*sweep*`` entry point on the boot path, before "
                    f"the first ``start_<service>()``.  (A doc comment "
                    f"mentioning it does not count.)"
                ),
            )
        )
        return out

    if first_start_lineno is not None and sweep_lineno > first_start_lineno:
        out.append(
            Finding(
                rule="shutdown_reaps_manifest_orphans",
                severity="error",
                file=daemon_rel,
                line=sweep_lineno,
                message=(
                    f"The manifest orphan sweep runs at line {sweep_lineno}, "
                    f"after the first service launch at line "
                    f"{first_start_lineno}.  A stale manifest must be reaped "
                    f"BEFORE any ``start_<service>()`` — otherwise the launch "
                    f"sees an alive-marked manifest with a dead pid and skips "
                    f"the service, which is the failure mode the sweep exists "
                    f"to prevent."
                ),
            )
        )
    return out

# Rule 20 (file_size_limit) lives in the sibling ``_quality.py`` submodule.
