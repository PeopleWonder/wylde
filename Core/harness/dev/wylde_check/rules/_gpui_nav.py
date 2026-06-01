"""GPUI nav-target rule (rule 39).

Carved out of :mod:`wylde_check.rules._gpui_contract` when that file
crossed the flat 700-LOC cap.  Hosts the single rule that walks
``request_nav(...)`` callsites and cross-references them against the
panel manifests that declare what nav keys exist.

* :func:`check_nav_targets_exist` — every literal-string
  ``request_nav("X")`` (and every ``request_nav(IDENT)`` where
  ``IDENT`` is a file-local ``const IDENT: &str = "..."``) must
  resolve to a panel actually declared by some ``manifest.json``
  under ``Core/GUI/Frontend/Panels/**``.  Variable-argument call
  sites whose value isn't a const string are intentionally skipped.
"""

from __future__ import annotations

import json
import re
import sys as _sys
from pathlib import Path
from typing import Dict, List, Set, Tuple

from .. import Finding
from .._walkers import _is_excluded, _read_text, _to_rel
from ._gpui_contract import (
    GPUI_PANELS_ROOT,
    GPUI_WORKSPACE_ROOT,
    _line_no_at,
    _walk_panel_manifests,
)

_pkg = _sys.modules[__name__.rsplit(".", 2)[0]]


# ── Walk helpers ─────────────────────────────────────────────────────


def _walk_gui_rs_for_nav() -> List[Path]:
    """Every ``.rs`` file under ``Core/GUI/`` that might call ``request_nav``.

    Skips the legacy Tauri+Svelte tree (``Core/GUI/src/`` +
    ``Core/GUI/src-tauri/``), the build output (``target/``), and the
    nav-bus source itself (where ``request_nav`` is defined).
    """
    base = _pkg.WYLDE_ROOT / GPUI_WORKSPACE_ROOT
    if not base.exists():
        return []
    out: List[Path] = []
    for path in base.rglob("*.rs"):
        if _is_excluded(path):
            continue
        rel = _to_rel(path)
        if rel.startswith("Core/GUI/src/") or rel.startswith("Core/GUI/src-tauri/"):
            continue
        if rel.endswith("/nav_bus.rs"):
            continue
        out.append(path)
    return out


# ── Panel registry ───────────────────────────────────────────────────


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


# ── Source mini-parsers ──────────────────────────────────────────────


# Matches `request_nav("X")` and the fully-qualified forms.  Only
# string-literal args are statically checkable — variable args fall
# out of the match and are intentionally skipped.
_REQUEST_NAV_LITERAL_RE = re.compile(
    r"(?:wylde_gui_pipe::|nav_bus::)?request_nav\s*\(\s*\"([^\"]+)\"\s*\)"
)

# A `request_nav(IDENT)` site — IDENT may resolve to a file-local
# `const IDENT: &str = "..."` declaration.
_REQUEST_NAV_IDENT_RE = re.compile(
    r"(?:wylde_gui_pipe::|nav_bus::)?request_nav\s*\(\s*([A-Za-z_][A-Za-z0-9_]*)\s*\)"
)

# `const TARGET: &str = "core/foo";` / `pub const TARGET: &str = "..."`
# anywhere in the file.  Lifetime annotations on the slice ref are
# accepted (``&'static str``).
_STR_CONST_RE = re.compile(
    r'^\s*(?:pub(?:\s*\([^)]*\))?\s+)?const\s+([A-Z][A-Z0-9_]*)\s*:\s*&(?:\'?[a-zA-Z_][a-zA-Z0-9_]*\s+)?str\s*=\s*"([^"]+)"',
    re.MULTILINE,
)


def _parse_str_constants(text: str) -> Dict[str, str]:
    """Map ``IDENT`` → literal string value for every file-local
    ``[pub] const IDENT: &str = "..."`` declaration."""
    out: Dict[str, str] = {}
    for m in _STR_CONST_RE.finditer(text):
        out[m.group(1)] = m.group(2)
    return out


def _strip_rust_comments(text: str) -> str:
    """Blank out Rust line + block comments so the regex doesn't
    fire inside ``//!`` module docs that reference the function name
    as an example.  Preserves byte offsets so line numbers line up
    with the original source.
    """
    out: List[str] = []
    i = 0
    n = len(text)
    while i < n:
        ch = text[i]
        if ch == '"':
            j = i + 1
            while j < n:
                if text[j] == "\\":
                    out.append(text[i:j + 2])
                    i = j + 2
                    j = i
                    continue
                if text[j] == '"':
                    out.append(text[i:j + 1])
                    i = j + 1
                    break
                j += 1
            else:
                out.append(text[i:])
                i = n
            continue
        if ch == "/" and i + 1 < n:
            nxt = text[i + 1]
            if nxt == "/":
                end = text.find("\n", i)
                if end == -1:
                    end = n
                out.append(" " * (end - i))
                i = end
                continue
            if nxt == "*":
                end = text.find("*/", i + 2)
                if end == -1:
                    end = n
                else:
                    end += 2
                blanked = "".join(
                    c if c == "\n" else " " for c in text[i:end]
                )
                out.append(blanked)
                i = end
                continue
        out.append(ch)
        i += 1
    return "".join(out)


# ── Rule 39: nav_targets_exist ───────────────────────────────────────


def check_nav_targets_exist() -> List[Finding]:
    """Every literal-string ``request_nav("X")`` must resolve to a panel
    that some manifest under ``Core/GUI/Frontend/Panels/**`` declares.

    The runtime nav bus silently absorbs unknown keys (``request_nav``
    returns ``false``), so an unknown target shows up as a no-op the
    user can't debug.  Catching it at lint time forces the panel
    registry and the nav callsites to stay in sync.

    Comments are stripped before the regex fires so module-doc
    references that name the API (``//! request_nav("core/<id>")``)
    don't false-fire.  String literals are preserved verbatim so an
    in-string ``//`` doesn't accidentally blank the call.
    """
    out: List[Finding] = []
    valid = _load_panel_registry_keys()
    if not valid:
        return out
    for path in _walk_gui_rs_for_nav():
        rel = _to_rel(path)
        text = _read_text(path)
        if not text:
            continue
        stripped = _strip_rust_comments(text)
        constants = _parse_str_constants(stripped)
        seen_spans: Set[Tuple[int, int]] = set()

        def _emit(key: str, span_start: int, span_end: int) -> None:
            if key in valid:
                return
            span = (span_start, span_end)
            if span in seen_spans:
                return
            seen_spans.add(span)
            lineno = _line_no_at(text, span_start)
            line_start = text.rfind("\n", 0, span_start) + 1
            line_end = text.find("\n", span_start)
            if line_end == -1:
                line_end = len(text)
            context_line = text[line_start:line_end].strip()
            out.append(
                Finding(
                    rule="nav_targets_exist",
                    severity="error",
                    file=rel,
                    line=lineno,
                    message=(
                        f"Panel navigates to `{key}` which is not a registered "
                        f"panel.  Valid first-party keys come from "
                        f"`{GPUI_PANELS_ROOT}/<X>/manifest.json` "
                        f"(`<service>/<id>`).  Runtime call silently no-ops."
                    ),
                    context=context_line[:200],
                )
            )

        for m in _REQUEST_NAV_LITERAL_RE.finditer(stripped):
            _emit(m.group(1), m.start(), m.end())
        for m in _REQUEST_NAV_IDENT_RE.finditer(stripped):
            ident = m.group(1)
            resolved = constants.get(ident)
            if resolved is None:
                continue
            _emit(resolved, m.start(), m.end())
    return out
