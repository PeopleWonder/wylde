"""Gateway-to-harness contract rule (rule 48).

The companion to rule 38 (``panel_verbs_exist_in_harness_registry``).
Rule 38 covers the **GUI → harness** edge — every panel ``pipe::call`` /
``stream_call`` verb must exist in the harness pipe registry.  But the
Gateway is a *second* client of that same pipe: every
``/api/...`` REST handler under
``rust/crates/wylde-gateway/src/`` shells out to a
``memory.<layer>.<verb>`` / ``chat.*`` / ``tools.*`` pipe action via
``harness_dispatch("verb", payload)`` (the canonical helper in
``routes/common.rs``) or a raw
``proxy_core::pipe_action("wylde-harness", "verb", payload)``.

Nothing checked that **Gateway → harness** edge.  Rule 38's docstring
even says so explicitly: "Calls to services without a discoverable
registry (``wylde-vpn``, ``wylde-gateway``'s REST surface) are
intentionally skipped" — that line is about the *inbound* side (panels
calling the Gateway's axum routes, which rule 41 owns).  The *outbound*
side (Gateway route handlers dispatching pipe verbs into the harness)
had no rule at all.  A typo'd or unported verb there is a latent
runtime ``no_action`` that only fires when a live HTTP caller exercises
that exact route — exactly the class of bug the panel-side rule 38
catches at edit time.

* :func:`check_gateway_verbs_exist_in_harness_registry` — every
  statically-resolvable harness-pipe verb dispatched from the Gateway
  crate must appear in the harness pipe registry, defined (identically
  to rule 38) as the **union of** the Rust ``ALL_PIPE_ACTIONS`` array
  (``rust/crates/wylde-harness/src/pipe.rs``) **and** the Python
  ``_ACTIONS`` dict (``Core/harness/pipe/__init__.py``).  The union is
  what an over-the-wire dispatch actually reaches: the Rust harness
  serves the ported verbs and surfaces the rest as ``no_action`` for
  the Python strangler-fig's in-process fallback, so a verb registered
  on *either* side is reachable.

Two dispatch shapes are recognised:

  * ``harness_dispatch("verb.name", payload)`` — the service is
    implicitly ``wylde-harness`` (the helper hard-codes it); the verb
    is the first string-literal argument.
  * ``pipe_action(SVC, "verb.name", payload)`` — only checked when
    ``SVC`` resolves (literal or file-local ``const``) to
    ``"wylde-harness"``; the verb is the second string-literal
    argument.

Dispatches whose verb isn't a static string literal (built from a
parameter, e.g. the MCP adapter's pass-through) are skipped — the rule
trades narrow scope for a zero false-positive rate, the same trade
rules 38/41 make.  A deliberate optimistic probe for an *optional*
harness verb (one the handler tolerates ``no_action`` from, falling
back to another path) opts out with an inline
``// wylde-check: optional-verb`` marker on the call line or the line
directly above.

Like the rest of the suite the rule walks the active tree read-only and
emits ``Finding`` objects without mutating state.
"""

from __future__ import annotations

import re
import sys as _sys
from pathlib import Path
from typing import List, Optional

from .. import Finding
from .._walkers import _is_excluded, _read_text, _to_rel
from ._gpui_contract import (
    _find_matching_close,
    _line_no_at,
    _load_harness_action_registry,
    _parse_service_constants,
    _resolve_service_token,
    _split_top_args,
    _string_literal_value,
)

_pkg = _sys.modules[__name__.rsplit(".", 2)[0]]


# ── Layout constants ─────────────────────────────────────────────────


# The Gateway crate whose REST handlers dispatch harness pipe verbs.
GATEWAY_SRC_ROOT: str = "rust/crates/wylde-gateway/src"

# The harness pipe service every dispatch in scope targets.
HARNESS_SERVICE: str = "wylde-harness"

# Inline opt-out for a deliberate optimistic probe of an optional verb
# (the handler tolerates `no_action` and falls back to another path).
_OPT_OUT_MARKER: str = "wylde-check: optional-verb"


# ── Dispatch-call extraction ─────────────────────────────────────────


# ``harness_dispatch(`` — the routes/common.rs helper that hard-codes
# the wylde-harness service.  The leading negative-lookbehind keeps the
# match from firing inside a longer identifier (there is none today, but
# it also means the `fn harness_dispatch(...)` *definition* is matched
# and then harmlessly dropped — its first arg `action: &str` isn't a
# string literal, so it resolves to None and is skipped).
_HARNESS_DISPATCH_RE = re.compile(r"(?<![A-Za-z0-9_])harness_dispatch\s*\(")

# ``pipe_action(`` — the lower-level proxy_core entry point.  Matches
# both the bare-imported and `crate::proxy_core::pipe_action` forms;
# the fn definition / `use` import don't carry a `(` immediately or
# resolve to a literal service, so they fall out naturally.
_PIPE_ACTION_RE = re.compile(r"(?<![A-Za-z0-9_])pipe_action\s*\(")


class _Dispatch:
    """One harness-pipe dispatch parsed out of a Gateway source file."""

    __slots__ = ("lineno", "verb", "call_start")

    def __init__(self, lineno: int, verb: str, call_start: int) -> None:
        self.lineno = lineno
        self.verb = verb
        self.call_start = call_start


def _scan_harness_dispatches(text: str) -> List[_Dispatch]:
    """Yield every statically-resolvable harness-pipe dispatch in ``text``.

    Covers ``harness_dispatch("verb", ...)`` (service implicit) and
    ``pipe_action(SVC, "verb", ...)`` where ``SVC`` resolves to
    ``"wylde-harness"``.  Dispatches whose verb isn't a literal string
    are dropped (returned list omits them) so the caller never has to
    reason about ``None`` verbs.
    """
    constants = _parse_service_constants(text)
    out: List[_Dispatch] = []

    # ── harness_dispatch("verb", payload) ────────────────────────────
    for m in _HARNESS_DISPATCH_RE.finditer(text):
        open_idx = m.end() - 1  # the '(' itself
        close_idx = _find_matching_close(text, open_idx)
        if close_idx is None:
            continue
        args = _split_top_args(text[open_idx + 1 : close_idx])
        if not args:
            continue
        verb = _string_literal_value(args[0])
        if verb is None:
            continue
        out.append(_Dispatch(_line_no_at(text, m.start()), verb, m.start()))

    # ── pipe_action(SVC, "verb", payload) ────────────────────────────
    for m in _PIPE_ACTION_RE.finditer(text):
        open_idx = m.end() - 1
        close_idx = _find_matching_close(text, open_idx)
        if close_idx is None:
            continue
        args = _split_top_args(text[open_idx + 1 : close_idx])
        if len(args) < 2:
            continue
        service = _resolve_service_token(args[0], constants)
        if service != HARNESS_SERVICE:
            # Literal-but-other-service, or an unresolvable token
            # (`service: &str` from the fn signature) — out of scope.
            continue
        verb = _string_literal_value(args[1])
        if verb is None:
            continue
        out.append(_Dispatch(_line_no_at(text, m.start()), verb, m.start()))

    return out


def _line_carries_opt_out(text: str, idx: int) -> bool:
    """True if the line containing ``idx`` or the line directly above
    carries the ``optional-verb`` opt-out marker (mirrors rule 43's
    same-line / line-above convention)."""
    line_start = text.rfind("\n", 0, idx) + 1
    line_end = text.find("\n", idx)
    if line_end == -1:
        line_end = len(text)
    if _OPT_OUT_MARKER in text[line_start:line_end]:
        return True
    prev_end = line_start - 1
    if prev_end < 0:
        return False
    prev_start = text.rfind("\n", 0, prev_end) + 1
    return _OPT_OUT_MARKER in text[prev_start:prev_end]


# ── Walk helper ──────────────────────────────────────────────────────


def _walk_gateway_rs_files() -> List[Path]:
    """Every ``.rs`` file under the Gateway crate's source tree."""
    base = _pkg.WYLDE_ROOT / GATEWAY_SRC_ROOT
    if not base.exists():
        return []
    out: List[Path] = []
    for path in base.rglob("*.rs"):
        if _is_excluded(path):
            continue
        out.append(path)
    return out


# ── Rule 48: gateway_verbs_exist_in_harness_registry ─────────────────


def check_gateway_verbs_exist_in_harness_registry() -> List[Finding]:
    """Every harness-pipe verb the Gateway dispatches must be registered
    on the harness pipe (Rust ``ALL_PIPE_ACTIONS`` ∪ Python ``_ACTIONS``).

    Walks ``rust/crates/wylde-gateway/src/**/*.rs`` for
    ``harness_dispatch("verb", ...)`` and
    ``pipe_action("wylde-harness", "verb", ...)`` callsites.  An
    unregistered verb is a latent runtime ``no_action`` on that REST
    route — caught here at edit time instead.  Dynamic-verb dispatches
    are skipped; a deliberate optional-verb probe opts out with the
    ``// wylde-check: optional-verb`` marker.
    """
    out: List[Finding] = []
    registry = _load_harness_action_registry()
    # If the registry couldn't be loaded at all (harness crate / Python
    # pipe not checked in), skip the rule rather than flag every verb.
    if not registry:
        return out
    for rs_path in _walk_gateway_rs_files():
        rel = _to_rel(rs_path)
        text = _read_text(rs_path)
        if not text:
            continue
        for disp in _scan_harness_dispatches(text):
            if disp.verb in registry:
                continue
            if _line_carries_opt_out(text, disp.call_start):
                continue
            out.append(
                Finding(
                    rule="gateway_verbs_exist_in_harness_registry",
                    severity="error",
                    file=rel,
                    line=disp.lineno,
                    message=(
                        f"Gateway dispatches `{HARNESS_SERVICE}.{disp.verb}` "
                        f"but no such verb is registered on the harness pipe "
                        f"(rust/crates/wylde-harness/src/pipe.rs::"
                        f"ALL_PIPE_ACTIONS or "
                        f"Core/harness/pipe/__init__.py::_ACTIONS).  Either "
                        f"add the verb to the harness, fix the typo, or — if "
                        f"the route deliberately probes an optional verb and "
                        f"tolerates `no_action` — annotate the call with "
                        f"`// {_OPT_OUT_MARKER}`.  Runtime fails with "
                        f"`no_action` on that REST path."
                    ),
                    context=f"dispatch(\"{disp.verb}\")",
                )
            )
    return out
