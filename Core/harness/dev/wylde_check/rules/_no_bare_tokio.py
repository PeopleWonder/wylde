"""No-bare-tokio-in-panel-src rule (rule 50).

A bare ``tokio::time::sleep(...)`` in the consent-reconnect backoff loop of
``Core/GUI/Frontend/Panels/Chat/src/chat_panel.rs`` panicked at startup:

    thread 'main' panicked at chat_panel.rs:544:17 — there is no reactor
    running, must be called from the context of a Tokio 1.x runtime

The gpui GUI runs on its own executor, not a tokio runtime, so any bare
tokio primitive reached from panel code blows up the moment it executes.
Unit tests didn't catch it because the test harness *does* run inside a
tokio runtime (``#[tokio::test]``); production gpui doesn't.

First-party panels must drive async work through the gpui executor —
``cx.spawn``, ``cx.background_executor().spawn``,
``cx.foreground_executor().spawn``, ``cx.background_executor().timer()`` —
never bare tokio spawners / timers / runtime constructors.

``check_no_bare_tokio_in_panel_src`` scans
``Core/GUI/Frontend/Panels/*/src/**/*.rs`` and flags bare-tokio call
sites:

* ``tokio::spawn(``
* ``tokio::time::sleep(`` / ``sleep_until(`` / ``interval(`` / ``timeout(``
* ``tokio::task::spawn(`` / ``spawn_blocking(`` / ``spawn_local(``
* direct runtime construction (``Runtime::new(`` /
  ``Builder::new_*().build(``)

It does **not** flag (all conservative — when guard status can't be proven
statically the call is still flagged):

* call sites inside a ``#[cfg(test)]`` module or under a ``#[tokio::test]``
  fn (test code legitimately has a runtime),
* call sites inside a ``#[tokio::main]`` fn (no panel should carry one,
  but a runtime is present if it does),
* call sites provably inside an ``if Handle::try_current().is_ok() { … }``
  guard,
* ``use`` import lines (only call sites matter),
* matches inside ``//`` / ``///`` / ``/* … */`` comments,
* a line carrying the explicit opt-out marker
  ``// wylde-check: tokio-runtime-provided`` (same line or the line above).

Like the rest of the suite the rule walks the active tree read-only and
emits ``Finding`` objects without mutating state.
"""

from __future__ import annotations

import re
from typing import List

from .. import Finding
from .._walkers import _read_text, _to_rel, _walk

# ── Layout constants ─────────────────────────────────────────────────

# First-party panel sources: Core/GUI/Frontend/Panels/<Name>/src/**/*.rs
_PANEL_SRC_RE = re.compile(r"^Core/GUI/Frontend/Panels/[^/]+/src/.+\.rs$")

# The executor steer, named in the finding so the fix is obvious.
_FIX_HINT: str = (
    "use the gpui executor instead (cx.spawn / "
    "cx.background_executor().spawn / cx.foreground_executor().spawn / "
    "cx.background_executor().timer()) — gpui has no tokio reactor, so a "
    "bare tokio primitive panics at runtime ('no reactor running')"
)

# Explicit per-line opt-out (same line or the line directly above a call).
_OPT_OUT: str = "wylde-check: tokio-runtime-provided"

# ── Bare-tokio call-site matchers ────────────────────────────────────
# Each is matched against the *code* portion of a line (comments stripped,
# import lines already excluded). All require an opening paren so a bare
# path reference (rare) isn't flagged.

_CALL_RES = [
    re.compile(r"\btokio::spawn\s*\("),
    re.compile(r"\btokio::time::sleep\s*\("),
    re.compile(r"\btokio::time::sleep_until\s*\("),
    re.compile(r"\btokio::time::interval\s*\("),
    re.compile(r"\btokio::time::timeout\s*\("),
    re.compile(r"\btokio::task::spawn\s*\("),
    re.compile(r"\btokio::task::spawn_blocking\s*\("),
    re.compile(r"\btokio::task::spawn_local\s*\("),
    re.compile(r"\bRuntime::new\s*\("),
]

# Builder::new_current_thread()…build() / new_multi_thread()…build() — the
# two halves can land on one line or be chained; flag when both the
# builder constructor and a .build( land on the same logical line.
_BUILDER_CTOR_RE = re.compile(r"\bBuilder::new_\w+\s*\(")
_BUILD_CALL_RE = re.compile(r"\.build\s*\(")

# Attribute / guard markers used for region tracking.
_CFG_TEST_RE = re.compile(r"#\[\s*cfg\s*\(\s*test\s*\)\s*\]")
_TOKIO_TEST_RE = re.compile(r"#\[\s*tokio::test")
_TOKIO_MAIN_RE = re.compile(r"#\[\s*tokio::main")
_HANDLE_GUARD_RE = re.compile(r"Handle::try_current\s*\(\s*\)\s*\.\s*is_ok\s*\(")


def _strip_comments(line: str, in_block: bool) -> tuple[str, bool]:
    """Return (code-only text, new in_block state).

    Removes ``/* … */`` block comments (tracking multi-line state) and the
    trailing ``//`` line comment. String-literal awareness is intentionally
    skipped — the tokio call patterns never legitimately appear inside a
    string in panel code, and being slightly eager only ever *adds* safety.
    """
    out = []
    i = 0
    n = len(line)
    while i < n:
        if in_block:
            end = line.find("*/", i)
            if end == -1:
                return "".join(out), True
            i = end + 2
            in_block = False
            continue
        if line.startswith("//", i):
            break  # rest of line is a comment
        if line.startswith("/*", i):
            in_block = True
            i += 2
            continue
        out.append(line[i])
        i += 1
    return "".join(out), in_block


def check_no_bare_tokio_in_panel_src() -> List[Finding]:
    """Flag bare tokio primitives in first-party gpui panel sources.

    Walks ``Core/GUI/Frontend/Panels/*/src/**/*.rs`` and reports any bare
    ``tokio::spawn`` / ``tokio::time::*`` / ``tokio::task::spawn*`` call,
    or a direct ``Runtime::new`` / ``Builder::new_*().build`` construction,
    that isn't inside test code, a ``#[tokio::main]`` fn, or a proven
    ``Handle::try_current().is_ok()`` guard. gpui runs on its own executor,
    so each is a latent "no reactor running" panic; the fix is to drive the
    work through the gpui executor.
    """
    out: List[Finding] = []
    for path in _walk((".rs",)):
        rel = _to_rel(path)
        if not _PANEL_SRC_RE.match(rel):
            continue
        text = _read_text(path)
        if not text:
            continue

        lines = text.splitlines()
        in_block_comment = False
        # Stack of brace-depths at which an allow-region (cfg(test) /
        # tokio::test / tokio::main / Handle guard) was opened. While the
        # current depth is > any recorded start depth we're inside it.
        allow_starts: List[int] = []
        depth = 0
        pending_allow = False  # an allow-attribute/guard awaiting its block

        for lineno, raw in enumerate(lines, start=1):
            code, in_block_comment = _strip_comments(raw, in_block_comment)

            # Region attributes / guards are recognised on the comment-
            # stripped text so a commented-out attribute doesn't arm a region.
            if (
                _CFG_TEST_RE.search(code)
                or _TOKIO_TEST_RE.search(code)
                or _TOKIO_MAIN_RE.search(code)
                or _HANDLE_GUARD_RE.search(code)
            ):
                pending_allow = True

            open_braces = code.count("{")
            close_braces = code.count("}")

            # An armed allow-region attaches to the next block that opens.
            if pending_allow and open_braces > 0:
                allow_starts.append(depth)
                pending_allow = False

            inside_allowed = len(allow_starts) > 0

            if not inside_allowed and code.strip():
                stripped = code.lstrip()
                is_use_line = stripped.startswith("use ")
                if not is_use_line:
                    opted_out = (
                        _OPT_OUT in raw
                        or (lineno >= 2 and _OPT_OUT in lines[lineno - 2])
                    )
                    if not opted_out:
                        matched = any(rx.search(code) for rx in _CALL_RES) or (
                            _BUILDER_CTOR_RE.search(code)
                            and _BUILD_CALL_RE.search(code)
                        )
                        if matched:
                            out.append(
                                Finding(
                                    rule="no_bare_tokio_in_panel_src",
                                    severity="error",
                                    file=rel,
                                    line=lineno,
                                    message=(
                                        "Bare tokio primitive in gpui panel "
                                        f"source — {_FIX_HINT}. If this code is "
                                        "genuinely reached inside a tokio "
                                        "runtime, annotate the line with "
                                        f"`// {_OPT_OUT}`."
                                    ),
                                    context=code.strip()[:200],
                                )
                            )

            # Update brace depth and close any allow-regions we've exited.
            depth += open_braces - close_braces
            while allow_starts and depth <= allow_starts[-1]:
                allow_starts.pop()

    return out
