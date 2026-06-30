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


def test_run_all_covers_forty_five_rules(isolated_tree: Any) -> None:
    wc, _ = isolated_tree
    result = wc.run_all()
    assert result["data"]["rules_checked"] == 49
    expected = {
        "no_internal_http",
        "manifest_paths",
        "tool_id_regex",
        "action_registry",
        "import_paths",
        "dead_service_refs",
        # rules 7, 9, 11 (inferencebar_purity / gui_action_contract /
        # gui_pipe_constants) retired at the slice-11 cutover.
        "gateway_scope",
        "gui_no_backend_bypass",
        "tool_docstring_required",
        "logging_setup_only",
        "no_external_subprocess",
        "spawn_paths_exist",
        "run_py_entry_point",
        "pipe_name_convention",
        "run_py_startup_sequence",
        "shutdown_handler_marks_stopped",
        "shutdown_reaps_manifest_orphans",
        "file_size_limit",
        "test_init_present",
        "memory_layer_boundaries",
        "action_docstring_required",
        "no_bare_except",
        "service_owns_its_state",
        "import_paths_rust",
        "no_silent_error_swallow_rust",
        "logging_setup_only_rust",
        "no_external_process_spawn_rust",
        # rule 30 (gui_error_reporting) retired at the slice-11 cutover.
        "manifest_sandbox_required",
        "no_cross_panel_imports",
        "no_legacy_gui_imports_in_panels",
        "webview_only_in_extension_handlers",
        "first_party_manifest_must_be_gpui_view",
        "panel_crate_must_be_workspace_member",
        "panel_verbs_exist_in_harness_registry",
        "nav_targets_exist",
        "required_services_includes_called_services",
        "rest_routes_exist_in_service",
        "manifest_factory_resolves",
        "stream_call_must_handle_cancel",
        # Rules 44-47 — slice-11 cutover.
        "launcher_enumerates_services_from_manifests",
        "shutdown_enumerates_services_from_manifests",
        "every_service_has_manifest",
        "service_manifest_schema",
        # Rule 48 — codebase-audit slice (2026-05-30).
        "gateway_verbs_exist_in_harness_registry",
        "no_python_gateway_imports",
        "no_bare_tokio_in_panel_src",
        "no_panic_in_panel_render",
        "silent_skip_in_service_start",
        # Rule 53 — hardcoded LLM system-prompt literals in Rust source
        # (prompt-engineering B11 slice, 2026-06-11).
        "no_hardcoded_prompts_rust",
    }
    assert set(result["data"]["summary"]["by_rule"].keys()) == expected


def test_run_all_selects_only_named_rules(isolated_tree: Any) -> None:
    wc, root = isolated_tree
    _write(root / "Core" / "harness" / "mod.py", "from Wylde.Core.shared import ipc\n")
    result = wc.run_all(only=["import_paths"])
    assert result["data"]["rules_checked"] == 1
    # All findings should be from the one selected rule.
    assert all(f["rule"] == "import_paths" for f in result["data"]["findings"])


def test_run_all_executes_forty_five_rules(isolated_tree: Any) -> None:
    wc, _root = isolated_tree
    assert len(wc._RULES) == 49
    result = wc.run_all()
    assert result["ok"] is True
    assert result["data"]["rules_checked"] == 49
