"""GPUI panel-polish rules (rules 41-43).

Three rules that tighten the panel↔service contract beyond what rules
33-40 cover.  Like the rest of the gpui-suite they walk the active tree
read-only and emit ``Finding`` objects without mutating state.

* :func:`check_rest_routes_exist_in_service` — every literal-shape
  ``wylde_gui_pipe::call(SVC, "METHOD", "/api/...", ...)`` whose service
  arg resolves to a Rust crate with an axum routing table must name a
  ``(method, path)`` pair that the routing table actually declares.
  Path parameters (``:id``) match panel-side wildcards (``{id}``).
  Today the rule only registers routes for ``wylde-gateway``; calls to
  services without an axum router (e.g. action-envelope harness calls)
  are intentionally skipped.

* :func:`check_manifest_factory_resolves` — every first-party panel
  ``manifest.json``'s ``source.factory`` string
  (``<crate_path>::<Type>::<fn>``) must resolve to a real
  ``pub fn <fn>(`` in the crate's source.  Catches deleted/renamed
  factory entry points at edit time so the panel-registry aggregator
  doesn't blow up at build time with an opaque link error.

* :func:`check_stream_call_must_handle_cancel` — every
  ``wylde_gui_pipe::stream_call(...)`` invocation must either be
  retained (``let stream = stream_call(...)`` / ``self.stream =
  Some(stream_call(...))`` / propagated via ``?`` / returned as the
  trailing expression of a helper) or carry the explicit opt-out
  marker ``// wylde-check: stream-discard-ok``.  Naked
  ``let _ = stream_call(...)`` or ``stream_call(...);`` statements
  drop the cancel handle immediately — the harness sees the abort and
  the stream never delivers a frame.
"""

from __future__ import annotations

import json
import re
import sys as _sys
from pathlib import Path
from typing import Dict, List, Optional, Tuple

from .. import Finding
from .._walkers import _is_excluded, _read_text, _to_rel
from ._gpui_contract import (
    _find_matching_close,
    _line_no_at,
    _parse_service_constants,
    _resolve_service_token,
    _scan_pipe_calls,
    _split_top_args,
    _string_literal_value,
    _walk_panel_ipc_files,
    _walk_panel_manifests,
)

_pkg = _sys.modules[__name__.rsplit(".", 2)[0]]


# ── Layout constants ─────────────────────────────────────────────────


GPUI_WORKSPACE_ROOT: str = "Core/GUI"
GPUI_PANELS_ROOT: str = "Core/GUI/Frontend/Panels"
GPUI_WORKSPACE_CARGO: str = "Core/GUI/Cargo.toml"
RUST_CRATES_ROOT: str = "rust/crates"


# Services whose routing surface lives in an axum router and is
# therefore in-scope for rule 41.  Adding a new entry here registers
# every ``route!()`` declaration under that crate's ``src/`` for the
# matcher.  Today this is just ``wylde-gateway``; if a future service
# grows its own REST surface, list it here.
ROUTE_INDEXED_SERVICES: Dict[str, str] = {
    "wylde-gateway": "rust/crates/wylde-gateway/src",
}


# REST methods that count as a "REST route" for rule 41.  Action-
# envelope calls (``POST /__action__``) and the special harness
# ``stream_call`` shape live in different registries (rule 38) and
# are intentionally excluded.
_REST_METHODS: Tuple[str, ...] = ("GET", "POST", "PUT", "DELETE", "PATCH")


# ── Rule 41: rest_routes_exist_in_service ────────────────────────────


# Matches the four canonical axum call shapes:
#   .route("/path", get(handler))
#   .route("/path", post(handler).delete(handler))
#   Router::new().route("/path", get(handler))  (just the .route portion)
#   axum::Router::new()...
# The regex is the *opening* ``.route(`` — we walk the body via
# `_find_matching_close` so we cover all argument styles.
_ROUTE_OPEN_RE = re.compile(r"\.route\s*\(")

# axum-style HTTP-method helpers inside the route body.  We match
# ``get(`` etc. anywhere in the body — combined ``.get(x).post(y)``
# yields both.
_METHOD_RE = re.compile(r"\b(get|post|put|delete|patch|head|options)\s*\(")


def _walk_service_route_files(service_root: str) -> List[Path]:
    """Yield every ``.rs`` file under the service's source tree."""
    base = _pkg.WYLDE_ROOT / service_root
    if not base.exists():
        return []
    out: List[Path] = []
    for path in base.rglob("*.rs"):
        if _is_excluded(path):
            continue
        out.append(path)
    return out


class _RouteEntry:
    __slots__ = ("method", "path", "file", "line")

    def __init__(self, method: str, path: str, file: str, line: int) -> None:
        self.method = method
        self.path = path
        self.file = file
        self.line = line


def _parse_routes_in_text(text: str, rel: str) -> List[_RouteEntry]:
    """Pull ``(METHOD, "/api/...")`` pairs out of every ``.route(...)``
    call in ``text``.  Best-effort — routes whose path is built from
    a constant or whose method handler isn't a literal verb are
    skipped (and not reported as errors)."""
    out: List[_RouteEntry] = []
    for m in _ROUTE_OPEN_RE.finditer(text):
        open_idx = m.end() - 1
        close_idx = _find_matching_close(text, open_idx)
        if close_idx is None:
            continue
        body = text[open_idx + 1 : close_idx]
        args = _split_top_args(body)
        if len(args) < 2:
            continue
        path = _string_literal_value(args[0])
        if path is None or not path.startswith("/"):
            continue
        methods_chunk = args[1]
        verbs = _METHOD_RE.findall(methods_chunk)
        if not verbs:
            continue
        lineno = _line_no_at(text, m.start())
        for verb in verbs:
            out.append(_RouteEntry(verb.upper(), path, rel, lineno))
    return out


def _load_route_registry() -> Dict[str, List[_RouteEntry]]:
    """``service_name`` → list of declared ``(method, path)`` routes."""
    out: Dict[str, List[_RouteEntry]] = {}
    for service, root in ROUTE_INDEXED_SERVICES.items():
        entries: List[_RouteEntry] = []
        for path in _walk_service_route_files(root):
            text = _read_text(path)
            if not text:
                continue
            entries.extend(_parse_routes_in_text(text, _to_rel(path)))
        out[service] = entries
    return out


# ``format!("/api/foo/{id}/bar")`` — extract the literal format-string
# without losing the wildcard segments.  Multi-line format!() bodies
# aren't covered (rare in the panel tree).
_FORMAT_BANG_RE = re.compile(r'\bformat!\s*\(\s*"((?:[^"\\]|\\.)+)"')


def _extract_call_path(arg: str) -> Optional[str]:
    """Resolve a ``call(...)``'s path arg to a string we can match.

    Literal: ``"/api/foo"`` → ``/api/foo``.
    ``format!("/api/foo/{id}")`` → ``/api/foo/{id}``.
    ``&format!("/api/foo/{id}")`` → same.
    Anything else (parameter, runtime concat, named const) → ``None``.
    """
    s = arg.strip()
    if s.startswith("&"):
        s = s[1:].lstrip()
    lit = _string_literal_value(s)
    if lit is not None:
        return lit
    m = _FORMAT_BANG_RE.search(s)
    if m:
        raw = m.group(1)
        # We only care about path-shape, so drop format-spec ``:?`` etc.
        # by stripping anything after the first ``:`` inside ``{...}``.
        return re.sub(r"\{([^{}:]+)(:[^{}]*)?\}", r"{\1}", raw)
    return None


def _normalize_panel_path(path: str) -> List[str]:
    """Tokenise a panel-side path for matching: literal segments stay
    literal; ``{name}`` segments become the wildcard sentinel ``*``."""
    tokens: List[str] = []
    for seg in path.split("/"):
        if not seg:
            continue
        if seg.startswith("{") and seg.endswith("}"):
            tokens.append("*")
        else:
            tokens.append(seg)
    return tokens


def _normalize_route_path(path: str) -> List[str]:
    """Tokenise an axum route path: ``:name`` and ``*name`` become the
    wildcard sentinel ``*``; literals stay literal."""
    tokens: List[str] = []
    for seg in path.split("/"):
        if not seg:
            continue
        if seg.startswith(":") or seg.startswith("*"):
            tokens.append("*")
        else:
            tokens.append(seg)
    return tokens


def _route_matches(panel_tokens: List[str], route_tokens: List[str]) -> bool:
    """Single-segment wildcard match.  Axum's ``*rest`` greedy capture
    is uncommon in this tree; treating it like a single-segment
    wildcard is the documented limitation."""
    if len(panel_tokens) != len(route_tokens):
        return False
    for p, r in zip(panel_tokens, route_tokens):
        if p == r:
            continue
        if p == "*" or r == "*":
            continue
        return False
    return True


def check_rest_routes_exist_in_service() -> List[Finding]:
    """For every literal-shape REST call from a panel into a
    route-indexed service, the ``(method, path)`` must appear in the
    service's axum router."""
    out: List[Finding] = []
    registry = _load_route_registry()
    # If no registry could be loaded — e.g. the gateway crate isn't
    # checked in — skip silently rather than fail-open across the tree.
    if not any(registry.values()):
        return out
    for ipc_path in _walk_panel_ipc_files():
        rel = _to_rel(ipc_path)
        text = _read_text(ipc_path)
        if not text:
            continue
        constants = _parse_service_constants(text)
        for call in _scan_pipe_calls(text):
            if call.kind != "call":
                continue
            service = _resolve_service_token(call.service_token, constants)
            if service is None or service not in ROUTE_INDEXED_SERVICES:
                continue
            if len(call.raw_args) < 3:
                continue
            method = _string_literal_value(call.raw_args[1])
            if method is None:
                continue
            method = method.upper()
            if method not in _REST_METHODS:
                continue
            path = _extract_call_path(call.raw_args[2])
            if path is None:
                # Path is a parameter / runtime concat — out of scope.
                continue
            # Action-envelope shape: route-rule doesn't own this surface.
            if path == "/__action__":
                continue
            panel_tokens = _normalize_panel_path(path)
            matched = False
            for entry in registry[service]:
                if entry.method != method:
                    continue
                if _route_matches(panel_tokens, _normalize_route_path(entry.path)):
                    matched = True
                    break
            if matched:
                continue
            out.append(
                Finding(
                    rule="rest_routes_exist_in_service",
                    severity="error",
                    file=rel,
                    line=call.lineno,
                    message=(
                        f"Panel calls `{method} {path}` on `{service}` but "
                        f"no matching route exists.  Either add the route, "
                        f"fix the path, or use the action-envelope pattern.  "
                        f"Runtime: 404 from the service."
                    ),
                    context=f"call({service}, \"{method}\", \"{path}\", ...)",
                )
            )
    return out


# ── Rule 42: manifest_factory_resolves ───────────────────────────────


# A factory string looks like ``wylde_panel_chat::ChatPanel::view`` or
# (less commonly) ``wylde_panel_chat::view``.  We only need to resolve
# the *first* segment (the crate) and the *last* segment (the function
# name).  Intermediate ``Type::`` segments are ignored — they're enforced
# at build time when the aggregator calls into the factory.
_FACTORY_RE = re.compile(r"^([A-Za-z_][A-Za-z0-9_]*)::(?:[A-Za-z_][A-Za-z0-9_]*::)*([A-Za-z_][A-Za-z0-9_]*)$")


def _load_workspace_member_crates() -> Dict[str, Path]:
    """Map crate-name-with-underscores → crate-source-root for every
    workspace member of the gpui workspace.

    Reads ``Core/GUI/Cargo.toml`` for the member list and each
    member's ``Cargo.toml`` for the canonical ``name`` field.
    Multi-crate paths (e.g. ``Frontend/Theme``) are resolved relative
    to the workspace root.
    """
    out: Dict[str, Path] = {}
    workspace = _pkg.WYLDE_ROOT / GPUI_WORKSPACE_CARGO
    if not workspace.exists():
        return out
    text = _read_text(workspace)
    if not text:
        return out
    members: List[str] = []
    m = re.search(r"\bmembers\s*=\s*\[", text)
    if m:
        rest = text[m.end():]
        depth = 1
        i = 0
        while i < len(rest) and depth > 0:
            ch = rest[i]
            if ch == "[":
                depth += 1
                i += 1
                continue
            if ch == "]":
                depth -= 1
                i += 1
                continue
            if ch in ('"', "'"):
                quote = ch
                j = i + 1
                while j < len(rest) and rest[j] != quote:
                    if rest[j] == "\\" and j + 1 < len(rest):
                        j += 2
                        continue
                    j += 1
                if j < len(rest):
                    members.append(rest[i + 1 : j])
                    i = j + 1
                    continue
            i += 1
    for member in members:
        crate_root = (_pkg.WYLDE_ROOT / GPUI_WORKSPACE_ROOT / member).resolve()
        cargo = crate_root / "Cargo.toml"
        if not cargo.exists():
            continue
        cargo_text = _read_text(cargo) or ""
        name_match = re.search(
            r'^\s*name\s*=\s*["\']([^"\']+)["\']', cargo_text, re.MULTILINE
        )
        if not name_match:
            continue
        canonical = name_match.group(1)
        out[canonical.replace("-", "_")] = crate_root
    return out


_PUB_FN_RE_CACHE: Dict[str, re.Pattern[str]] = {}


def _pub_fn_re(name: str) -> re.Pattern[str]:
    pattern = _PUB_FN_RE_CACHE.get(name)
    if pattern is None:
        pattern = re.compile(rf"\bpub(?:\s*\([^)]*\))?\s+(?:async\s+)?fn\s+{re.escape(name)}\b")
        _PUB_FN_RE_CACHE[name] = pattern
    return pattern


def _crate_has_pub_fn(crate_root: Path, fn_name: str) -> bool:
    """Best-effort grep for ``pub fn <fn_name>(`` (incl. ``pub(crate)``
    and ``pub async fn``) anywhere under the crate's ``src/``."""
    src = crate_root / "src"
    if not src.exists():
        return False
    rx = _pub_fn_re(fn_name)
    for path in src.rglob("*.rs"):
        if _is_excluded(path):
            continue
        text = _read_text(path)
        if not text:
            continue
        if rx.search(text):
            return True
    return False


def check_manifest_factory_resolves() -> List[Finding]:
    """Each first-party panel ``manifest.json``'s ``factory`` string must
    name a workspace-member crate and a ``pub fn`` that exists in that
    crate's source tree."""
    out: List[Finding] = []
    crates = _load_workspace_member_crates()
    if not crates:
        # Workspace not present — skip silently rather than false-flag
        # every manifest with a "missing crate".
        return out
    for manifest_path in _walk_panel_manifests():
        rel = _to_rel(manifest_path)
        text = _read_text(manifest_path)
        if not text:
            continue
        try:
            data = json.loads(text)
        except (ValueError, TypeError):
            # JSON-shape diagnostics belong to rule 36 — skip.
            continue
        if not isinstance(data, dict):
            continue
        panels = data.get("panels")
        if not isinstance(panels, list):
            continue
        for idx, panel in enumerate(panels):
            if not isinstance(panel, dict):
                continue
            source = panel.get("source")
            if not isinstance(source, dict):
                continue
            if source.get("kind") != "gpui_view":
                # iframe panels carry a url, not a factory.
                continue
            factory = source.get("factory")
            pid = panel.get("id", f"#{idx}")
            if not isinstance(factory, str) or not factory:
                out.append(
                    Finding(
                        rule="manifest_factory_resolves",
                        severity="error",
                        file=rel,
                        line=0,
                        message=(
                            f"panel {pid!r} has no `source.factory` string; "
                            f"gpui_view panels must name their entry point."
                        ),
                    )
                )
                continue
            m = _FACTORY_RE.match(factory)
            if not m:
                out.append(
                    Finding(
                        rule="manifest_factory_resolves",
                        severity="error",
                        file=rel,
                        line=0,
                        message=(
                            f"panel {pid!r} factory {factory!r} is not a "
                            f"recognized path-shape "
                            f"(`<crate>::<...>::<fn>`)."
                        ),
                    )
                )
                continue
            crate_segment = m.group(1)
            fn_name = m.group(2)
            crate_root = crates.get(crate_segment)
            if crate_root is None:
                out.append(
                    Finding(
                        rule="manifest_factory_resolves",
                        severity="error",
                        file=rel,
                        line=0,
                        message=(
                            f"panel {pid!r} factory {factory!r}: crate "
                            f"`{crate_segment}` is not a workspace member.  "
                            f"Aggregator will fail at build."
                        ),
                    )
                )
                continue
            if not _crate_has_pub_fn(crate_root, fn_name):
                out.append(
                    Finding(
                        rule="manifest_factory_resolves",
                        severity="error",
                        file=rel,
                        line=0,
                        message=(
                            f"panel {pid!r} factory {factory!r}: no "
                            f"`pub fn {fn_name}` found anywhere in "
                            f"`{_to_rel(crate_root)}/src/`.  Aggregator "
                            f"will fail at build."
                        ),
                    )
                )
    return out


# ── Rule 43: stream_call_must_handle_cancel ──────────────────────────


_STREAM_OPT_OUT = "wylde-check: stream-discard-ok"
_STREAM_CALL_OPEN_RE = re.compile(r"\b(?:wylde_gui_pipe::|pipe::)stream_call\s*\(")


def _stream_call_is_retained(text: str, call_start: int, close_idx: int) -> bool:
    """Decide whether the stream-call invocation at ``call_start`` is
    retained vs. silently dropped.

    Walks the few chars before ``call_start`` (back to the previous
    statement boundary) and a few after ``close_idx`` (to the next
    semicolon).  Returns True for any of the safe shapes:

      * Trailing expression of a block:  the close-paren is followed
        only by whitespace and ``}`` / end-of-file — no terminating
        ``;`` — so the result is the block's value.
      * Assignment:  prefix contains ``=`` (covers ``let x =``,
        ``self.x =``, ``self.x = Some(``, etc.) AND the binding is
        not the throwaway ``let _``.
      * Propagation:  the close-paren is followed by ``.await?`` or
        ``?`` or ``.await``.
      * Return / break / match scrutinee:  prefix ends in
        ``return`` / ``break`` / ``match``.

    Naked ``let _ = stream_call(...)`` and bare ``stream_call(...);``
    statements fail every check and fall through to ``False``.
    """
    n = len(text)
    # ── Walk back to the previous statement boundary ─────────────────
    # Statement boundaries are ``;`` and the block-opener ``{``.
    # Unmatched ``(`` / ``[`` are NOT boundaries — they're just
    # expression context (think ``Some(stream_call(...))``), so we
    # walk past them and keep looking for the real statement start.
    j = call_start - 1
    depth = 0
    while j >= 0:
        ch = text[j]
        if ch in ")]}":
            depth += 1
        elif ch in "([{":
            if depth > 0:
                depth -= 1
            elif ch == "{":
                break
            # Unmatched ``(`` / ``[`` at depth 0 — continue walking.
        elif depth == 0 and ch == ";":
            break
        j -= 1
    prefix = text[j + 1 : call_start]
    prefix_stripped = prefix.strip()

    # ── Walk forward to the next semicolon / newline boundary ────────
    k = close_idx + 1
    while k < n and text[k] in " \t":
        k += 1
    suffix_chars: List[str] = []
    while k < n and text[k] != "\n":
        suffix_chars.append(text[k])
        k += 1
    suffix = "".join(suffix_chars).strip()

    # ── Propagation forms ────────────────────────────────────────────
    if suffix.startswith("?") or suffix.startswith(".await"):
        return True
    if suffix.startswith(".") and ("?" in suffix or "await" in suffix):
        return True

    # ── Trailing expression of a block ───────────────────────────────
    # If the prefix has nothing of substance and the suffix is empty or
    # starts with ``}``, the call is the block's return value.
    if (not suffix or suffix.startswith("}")) and ";" not in suffix:
        # Make sure the prefix isn't an `let _ =` shape (still a discard
        # in a block that has no other content).
        if re.match(r"^let\s+_\s*=", prefix_stripped):
            return False
        # If the prefix is empty / a control keyword / an assignment
        # (incl. `return X = ...` is nonsense, so just treat any `=`
        # as binding) — accept.
        return True

    # ── Naked discard ───────────────────────────────────────────────
    if re.match(r"^let\s+_\s*=", prefix_stripped):
        return False
    if not prefix_stripped:
        # Bare ``stream_call(...);`` statement.  Drop.
        return False
    if prefix_stripped in ("return", "break", "match"):
        return True
    # ``return <expr>;`` shape — the assignment-less ``return`` is the
    # retention mechanism.
    if re.search(r"\breturn\b", prefix_stripped):
        return True
    if re.search(r"\bmatch\b\s*$", prefix_stripped):
        return True

    # ── Assignment shape ────────────────────────────────────────────
    # Any ``=`` in the prefix that isn't part of ``==`` / ``!=`` /
    # ``<=`` / ``>=`` indicates a binding.
    if re.search(r"(?<![=!<>])=(?!=)", prefix_stripped):
        return True

    return False


def _line_carries_marker(text: str, idx: int) -> bool:
    """True if the line containing ``idx`` or the line directly above
    carries the explicit ``stream-discard-ok`` opt-out marker."""
    line_start = text.rfind("\n", 0, idx) + 1
    line_end = text.find("\n", idx)
    if line_end == -1:
        line_end = len(text)
    if _STREAM_OPT_OUT in text[line_start:line_end]:
        return True
    prev_end = line_start - 1
    if prev_end < 0:
        return False
    prev_start = text.rfind("\n", 0, prev_end) + 1
    return _STREAM_OPT_OUT in text[prev_start:prev_end]


def check_stream_call_must_handle_cancel() -> List[Finding]:
    """Each ``stream_call`` invocation in a panel source must either
    retain the returned ``PipeStream`` (via binding, ``self.<field>``,
    propagation, return, or trailing-expression position) or carry
    the inline ``// wylde-check: stream-discard-ok`` marker."""
    out: List[Finding] = []
    base = _pkg.WYLDE_ROOT / GPUI_PANELS_ROOT
    if not base.exists():
        return out
    for path in base.rglob("*.rs"):
        if _is_excluded(path):
            continue
        rel = _to_rel(path)
        text = _read_text(path)
        if not text:
            continue
        for m in _STREAM_CALL_OPEN_RE.finditer(text):
            open_idx = m.end() - 1
            close_idx = _find_matching_close(text, open_idx)
            if close_idx is None:
                continue
            call_start = m.start()
            if _line_carries_marker(text, call_start):
                continue
            if _stream_call_is_retained(text, call_start, close_idx):
                continue
            lineno = _line_no_at(text, call_start)
            line_start = text.rfind("\n", 0, call_start) + 1
            line_end = text.find("\n", call_start)
            if line_end == -1:
                line_end = len(text)
            context_line = text[line_start:line_end].strip()
            out.append(
                Finding(
                    rule="stream_call_must_handle_cancel",
                    severity="error",
                    file=rel,
                    line=lineno,
                    message=(
                        "stream_call result not stored in a Drop-handle "
                        "field.  The stream may be cancelled prematurely "
                        "or leak.  Either store it (`let s = stream_call"
                        "(...)`, `self.stream = Some(stream_call(...))`) "
                        "or annotate `// wylde-check: stream-discard-ok` "
                        "if intentional."
                    ),
                    context=context_line[:200],
                )
            )
    return out
