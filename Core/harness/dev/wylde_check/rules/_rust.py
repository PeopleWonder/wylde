"""Rust-side architectural rules.

Mirrors the Python rules where the equivalent constraint applies to the
Rust workspace at ``rust/crates/``:

* :func:`check_import_paths_rust` — discourages deep ``super::super``
  traversal and cross-crate imports that bypass ``wylde-shared``.
  Exempts the Core-plugin SDK (``wylde_plugin_api``, importable
  everywhere) and ``wylde_plugin_*`` crates from ``wylde-harness``
  (the plugin host) — see the rule docstring.
* :func:`check_no_silent_error_swallow_rust` — flags ``let _ = expr;``
  and trailing ``.ok();`` patterns that drop a Result without logging.
* :func:`check_logging_setup_only_rust` — only the canonical
  ``wylde_shared::logging::configure_logging`` may build/init the
  tracing subscriber.
* :func:`check_no_external_process_spawn_rust` — ``Command::new`` use
  is restricted to the ``wylde-lifecycle`` crate.

All rules walk ``rust/crates/*/src/**/*.rs`` and skip integration tests
(``rust/crates/*/tests/**/*.rs``) which legitimately import each
crate's public API.
"""

from __future__ import annotations

import re
import sys as _sys
from pathlib import Path
from typing import List, Optional

from .. import Finding
from .._config import (
    RUST_CRATES_ROOT,
    RUST_CROSS_CRATE_EDGE_EXEMPTIONS,
    RUST_DEEP_SUPER_RE,
    RUST_DISCARD_RESULT_MARKER,
    RUST_LET_UNDERSCORE_RE,
    RUST_LOG_ROTATION_FACTORY_FILE,
    RUST_LOGGING_INIT_OK_MARKER,
    RUST_LOGGING_INIT_PATTERNS,
    RUST_PROCESS_SPAWN_ALLOWED_CRATES,
    RUST_PROCESS_SPAWN_OK_MARKER,
    RUST_PROCESS_SPAWN_PATTERNS,
    RUST_SHARED_SURFACE_CRATES,
    RUST_UNBOUNDED_APPEND_MARKER,
    RUST_UNBOUNDED_APPEND_PATTERNS,
    RUST_USE_CRATE_RE,
)
from .._walkers import _is_excluded, _read_text, _to_rel

_pkg = _sys.modules[__name__.rsplit(".", 2)[0]]


def _crate_of(rel: str) -> Optional[str]:
    """Extract the crate folder name from a path under ``rust/crates/``.

    ``rust/crates/wylde-vram-broker/src/foo.rs`` → ``"wylde-vram-broker"``.
    Returns ``None`` for files that aren't inside the workspace.
    """
    prefix = RUST_CRATES_ROOT + "/"
    if not rel.startswith(prefix):
        return None
    rest = rel[len(prefix) :]
    return rest.split("/", 1)[0] if "/" in rest else None


def _crate_use_name(crate_folder: str) -> str:
    """Cargo treats hyphens as underscores in ``use`` paths."""
    return crate_folder.replace("-", "_")


def _walk_rust_sources() -> List[Path]:
    """Yield every ``rust/crates/<crate>/src/**/*.rs`` file."""
    out: List[Path] = []
    crates_root = _pkg.WYLDE_ROOT / RUST_CRATES_ROOT
    if not crates_root.exists():
        return out
    for crate_dir in sorted(crates_root.iterdir()):
        if not crate_dir.is_dir():
            continue
        src_dir = crate_dir / "src"
        if not src_dir.exists():
            continue
        for path in src_dir.rglob("*.rs"):
            if _is_excluded(path):
                continue
            out.append(path)
    return out


def _is_doc_or_comment(stripped: str) -> bool:
    return (
        stripped.startswith("//")
        or stripped.startswith("/*")
        or stripped.startswith("*")
    )


# ── Rule 26: import_paths_rust ───────────────────────────────────────


def check_import_paths_rust() -> List[Finding]:
    """Wylde Rust crates must depend on each other only via
    ``wylde-shared``.  Deep ``super::super::`` chains are also flagged
    — by the time you need to traverse three module levels up, the
    module organisation is wrong.

    Principled exemptions (taxonomy reorg TX S4 — Core plugins):

    * ``wylde_plugin_api`` is importable from **everywhere** — it is the
      Core-plugin SDK, a shared *authoring surface* exactly like
      ``wylde_shared``: pure types + trait, no service logic, no peer
      surface being bypassed.
    * ``wylde_plugin_*`` crates (the plugins themselves) are importable
      from **wylde-harness only** — the harness IS the plugin host, and
      compile-time linkage (one dep line + one ``Box::new`` line) is the
      plugins' deliberate discovery mechanism.  Any other crate linking
      a plugin would be routing around the host.

    Note: this rule walks ``rust/crates/*/src`` only, so the plugin
    crates themselves — which live at ``Core/Plugins/<name>/`` by design
    and import ``wylde_plugin_api`` — are not seen by it at all.  The
    exemptions above exist for the workspace side: the harness host's
    ``use wylde_plugin_*`` lines and any crate importing the SDK.
    """
    out: List[Finding] = []
    for path in _walk_rust_sources():
        rel = _to_rel(path)
        crate = _crate_of(rel)
        if crate is None:
            continue
        own_use_name = _crate_use_name(crate)
        text = _read_text(path)
        if not text:
            continue
        for lineno, line in enumerate(text.splitlines(), start=1):
            stripped = line.lstrip()
            if _is_doc_or_comment(stripped):
                continue
            if RUST_DEEP_SUPER_RE.search(line):
                out.append(
                    Finding(
                        rule="import_paths_rust",
                        severity="error",
                        file=rel,
                        line=lineno,
                        message=(
                            "Deep `super::super::*` traversal — three or "
                            "more module levels up indicates the module "
                            "graph is wrong.  Refactor so callers reach "
                            "siblings via `crate::*` instead."
                        ),
                        context=line.strip()[:200],
                    )
                )
            for m in RUST_USE_CRATE_RE.finditer(line):
                imported = m.group(1)
                if imported == own_use_name:
                    continue
                if imported == "wylde_shared":
                    continue
                # TX S4: the Core-plugin SDK is a shared authoring
                # surface like wylde_shared — importable everywhere.
                if imported == "wylde_plugin_api":
                    continue
                # Shared authoring surfaces / client crates — pure library
                # crates and the sanctioned ``*-client`` peers (see
                # RUST_SHARED_SURFACE_CRATES for the per-crate reason).
                if imported in RUST_SHARED_SURFACE_CRATES:
                    continue
                # TX S4: plugin crates are linked by the harness host
                # only (compile-time discovery — see
                # rust/crates/wylde-harness/src/plugins/mod.rs).
                if imported.startswith("wylde_plugin_") and crate == "wylde-harness":
                    continue
                # Per-edge deliberate-dependency carve-outs (e.g. the
                # wylde-gateway REST facade linking wylde-harness).
                if (crate, imported) in RUST_CROSS_CRATE_EDGE_EXEMPTIONS:
                    continue
                out.append(
                    Finding(
                        rule="import_paths_rust",
                        severity="error",
                        file=rel,
                        line=lineno,
                        message=(
                            f"Cross-crate import {imported!r} bypasses "
                            f"wylde-shared.  Wylde crates may only depend "
                            f"on each other via the shared crate / IPC "
                            f"contract; talk to the peer via "
                            f"`wylde_shared::ipc::call_action` instead."
                        ),
                        context=line.strip()[:200],
                    )
                )
    return out


# ── Rule 27: no_silent_error_swallow_rust ────────────────────────────


def _line_is_marker_suppressed(line: str) -> bool:
    return RUST_DISCARD_RESULT_MARKER in line


def _discard_marker_in_window(lines: List[str], idx: int) -> bool:
    """True if the discard-result-ok marker sits on line ``idx`` (0-based) or
    an immediately adjacent line.

    The window matters because ``rustfmt`` parks an overflowing trailing
    comment on the following line: ``let _ = foo(really_long_args);  // marker``
    becomes the statement on one line and the ``// marker`` on the next when
    the combined line exceeds ``max_width``. Checking the neighbours keeps a
    deliberate opt-out honoured regardless of how the formatter lays it out.
    """
    for j in (idx - 1, idx, idx + 1):
        if 0 <= j < len(lines) and RUST_DISCARD_RESULT_MARKER in lines[j]:
            return True
    return False


# A statement whose trailing `.ok();` result is BOUND — `let name = …​.ok();`
# (but not `let _ = …​`) or an assignment `lhs = …​.ok();` / `self.x = …​.ok();`.
# A bound Option is retained, not swallowed, so such lines are not flagged.
_RUST_OK_BINDING_RE = re.compile(r"^(?:let\s+(?!_\b)[A-Za-z_]|[A-Za-z_][\w.\[\]()]*\s*=(?!=))")


def _let_underscore_likely_swallows_result(expr: str) -> bool:
    """Best-effort heuristic for whether ``expr`` is Result-shaped.

    We can't statically type-check from a regex.  Heuristics:
    * ``?`` operator – propagation paths can't appear in ``let _ = ...``
      anyway, so its absence is mostly a non-signal.
    * Explicit ``Result::`` / ``Ok(...)`` / ``Err(...)`` constructors
      are obvious Result expressions.
    * Common Result-returning methods: ``.send(``, ``.write(``,
      ``.read(``, ``.spawn(``, ``.lock(``, ``.try_lock(``,
      ``.try_send(``, ``.try_recv(``, ``.remove_file(``,
      ``.rename(``, ``.create(``, ``atomic_write(``,
      ``register_*(``, ``unregister_*(``, ``stop(``.
    * ``tokio::spawn(`` returns a JoinHandle — not a Result on its own
      but the inner future often is — exclude for now to avoid noise.

    A small allowlist of *known-not-Result* expressions returns False
    so we keep precision; everything else returns True.  False
    positives are diagnosable via the marker comment.
    """
    e = expr.strip()
    if not e:
        return False
    # A trailing `?` PROPAGATES the error (only the Ok value is dropped by the
    # `let _ =`), so `let _ = foo()?;` is not a swallow — the error is handled.
    if e.rstrip().endswith("?"):
        return False
    # Trivially-not-Result lhs's that are common idioms.
    no_result_substrings = (
        "tokio::spawn",
        "std::mem::take",
        "Arc::clone",
        "Arc::new",
        "Box::new",
        "Vec::new",
        "HashMap::new",
        "String::new",
        "OnceLock::new",
        "Mutex::new",
        "RwLock::new",
        "Default::default",
        "PhantomData",
    )
    if any(s in e for s in no_result_substrings):
        return False
    # Functions/methods that very commonly return Result.  Conservative —
    # add to this list as new patterns appear.
    result_substrings = (
        "?",
        "::send",
        ".send(",
        ".write(",
        ".write_all(",
        ".read(",
        ".read_to_string(",
        ".spawn(",
        ".kill(",
        ".wait(",
        ".lock(",
        ".try_lock(",
        ".try_send(",
        ".try_recv(",
        ".remove_file(",
        ".rename(",
        ".create(",
        ".create_dir_all(",
        "atomic_write(",
        "register_action",
        "unregister_action",
        "dispatch_action",
        ".stop(",
        ".init(",
        ".try_init(",
        ".bind(",
        ".connect(",
        ".accept(",
        ".flush(",
        ".sync_all(",
        "serde_json::to_string",
        "serde_json::from_str",
        "serde_json::to_vec",
        "fs::write",
        "fs::read",
        "fs::remove_file",
        "fs::rename",
    )
    return any(s in e for s in result_substrings)


def check_no_silent_error_swallow_rust() -> List[Finding]:
    """Flag ``let _ = <result>;`` and trailing ``.ok();`` patterns that
    drop a Result without logging.  An inline marker
    ``// wylde-check: discard-result-ok`` on the same line suppresses
    the rule when a deliberate discard is appropriate (e.g. shutdown
    paths where logging the error would itself fail).
    """
    out: List[Finding] = []
    for path in _walk_rust_sources():
        rel = _to_rel(path)
        text = _read_text(path)
        if not text:
            continue
        lines = text.splitlines()
        for lineno, line in enumerate(lines, start=1):
            if _discard_marker_in_window(lines, lineno - 1):
                continue
            stripped = line.lstrip()
            if _is_doc_or_comment(stripped):
                continue
            m = RUST_LET_UNDERSCORE_RE.match(line)
            if m and _let_underscore_likely_swallows_result(m.group("expr")):
                out.append(
                    Finding(
                        rule="no_silent_error_swallow_rust",
                        severity="error",
                        file=rel,
                        line=lineno,
                        message=(
                            "`let _ = <result>;` silently drops a Result.  "
                            "Either propagate with `?`, log the error, or "
                            "add `// wylde-check: discard-result-ok` if the "
                            "discard is deliberate (e.g. a shutdown path)."
                        ),
                        context=line.strip()[:200],
                    )
                )
            # `.ok();` at the very end of a statement converts Result
            # to () via discarding the error — flag it.  Chained forms
            # like `.ok().map(...)` or `.ok().unwrap_or_default()` are
            # using Result as an Option pipeline, not dropping it.
            #
            # A BOUND result (`let prev = …​.ok();`, `self.x = …​.ok();`) keeps
            # the Option and is the idiomatic Result→Option conversion — the
            # value is retained (e.g. saved to restore later), not dropped —
            # so only a bare expression statement (`foo().ok();`) is a swallow.
            if (
                line.rstrip().endswith(".ok();")
                and ".ok().map" not in line
                and not _RUST_OK_BINDING_RE.match(line.lstrip())
            ):
                out.append(
                    Finding(
                        rule="no_silent_error_swallow_rust",
                        severity="error",
                        file=rel,
                        line=lineno,
                        message=(
                            "Trailing `.ok();` drops the Result without "
                            "logging the error.  Either propagate, log, or "
                            "add `// wylde-check: discard-result-ok`."
                        ),
                        context=line.strip()[:200],
                    )
                )
    return out


# ── Rule 28: logging_setup_only_rust ─────────────────────────────────


def check_logging_setup_only_rust() -> List[Finding]:
    """Only ``wylde_shared::logging::configure_logging`` may build /
    initialise the tracing subscriber.  Every other crate calls
    ``configure_logging`` and inherits the canonical format.
    """
    out: List[Finding] = []
    canonical = "rust/crates/wylde-shared/src/logging.rs"
    for path in _walk_rust_sources():
        rel = _to_rel(path)
        if rel == canonical:
            continue
        text = _read_text(path)
        if not text:
            continue
        for lineno, line in enumerate(text.splitlines(), start=1):
            stripped = line.lstrip()
            if _is_doc_or_comment(stripped):
                continue
            if RUST_LOGGING_INIT_OK_MARKER in line:
                continue
            for pat in RUST_LOGGING_INIT_PATTERNS:
                if pat.search(line):
                    out.append(
                        Finding(
                            rule="logging_setup_only_rust",
                            severity="error",
                            file=rel,
                            line=lineno,
                            message=(
                                "Direct tracing-subscriber initialisation "
                                "detected.  Call "
                                "`wylde_shared::logging::configure_logging` "
                                "instead so every Rust service emits the "
                                "canonical Wylde log format."
                            ),
                            context=line.strip()[:200],
                        )
                    )
                    break
    return out


# ── Rule 29: no_external_process_spawn_rust ──────────────────────────


def check_no_external_process_spawn_rust() -> List[Finding]:
    """``Command::new`` / ``tokio::process::Command::new`` may only be
    called from crates listed in
    :data:`RUST_PROCESS_SPAWN_ALLOWED_CRATES`. Today that's
    ``wylde-lifecycle`` (service supervisor) and
    ``wylde-extension-bridge`` (MCP-server host). Adding to this list
    needs a documented architectural reason — supervising other
    processes is generally the lifecycle daemon's job.
    """
    out: List[Finding] = []
    for path in _walk_rust_sources():
        rel = _to_rel(path)
        crate = _crate_of(rel)
        if crate in RUST_PROCESS_SPAWN_ALLOWED_CRATES:
            continue
        text = _read_text(path)
        if not text:
            continue
        for lineno, line in enumerate(text.splitlines(), start=1):
            stripped = line.lstrip()
            if _is_doc_or_comment(stripped):
                continue
            if RUST_PROCESS_SPAWN_OK_MARKER in line:
                continue
            for pat in RUST_PROCESS_SPAWN_PATTERNS:
                if pat.search(line):
                    out.append(
                        Finding(
                            rule="no_external_process_spawn_rust",
                            severity="error",
                            file=rel,
                            line=lineno,
                            message=(
                                f"External process spawn is restricted to "
                                f"{list(RUST_PROCESS_SPAWN_ALLOWED_CRATES)}.  "
                                f"This file is in `{crate}`; route the "
                                f"request through a lifecycle pipe action "
                                f"or add `{crate}` to "
                                f"RUST_PROCESS_SPAWN_ALLOWED_CRATES with a "
                                f"documented architectural reason."
                            ),
                            context=line.strip()[:200],
                        )
                    )
                    break
    return out


# ── Rule 54: no_unbounded_log_sink_rust ──────────────────────────────


def check_no_unbounded_log_sink_rust() -> List[Finding]:
    """Every persistent file log must inherit the shared rotation policy.

    The canonical logging module owns the ONE sanctioned append-only
    open (behind ``RotatingLog`` / ``open_rotating_append``) and is
    skipped.  A raw ``OpenOptions::…append(true)`` anywhere else is the
    tell-tale of an ad-hoc, uncapped log sink that bypasses rotation —
    exactly the ``ipc.jsonl`` failure mode (unbounded append → disk
    fills silently).  Route persistent logs through
    ``wylde_shared::logging::rotating_sink`` (or ``open_rotating_append``
    for a subprocess redirect) so they are bounded by construction.

    A same-line ``// wylde-check: unbounded-append-ok`` marker suppresses
    the rule when an append genuinely is not a growing log and a bound is
    inappropriate — the exception must be justified in-line, not the norm.
    """
    out: List[Finding] = []
    for path in _walk_rust_sources():
        rel = _to_rel(path)
        if rel == RUST_LOG_ROTATION_FACTORY_FILE:
            continue
        text = _read_text(path)
        if not text:
            continue
        for lineno, line in enumerate(text.splitlines(), start=1):
            if RUST_UNBOUNDED_APPEND_MARKER in line:
                continue
            stripped = line.lstrip()
            if _is_doc_or_comment(stripped):
                continue
            for pat in RUST_UNBOUNDED_APPEND_PATTERNS:
                if pat.search(line):
                    out.append(
                        Finding(
                            rule="no_unbounded_log_sink_rust",
                            severity="error",
                            file=rel,
                            line=lineno,
                            message=(
                                "Raw append-only file open bypasses the "
                                "shared log-rotation policy. Route persistent "
                                "logs through "
                                "`wylde_shared::logging::rotating_sink` (or "
                                "`open_rotating_append` for a subprocess "
                                "stdout/stderr redirect) so they inherit the "
                                "size + retention cap by construction. If this "
                                "append is genuinely not a growing log, add "
                                "`// wylde-check: unbounded-append-ok` on the "
                                "same line with a reason."
                            ),
                            context=line.strip()[:200],
                        )
                    )
                    break
    return out
