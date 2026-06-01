"""No-Python-Gateway-import rule (rule 49).

The Python FastAPI Gateway (``Wylde/Gateway/``) was deleted on
2026-05-30: the dead server (``app.py`` / ``run.py`` / ``routes/`` / …)
was removed outright, and the three in-process client libraries that had
kept the folder alive moved into ``Core/shared/`` —
``Gateway/client.py`` → ``Core/shared/egress_client.py``,
``Gateway/auth/`` (device-token subset) → ``Core/shared/gateway_auth/``,
``Gateway/extension_routes.py`` → ``Core/shared/extension_routes.py``.
The CIDR / auth-boundary surface now lives only in the Rust
``wylde-gateway`` crate.

There must never be a ``from Gateway`` / ``import Gateway`` (or the
``Wylde.``-prefixed variant) again — the package is gone, so any such
import is an ``ImportError`` waiting to happen and a sign that code is
reaching for the deleted server shell instead of the relocated shared
libraries. This rule catches it at edit time.

* :func:`check_no_python_gateway_imports` — every active ``.py`` file is
  scanned for an import statement that targets the top-level ``Gateway``
  package. Two shapes are matched:

    * ``from [Wylde.]Gateway[.sub] import ...``
    * ``import [Wylde.]Gateway[.sub] [as alias]``

  The patterns require real import syntax (the ``import`` keyword for the
  ``from`` form; end-of-line / ``as`` for the bare form), so prose in a
  docstring that merely starts with the word "from Gateway …" doesn't
  false-fire. ``GatewayFoo`` and similar longer identifiers are not
  matched (word boundary after ``Gateway``).

The ``wylde_check`` package and its tests are skipped — they carry the
``from Gateway`` pattern as data (this rule's own fixtures, plus the
historical references in :mod:`wylde_check` docstrings).

Like the rest of the suite the rule walks the active tree read-only and
emits ``Finding`` objects without mutating state.
"""

from __future__ import annotations

import re
from typing import List

from .. import Finding
from .._walkers import _read_text, _to_rel, _walk


# ── Layout constants ─────────────────────────────────────────────────


# The relocated homes, named in the finding so the fix is obvious.
_RELOCATION_HINT: str = (
    "the Python Gateway was deleted 2026-05-30 — its client libraries "
    "moved to Core/shared/ (egress_client, gateway_auth, extension_routes)"
)


# ── Import-statement matchers ────────────────────────────────────────


# ``from [Wylde.]Gateway[.sub.sub] import ...`` — requires the trailing
# ``import`` keyword so a docstring line beginning "from Gateway …" can't
# match.
_FROM_RE = re.compile(r"^\s*from\s+(?:Wylde\.)?Gateway(?:\.\w+)*\s+import\b")

# ``import [Wylde.]Gateway[.sub]`` optionally aliased — anchored to
# end-of-line (modulo a trailing comment) so "import Gateway routes …"
# prose doesn't match.
_IMPORT_RE = re.compile(
    r"^\s*import\s+(?:Wylde\.)?Gateway(?:\.\w+)*(?:\s+as\s+\w+)?\s*(?:#.*)?$"
)


def check_no_python_gateway_imports() -> List[Finding]:
    """Flag any import of the deleted top-level ``Gateway`` package.

    Walks every active ``.py`` file for ``from [Wylde.]Gateway … import``
    and ``import [Wylde.]Gateway`` statements. The package no longer
    exists, so each is a latent ``ImportError``; the fix is to point at
    the relocated shared library (``Core/shared/egress_client`` /
    ``Core/shared/gateway_auth`` / ``Core/shared/extension_routes``).
    The ``wylde_check`` package and its tests are skipped — they hold the
    pattern as fixture / documentation data.
    """
    out: List[Finding] = []
    for path in _walk((".py",)):
        rel = _to_rel(path)
        # The checker's own rules + tests legitimately carry the pattern
        # as data (this rule's fixtures, doc references in __init__).
        if "/wylde_check/" in rel:
            continue
        text = _read_text(path)
        if not text:
            continue
        for lineno, line in enumerate(text.splitlines(), start=1):
            stripped = line.lstrip()
            if stripped.startswith("#"):
                continue
            if _FROM_RE.match(line) or _IMPORT_RE.match(line):
                out.append(
                    Finding(
                        rule="no_python_gateway_imports",
                        severity="error",
                        file=rel,
                        line=lineno,
                        message=(
                            "Import of the deleted top-level `Gateway` "
                            f"package ({_RELOCATION_HINT}). Repoint at the "
                            "relocated shared library: `Core.shared.egress_client` "
                            "(was Gateway.client), `Core.shared.gateway_auth` "
                            "(was Gateway.auth), or `Core.shared.extension_routes` "
                            "(was Gateway.extension_routes)."
                        ),
                        context=line.strip()[:200],
                    )
                )
    return out
