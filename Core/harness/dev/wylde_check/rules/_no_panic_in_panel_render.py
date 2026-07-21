"""No-panic-primitives-in-panel-render rule (rule 51).

Two ``snapshot.gpus.first().unwrap()`` calls in the render path of
``Core/GUI/Frontend/Panels/Dashboard/src/lib.rs`` took down the whole GUI
at startup:

    thread 'main' panicked at lib.rs — called `Option::unwrap()` on a
    `None` value
    process exited with code 101

On a cold start the VRAM broker's inventory hasn't landed yet, so
``snapshot.gpus`` is empty, ``.first()`` returns ``None``, and ``.unwrap()``
panics before the window is even visible. Panels are the most user-visible
layer and the closest thing Wylde has to "live rendering": a panic in a
panel takes down the *entire* gpui shell because every panel shares the one
event loop. Unit tests miss these because the test code paths feed
populated inventories; production cold-start does not.

``check_no_panic_in_panel_render`` scans
``Core/GUI/Frontend/Panels/*/src/**/*.rs`` and flags panic primitives in
panel code:

* ``.unwrap()``
* ``.expect(`` …
* ``unwrap_or_panic!`` / ``unreachable!`` / ``unimplemented!`` / ``todo!``
* bare ``panic!(``

The fix is to use ``.unwrap_or(default)`` / ``.unwrap_or_else(|| …)`` /
``if let Some(x) = …`` / ``match`` / the ``?`` operator, or to surface a
fallback render value like ``"—"`` (the convention the Dashboard panel
already uses for an absent ``active_model``).

It does **not** flag:

* call sites inside a ``#[cfg(test)]`` / ``#[tokio::test]`` / ``#[test]``
  block (test panics are expected),
* call sites inside a ``const`` / ``static`` item (compile-time; a const
  initializer that "panics" is a build error, not a runtime crash),
* matches inside ``//`` / ``///`` / ``/* … */`` comments,
* matches inside string / raw-string literals (error messages that merely
  *mention* "unwrap"/"panic" are fine),
* a line carrying the explicit opt-out marker
  ``// wylde-check: panel-panic-allowed`` (same line or the line above) —
  **provided** a ``// SAFETY:`` or ``// INVARIANT:`` justification comment
  sits in the same small window. An opt-out *without* that justification is
  itself a violation (a different, pointed message), because the opt-out is
  meant to be rare and always explained.

Like the rest of the suite the rule walks the active tree read-only and
emits ``Finding`` objects without mutating state.
"""

from __future__ import annotations

import re
from typing import List, Tuple

from .. import Finding
from .._walkers import _read_text, _to_rel, _walk

# ── Layout constants ─────────────────────────────────────────────────

# First-party panel sources: Core/GUI/Frontend/Panels/<Name>/src/**/*.rs
_PANEL_SRC_RE = re.compile(r"^Core/GUI/Frontend/Panels/[^/]+/src/.+\.rs$")

_FIX_HINT: str = (
    "a panic in a panel takes down the whole gpui shell (panels share the "
    "event loop). Use .unwrap_or(default) / .unwrap_or_else(|| …) / "
    "if let Some(x) = … / match / the `?` operator, or surface a fallback "
    'render value like "—" (the Dashboard active_model convention)'
)

# Explicit per-line opt-out (same line or the line directly above a flag).
_OPT_OUT: str = "wylde-check: panel-panic-allowed"

# Required justification when the opt-out is used.
_JUSTIFY_RE = re.compile(r"//.*\b(SAFETY|INVARIANT)\b", re.IGNORECASE)

# ── Panic-primitive matchers ─────────────────────────────────────────
# Each is matched against the *code* portion of a line (comments AND
# string literals stripped — the latter matters here because words like
# "unwrap"/"panic" legitimately appear inside error-message strings).

_PANIC_RES: Tuple[Tuple[re.Pattern, str], ...] = (
    # `.unwrap()` only — `.unwrap_or(...)`, `.unwrap_or_else(...)`,
    # `.unwrap_or_default()` all have `_` after `unwrap` and never match.
    (re.compile(r"\.unwrap\s*\(\s*\)"), ".unwrap()"),
    (re.compile(r"\.expect\s*\("), ".expect(...)"),
    (re.compile(r"\bunwrap_or_panic!"), "unwrap_or_panic!"),
    (re.compile(r"\bunreachable!"), "unreachable!"),
    (re.compile(r"\bunimplemented!"), "unimplemented!"),
    (re.compile(r"\btodo!"), "todo!"),
    (re.compile(r"\bpanic!"), "panic!"),
)

# Attribute markers used for region tracking (test code is exempt).
_CFG_TEST_RE = re.compile(r"#\[\s*cfg\s*\(\s*test\s*\)\s*\]")
_TOKIO_TEST_RE = re.compile(r"#\[\s*tokio::test")
_TEST_RE = re.compile(r"#\[\s*test\s*\]")

# `const` / `static` item start (compile-time; can't panic at runtime).
_CONST_DECL_RE = re.compile(r"^\s*(pub\s*(\([^)]*\)\s*)?)?(const|static)\s+")

# Raw-string opener: r"…" / r#"…"# / r##"…"## …
_RAW_STR_START = re.compile(r"r(#*)\"")


class _StripState:
    """Tiny mutable carrier for cross-line comment / string state."""

    __slots__ = ("in_block", "str_closer")

    def __init__(self) -> None:
        self.in_block = False
        # ``None`` when not inside a string; otherwise the closing token we
        # are scanning for (``"`` for a regular string, ``"##…`` for a raw
        # string with N hashes).
        self.str_closer = None  # type: str | None


def _strip_code(line: str, st: _StripState) -> str:
    """Return the code-only portion of ``line``.

    Removes ``//`` line comments, ``/* … */`` block comments (multi-line),
    regular string literals (escape-aware), and raw-string literals
    (``r"…"`` / ``r#"…"#``). Multi-line strings/comments are tracked via
    ``st`` so a literal that spans lines doesn't leak its words back into
    the matchable code on the next line.
    """
    out: List[str] = []
    i = 0
    n = len(line)
    while i < n:
        if st.in_block:
            end = line.find("*/", i)
            if end == -1:
                return "".join(out)
            i = end + 2
            st.in_block = False
            continue
        if st.str_closer is not None:
            if st.str_closer == '"':
                # Regular string: scan for an unescaped closing quote.
                closed = False
                while i < n:
                    c = line[i]
                    if c == "\\":
                        i += 2
                        continue
                    if c == '"':
                        st.str_closer = None
                        i += 1
                        closed = True
                        break
                    i += 1
                if not closed:
                    return "".join(out)
                continue
            # Raw string: no escapes; scan for the literal closer.
            idx = line.find(st.str_closer, i)
            if idx == -1:
                return "".join(out)
            i = idx + len(st.str_closer)
            st.str_closer = None
            continue
        # Not currently inside a comment or string.
        if line.startswith("//", i):
            break
        if line.startswith("/*", i):
            st.in_block = True
            i += 2
            continue
        # Raw-string start — only when `r` doesn't continue an identifier.
        prev_ident = i > 0 and (line[i - 1].isalnum() or line[i - 1] == "_")
        if not prev_ident:
            m = _RAW_STR_START.match(line, i)
            if m:
                st.str_closer = '"' + "#" * len(m.group(1))
                i = m.end()
                continue
        if line[i] == '"':
            st.str_closer = '"'
            i += 1
            continue
        out.append(line[i])
        i += 1
    return "".join(out)


def check_no_panic_in_panel_render() -> List[Finding]:
    """Flag panic primitives in first-party gpui panel sources.

    Walks ``Core/GUI/Frontend/Panels/*/src/**/*.rs`` and reports any
    ``.unwrap()`` / ``.expect(...)`` / ``unwrap_or_panic!`` /
    ``unreachable!`` / ``unimplemented!`` / ``todo!`` / ``panic!(`` that
    isn't inside test code or a ``const``/``static`` item. A panic in a
    panel takes down the whole gpui shell; the fix is an Option-guarded
    fallback (the chat/dashboard ``"—"`` convention). A deliberate
    opt-out (``// wylde-check: panel-panic-allowed`` + a ``// SAFETY:`` /
    ``// INVARIANT:`` justification) suppresses a single site; an opt-out
    without the justification is itself flagged.
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
        st = _StripState()
        # Brace-depths at which a test allow-region was opened (region is
        # active while current depth > the recorded start depth).
        allow_starts: List[int] = []
        depth = 0
        pending_allow = False  # a test attribute awaiting its block
        in_const = False  # inside a const/static item (until its `;`)

        for lineno, raw in enumerate(lines, start=1):
            code = _strip_code(raw, st)

            # Test attributes recognised on the stripped text so a
            # commented-out attribute doesn't arm a region.
            if (
                _CFG_TEST_RE.search(code)
                or _TOKIO_TEST_RE.search(code)
                or _TEST_RE.search(code)
            ):
                pending_allow = True

            # Open a const/static span (closes at the statement's `;`).
            if not in_const and _CONST_DECL_RE.match(code):
                in_const = True

            open_braces = code.count("{")
            close_braces = code.count("}")

            if pending_allow and open_braces > 0:
                allow_starts.append(depth)
                pending_allow = False

            inside_allowed = bool(allow_starts) or in_const

            if not inside_allowed and code.strip():
                stripped = code.lstrip()
                is_use_line = stripped.startswith("use ")
                if not is_use_line:
                    matched = next(
                        (label for rx, label in _PANIC_RES if rx.search(code)),
                        None,
                    )
                    if matched is not None:
                        # The line BELOW is included because rustfmt parks an
                        # overflowing trailing comment there — a marked
                        # `.expect(...)` whose line exceeds max_width keeps the
                        # code and pushes `// … panel-panic-allowed` to the next
                        # line. Checking it keeps a deliberate opt-out honoured.
                        below = lines[lineno] if lineno < len(lines) else ""
                        opted_out = (
                            _OPT_OUT in raw
                            or (lineno >= 2 and _OPT_OUT in lines[lineno - 2])
                            or _OPT_OUT in below
                        )
                        if opted_out:
                            # Opt-out honoured only with a SAFETY / INVARIANT
                            # justification in the small surrounding window
                            # (flagged line, the two above, and the line below —
                            # where rustfmt may have parked the comment).
                            window = [raw, below]
                            if lineno >= 2:
                                window.append(lines[lineno - 2])
                            if lineno >= 3:
                                window.append(lines[lineno - 3])
                            justified = any(_JUSTIFY_RE.search(w) for w in window)
                            if not justified:
                                out.append(
                                    Finding(
                                        rule="no_panic_in_panel_render",
                                        severity="error",
                                        file=rel,
                                        line=lineno,
                                        message=(
                                            "Opt-out marker "
                                            f"`// {_OPT_OUT}` used without a "
                                            "required `// SAFETY:` or "
                                            "`// INVARIANT:` justification "
                                            "comment on the line(s) above. "
                                            "Either remove the panic "
                                            f"primitive ({matched}) or "
                                            "document why panicking is "
                                            "genuinely correct here."
                                        ),
                                        context=code.strip()[:200],
                                    )
                                )
                        else:
                            out.append(
                                Finding(
                                    rule="no_panic_in_panel_render",
                                    severity="error",
                                    file=rel,
                                    line=lineno,
                                    message=(
                                        f"Panic primitive ({matched}) in gpui "
                                        f"panel render path — {_FIX_HINT}. If "
                                        "panicking is genuinely correct, "
                                        f"annotate with `// {_OPT_OUT}` plus a "
                                        "`// SAFETY:` / `// INVARIANT:` "
                                        "justification."
                                    ),
                                    context=code.strip()[:200],
                                )
                            )

            # Close a const/static span at its terminating `;`.
            if in_const and ";" in code:
                in_const = False

            # Update brace depth and close exited test allow-regions.
            depth += open_braces - close_braces
            while allow_starts and depth <= allow_starts[-1]:
                allow_starts.pop()

    return out
