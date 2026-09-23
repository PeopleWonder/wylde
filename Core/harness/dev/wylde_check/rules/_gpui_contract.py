"""GPUI panel-to-harness contract rules (rules 38 + 40).

Two rules scoped to the gpui-era GUI workspace at ``Core/GUI/``.
They catch panel↔harness drift at edit-time rather than runtime by
walking each panel's ``src/ipc.rs`` and cross-referencing the verbs
and services it names against the matching registry on the service
side.

Rule 39 (``nav_targets_exist``) carved out to :mod:`_gpui_nav` when
this file crossed the flat 700-LOC cap — same suite, separate file
for the unrelated nav-bus machinery.

* :func:`check_panel_verbs_exist_in_harness_registry` — every
  ``wylde_gui_pipe::call(SVC, "POST", "/__action__", json!({"action":
  "X", ...}))`` and every ``wylde_gui_pipe::stream_call(SVC, "X", ...)``
  whose resolved service has a discoverable action registry
  (``wylde-harness`` ∪ ``wylde-extension-bridge`` ∪ ``wylde-ollama`` ∪
  ``wylde-voice``) must name a verb that appears
  in that service's ``ALL_PIPE_ACTIONS`` / ``ALL_ACTIONS`` array.
  Services with no declared registry are intentionally skipped; a
  service that *is* declared but whose registry fails to load is an
  ``error``, not a skip (issue #116).  The registries themselves live
  in :mod:`_harness_registry`.

* :func:`check_required_services_includes_called_services` — every
  panel ``manifest.json`` must declare in ``required_services`` every
  service its sibling ``src/ipc.rs`` calls (under-declaration → ERROR);
  conversely, every service listed in ``required_services`` should be
  actually called (over-declaration → WARNING).

The rules walk ``Core/GUI/Frontend/Panels/*/src/ipc.rs`` for the
contract surface.  All are advisory; like the rest of the
``wylde_check`` suite they emit ``Finding`` objects and never mutate
state.
"""

from __future__ import annotations

import json
import re
import sys as _sys
from pathlib import Path
from typing import Dict, List, Optional, Set

from .. import Finding
from .._walkers import _is_excluded, _read_text, _to_rel
from ._harness_registry import (  # re-exported: rule 48 and the tests import these from here
    RUST_HARNESS_PIPE_FILE,
    RUST_SERVICE_REGISTRIES,
    HarnessRegistryUnavailable,
    _load_all_service_registries,
    _load_harness_action_registry,
    _load_service_registry_verbs,
    _scan_action_array,
)

_pkg = _sys.modules[__name__.rsplit(".", 2)[0]]


# ── Layout constants (mirroring _gpui.py) ────────────────────────────


GPUI_WORKSPACE_ROOT: str = "Core/GUI"
GPUI_PANELS_ROOT: str = "Core/GUI/Frontend/Panels"



# ── Walk helpers ─────────────────────────────────────────────────────


def _walk_panel_ipc_files() -> List[Path]:
    """Every ``Core/GUI/Frontend/Panels/*/src/ipc.rs`` file."""
    base = _pkg.WYLDE_ROOT / GPUI_PANELS_ROOT
    if not base.exists():
        return []
    out: List[Path] = []
    for child in sorted(base.iterdir()):
        if not child.is_dir():
            continue
        ipc = child / "src" / "ipc.rs"
        if ipc.exists() and not _is_excluded(ipc):
            out.append(ipc)
    return out


def _walk_panel_manifests() -> List[Path]:
    """Every ``manifest.json`` under ``Core/GUI/Frontend/Panels/**``."""
    base = _pkg.WYLDE_ROOT / GPUI_PANELS_ROOT
    if not base.exists():
        return []
    out: List[Path] = []
    for path in base.rglob("manifest.json"):
        if _is_excluded(path):
            continue
        out.append(path)
    return out


# ``_walk_gui_rs_for_nav`` carved out to :mod:`_gpui_nav` along with
# rule 39 when this file crossed the flat 700-LOC cap.


# ── Rust source mini-parser ──────────────────────────────────────────


_SERVICE_CONST_RE = re.compile(
    r'^\s*(?:pub\s+)?const\s+([A-Z][A-Z0-9_]*)\s*:\s*&(?:\'?[a-zA-Z_][a-zA-Z0-9_]*\s*)?str\s*=\s*"(wylde-[a-z][a-z0-9-]*)"',
    re.MULTILINE,
)


def _parse_service_constants(text: str) -> Dict[str, str]:
    """Map ``IDENT`` → ``"wylde-foo"`` for every ``pub const IDENT: &str = "wylde-..."``.

    Catches the canonical ``pub const SVC_HARNESS: &str = "wylde-harness"``
    shape every panel uses to name its target service.
    """
    out: Dict[str, str] = {}
    for m in _SERVICE_CONST_RE.finditer(text):
        out[m.group(1)] = m.group(2)
    return out


def _line_no_at(text: str, idx: int) -> int:
    """1-based line number of byte index ``idx`` within ``text``."""
    return text.count("\n", 0, idx) + 1


def _find_matching_close(text: str, open_idx: int) -> Optional[int]:
    """Return the index of the ``)`` matching the ``(`` at ``open_idx``.

    Tracks nested parens, brackets, braces, and string literals
    (double-quoted, with backslash escapes).  Returns ``None`` if the
    call is unterminated within the file (truncated source).
    """
    depth = 0
    i = open_idx
    n = len(text)
    while i < n:
        ch = text[i]
        if ch == '"':
            j = i + 1
            while j < n:
                if text[j] == "\\":
                    j += 2
                    continue
                if text[j] == '"':
                    break
                j += 1
            i = j + 1
            continue
        if ch in "({[":
            depth += 1
            i += 1
            continue
        if ch in ")}]":
            depth -= 1
            if depth == 0 and ch == ")":
                return i
            i += 1
            continue
        i += 1
    return None


def _split_top_args(body: str) -> List[str]:
    """Split a Rust call body into top-level comma-separated args.

    Tracks paren / brace / bracket depth + double-quoted string
    literals so commas inside ``json!({"a": 1, "b": 2})`` stay grouped
    with their owning arg.  Trims whitespace + trailing trailing-comma
    artefacts off each result.
    """
    out: List[str] = []
    depth = 0
    start = 0
    i = 0
    n = len(body)
    while i < n:
        ch = body[i]
        if ch == '"':
            j = i + 1
            while j < n:
                if body[j] == "\\":
                    j += 2
                    continue
                if body[j] == '"':
                    break
                j += 1
            i = j + 1
            continue
        if ch in "({[":
            depth += 1
            i += 1
            continue
        if ch in ")}]":
            depth -= 1
            i += 1
            continue
        if ch == "," and depth == 0:
            out.append(body[start:i].strip())
            start = i + 1
        i += 1
    tail = body[start:].strip()
    if tail:
        out.append(tail)
    return out


def _string_literal_value(arg: str) -> Optional[str]:
    """If ``arg`` is exactly a double-quoted string literal, return its
    contents (unescaped only for the trivial ``\\"`` case).  Else ``None``."""
    s = arg.strip()
    if not (s.startswith('"') and s.endswith('"') and len(s) >= 2):
        return None
    inner = s[1:-1]
    return inner.replace('\\"', '"')


_ACTION_KEY_RE = re.compile(r'"action"\s*:\s*"([A-Za-z][A-Za-z0-9_.]*)"')


def _extract_action_from_body(arg: str) -> Optional[str]:
    """Pull the ``"action": "verb.name"`` literal out of an envelope arg.

    The standard ``call`` envelope is
    ``Some(json!({"action": "chat.start_turn", "payload": ...}))``;
    we just regex-match the canonical shape.  Returns ``None`` when the
    arg doesn't carry a static-string action (e.g. a variable-built
    envelope).
    """
    m = _ACTION_KEY_RE.search(arg)
    if m:
        return m.group(1)
    return None


# ── Pipe-call extraction ─────────────────────────────────────────────


# Matches both fully-qualified and bare-imported forms:
#   wylde_gui_pipe::call(...)
#   wylde_gui_pipe::stream_call(...)
#   pipe::call(...) / pipe::stream_call(...)  — shorter import alias
_PIPE_CALL_RE = re.compile(
    r"\b(?:wylde_gui_pipe::|pipe::)(call|stream_call)\s*\("
)


class _PipeCall:
    """One ``call`` / ``stream_call`` invocation parsed out of an ipc.rs."""

    __slots__ = ("lineno", "kind", "service_token", "action", "raw_args")

    def __init__(
        self,
        lineno: int,
        kind: str,
        service_token: str,
        action: Optional[str],
        raw_args: List[str],
    ) -> None:
        self.lineno = lineno
        self.kind = kind  # "call" | "stream_call"
        self.service_token = service_token  # raw text of arg-0
        self.action = action  # resolved verb, or None when unparseable
        self.raw_args = raw_args


def _scan_pipe_calls(text: str) -> List[_PipeCall]:
    """Yield every ``pipe::call`` / ``pipe::stream_call`` invocation."""
    out: List[_PipeCall] = []
    for m in _PIPE_CALL_RE.finditer(text):
        kind = m.group(1)
        open_idx = m.end() - 1  # the '(' itself
        close_idx = _find_matching_close(text, open_idx)
        if close_idx is None:
            continue
        body = text[open_idx + 1 : close_idx]
        args = _split_top_args(body)
        if not args:
            continue
        service_token = args[0]
        action: Optional[str] = None
        if kind == "stream_call":
            # stream_call(svc, "verb.name", payload)
            if len(args) >= 2:
                action = _string_literal_value(args[1])
        else:
            # call(svc, method, path, body?) — verb lives in body
            if len(args) >= 4:
                action = _extract_action_from_body(args[3])
        lineno = _line_no_at(text, m.start())
        out.append(_PipeCall(lineno, kind, service_token, action, args))
    return out


def _resolve_service_token(
    token: str, constants: Dict[str, str]
) -> Optional[str]:
    """Resolve a service-arg token to a canonical ``"wylde-..."`` literal.

    A literal ``"wylde-foo"`` resolves to ``"wylde-foo"``; an identifier
    ``SVC_X`` resolves via the file-local constant map; anything else
    (e.g. ``&svc`` from a parameter) returns ``None`` so the caller can
    treat the site as not-statically-resolvable.
    """
    lit = _string_literal_value(token)
    if lit is not None:
        return lit
    ident = token.strip()
    return constants.get(ident)


# ── Panel registry (manifests) ───────────────────────────────────────


def _load_panel_registry_keys() -> Set[str]:
    """Set of ``"<service>/<id>"`` keys declared by first-party manifests.

    Mirrors the runtime registry's key shape (see
    ``Core/GUI/Manifest/Extension_handlers/src/registry.rs::registry_key``):
    every ``manifests.json`` has a top-level ``service`` and each entry
    in its ``panels`` array supplies the ``id``; the key is the joined
    pair.  Extension panels (``ext:<id>/<x>``) aren't covered — they
    don't exist statically.
    """
    keys: Set[str] = set()
    for path in _walk_panel_manifests():
        text = _read_text(path)
        if not text:
            continue
        try:
            data = json.loads(text)
        except (ValueError, TypeError):
            continue
        if not isinstance(data, dict):
            continue
        service = data.get("service")
        panels = data.get("panels")
        if not isinstance(service, str) or not isinstance(panels, list):
            continue
        for panel in panels:
            if not isinstance(panel, dict):
                continue
            pid = panel.get("id")
            if isinstance(pid, str) and pid:
                keys.add(f"{service}/{pid}")
    return keys


# ── Rule 38: panel_verbs_exist_in_harness_registry ───────────────────


def check_panel_verbs_exist_in_harness_registry() -> List[Finding]:
    """For every panel-side ``pipe::call`` / ``stream_call`` that targets
    a service with a discoverable in-process action registry, the named
    verb must appear in that service's registry.

    Today the rule indexes:

      * ``wylde-harness`` — Rust ``ALL_PIPE_ACTIONS``.  Mandatory: if
        this registry cannot be loaded the rule reports an ``error``
        rather than skipping (issue #116).
      * ``wylde-extension-bridge`` / ``wylde-ollama`` /
        ``wylde-voice`` — each service's
        ``ALL_ACTIONS: [&str; N] = [...]`` array.

    Services without a discoverable registry (``wylde-vpn``,
    ``wylde-gateway``'s REST surface) are intentionally skipped — the
    Gateway's HTTP surface lives in rule 41 instead.  Calls where the
    service or the action can't be statically resolved
    (parameter-passed service, dynamic action) are also skipped.
    """
    out: List[Finding] = []
    try:
        registries = _load_all_service_registries()
    except HarnessRegistryUnavailable as exc:
        # A rule that cannot load its input has not passed — it has
        # failed to run.  Reporting an error here is what stops this
        # rule from silently going dead the next time the harness crate
        # is restructured (issue #116).
        return [
            Finding(
                rule="panel_verbs_exist_in_harness_registry",
                severity="error",
                file=RUST_HARNESS_PIPE_FILE,
                line=0,
                message=(
                    f"Cannot verify panel pipe verbs: {exc}.  This rule "
                    f"guards every panel→service call in "
                    f"{GPUI_PANELS_ROOT}; with no registry it can check "
                    f"nothing, so it fails rather than passing vacuously.  "
                    f"Repoint ``RUST_HARNESS_PIPE_FILE`` at the harness "
                    f"pipe registry."
                ),
            )
        ]
    # One finding per broken service registry, not one per callsite.
    _empty_registry_reported: Set[str] = set()
    for ipc_path in _walk_panel_ipc_files():
        rel = _to_rel(ipc_path)
        text = _read_text(ipc_path)
        if not text:
            continue
        constants = _parse_service_constants(text)
        for call in _scan_pipe_calls(text):
            service = _resolve_service_token(call.service_token, constants)
            if service is None:
                continue
            registry = registries.get(service)
            if registry is None:
                # Service carries no declared registry at all
                # (``wylde-vpn``, the Gateway's REST surface).  Genuinely
                # out of scope — rule 41 covers the Gateway instead.
                continue
            if not registry:
                # Declared in RUST_SERVICE_REGISTRIES but loaded empty:
                # the crate was restructured out from under the rule.
                # That is a broken gate, not a clean service (#116).
                if service not in _empty_registry_reported:
                    _empty_registry_reported.add(service)
                    out.append(
                        Finding(
                            rule="panel_verbs_exist_in_harness_registry",
                            severity="error",
                            file=RUST_SERVICE_REGISTRIES[service],
                            line=0,
                            message=(
                                f"Service {service!r} is registered in "
                                f"RUST_SERVICE_REGISTRIES but its action "
                                f"registry loaded empty — no "
                                f"``ALL_ACTIONS``/``ALL_PIPE_ACTIONS`` array "
                                f"found under "
                                f"{RUST_SERVICE_REGISTRIES[service]!r}.  Every "
                                f"panel call to {service!r} is therefore "
                                f"unchecked.  Repoint the entry or remove it "
                                f"if the service no longer exposes verbs."
                            ),
                        )
                    )
                continue
            if call.action is None:
                # Dynamic action body — out of scope for this rule.
                continue
            if call.action in registry:
                continue
            if service == "wylde-harness":
                hint = f"({RUST_HARNESS_PIPE_FILE}::ALL_PIPE_ACTIONS)"
            else:
                hint = f"({RUST_SERVICE_REGISTRIES.get(service, service)}::ALL_ACTIONS)"
            out.append(
                Finding(
                    rule="panel_verbs_exist_in_harness_registry",
                    severity="error",
                    file=rel,
                    line=call.lineno,
                    message=(
                        f"Panel calls `{service}.{call.action}` but no "
                        f"such verb is registered in the service "
                        f"{hint}.  Either add the verb to the service, "
                        f"fix the typo, or remove the call.  Runtime "
                        f"fails with `no_action`."
                    ),
                    context=f"{call.kind}(svc, ..., \"{call.action}\")",
                )
            )
    return out


# Rule 39 (nav_targets_exist) carved out to :mod:`_gpui_nav` when this
# file crossed the flat 700-LOC cap.


# ── Rule 40: required_services_includes_called_services ──────────────


def _services_called_from_ipc(text: str) -> Set[str]:
    """Set of ``"wylde-..."`` services every ``pipe::call`` / ``stream_call``
    in ``text`` resolves to.  Unresolvable service tokens are dropped."""
    constants = _parse_service_constants(text)
    out: Set[str] = set()
    for call in _scan_pipe_calls(text):
        svc = _resolve_service_token(call.service_token, constants)
        if svc:
            out.add(svc)
    return out


def check_required_services_includes_called_services() -> List[Finding]:
    """Cross-check a panel's ``required_services`` against the services
    its ``src/ipc.rs`` actually calls — in both directions:

    * **Under-declaration** (ERROR): a service called from the panel's
      ``src/ipc.rs`` that isn't in ``required_services``.  The Shell's
      ServiceUnavailable stub only fires for services in this list, so
      an under-declared manifest leaves the panel rendering a broken
      half-state when the called service is down.

    * **Over-declaration** (WARNING): a service listed in
      ``required_services`` that the panel doesn't actually call.  The
      panel grays out unnecessarily when that service is down — users
      see ServiceUnavailable for a panel that would have worked fine.

    Panels whose docstring-documented design is "render degraded
    cards instead of the ServiceUnavailable stub" can opt out per
    panel by adding the rule name to a top-level
    ``wylde_check_opt_outs`` array in the manifest, e.g.
    ``"wylde_check_opt_outs": ["required_services_includes_called_services"]``.
    The opt-out only applies to the panel(s) defined in the manifest
    that carries it; it doesn't affect siblings.
    """
    out: List[Finding] = []
    rule_name = "required_services_includes_called_services"
    for manifest_path in _walk_panel_manifests():
        manifest_rel = _to_rel(manifest_path)
        # The panel directory is the parent of the manifest.json.
        panel_dir = manifest_path.parent
        ipc_path = panel_dir / "src" / "ipc.rs"
        if not ipc_path.exists():
            continue
        text = _read_text(manifest_path)
        if not text:
            continue
        try:
            data = json.loads(text)
        except (ValueError, TypeError):
            # JSON-shape diagnostics are rule 36's responsibility — skip
            # silently here so we don't double-flag.
            continue
        if not isinstance(data, dict):
            continue
        opt_outs = data.get("wylde_check_opt_outs")
        if isinstance(opt_outs, list) and rule_name in opt_outs:
            continue
        panels = data.get("panels")
        if not isinstance(panels, list) or not panels:
            continue
        # Union the required_services across every entry in the array.
        declared: Set[str] = set()
        for panel in panels:
            if not isinstance(panel, dict):
                continue
            req = panel.get("required_services")
            if isinstance(req, list):
                for s in req:
                    if isinstance(s, str) and s:
                        declared.add(s)

        ipc_text = _read_text(ipc_path)
        if not ipc_text:
            continue
        called = _services_called_from_ipc(ipc_text)
        missing = sorted(s for s in called if s not in declared)
        for svc in missing:
            out.append(
                Finding(
                    rule="required_services_includes_called_services",
                    severity="error",
                    file=manifest_rel,
                    line=0,
                    message=(
                        f"Panel calls service `{svc}` from "
                        f"`{_to_rel(ipc_path)}` but `required_services` "
                        f"in this manifest doesn't include it.  The Shell's "
                        f"ServiceUnavailable stub won't fire when `{svc}` "
                        f"is down — users would see a broken panel instead "
                        f"of the documented degraded state."
                    ),
                    context=f"required_services: {sorted(declared)}",
                )
            )
        extra = sorted(s for s in declared if s not in called)
        for svc in extra:
            out.append(
                Finding(
                    rule="required_services_includes_called_services",
                    severity="warning",
                    file=manifest_rel,
                    line=0,
                    message=(
                        f"Service `{svc}` is in required_services but the "
                        f"panel doesn't call it.  Either remove from "
                        f"manifest or call it; otherwise the "
                        f"ServiceUnavailable stub fires when `{svc}` is "
                        f"down even though the panel would still work."
                    ),
                    context=f"required_services: {sorted(declared)}",
                )
            )
    return out
