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
  in that service's ``ALL_PIPE_ACTIONS`` / ``ALL_ACTIONS`` array (and,
  for the harness, the Python ``_ACTIONS`` dict).  Services without a
  discoverable registry are intentionally skipped.

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

_pkg = _sys.modules[__name__.rsplit(".", 2)[0]]


# ── Layout constants (mirroring _gpui.py) ────────────────────────────


GPUI_WORKSPACE_ROOT: str = "Core/GUI"
GPUI_PANELS_ROOT: str = "Core/GUI/Frontend/Panels"

# Canonical sources for the harness pipe-action registry.
RUST_HARNESS_PIPE_FILE: str = "rust/crates/wylde-harness/src/pipe.rs"
PYTHON_HARNESS_PIPE_FILE: str = "Core/harness/pipe/__init__.py"

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


def _load_harness_action_registry() -> Set[str]:
    """Union of every harness verb registered on Rust + Python sides.

    Reads ``ALL_PIPE_ACTIONS`` from ``rust/crates/wylde-harness/src/pipe.rs``
    and ``_ACTIONS = {...}`` from ``Core/harness/pipe/__init__.py``; the
    union is what an over-the-wire panel call would actually reach.
    """
    verbs: Set[str] = set()
    rust_path = _pkg.WYLDE_ROOT / RUST_HARNESS_PIPE_FILE
    if rust_path.exists():
        text = _read_text(rust_path) or ""
        verbs |= _scan_action_array(text, _RUST_PIPE_ACTIONS_RE)
    py_path = _pkg.WYLDE_ROOT / PYTHON_HARNESS_PIPE_FILE
    if py_path.exists():
        text = _read_text(py_path) or ""
        m = _PY_ACTIONS_DICT_RE.search(text)
        if m:
            for lit in _PY_ACTION_KEY_RE.findall(m.group(1)):
                verbs.add(lit)
    return verbs


def _load_all_service_registries() -> Dict[str, Set[str]]:
    """``service_name`` → set of verbs that service serves.

    Combines the harness's Rust + Python registries (under
    ``wylde-harness``) with every other service in
    ``RUST_SERVICE_REGISTRIES``.  Services without a discoverable
    registry contribute the empty set — rule 38 then skips them
    rather than false-flagging every call.
    """
    out: Dict[str, Set[str]] = {}
    out["wylde-harness"] = _load_harness_action_registry()
    for service, src_root in RUST_SERVICE_REGISTRIES.items():
        if service == "wylde-harness":
            continue
        out[service] = _load_service_registry_verbs(src_root)
    return out


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

      * ``wylde-harness`` — union of Rust ``ALL_PIPE_ACTIONS`` and
        Python ``_ACTIONS``.
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
    registries = _load_all_service_registries()
    # If nothing at all could be loaded, we're in a tree without any
    # service crates checked in — skip the rule rather than fire empty.
    if not any(registries.values()):
        return out
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
            # Empty registry → service not indexed; intentionally skip.
            if not registry:
                continue
            if call.action is None:
                # Dynamic action body — out of scope for this rule.
                continue
            if call.action in registry:
                continue
            if service == "wylde-harness":
                hint = (
                    f"({RUST_HARNESS_PIPE_FILE}::ALL_PIPE_ACTIONS or "
                    f"{PYTHON_HARNESS_PIPE_FILE}::_ACTIONS)"
                )
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
