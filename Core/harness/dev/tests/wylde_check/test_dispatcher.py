"""Tests for the top-level run_all dispatcher — envelope shape, rule
count, and selective execution.
"""

from __future__ import annotations

from typing import Any

from .conftest import _write


def test_run_all_envelope_shape(isolated_tree: Any) -> None:
    wc, _ = isolated_tree
    result = wc.run_all()
    assert result["ok"] is True
    data = result["data"]
    assert "rules_checked" in data
    assert "findings" in data
    assert "summary" in data
    s = data["summary"]
    assert "by_rule" in s
    assert "by_severity" in s
    assert set(s["by_severity"].keys()) == {"error", "warning", "info"}
    assert s["total"] == len(data["findings"])


def test_run_all_covers_every_registered_rule(isolated_tree: Any) -> None:
    """Every rule in ``_RULES`` runs, and the roster is exactly the set
    named below.

    The literal-set assertion is the point: it catches a rule being
    added without a name, silently renamed, or — the #116 failure mode —
    quietly dropped from the dispatcher.  A rule that never runs reports
    no findings, which is indistinguishable from a clean pass.
    """
    wc, _ = isolated_tree
    result = wc.run_all()
    assert result["data"]["rules_checked"] == len(wc._RULES)
    expected = {
        # rules 1/2/3/4/5 retired 2026-07-20; rules 7/9/11 retired at the
        # slice-11 cutover.
        "dead_service_refs",
        # rule 8 (gateway_scope) retired 2026-07-20 (Python Gateway gone).
        "gui_no_backend_bypass",
        # rules 12/13/14/15/16/18/19 retired 2026-07-20.
        "pipe_name_convention",
        "shutdown_reaps_manifest_orphans",
        "file_size_limit",
        # rules 21/22/23/24 retired 2026-07-20; rule 30 at slice-11.
        "service_owns_its_state",
        "import_paths_rust",
        "no_silent_error_swallow_rust",
        "logging_setup_only_rust",
        "no_external_process_spawn_rust",
        # rule 32 (manifest_sandbox_required) retired 2026-07-20.
        "no_cross_panel_imports",
        "no_legacy_gui_imports_in_panels",
        "webview_only_in_extension_handlers",
        "first_party_manifest_must_be_gpui_view",
        "panel_crate_must_be_workspace_member",
        "panel_verbs_exist_in_harness_registry",
        "nav_targets_exist",
        "required_services_includes_called_services",
        # rule 41 (rest_routes_exist_in_service) retired 2026-07-20.
        "manifest_factory_resolves",
        "stream_call_must_handle_cancel",
        # Rules 44-45 — slice-11 cutover (rules 46/47 retired 2026-07-20).
        "launcher_enumerates_services_from_manifests",
        "shutdown_enumerates_services_from_manifests",
        # Rule 48 — codebase-audit slice (2026-05-30).  Rule 49
        # (no_python_gateway_imports) retired 2026-07-20.
        "gateway_verbs_exist_in_harness_registry",
        "no_bare_tokio_in_panel_src",
        "no_panic_in_panel_render",
        "silent_skip_in_service_start",
        # Rule 53 — hardcoded LLM system-prompt literals in Rust source
        # (prompt-engineering B11 slice, 2026-06-11).
        "no_hardcoded_prompts_rust",
        # Rule 54 — unbounded log sinks (0.2 Stability audit finding C,
        # #98, 2026-07-18).
        "no_unbounded_log_sink_rust",
        # Rule 51 — every rule's configured target path must exist.  The
        # generalization of #101 and #116: a rule pointed at a deleted
        # file goes quiet, not red, so the next such deletion has to be
        # caught by the engine checking itself (#116).
        "rule_targets_exist",
        # Rule 55 — personal identifiers in a public repo (scrub-drift
        # slice, 2026-07-19): real home-directory paths, and the
        # maintainer's name matched as salted digests.
        "no_personal_identifiers",
        # Rule 56 — multi-test bolt:// binaries must serialize on a DB_LOCK
        # and be run in the live-graph CI leg (0.2 Stability, #226). Makes
        # the #83 self-collision class a structural gate.
        "graph_test_serialized_on_db_lock",
        # Rule 57 — a surface that renders a per-item endpoint must carry and
        # render its availability (0.2 Stability, #239). Rule 40 gates a panel
        # against its services; the unit that can be dead is the item.
        "service_backed_surface_declares_availability",
        # Rule 58 — every GUI chat entry point is driven by the all-surfaces
        # chat-turn e2e (#236). Chat is the primary path and has more than one
        # entry point; a new surface must not ship covered by nothing.
        "chat_surfaces_are_e2e_covered",
        # Rule 59 — every interactive GUI control is wired and walkable
        # (#247). Panel-walk proves a panel LOADS; nothing proved a control
        # in it DOES anything. WARNING for the pilot, ERROR after migration.
        "gui_controls_are_wired_and_walkable",
        # Rule 60 — a unit test touching a process-global broadcast bus owns
        # its channel or serializes on a test-module guard (#246). The
        # `src/`-unit-test half of rule 56's #83 class, with no
        # minimum-count carve-out: #246's colliders never named the bus.
        "global_bus_test_isolation",
        # Rule 61 — rule 59's companion and the other half of #247: a GUI crate
        # that builds controls must have a control_walk declaring every one of
        # its control-building sources. Landed with the deletion of rule 59's
        # (now drained) grandfather ratchet, once every panel was walked.
        "every_control_building_crate_is_walked",
    }
    assert set(result["data"]["summary"]["by_rule"].keys()) == expected


def test_run_all_selects_only_named_rules(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(
        root / "Core" / "harness" / "mod.py",
        "SVC = 'wylde-orchestrator'  # dead reference\n",  # wylde-check: dead-ref-ok
    )
    result = wc.run_all(only=["dead_service_refs"])
    assert result["data"]["rules_checked"] == 1
    # All findings should be from the one selected rule.
    assert all(f["rule"] == "dead_service_refs" for f in result["data"]["findings"])


def test_run_all_executes_every_registered_rule(isolated_tree: Any) -> None:
    """The rule count is pinned to an explicit literal on purpose.

    ``len(_RULES)`` alone would happily follow a rule being deleted.
    Pinning the number means removing a rule is a deliberate edit here
    too — the same "drift must be noticed, not absorbed" principle that
    #116 was about.  Bump this when a rule is genuinely added or retired.
    """
    wc, _root = isolated_tree
    assert len(wc._RULES) == 36
    result = wc.run_all()
    assert result["ok"] is True
    assert result["data"]["rules_checked"] == len(wc._RULES)
