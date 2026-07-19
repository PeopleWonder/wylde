"""Harness + service action registries — the input rules 38 and 48 check against.

Extracted from ``_gpui_contract.py`` for issue #116.  Two reasons, both
structural rather than cosmetic:

* Rules 38 (panel → service) and 48 (Gateway → harness) both check verbs
  against these registries.  Rule 48 lives in ``_gateway_contract.py``
  and was reaching sideways into ``_gpui_contract`` to borrow the
  loader; the shared input now has its own home and both consumers
  import it from here.
* The registry loader is exactly what rotted in #116 — the constants
  outlived the files they named, the loader returned an empty set, and
  both rules passed vacuously.  Keeping it in one small module with the
  failure mode documented at the top makes the next repointing obvious
  instead of buried 300 lines into a 700-line file.

The load-failure contract is the important part: these loaders **raise**
rather than returning empty.  An empty registry is indistinguishable
from "nothing to check", and that conflation is the bug.
"""

from __future__ import annotations

import re
import sys as _sys
from typing import Dict, Set

from .._walkers import _is_excluded, _read_text

_pkg = _sys.modules[__name__.rsplit(".", 2)[0]]


# Canonical source for the harness pipe-action registry.
#
# Repointed for issue #116: this was ``.../src/pipe.rs`` (the crate grew
# a module directory) alongside a ``Core/harness/pipe/__init__.py``
# Python half that the Rust cutover deleted entirely.  Both paths were
# absent from ``develop``, so the registry loaded empty and rules 38/48
# passed vacuously.  There is exactly one registry now, and a failure to
# load it is a hard error — see ``_load_harness_action_registry``.
RUST_HARNESS_PIPE_FILE: str = "rust/crates/wylde-harness/src/pipe/mod.rs"

# Every Rust crate that exposes an in-process action registry.  Each
# value is the crate's ``src/`` root — the rule scans every ``.rs``
# file there for ``ALL_PIPE_ACTIONS`` (harness shape, ``&[&str]``) and
# ``ALL_ACTIONS`` (service shape, ``[&str; N]``) literal-array
# declarations.  Adding a new entry registers its action surface for
# rule 38; the lookup then knows what verbs that service legitimately
# serves.
RUST_SERVICE_REGISTRIES: Dict[str, str] = {
    "wylde-harness": "rust/crates/wylde-harness/src",
    "wylde-extension-bridge": "rust/crates/wylde-extension-bridge/src",
    "wylde-ollama": "rust/crates/wylde-ollama/src",
    "wylde-voice": "rust/crates/wylde-voice/src",
}


# ── Harness action registry (Rust + Python) ──────────────────────────


_RUST_PIPE_ACTIONS_RE = re.compile(
    r"pub\s+const\s+ALL_PIPE_ACTIONS\s*:\s*&\[&str\]\s*=\s*&\[([^\]]*)\]",
    re.DOTALL,
)
# The non-harness service crates declare ``const ALL_ACTIONS: [&str; N] = [...]``
# (no ``pub``, fixed-size array).  Match both that shape and the
# ``pub const ALL_ACTIONS`` variant some services use.
_RUST_ALL_ACTIONS_RE = re.compile(
    r"(?:pub\s+)?const\s+ALL_ACTIONS\s*:\s*\[&str\s*;\s*\d+\s*\]\s*=\s*\[([^\]]*)\]",
    re.DOTALL,
)
_RUST_STRING_LITERAL_RE = re.compile(r'"([^"\\]+)"')

_PY_ACTIONS_DICT_RE = re.compile(
    r"_ACTIONS\s*=\s*\{(.*?)\n\s*\}",
    re.DOTALL,
)
_PY_ACTION_KEY_RE = re.compile(r'"([A-Za-z][A-Za-z0-9_.]*)"\s*:')


def _scan_action_array(text: str, regex: re.Pattern[str]) -> Set[str]:
    """Pull every literal string out of an action-array declaration."""
    verbs: Set[str] = set()
    for m in regex.finditer(text):
        for lit in _RUST_STRING_LITERAL_RE.findall(m.group(1)):
            verbs.add(lit)
    return verbs


def _load_service_registry_verbs(src_root: str) -> Set[str]:
    """Union of every verb declared in any ``ALL_PIPE_ACTIONS`` /
    ``ALL_ACTIONS`` array under ``src_root``."""
    verbs: Set[str] = set()
    base = _pkg.WYLDE_ROOT / src_root
    if not base.exists():
        return verbs
    for path in base.rglob("*.rs"):
        if _is_excluded(path):
            continue
        text = _read_text(path)
        if not text:
            continue
        verbs |= _scan_action_array(text, _RUST_PIPE_ACTIONS_RE)
        verbs |= _scan_action_array(text, _RUST_ALL_ACTIONS_RE)
    return verbs


class HarnessRegistryUnavailable(RuntimeError):
    """The harness pipe-action registry could not be loaded.

    Raised rather than returning an empty set so that no caller can
    mistake "I could not check" for "I checked and found nothing".  That
    conflation is the bug this exception exists to make impossible —
    rules 38 and 48 both used to ``return out`` on an empty registry and
    reported a clean pass while checking nothing (issue #116).
    """


def _load_harness_action_registry() -> Set[str]:
    """Every harness verb registered on the pipe.

    Reads ``ALL_PIPE_ACTIONS`` from
    ``rust/crates/wylde-harness/src/pipe/mod.rs`` — the single registry a
    panel or Gateway call actually reaches at runtime.

    Raises:
        HarnessRegistryUnavailable: the file is missing, unreadable, or
            declares no verbs.  An empty registry means the rule cannot
            do its job; that is a failure, not a pass.
    """
    rust_path = _pkg.WYLDE_ROOT / RUST_HARNESS_PIPE_FILE
    if not rust_path.exists():
        raise HarnessRegistryUnavailable(
            f"harness pipe registry not found at {RUST_HARNESS_PIPE_FILE!r}"
        )
    text = _read_text(rust_path) or ""
    verbs = _scan_action_array(text, _RUST_PIPE_ACTIONS_RE)
    if not verbs:
        raise HarnessRegistryUnavailable(
            f"{RUST_HARNESS_PIPE_FILE!r} declares no verbs — expected a "
            f"``pub const ALL_PIPE_ACTIONS: &[&str] = &[...]`` array"
        )
    return verbs


def _load_all_service_registries() -> Dict[str, Set[str]]:
    """``service_name`` → set of verbs that service serves.

    Combines the harness pipe registry (under ``wylde-harness``) with
    every other service in ``RUST_SERVICE_REGISTRIES``.  Services
    without a discoverable registry contribute the empty set — rule 38
    then skips them rather than false-flagging every call.

    ``wylde-harness`` is the exception: it is the highest-traffic edge
    in the tree and its registry is mandatory, so a load failure
    propagates as ``HarnessRegistryUnavailable`` rather than degrading
    into an empty set that reads as coverage (issue #116).
    """
    out: Dict[str, Set[str]] = {}
    out["wylde-harness"] = _load_harness_action_registry()
    for service, src_root in RUST_SERVICE_REGISTRIES.items():
        if service == "wylde-harness":
            continue
        out[service] = _load_service_registry_verbs(src_root)
    return out


