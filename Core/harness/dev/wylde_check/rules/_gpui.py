"""GPUI-workspace architecture rules (rules 33-36).

Four rules scoped to the gpui-era GUI workspace at ``Core/GUI/``.
Rule 37 (``panel_crate_must_be_workspace_member``) carved out to
:mod:`wylde_check.rules._gpui_workspace` when this file crossed the
flat 700-LOC cap.

* :func:`check_no_cross_panel_imports` — a ``wylde-panel-*`` crate's
  ``Cargo.toml`` may only depend on the shared-infrastructure crates
  (``wylde-theme`` / ``wylde-gui-pipe`` / ``wylde-gpui-input`` /
  ``wylde-panel-registry``).  Direct panel-to-panel imports would build
  a coupling graph that breaks the "one panel per crate" boundary.

* :func:`check_no_legacy_gui_imports_in_panels` — no ``tauri::*`` use
  paths anywhere under ``Core/GUI/Frontend/Panels/**``.  Panel crates
  are gpui-native; the legacy Tauri tree lives in
  ``Core/GUI/src-tauri/`` and stays out of the gpui workspace.  (The
  Svelte matcher was retired 2026-07-20 — that tree was deleted at the
  slice-11 cutover.)

* :func:`check_webview_only_in_extension_handlers` — ``wry::*`` imports
  are reserved for the ``wylde-webview`` crate at
  ``Core/GUI/Frontend/Extension_handlers/WebView/``.  WebView is
  iframe-extension machinery; first-party panels must be native gpui.

* :func:`check_first_party_manifest_must_be_gpui_view` — every
  ``manifest.json`` under ``Core/GUI/Frontend/Panels/**`` declares
  ``source.kind == "gpui_view"`` for every entry in its ``panels`` array.
  (The symmetric ``Extensions/**`` half was retired 2026-07-20 — that
  tree no longer exists.)

* :func:`check_panel_crate_must_be_workspace_member` — every
  ``Cargo.toml`` found under ``Core/GUI/Frontend/Panels/*/Cargo.toml``
  appears in the ``members = [...]`` array of ``Core/GUI/Cargo.toml``.
  Conversely, every ``Frontend/Panels/*`` entry in ``members`` must
  resolve to an existing ``Cargo.toml`` — dangling member entries fail
  ``cargo metadata`` at build time, but the rule catches them at the
  architecture layer so the failure surfaces sooner.

All five rules walk ``Core/GUI/`` exclusively; the legacy Tauri+Svelte
tree under ``Core/GUI/src/`` + ``Core/GUI/src-tauri/`` is out of scope
(covered by rules 7-11 + 30 on the Svelte side, and excluded from
``RUST_CRATES_ROOT`` on the Rust side).
"""

from __future__ import annotations

import json
import re
import sys as _sys
from pathlib import Path
from typing import List, Optional, Set, Tuple

from .. import Finding
from .._walkers import _is_excluded, _read_text, _to_rel

_pkg = _sys.modules[__name__.rsplit(".", 2)[0]]


# ── GPUI workspace layout constants ──────────────────────────────────


GPUI_WORKSPACE_ROOT: str = "Core/GUI"
GPUI_PANELS_ROOT: str = "Core/GUI/Frontend/Panels"
GPUI_EXTENSION_HANDLERS_ROOT: str = "Core/GUI/Frontend/Extension_handlers"
GPUI_WEBVIEW_ROOT: str = "Core/GUI/Frontend/Extension_handlers/WebView"
GPUI_WORKSPACE_CARGO: str = "Core/GUI/Cargo.toml"

# Panel crates may depend on these and only these wylde-* internal
# crates.  Anything outside this allowlist that starts with ``wylde-``
# is flagged by rule 33.  Crates in the broader Rust workspace at
# ``rust/crates/*`` (e.g. ``wylde-harness``) are intentionally not
# allowed here either — panels reach the harness through the pipe
# surface, not by depending on the harness crate directly.
PANEL_SHARED_INFRA_CRATES: Tuple[str, ...] = (
    "wylde-theme",
    "wylde-gui-pipe",
    "wylde-gpui-input",
    "wylde-panel-registry",
)


# ── Walk helpers ──────────────────────────────────────────────────────


def _walk_gpui_rs(roots: Tuple[str, ...]) -> List[Path]:
    """Yield ``.rs`` files under each ``root`` (relative to WYLDE_ROOT),
    skipping the EXCLUDED_DIRS-set targets (``target/``, ``dist/``, ...)
    and the legacy Tauri+Svelte subtrees the gpui workspace excludes."""
    out: List[Path] = []
    seen: Set[Path] = set()
    for root in roots:
        base = _pkg.WYLDE_ROOT / root
        if not base.exists():
            continue
        for path in base.rglob("*.rs"):
            if _is_excluded(path):
                continue
            rel = _to_rel(path)
            # Legacy Tauri+Svelte tree — out of scope for every gpui rule.
            if rel.startswith("Core/GUI/src/") or rel.startswith("Core/GUI/src-tauri/"):
                continue
            key = path.resolve()
            if key in seen:
                continue
            seen.add(key)
            out.append(path)
    return out


def _walk_panel_cargo_tomls() -> List[Path]:
    """Every ``Core/GUI/Frontend/Panels/*/Cargo.toml`` file."""
    base = _pkg.WYLDE_ROOT / GPUI_PANELS_ROOT
    if not base.exists():
        return []
    out: List[Path] = []
    for child in sorted(base.iterdir()):
        if not child.is_dir():
            continue
        cargo = child / "Cargo.toml"
        if cargo.exists() and not _is_excluded(cargo):
            out.append(cargo)
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


def _is_rust_doc_or_comment(stripped: str) -> bool:
    """Same heuristic the Rust rules use — anchored to the start of a
    leading-whitespace-stripped line."""
    return (
        stripped.startswith("//")
        or stripped.startswith("/*")
        or stripped.startswith("*")
    )


def _strip_rust_inline_comment(line: str) -> str:
    """Drop a trailing ``//`` comment chunk so an in-code use path
    isn't masked by a doc reference on the same line."""
    idx = line.find("//")
    if idx >= 0:
        return line[:idx]
    return line


# ── Cargo.toml mini-parser ────────────────────────────────────────────


# Catches:
#   name = "value"          (table-value style)
#   name = { ... }          (inline-table dep — version+features etc.)
#   name.workspace = true   (workspace-managed)
# Anchored to the start of a line so we don't pick up commented-out forms.
_CARGO_DEP_LINE_RE = re.compile(r"^\s*([A-Za-z0-9_.\-]+)\s*=")
_CARGO_SECTION_RE = re.compile(r"^\s*\[([^\]]+)\]\s*$")


def _parse_cargo_deps(text: str) -> List[Tuple[int, str]]:
    """Return ``(line_number, dep_name)`` pairs for every entry inside a
    ``[dependencies]`` / ``[dev-dependencies]`` / ``[build-dependencies]``
    /  ``[target.*.dependencies]`` section.

    The dep name is the *crate* name as it appears on the left-hand side
    (so ``wylde-panel-chat.workspace = true`` and
    ``wylde-panel-chat = { path = "..." }`` both yield ``"wylde-panel-chat"``).
    """
    out: List[Tuple[int, str]] = []
    in_deps = False
    for lineno, raw in enumerate(text.splitlines(), start=1):
        line = raw.rstrip()
        sec = _CARGO_SECTION_RE.match(line)
        if sec:
            name = sec.group(1).strip()
            in_deps = (
                name == "dependencies"
                or name == "dev-dependencies"
                or name == "build-dependencies"
                or name.endswith(".dependencies")
                or name.endswith(".dev-dependencies")
            )
            continue
        if not in_deps:
            continue
        stripped = line.lstrip()
        if not stripped or stripped.startswith("#"):
            continue
        m = _CARGO_DEP_LINE_RE.match(line)
        if not m:
            continue
        dep_name = m.group(1).split(".", 1)[0]
        out.append((lineno, dep_name))
    return out


# ── Rule 33: no_cross_panel_imports ──────────────────────────────────


def check_no_cross_panel_imports() -> List[Finding]:
    """Each ``wylde-panel-*`` crate may only depend on the shared
    infrastructure crates (``wylde-theme`` / ``wylde-gui-pipe`` /
    ``wylde-gpui-input`` / ``wylde-panel-registry``).

    Any other ``wylde-panel-*`` dependency is a panel-to-panel import:
    the panel registry is the only legitimate place where one crate
    knows about another panel's existence.
    """
    out: List[Finding] = []
    allow = set(PANEL_SHARED_INFRA_CRATES)
    for cargo in _walk_panel_cargo_tomls():
        rel = _to_rel(cargo)
        text = _read_text(cargo)
        if not text:
            continue
        own_crate: Optional[str] = None
        m = re.search(r'^\s*name\s*=\s*["\']([^"\']+)["\']', text, re.MULTILINE)
        if m:
            own_crate = m.group(1)
        for lineno, dep in _parse_cargo_deps(text):
            if not dep.startswith("wylde-"):
                continue
            if dep == own_crate:
                # Self-references aren't legal in Cargo, but if one
                # shows up it isn't *this* rule's job to flag it.
                continue
            if dep in allow:
                continue
            if dep.startswith("wylde-panel-"):
                out.append(
                    Finding(
                        rule="no_cross_panel_imports",
                        severity="error",
                        file=rel,
                        line=lineno,
                        message=(
                            f"Panel crate depends on sibling panel crate "
                            f"{dep!r}.  Panels must not import each other; "
                            f"cross-panel state belongs in wylde-gui-pipe "
                            f"(nav_bus, shared types) or wylde-panel-registry."
                        ),
                        context=f"{dep} = ...",
                    )
                )
                continue
            # Any other wylde-* crate the panel reaches for is also a
            # boundary break — panels talk to the backend through the
            # pipe surface, not by linking the harness crate directly.
            out.append(
                Finding(
                    rule="no_cross_panel_imports",
                    severity="error",
                    file=rel,
                    line=lineno,
                    message=(
                        f"Panel crate depends on {dep!r}.  Allowed "
                        f"shared-infra crates: "
                        f"{', '.join(sorted(allow))}.  Backend access "
                        f"goes through wylde-gui-pipe, not direct "
                        f"crate dependency."
                    ),
                    context=f"{dep} = ...",
                )
            )
    return out


# ── Rule 34: no_legacy_gui_imports_in_panels ─────────────────────────


_TAURI_USE_RE = re.compile(r"\btauri\s*::")


def check_no_legacy_gui_imports_in_panels() -> List[Finding]:
    """No ``tauri::*`` use paths anywhere under
    ``Core/GUI/Frontend/Panels/**``.

    Panel crates are gpui-native — the legacy Tauri tree at
    ``Core/GUI/src-tauri/`` is built by its own workspace and deleted at
    cutover.  References from a panel crate would re-couple the two
    worlds.

    The Svelte half of this rule was RETIRED on 2026-07-20.  It matched
    ``.svelte`` / ``svelte::`` / ``"svelte"`` in panel sources, but the
    Svelte tree (``Core/GUI/src/``) was deleted at the slice-11 cutover
    and zero ``.svelte``/``.js``/``.ts`` files remain.  Its only surviving
    finding was a false positive on
    ``Core/GUI/Frontend/Panels/Workspaces/src/files/icon_map.rs``, where
    ``("svelte", &["svelte"])`` is a file-icon table row, not an import.
    """
    out: List[Finding] = []
    for path in _walk_gpui_rs((GPUI_PANELS_ROOT,)):
        rel = _to_rel(path)
        text = _read_text(path)
        if not text:
            continue
        for lineno, raw_line in enumerate(text.splitlines(), start=1):
            stripped = raw_line.lstrip()
            if _is_rust_doc_or_comment(stripped):
                continue
            line = _strip_rust_inline_comment(raw_line)
            if _TAURI_USE_RE.search(line):
                out.append(
                    Finding(
                        rule="no_legacy_gui_imports_in_panels",
                        severity="error",
                        file=rel,
                        line=lineno,
                        message=(
                            "Panel crate references `tauri::*`.  Panels "
                            "are gpui-native; the legacy Tauri tree at "
                            "Core/GUI/src-tauri/ is excluded from the "
                            "gpui workspace and removed at cutover."
                        ),
                        context=raw_line.strip()[:200],
                    )
                )
                continue
    return out


# ── Rule 35: webview_only_in_extension_handlers ──────────────────────


_WRY_USE_RE = re.compile(r"\bwry\s*::")


def check_webview_only_in_extension_handlers() -> List[Finding]:
    """`wry::*` and other webview-flavored direct imports are allowed
    only inside ``Core/GUI/Frontend/Extension_handlers/WebView/**``.

    Rationale: the WebView crate exists to host extension iframe panels.
    Pulling ``wry::`` into any first-party panel would let the panel
    embed a browser engine instead of rendering native gpui — exactly
    the architectural break this rule guards against.

    The Shell legitimately renders iframe slots, but only via the
    ``wylde-webview`` crate's gpui-friendly wrapper — Shell never reaches
    for ``wry::`` itself.
    """
    out: List[Finding] = []
    for path in _walk_gpui_rs((GPUI_WORKSPACE_ROOT,)):
        rel = _to_rel(path)
        if rel.startswith(GPUI_WEBVIEW_ROOT + "/"):
            continue
        text = _read_text(path)
        if not text:
            continue
        for lineno, raw_line in enumerate(text.splitlines(), start=1):
            stripped = raw_line.lstrip()
            if _is_rust_doc_or_comment(stripped):
                continue
            line = _strip_rust_inline_comment(raw_line)
            if not _WRY_USE_RE.search(line):
                continue
            out.append(
                Finding(
                    rule="webview_only_in_extension_handlers",
                    severity="error",
                    file=rel,
                    line=lineno,
                    message=(
                        "WebView is reserved for iframe-panel rendering "
                        "machinery; first-party panels must be native "
                        "gpui.  Route this through the `wylde-webview` "
                        "wrapper crate at "
                        f"{GPUI_WEBVIEW_ROOT}/ instead of importing "
                        "`wry` directly."
                    ),
                    context=raw_line.strip()[:200],
                )
            )
    return out


# ── Rule 36: first_party_manifest_must_be_gpui_view ──────────────────


def check_first_party_manifest_must_be_gpui_view() -> List[Finding]:
    """Every ``manifest.json`` under ``Core/GUI/Frontend/Panels/**`` must
    declare ``source.kind == "gpui_view"`` for every entry in its
    ``panels`` array.  ``iframe`` was the iframe-extension shape.

    NARROWED 2026-07-20: the rule used to carry a symmetric second half
    asserting that every ``Extensions/<X>/`` manifest's ``ui_panels``
    entry declared ``source.kind == "iframe"``.  ``Extensions/`` no
    longer exists, so that half walked nothing and could only ever
    report a pass — the dead-gate shape issue #101 called out.  It was
    removed along with its ``EXTENSIONS_ROOT`` walk.
    """
    return _check_first_party_panel_manifests()


def _check_first_party_panel_manifests() -> List[Finding]:
    """The original rule body — first-party panel manifests must use
    ``gpui_view`` kind."""
    out: List[Finding] = []
    for path in _walk_panel_manifests():
        rel = _to_rel(path)
        text = _read_text(path)
        if not text:
            continue
        try:
            data = json.loads(text)
        except (ValueError, TypeError) as exc:
            out.append(
                Finding(
                    rule="first_party_manifest_must_be_gpui_view",
                    severity="error",
                    file=rel,
                    line=0,
                    message=f"panel manifest is not valid JSON: {exc}",
                )
            )
            continue
        if not isinstance(data, dict):
            out.append(
                Finding(
                    rule="first_party_manifest_must_be_gpui_view",
                    severity="error",
                    file=rel,
                    line=0,
                    message="panel manifest must be a JSON object",
                )
            )
            continue
        panels = data.get("panels")
        if not isinstance(panels, list) or not panels:
            out.append(
                Finding(
                    rule="first_party_manifest_must_be_gpui_view",
                    severity="error",
                    file=rel,
                    line=0,
                    message="panel manifest has no `panels` array",
                )
            )
            continue
        for idx, panel in enumerate(panels):
            if not isinstance(panel, dict):
                out.append(
                    Finding(
                        rule="first_party_manifest_must_be_gpui_view",
                        severity="error",
                        file=rel,
                        line=0,
                        message=f"panels[{idx}] is not an object",
                    )
                )
                continue
            source = panel.get("source")
            if not isinstance(source, dict):
                pid = panel.get("id", f"#{idx}")
                out.append(
                    Finding(
                        rule="first_party_manifest_must_be_gpui_view",
                        severity="error",
                        file=rel,
                        line=0,
                        message=(
                            f"panel {pid!r} has no `source` object; "
                            f"first-party panels must declare "
                            f'`source.kind: "gpui_view"`'
                        ),
                    )
                )
                continue
            kind = source.get("kind")
            if kind != "gpui_view":
                pid = panel.get("id", f"#{idx}")
                out.append(
                    Finding(
                        rule="first_party_manifest_must_be_gpui_view",
                        severity="error",
                        file=rel,
                        line=0,
                        message=(
                            f"panel {pid!r} has source.kind = {kind!r}; "
                            f'first-party panels must be "gpui_view" '
                            f'("iframe" is reserved for Extensions/**)'
                        ),
                    )
                )
    return out


# Rule 37 (panel_crate_must_be_workspace_member) carved out to
# :mod:`_gpui_workspace` when this file crossed the flat 700-LOC cap.
