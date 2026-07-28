"""No axum types in a non-gateway crate's public API (rule 63).

The enforcement companion to #290's axum containment. axum is an HTTP
*framework*; it is deliberately housed in ``wylde-gateway``. Two other crates
run their own standalone axum servers (``wylde-treesitter``'s N8N front door,
``wylde-vpn``'s control plane), but axum is confined to each crate's ``http``
module and — after #293 — no axum type appears in either crate's *public* API
(``router()`` was tightened from ``pub`` to ``pub(crate)``; the only public
entrypoint, ``serve(port) -> anyhow::Result<()>``, exposes none).

Nothing enforced that. This rule does: it flags a fully-``pub`` function or type
alias, in any crate **other than ``wylde-gateway``**, whose signature names an
axum type (``axum::…``, ``Router``, or ``IntoResponse``). A breaking axum bump
can then only ever touch the ``http`` modules that already own it — it can never
start bleeding across a crate's public boundary into a shared API, which is the
failure mode that turns a framework bump from a contained edit into a sweep.

``pub(crate)`` / ``pub(super)`` / private items are fine — those don't cross the
crate boundary. ``wylde-gateway`` is exempt (axum lives there by design). Test
modules are skipped. Like the rest of the suite it walks read-only and emits
``Finding`` objects.
"""

from __future__ import annotations

import re
from typing import List

from .. import Finding
from .._config import RUST_CRATES_ROOT
from .._walkers import _is_test_path, _read_text, _to_rel, _walk

# Crate that is allowed to expose axum (the HTTP framework lives here).
_EXEMPT_CRATE = "wylde-gateway"

# axum type tokens specific enough not to collide with unrelated types:
# a fully-qualified ``axum::…`` path, or the two bare names that are
# unambiguously axum in this tree (``Router``, ``IntoResponse``).
_AXUM_TOKEN_RE = re.compile(r"axum::|\bRouter\b|\bIntoResponse\b")

# A fully-`pub` item (NOT `pub(crate)` / `pub(super)` / `pub(in …)`), of a kind
# whose signature can carry a type: a fn, or a type alias. `pub(` is excluded by
# the negative lookahead.
_PUB_FN_RE = re.compile(r"^\s*pub\s+(?!\()(?:async\s+)?fn\b")
_PUB_TYPE_RE = re.compile(r"^\s*pub\s+(?!\()type\b")
_SIG_END_RE = re.compile(r"[{;]")


def _crate_of(rel: str) -> str:
    """`rust/crates/<crate>/src/…` → `<crate>` (or "" if not under crates)."""
    parts = rel.split("/")
    return parts[2] if len(parts) > 2 and parts[1] == "crates" else ""


def check_no_axum_types_in_public_api() -> List[Finding]:
    """Flag fully-public fn / type signatures naming an axum type outside the
    gateway.

    Accumulates each ``pub fn`` signature across lines up to its ``{`` or ``;``
    (return types often trail the parameter list) before matching, so a
    multi-line signature isn't missed. Findings are ``warning`` but still fail
    the gate; the fix is to tighten the item to ``pub(crate)`` (or keep the axum
    type out of the signature entirely).
    """
    out: List[Finding] = []
    for path in _walk((".rs",), roots=(RUST_CRATES_ROOT,)):
        rel = _to_rel(path)
        if _crate_of(rel) == _EXEMPT_CRATE:
            continue
        if _is_test_path(rel):
            continue
        text = _read_text(path)
        if "axum" not in text:  # crate/file doesn't touch axum → nothing to leak
            continue

        lines = text.splitlines()
        in_sig = False
        sig_start = 0
        sig_buf = ""
        for idx, raw in enumerate(lines, start=1):
            code = raw.split("//", 1)[0]  # drop trailing line comment (rough but safe)
            if in_sig:
                sig_buf += " " + code
                if _SIG_END_RE.search(code):
                    in_sig = False
                    if _AXUM_TOKEN_RE.search(sig_buf):
                        out.append(_leak(rel, sig_start, sig_buf))
                continue

            if _PUB_FN_RE.search(code):
                if _SIG_END_RE.search(code):
                    if _AXUM_TOKEN_RE.search(code):
                        out.append(_leak(rel, idx, code))
                else:
                    in_sig, sig_start, sig_buf = True, idx, code
            elif _PUB_TYPE_RE.search(code) and _AXUM_TOKEN_RE.search(code):
                out.append(_leak(rel, idx, code))

    return out


def _leak(rel: str, line: int, sig: str) -> Finding:
    return Finding(
        rule="no_axum_types_in_public_api",
        severity="warning",
        file=rel,
        line=line,
        message=(
            "fully-public item names an axum type outside `wylde-gateway`. axum "
            "is an HTTP framework and must stay contained to the crate's `http` "
            "module (see #290). Tighten this to `pub(crate)` (or keep the axum "
            "type out of the public signature) so a breaking axum bump can't "
            "spread across the crate boundary."
        ),
        context=sig.strip()[:200],
    )
