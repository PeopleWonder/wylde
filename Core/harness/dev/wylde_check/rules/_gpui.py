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
    "wylde-gui-controls",
    "wylde-gpui-input",
    "wylde-panel-registry",
)

# Additional shared crates a panel may depend on: gpui WIDGET crates and
# pure TOPOLOGY/type libs that are not themselves panels and carry no
# backend pipe an importer would be bypassing. Kept separate from the core
# infra list above (which the finding message quotes) so the reason for
# each is explicit.
PANEL_EXTRA_ALLOWED_CRATES: Tuple[str, ...] = (
    "wylde-gui-test-support",  # shared test harness — dev-dependency only, not shipped
    "wylde-stack",  # roster + service_name topology lib (Dashboard service strip)
    "wylde-updater",  # updater types/lib for the Settings update UI (a lib, not a service)
    "wylde-gpui-code-editor",  # shared gpui code-editor widget (Workspaces IDE)
    "wylde-anchor-actions",  # shared anchor action definitions (Chat / Workspaces)
)

# Per-edge panel→panel carve-outs: `(owning_panel_crate, depended_panel)`
# pairs that are a DELIBERATE, documented composition rather than accidental
# coupling. NOTE for the maintainer: Workspaces mounts the SHARED singleton
# ChatPanel's `InferenceBarDock` at its base (workspaces_panel.rs) — a real
# panel→panel dependency. If that dock should live in a shared crate instead,
# that is a separate refactor; this carve-out names the coupling that exists
# today rather than hiding it behind the generic allowlist.
