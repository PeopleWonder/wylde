"""Boot / shutdown rules (44-45).

Added at the slice-11 cutover. These enforce the single-source contract:
boot and shutdown are both derived from ONE source of truth (never a
hand-kept roster). The modular-service architecture is the
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

Retired 2026-07-20 (dead-rule retirement): ``every_service_has_manifest``
(rule 46) and ``service_manifest_schema`` (rule 47).  Both keyed on
top-level per-service folders carrying a ``manifest.json`` — the Python
service tree they discovered was deleted in the Rust cutover.
"""

from __future__ import annotations

import sys as _sys
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


def _strip_rust_comments(text: str) -> str:
    """``text`` with ``//``, ``//!`` and ``///`` comments removed.

    Block comments (``/* … */``) are left alone: the lifecycle targets
    don't use them, and a naive strip would corrupt string literals
    containing ``/*``.  Line comments are the ones that matter here —
    every token these rules test for is also *named* in a doc comment
    beside the real call.

    Without this, rules 44/45 were satisfiable by prose: deleting the
    real ``boot_sequence()`` call at ``daemon.rs:187`` while leaving the
    doc comment at ``:180`` that merely mentions it kept the rule green
    (issue #116).  A gate that a comment can satisfy is not a gate.
    """
    out: List[str] = []
    for line in text.splitlines():
        stripped = line.lstrip()
        if stripped.startswith("//"):
            continue
        out.append(line.split("//", 1)[0])
    return "\n".join(out)


def _require_token(file_rel: str, token: str, rule: str, message: str) -> List[Finding]:
    """Fire unless ``file_rel`` exists AND contains ``token`` **in code**.

    A **missing file** and a **missing token** both fire — this is the
    fix at the heart of issue #101: the old rules guarded their body with
    ``if <file>.exists()``, so a deleted target file skipped the check and
    the rule passed green (a dead gate). Here, the single source going
    missing is itself the failure.

    Comments are stripped before the test (issue #116) so a doc comment
    mentioning the token cannot stand in for the call itself.
    """
    path = _pkg.WYLDE_ROOT / file_rel
    text = _read_text(path) if path.exists() else ""
    text = _strip_rust_comments(text)
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
      services itself.

    This rule does NOT check service coverage of the GUI's hard-kill and
    drain-wait sets, and a pass here says nothing about it. It used to
    exempt the ``WYLDE_SERVICE_PROCESSES`` / ``WYLDE_KILL_TARGETS``
    constants explicitly as "a recognised last resort"; that exemption
    was load-bearing for issue #124, where both were hand-typed arrays
    naming four of eleven killable services and the drain wait polled the
    same four — so it reported a clean shutdown with eight services still
    alive. Those constants no longer exist; both sets derive from
    ``wylde_stack::shutdown_targets``.

    The SEMANTIC gates are Rust tests, not this rule:
    * shutdown-set == boot-set —
      ``daemon_managed::tests::boot_shutdown_dispatch_sets_agree``;
    * GUI shutdown coverage (the counting gate, #124) —
      ``rust/crates/wylde-stack/tests/shutdown_target_coverage.rs``,
      which also fails if ``shutdown.rs`` regrows a hand-typed image
      list.
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
