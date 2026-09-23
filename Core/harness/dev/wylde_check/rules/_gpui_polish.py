"""GPUI panel-polish rules (rules 42-43).

Two rules that tighten the panel↔service contract beyond what rules
33-40 cover.  Like the rest of the gpui-suite they walk the active tree
read-only and emit ``Finding`` objects without mutating state.

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
from typing import Dict, List

from .. import Finding
from .._walkers import _is_excluded, _read_text, _to_rel
from ._gpui_contract import (
    _find_matching_close,
    _line_no_at,
    _walk_panel_manifests,
)

_pkg = _sys.modules[__name__.rsplit(".", 2)[0]]


# ── Layout constants ─────────────────────────────────────────────────


GPUI_WORKSPACE_ROOT: str = "Core/GUI"
GPUI_PANELS_ROOT: str = "Core/GUI/Frontend/Panels"
GPUI_WORKSPACE_CARGO: str = "Core/GUI/Cargo.toml"
RUST_CRATES_ROOT: str = "rust/crates"


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
