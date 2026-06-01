"""No-silent-skip-in-service-start rule (rule 52).

Every per-service ``start_<service>()`` in the lifecycle daemon
(``rust/crates/wylde-lifecycle/src/state/services.rs``) historically opened
with a bare guard::

    if is_service_alive(service_name::HARNESS) {
        return Ok(());   // ← no log; caller sees nothing
    }

When a service was killed without a graceful shutdown (Ctrl-C, taskkill,
SIGKILL) its manifest stayed on disk marking it "alive" with a now-dead
pid. The boot loop trusted that, returned ``Ok`` *silently*, and skipped
the spawn — so on 2026-05-31 the harness, extension_bridge, ollama,
trainer_worker and trainer all stayed dark with nothing in the daemon log
explaining why. the Wylde user only recovered by hand-wiping the manifest dir.

The fix is defence-in-depth: a synchronous boot orphan-sweep deletes
dead-pid manifests before the spawns (so the stale state self-heals), and
*every* early-return that skips a spawn must log ONE line saying why. This
rule pins the second half: if ``start_X`` returns ``Ok`` but didn't spawn
anything, the daemon's log must say WHY.

``check_silent_skip_in_service_start`` scans
``rust/crates/wylde-lifecycle/src/state/services.rs`` and, for every
function whose name matches ``start_[a-z_]+``, flags any ``return Ok(())``
/ ``return Ok(<expr>)`` whose enclosing brace-block contains no preceding
``tracing::`` call.

It does **not** flag:

* the successful-spawn tail of a function — that path ends with a bare
  ``Ok(())`` *expression* (after ``record_spawn(...)`` / a ``match`` arm),
  never an early ``return Ok(...)`` — so a tail ``Ok(())`` is never matched,
* ``return Ok`` in any function *not* named ``start_*`` (e.g. the shared
  ``stop_service`` helper),
* ``return Ok`` whose own block already carries a ``tracing::`` call before
  it (the normal "log the reason then return" shape),
* matches inside ``//`` / ``///`` / ``/* … */`` comments or string / raw-
  string literals (so a ``"return Ok"`` in a doc-comment can't false-fire,
  and a ``"{}"`` format string can't unbalance the brace tracking),
* a line carrying the explicit opt-out marker
  ``// wylde-check: silent-skip-allowed`` (same line or the line directly
  above) — meant to be **rare**.

Like the rest of the suite the rule walks the active tree read-only and
emits ``Finding`` objects without mutating state.
"""

from __future__ import annotations

import re
from typing import List

from .. import Finding
from .._walkers import _read_text, _to_rel, _walk

# ── Layout constants ─────────────────────────────────────────────────

# The one file this rule governs: the lifecycle daemon's per-service
# start/stop table.
_SERVICES_RE = re.compile(
    r"rust/crates/wylde-lifecycle/src/state/services\.rs$"
)

# `fn <name>(` / `fn <name><` (generic) — captures the function name so we
# can tell a `start_<service>` body from everything else in the file.
_FN_RE = re.compile(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)\s*[(<]")
_START_FN_RE = re.compile(r"^start_[a-z_]+$")

# Early-return that yields Ok — the tail `Ok(())` *expression* has no
# `return` keyword and is intentionally never matched.
_RETURN_OK_RE = re.compile(r"\breturn\s+Ok\s*\(")

# Any tracing call counts as "logged the reason".
_TRACING_RE = re.compile(r"\btracing::")

# Explicit per-line opt-out (same line or the line directly above).
_OPT_OUT: str = "wylde-check: silent-skip-allowed"

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
    (``r"…"`` / ``r#"…"#``). Stripping strings matters here because the
    ``tracing::`` format strings contain ``{`` / ``}`` (e.g.
    ``"manifest pid={}"``) which would otherwise unbalance the brace-depth
    tracking the block check relies on.
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


def check_silent_skip_in_service_start() -> List[Finding]:
    """Flag silent early-returns in lifecycle ``start_<service>`` functions.

    Walks ``rust/crates/wylde-lifecycle/src/state/services.rs`` and reports
    any ``return Ok(...)`` inside a ``start_[a-z_]+`` function whose
    enclosing block carries no ``tracing::`` call before it. Such a return
    skips a spawn with nothing in the daemon log to say why — the exact gap
    that left five services dark behind stale manifests on 2026-05-31. The
    fix is one ``tracing::info!`` / ``tracing::warn!`` per skip; a genuine
    side-effect-free Ok can opt out with ``// wylde-check: silent-skip-allowed``.
    """
    out: List[Finding] = []
    for path in _walk((".rs",)):
        rel = _to_rel(path)
        if not _SERVICES_RE.search(rel):
            continue
        text = _read_text(path)
        if not text:
            continue

        lines = text.splitlines()
        st = _StripState()
        # One bool per open brace block (whole-file balanced): True once the
        # innermost block has seen a `tracing::` call. `block_seen[-1]` is
        # always the block immediately enclosing the current line.
        block_seen: List[bool] = []
        current_fn_is_start = False

        for lineno, raw in enumerate(lines, start=1):
            code = _strip_code(raw, st)

            fn_m = _FN_RE.search(code)
            if fn_m:
                current_fn_is_start = bool(_START_FN_RE.match(fn_m.group(1)))

            # A tracing call marks the current innermost block as "logged".
            if block_seen and _TRACING_RE.search(code):
                block_seen[-1] = True

            # An early `return Ok(...)` inside a start_ function whose block
            # hasn't logged is a silent skip.
            if current_fn_is_start and _RETURN_OK_RE.search(code):
                innermost_logged = block_seen[-1] if block_seen else False
                opted_out = _OPT_OUT in raw or (
                    lineno >= 2 and _OPT_OUT in lines[lineno - 2]
                )
                if not innermost_logged and not opted_out:
                    out.append(
                        Finding(
                            rule="silent_skip_in_service_start",
                            severity="error",
                            file=rel,
                            line=lineno,
                            message=(
                                "Silent `return Ok(...)` in a lifecycle "
                                "start_<service> function — this skips a spawn "
                                "with nothing in the daemon log to say why "
                                "(the stale-manifest silent-skip class that "
                                "left five services dark on 2026-05-31). Add a "
                                "`tracing::info!`/`tracing::warn!` in this "
                                "branch explaining the skip (e.g. \"already "
                                "alive (manifest pid=…); skipping spawn\"). If "
                                "this Ok genuinely has no spawn-skip to "
                                "explain, annotate with "
                                f"`// {_OPT_OUT}`."
                            ),
                            context=code.strip()[:200],
                        )
                    )

            # Update brace depth / block stack (strings + comments already
            # stripped, so only real code braces count).
            for ch in code:
                if ch == "{":
                    block_seen.append(False)
                elif ch == "}":
                    if block_seen:
                        block_seen.pop()

    return out
