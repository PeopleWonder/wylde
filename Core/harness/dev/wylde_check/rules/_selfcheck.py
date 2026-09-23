"""Rule 51: every rule's configured input corpus must be non-empty.

The failure this exists to prevent
----------------------------------

A rule that points at a deleted file does not go red — it goes *quiet*.
Its walker finds nothing, its loop runs zero times, and it reports a
clean pass while checking nothing at all.  The tree looks greener the
more of the rule engine rots.

That has now happened repeatedly.  Rules 44/45 pointed at
``Core/Lifecycle/launcher.py`` and ``Core/Lifecycle/shutdown.py`` after
the Rust cutover deleted them, and passed green for months (issue #101).
Rule 48 pointed at ``rust/crates/wylde-harness/src/pipe.rs`` and
``Core/harness/pipe/__init__.py`` — the first renamed to ``pipe/mod.rs``,
the second deleted outright — leaving 46 Gateway verbs unchecked
(issue #116).  Then the whole Python-linter class rotted at once: ~20
rules kept walking ``ACTIVE_ROOTS`` for ``*.py`` after the last
production ``.py`` was ported to Rust, so every walk matched nothing and
every rule "passed" — found by a hand audit, not by this gate (#114).

Why the previous version of this rule missed that
--------------------------------------------------

The prior implementation asserted *path existence* over six hand-typed
paths carried forward from the #101 and #116 post-mortems.  Two holes,
both fatal to the Python-linter class:

* **Existence, not cardinality.**  ``Core/GUI/Frontend/Panels`` existing
  tells you nothing about whether a walk under it matched 121 files or
  zero.  A rule whose target *directory* survives but whose file class
  was emptied — exactly the Python situation, where ``Core/`` is full of
  Rust but holds no walkable ``.py`` — passed this check trivially.
* **Six entries, not the whole suite.**  A rule whose target died for a
  reason nobody had already been burned by was invisible: it was never
  in the table.

The fix is this file: assert **matched-file count > 0** for every
surviving rule's input root, not path existence — so a corpus that
collapses to zero files goes red on the PR that empties it, whatever the
reason and whichever rule it disarms.

Why a rule rather than an import-time assertion
-----------------------------------------------

An import-time check would fire inside the unit suite, where
``WYLDE_ROOT`` is monkeypatched to a synthetic ``tmp_path`` that
deliberately contains almost none of these files.  Every test would
explode.  As a rule it runs against whatever root is bound at call time
and is skipped by tests that don't select it.
"""

from __future__ import annotations

import sys as _sys
from typing import List, Optional, Tuple

from .. import Finding
from .._walkers import _walk

_pkg = _sys.modules[__name__.rsplit(".", 2)[0]]


# One spec per rule input corpus that would silently disarm a rule if it
# emptied out.  Each is ``(root, extensions, owner)``:
#
# * ``extensions is None`` — ``root`` is a specific file (or directory) a
#   rule reads *wholesale*; existence is its cardinality, so it must
#   simply be present.  (An empty registry inside a present file is a
#   different failure the owning rule catches itself, hard.)
# * ``extensions`` is a tuple of suffixes — ``root`` is a *walk root*; at
#   least one file with one of those suffixes must exist beneath it
#   (EXCLUDED_DIRS like ``target/`` and ``__pycache__/`` are skipped, via
#   ``_walk``).  This is the cardinality check the previous existence
#   check lacked.
#
# Coverage is the whole surviving suite's input families, not a handful
# of incident paths — that breadth is the point.  Rules that walk the
# entire repo across many extensions (``dead_service_refs``,
# ``no_personal_identifiers``, ``pipe_name_convention``) are omitted on
# purpose: their corpus cannot collapse while any source file of any kind
# survives, so a cardinality gate on them would only ever fire on an
# empty checkout.
_TargetSpec = Tuple[str, Optional[Tuple[str, ...]], str]

RULE_TARGET_SPECS: Tuple[_TargetSpec, ...] = (
    # ── specific files read wholesale (existence == cardinality) ──
    (
        "rust/crates/wylde-lifecycle/src/daemon.rs",
        None,
        "shutdown_reaps_manifest_orphans, launcher_enumerates_services_from_manifests",
    ),
    (
        "rust/crates/wylde-lifecycle/src/state/orphan_sweep.rs",
        None,
        "shutdown_reaps_manifest_orphans",
    ),
    (
        "rust/crates/wylde-lifecycle/src/state/mod.rs",
        None,
        "shutdown_enumerates_services_from_manifests",
    ),
    (
        "rust/crates/wylde-lifecycle/src/state/services.rs",
        None,
        "silent_skip_in_service_start",
    ),
    (
        "rust/crates/wylde-lifecycle/src/daemon_managed.rs",
        None,
        "launcher_enumerates_services_from_manifests, "
        "shutdown_enumerates_services_from_manifests",
    ),
    (
        "rust/crates/wylde-harness/src/pipe/mod.rs",
        None,
        "panel_verbs_exist_in_harness_registry, "
        "gateway_verbs_exist_in_harness_registry",
    ),
    (
        "Core/GUI/Cargo.toml",
        None,
        "panel_crate_must_be_workspace_member",
    ),
    (
        ".github/workflows/ci.yml",
        None,
        "graph_test_serialized_on_db_lock",
    ),
    (
        "Core/GUI/Frontend/Panels/Chat/src/chat_panel.rs",
        None,
        "chat_surfaces_are_e2e_covered",
    ),
    (
        "Core/GUI/Frontend/Panels/Chat/tests/chat_turn_e2e.rs",
        None,
        "chat_surfaces_are_e2e_covered",
    ),
    # ── walk roots (cardinality: at least one matching file) ──
    (
        "rust/crates",
        (".rs",),
        "import_paths_rust, no_silent_error_swallow_rust, "
        "logging_setup_only_rust, no_external_process_spawn_rust, "
        "no_hardcoded_prompts_rust, no_unbounded_log_sink_rust, "
        "service_owns_its_state, file_size_limit, "
        "graph_test_serialized_on_db_lock",
    ),
    (
        "rust/crates/wylde-gateway/src",
        (".rs",),
        "gateway_verbs_exist_in_harness_registry",
    ),
    (
        "Core/GUI",
        (".rs",),
        "gui_no_backend_bypass, webview_only_in_extension_handlers, "
        "nav_targets_exist, file_size_limit",
    ),
    (
        # Rule 59's corpus, listed separately from "Core/GUI" above because
        # it walks the Shell as well as the Frontend — the Shell owns the nav
        # chrome, and a control gate blind to it would be half a gate.
        "Core/GUI/Shell/src",
        (".rs",),
        "gui_controls_are_wired_and_walkable",
    ),
    (
        "Core/GUI/Frontend/Panels",
        (".rs",),
        "no_legacy_gui_imports_in_panels, no_bare_tokio_in_panel_src, "
        "no_panic_in_panel_render, stream_call_must_handle_cancel",
    ),
    (
        "Core/GUI/Frontend/Panels",
        (".json",),
        "first_party_manifest_must_be_gpui_view, "
        "required_services_includes_called_services, manifest_factory_resolves, "
        "service_backed_surface_declares_availability",
    ),
    (
        # The producer half of rule 57's corpus. Wholesale, because exactly
        # one file in `rust/` models a GUI-rendered remote surface — if it
        # moves, rule 57 silently stops policing the side that mints the
        # availability verdict.
        "rust/crates/wylde-extension-bridge/src/host.rs",
        None,
        "service_backed_surface_declares_availability",
    ),
    (
        "Core/GUI/Frontend/Panels",
        (".toml",),
        "no_cross_panel_imports, panel_crate_must_be_workspace_member",
    ),
)


def _corpus_present(root_rel: str, exts: Optional[Tuple[str, ...]]) -> bool:
    """True iff the rule's input corpus at ``root_rel`` is non-empty.

    For a wholesale target (``exts is None``) that means the path exists.
    For a walk root it means ``_walk`` finds ≥1 file with one of ``exts``
    beneath it — the cardinality check.
    """
    if exts is None:
        return (_pkg.WYLDE_ROOT / root_rel).exists()
    return len(_walk(exts, roots=(root_rel,))) > 0


def check_rule_targets_exist() -> List[Finding]:
    """Every rule's input corpus in :data:`RULE_TARGET_SPECS` must be
    non-empty.

    Fires an ``error`` per collapsed corpus, naming the rule(s) it just
    disarmed.  This is the generalization of the #101, #116 and #114
    fixes: the next time a refactor deletes a file — or empties out the
    last file of a class a rule walks for — the PR that does it goes red,
    rather than the rule going quiet.

    If a corpus is *intentionally* emptied, the fix is to repoint or
    retire the owning rule and update this table in the same commit —
    precisely the step every prior incident skipped.
    """
    out: List[Finding] = []
    for root_rel, exts, owner in RULE_TARGET_SPECS:
        if _corpus_present(root_rel, exts):
            continue
        if exts is None:
            what = f"Rule target {root_rel!r} does not exist"
        else:
            what = (
                f"Rule walk root {root_rel!r} contains no "
                f"{'/'.join(exts)} files"
            )
        out.append(
            Finding(
                rule="rule_targets_exist",
                severity="error",
                file=root_rel,
                line=0,
                message=(
                    f"{what}, so {owner} can no longer do its job — and a "
                    f"rule with an empty input corpus reports a pass, not a "
                    f"failure.  Repoint or retire that rule and update "
                    f"RULE_TARGET_SPECS in wylde_check/rules/_selfcheck.py "
                    f"in the same change."
                ),
            )
        )
    return out
