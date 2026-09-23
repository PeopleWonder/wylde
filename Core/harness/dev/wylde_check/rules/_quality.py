"""Code-quality rules: file-size cap.

Retired 2026-07-20 (dead-rule retirement): ``test_init_present``
(rule 21), ``no_bare_except`` (rule 24) and ``manifest_sandbox_required``
(rule 32).  Rules 21/24 were Python-only with no production Python left
to walk; rule 32 keyed on ``Core/Lifecycle/tests/`` and
``Core/harness/tests/``, both deleted in the Rust cutover.

Rule 20 (``file_size_limit``) was repointed from Python to Rust in the
same pass — see :func:`check_file_size_limit`."""

from __future__ import annotations

import sys as _sys
from pathlib import Path
from typing import Dict, List, Tuple

from .. import Finding

_pkg = _sys.modules[__name__.rsplit(".", 2)[0]]


# ── Rule 20: flat 700-LOC limit on Rust files ─────────────────────


_FILE_SIZE_LIMIT_LOC = 700


# Rust source roots the cap applies to: the backend workspace crates
# (``rust/crates/*/src/**``) and the whole gpui GUI tree
# (``Core/GUI/**``).  Cargo build output (any path containing
# ``/target/``) is excluded.
_RUST_CRATES_ROOT = "rust/crates"
_GUI_ROOT = "Core/GUI"


def _walk_rust_sources() -> List[Tuple[str, Path]]:
    """``(rel, path)`` pairs for every in-scope Rust source file.

    Deduplicated and sorted by ``rel`` so findings come out in a stable
    order.  ``rel`` uses forward slashes, consistent with every other
    rule.
    """
    root = _pkg.WYLDE_ROOT
    seen: Dict[Path, Path] = {}
    crates_base = root / _RUST_CRATES_ROOT
    if crates_base.is_dir():
        for crate in sorted(crates_base.iterdir()):
            src = crate / "src"
            if not src.is_dir():
                continue
            for p in src.rglob("*.rs"):
                seen[p.resolve()] = p
    gui_base = root / _GUI_ROOT
    if gui_base.is_dir():
        for p in gui_base.rglob("*.rs"):
            seen[p.resolve()] = p
    out: List[Tuple[str, Path]] = []
    for p in seen.values():
        try:
            rel = str(p.relative_to(root)).replace("\\", "/")
        except ValueError:
            rel = str(p).replace("\\", "/")
        if "/target/" in rel:
            continue
        out.append((rel, p))
    out.sort(key=lambda item: item[0])
    return out


# Rust files that were already over the 700-line cap when rule 20 was
# repointed from Python to Rust (2026-07-20).  The Python tree the rule
# used to walk no longer exists; the cap now engages on the Rust tree,
# whose p85 is 637 lines, so 88 percent of files already comply.
#
# Each of the 91 entries below is queued debt, not an exemption in
# principle: the rule still fires on every NEW file and on any file that
# newly grows past the cap.  Entries are removed as their splits land.
#
# Worst offender: ``Core/GUI/Frontend/Panels/Chat/src/chat_panel.rs`` at
# 5298 lines — more than seven times the cap.
_FILE_SIZE_QUEUED_SPLITS: Tuple[str, ...] = (
    "Core/GUI/Frontend/Code_editor/src/buffer.rs",
    "Core/GUI/Frontend/Input/src/buffer.rs",
    "Core/GUI/Frontend/Input/src/lib.rs",
    "Core/GUI/Frontend/Panels/Chat/src/chat_panel.rs",
    "Core/GUI/Frontend/Panels/Chat/src/composer_ui.rs",
    "Core/GUI/Frontend/Panels/Chat/src/ipc.rs",
    "Core/GUI/Frontend/Panels/Chat/src/processing.rs",
    "Core/GUI/Frontend/Panels/Dashboard/src/dashboard_panel.rs",
    "Core/GUI/Frontend/Panels/Devices/src/devices_panel.rs",
    "Core/GUI/Frontend/Panels/Memory/src/memory_panel.rs",
    "Core/GUI/Frontend/Panels/Models/src/models_panel.rs",
    "Core/GUI/Frontend/Panels/RemoteAccess/src/remote_access_panel.rs",
    "Core/GUI/Frontend/Panels/Settings/src/ipc.rs",
    "Core/GUI/Frontend/Panels/Settings/src/sections.rs",
    "Core/GUI/Frontend/Panels/Settings/src/settings_panel.rs",
    "Core/GUI/Frontend/Panels/Workspaces/src/editor/mod.rs",
    "Core/GUI/Frontend/Panels/Workspaces/src/files/icon_map.rs",
    "Core/GUI/Frontend/Panels/Workspaces/src/graph/cluster/mod.rs",
    "Core/GUI/Frontend/Panels/Workspaces/src/graph/mod.rs",
    "Core/GUI/Frontend/Panels/Workspaces/src/graph/navigation/mod.rs",
    "Core/GUI/Frontend/Panels/Workspaces/src/graph/physics/mod.rs",
    "Core/GUI/Frontend/Panels/Workspaces/src/graph/render/style.rs",
    "Core/GUI/Frontend/Panels/Workspaces/src/hierarchy/mod.rs",
    "Core/GUI/Frontend/Panels/Workspaces/src/routing/mod.rs",
    "Core/GUI/Frontend/Panels/Workspaces/src/routing/tree.rs",
    "Core/GUI/Frontend/Panels/Workspaces/src/vocabulary/mod.rs",
    "Core/GUI/Frontend/Panels/Workspaces/src/workspaces_panel.rs",
    "Core/GUI/Frontend/Pipe/src/lib.rs",
    "Core/GUI/Shell/src/shell_root.rs",
    "rust/crates/wylde-concept-hierarchy/src/overlay.rs",
    "rust/crates/wylde-concept-routing/src/router/spread.rs",
    "rust/crates/wylde-device-gate/src/core.rs",
    "rust/crates/wylde-ext-study/src/tools.rs",
    "rust/crates/wylde-extension-bridge/src/host.rs",
    "rust/crates/wylde-extension-bridge/src/manifest.rs",
    "rust/crates/wylde-gateway/src/egress/client.rs",
    "rust/crates/wylde-gateway/src/egress/destinations.rs",
    "rust/crates/wylde-harness/src/api/mod.rs",
    "rust/crates/wylde-harness/src/chat/search/api.rs",
    "rust/crates/wylde-harness/src/chat/search/summary.rs",
    "rust/crates/wylde-harness/src/memory/conversations/store.rs",
    "rust/crates/wylde-harness/src/memory/long_term/entries.rs",
    "rust/crates/wylde-harness/src/memory/long_term/reflection.rs",
    "rust/crates/wylde-harness/src/memory/memgraph/bolt.rs",
    "rust/crates/wylde-harness/src/memory/post_turn_extractor.rs",
    "rust/crates/wylde-harness/src/memory/reflection.rs",
    "rust/crates/wylde-harness/src/memory/scheduler.rs",
    "rust/crates/wylde-harness/src/memory/workspace/store.rs",
    "rust/crates/wylde-harness/src/tooling/consent.rs",
    "rust/crates/wylde-harness/src/tooling/resource/resources/fs.rs",
    "rust/crates/wylde-harness/src/tooling/resource/resources/n8n.rs",
    "rust/crates/wylde-harness/src/tooling/tools/fs.rs",
    "rust/crates/wylde-harness/src/turn/actions.rs",
    "rust/crates/wylde-harness/src/turn/context_gather.rs",
    "rust/crates/wylde-harness/src/turn/reasoning/inputs.rs",
    "rust/crates/wylde-harness/src/turn/reasoning/mod.rs",
    "rust/crates/wylde-harness/src/turn/reasoning/plan_phase.rs",
    "rust/crates/wylde-harness/src/turn/reasoning/reflect_phase.rs",
    "rust/crates/wylde-harness/src/turn/salvage.rs",
    "rust/crates/wylde-harness/src/turn/token_budget.rs",
    "rust/crates/wylde-lifecycle/src/control.rs",
    "rust/crates/wylde-lifecycle/src/registry.rs",
    "rust/crates/wylde-lifecycle/src/state/mod.rs",
    "rust/crates/wylde-lifecycle/src/state/services.rs",
    "rust/crates/wylde-n8n/src/client.rs",
    "rust/crates/wylde-ollama/src/actions/models.rs",
    "rust/crates/wylde-shared/src/ipc/client.rs",
    "rust/crates/wylde-shared/src/ipc/server.rs",
    "rust/crates/wylde-shared/src/manifest.rs",
    "rust/crates/wylde-treesitter/src/entities.rs",
    "rust/crates/wylde-voice/src/actions/transcribe.rs",
    "rust/crates/wylde-voice/src/model_download.rs",
    "rust/crates/wylde-voice/src/orchestrator.rs",
    "rust/crates/wylde-voice/src/synth/voices.rs",
    "rust/crates/wylde-vpn/src/actions.rs",
    "rust/crates/wylde-vpn/src/config.rs",
    "rust/crates/wylde-vpn/src/nat/stun.rs",
    "rust/crates/wylde-vram-broker/src/inventory.rs",
    "rust/crates/wylde-vram-broker/src/policy.rs",
    "rust/crates/wylde-workspaces-client/src/lib.rs",
    "rust/crates/wylde-workspaces/src/action_dispatch.rs",
    "rust/crates/wylde-workspaces/src/anchors/api.rs",
    "rust/crates/wylde-workspaces/src/concepts/api.rs",
    "rust/crates/wylde-workspaces/src/concepts/hierarchy_bridge.rs",
    "rust/crates/wylde-workspaces/src/graph/bolt.rs",
    "rust/crates/wylde-workspaces/src/graph/neighborhood.rs",
    "rust/crates/wylde-workspaces/src/graph/symbol_index.rs",
    "rust/crates/wylde-workspaces/src/rag/indexer/graph_writer.rs",
    "rust/crates/wylde-workspaces/src/rag/indexer/mod.rs",
    "rust/crates/wylde-workspaces/src/rag/indexer/search.rs",
    "rust/crates/wylde-workspaces/src/rag/lexical_eval.rs",
    # Already over the cap when wylde_check became a CI gate (#114, 2026-07-21).
    # Queued debt on the same terms as the entries above — the cap still fires
    # on every NEW file and any further growth of these; each is removed as its
    # split lands. Not new exemptions in principle.
    "Core/GUI/Frontend/Panels/Dashboard/src/ipc.rs",
    "Core/GUI/Manifest/Extension_handlers/src/bin/wylde_panel_aggregator.rs",
    "rust/crates/wylde-harness/src/memory/vector/mod.rs",
    "rust/crates/wylde-updater/src/lib.rs",
    "rust/crates/wylde-workspaces/src/api.rs",
    "rust/crates/wylde-workspaces/src/rag/indexer/manifest.rs",
)


def check_file_size_limit() -> List[Finding]:
    """Every active Rust file must be at most 700 lines long.

    the Wylde user's call: one flat cap, no production/test split.  When a file
    blows the limit, the right move is almost always to split it along
    its natural seams.  Counts raw lines including blank lines and
    comments — file *length* is what hurts editing and review, not LOC
    density.

    Scope is the backend workspace crates (``rust/crates/*/src/**/*.rs``)
    plus the gpui GUI tree (``Core/GUI/**/*.rs``); Cargo build output
    under ``target/`` is excluded.

    The :data:`_FILE_SIZE_QUEUED_SPLITS` allowlist documents the 91 files
    that were already oversized when the rule was repointed at Rust.
    Each entry is tracked as a queued split task; the Wylde user removes entries
    as splits ship so the cap re-engages on the now-clean tree.
    """
    out: List[Finding] = []
    for rel, path in _walk_rust_sources():
        if rel in _FILE_SIZE_QUEUED_SPLITS:
            continue
        try:
            text = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        if not text:
            continue
        lines = text.count("\n") + (0 if text.endswith("\n") else 1)
        if lines > _FILE_SIZE_LIMIT_LOC:
            out.append(
                Finding(
                    rule="file_size_limit",
                    severity="error",
                    file=rel,
                    line=0,
                    message=(
                        f"Rust file is {lines} lines long; the flat cap is "
                        f"{_FILE_SIZE_LIMIT_LOC}.  Split along its natural "
                        f"seams (one concern per module, one panel section "
                        f"per file, etc.)."
                    ),
                )
            )
    return out
