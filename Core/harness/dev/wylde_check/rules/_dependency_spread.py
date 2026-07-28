"""Dependency-spread ratchet (rule 62).

The forward-looking half of #290 (dependency isolation). The isolation work
itself — wrapping `rand` behind `wylde_shared::rng`, `cpal` behind
`wylde-voice::audio_device` — is a one-time cleanup; nothing stops the *next*
churny dependency from silently spreading across the workspace the way `rand`
(2 crates) and `axum` (3) had, so that the day one of them broke, the fix was a
shotgun edit everywhere it touched.

This rule is the ratchet. It measures each external dependency's **crate
spread** — how many crates take a direct dependency edge on it — and fails the
gate when a dependency grows past its recorded baseline. Crate spread (not raw
call-site count) is the metric that matters: a breaking bump breaks you *per
crate* (each crate's Cargo.toml + build), and the number only moves when a
dependency edge is added or removed, so the baseline stays stable across
ordinary code edits.

Three tiers (a grandfather ratchet, like the earlier allowlist rules):

* **Contained** (``DEPENDENCY_CONTAINED``) — deps that #290 deliberately routed
  through a single owning crate's adapter. They must stay at that one crate; a
  direct dep anywhere else means the adapter was bypassed.
* **Baselined** (``DEPENDENCY_SPREAD_BASELINE``) — every dep whose current
  spread is accepted as-is. The rule fails only when spread *grows* beyond the
  recorded number. Lowering a number after a cleanup ratchets the gate tighter;
  raising one is a deliberate, reviewed edit in ``_config.py``.
* **New** — a dep that is neither contained nor baselined trips as soon as it
  spans more than ``DEPENDENCY_SPREAD_NEW_MAX`` crates, forcing a conscious
  decision the moment something starts to spread.

The baseline is seeded from the tree as committed, so day one is green (a
self-test pins that); the rule only bites *future* growth. Like the rest of the
suite it walks the active tree read-only and emits ``Finding`` objects.
"""

from __future__ import annotations

import re
from collections import defaultdict
from typing import Dict, List, Set

from .. import Finding
from .._config import (
    DEPENDENCY_CONTAINED,
    DEPENDENCY_SPREAD_BASELINE,
    DEPENDENCY_SPREAD_NEW_MAX,
)
from .._walkers import _read_text, _to_rel, _walk

# A dependency key at the start of a line inside a [*dependencies] table:
#   foo = "1"        |   foo = { version = "1", ... }   |   foo.workspace = true
#   "foo-bar" = ...  (quoted keys are rare but legal)
_DEP_KEY_RE = re.compile(r'^\s*("?)([A-Za-z0-9_-]+)\1\s*(=|\.\s*workspace)')


def _crate_name(crate_dir: str) -> str:
    """Basename of a crate directory (…/wylde-shared → wylde-shared)."""
    return crate_dir.rsplit("/", 1)[-1]


def _external_dep_spread() -> Dict[str, Set[str]]:
    """Map each external crate name to the set of crate directories that take a
    direct dependency edge on it.

    Walks every ``Cargo.toml`` under the active roots and reads the dependency
    tables (``[dependencies]``, ``[dev-dependencies]``, ``[build-dependencies]``
    and their ``[target.'…'.dependencies]`` variants — every table whose header
    ends in ``dependencies]``). Internal path crates are skipped: anything named
    ``wylde*`` and any entry carrying an explicit ``path =`` (a local, non-
    crates.io dependency has no upstream bump to worry about).
    """
    spread: Dict[str, Set[str]] = defaultdict(set)
    for path in _walk((".toml",)):
        if path.name != "Cargo.toml":
            continue
        crate_dir = _to_rel(path).rsplit("/", 1)[0]
        in_deps = False
        for line in _read_text(path).splitlines():
            stripped = line.strip()
            if stripped.startswith("["):
                # A dependency table header ends in "dependencies]"; anything
                # else ([package], [features], [lints], …) closes the section.
                in_deps = stripped.endswith("dependencies]")
                continue
            if not in_deps or not stripped or stripped.startswith("#"):
                continue
            m = _DEP_KEY_RE.match(line)
            if not m:
                continue
            name = m.group(2)
            if name.startswith("wylde"):
                continue  # internal path crate
            if "path =" in line or "path=" in line:
                continue  # local path dep — no upstream to break
            spread[name].add(crate_dir)
    return spread


def check_dependency_spread_ratchet() -> List[Finding]:
    """Flag external dependencies whose crate-spread has grown past baseline.

    See the module docstring for the three tiers. Findings are ``warning`` but
    still fail the gate (per repo policy the suite fails on any finding); each
    message names the escape — wrap the dep behind an adapter, or, if the
    spread is intentional, adjust ``_config.py``.
    """
    out: List[Finding] = []
    spread = _external_dep_spread()

    def _manifest_of(crate_dir: str) -> str:
        return f"{crate_dir}/Cargo.toml"

    # 1. Contained deps must stay at their single owning crate.
    for dep, owner in sorted(DEPENDENCY_CONTAINED.items()):
        offenders = sorted(
            c for c in spread.get(dep, set()) if _crate_name(c) != owner
        )
        for crate_dir in offenders:
            out.append(
                Finding(
                    rule="dependency_spread_ratchet",
                    severity="warning",
                    file=_manifest_of(crate_dir),
                    line=0,
                    message=(
                        f"`{dep}` is a contained dependency — #290 routes it through "
                        f"`{owner}`'s adapter so a breaking bump is a one-file change. "
                        f"Crate `{_crate_name(crate_dir)}` took a direct `{dep}` "
                        f"dependency, bypassing the adapter. Depend on `{owner}` and "
                        f"call through it instead."
                    ),
                )
            )

    # 2. Baselined deps must not exceed their recorded spread.
    for dep, base in sorted(DEPENDENCY_SPREAD_BASELINE.items()):
        crates = sorted(spread.get(dep, set()))
        if len(crates) > base:
            out.append(
                Finding(
                    rule="dependency_spread_ratchet",
                    severity="warning",
                    file=_manifest_of(crates[-1]),
                    line=0,
                    message=(
                        f"`{dep}` now spans {len(crates)} crates (baseline {base}). A "
                        f"breaking bump would touch every one. Wrap it behind a thin "
                        f"adapter (#290), or, if this spread is intentional, raise "
                        f"`{dep}`'s entry in DEPENDENCY_SPREAD_BASELINE (_config.py) "
                        f"with a note. Crates: {[_crate_name(c) for c in crates]}."
                    ),
                )
            )

    # 3. New deps (neither contained nor baselined) that already spread.
    known = set(DEPENDENCY_CONTAINED) | set(DEPENDENCY_SPREAD_BASELINE)
    for dep, crates_set in sorted(spread.items()):
        if dep in known:
            continue
        crates = sorted(crates_set)
        if len(crates) > DEPENDENCY_SPREAD_NEW_MAX:
            out.append(
                Finding(
                    rule="dependency_spread_ratchet",
                    severity="warning",
                    file=_manifest_of(crates[-1]),
                    line=0,
                    message=(
                        f"new dependency `{dep}` already spans {len(crates)} crates "
                        f"(threshold {DEPENDENCY_SPREAD_NEW_MAX}). Decide now: contain "
                        f"it behind an adapter (#290), or add a reviewed baseline entry "
                        f"in DEPENDENCY_SPREAD_BASELINE (_config.py). Crates: "
                        f"{[_crate_name(c) for c in crates]}."
                    ),
                )
            )

    return out
