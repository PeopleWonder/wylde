"""Phase-6 smoke test for harness/tooling.

Exercises the in-process tool_registry + tool_runner against the real
filesystem catalog. Two scenarios:

1. ``test_catalog_includes_phase6_tools`` — confirms the registry sees every
   tool we pulled forward and that each manifest carries the required keys.
2. ``test_runner_dispatches_tool_search`` / ``test_runner_dispatches_list_files``
   — round-trip dispatch through the runner, verifying the envelope shape
   and that retries don't fire on success.

Run with: ``pytest Core/harness/tooling/tests/test_smoke.py``

The tests assume the project root is on ``sys.path`` such that
``Wylde.Core.harness.tooling`` (or ``Core.harness.tooling`` — the runner
adapts to either) imports cleanly.
"""

from __future__ import annotations

import importlib
from pathlib import Path
from typing import Any

import pytest


_TOOLING_PKG = None


def _tooling() -> Any:
    """Resolve the tooling package without hard-coding a project root."""
    global _TOOLING_PKG
    if _TOOLING_PKG is not None:
        return _TOOLING_PKG
    for candidate in ("Wylde.Core.harness.tooling", "Core.harness.tooling"):
        try:
            _TOOLING_PKG = importlib.import_module(candidate)
            return _TOOLING_PKG
        except ImportError:
            continue
    pytest.skip("harness.tooling not importable from this sys.path")


# Tools we expect to find in the catalog — the Phase-6 ported set, plus the
# two pre-existing meta tools and the rag/ + visual/ groups added later in
# Phase 6, plus the Phase-8 n8n group. The n8n tools live under
# ``Wylde/N8N/tools/`` (service-owned) rather than under the harness
# ``tools/`` tree; the registry walker unions both locations into one
# catalog, so this test is location-agnostic — it just asserts that every
# expected id is present, regardless of which tree contributed it.
# Update when you add a new manifest anywhere in the catalog.
EXPECTED_TOOLS = {
    # meta
    "tool_search",
    "graph_query",
    # code
    "execute_python",
    "execute_bash",
    # fs
    "read_file",
    "write_file",
    "edit_file",
    "list_files",
    # git
    "git_status",
    "git_diff",
    "git_log",
    "git_add",
    "git_commit",
    "git_branch",
    "git_stash",
    # search
    "code_search",
    "code_search_files",
    # diff
    "show_diff",
    "apply_patch",
    # test
    "run_tests",
    "run_test_file",
    # ollama
    "preload_model",
    "evict_model",
    "list_loaded_models",
    "auto_evict_lru",
    # rag (8 — pulled forward from _legacy/core/wylde-rag/tools/)
    "rag_ask",
    "rag_index",
    "rag_reindex",
    "rag_feedback",
    "rag_misses",
    "rag_chunk_usage",
    "rag_graph_stats",
    "rag_prune",
    # visual (15 — split from _legacy/core/tool-runner/tools/visual_interact.py)
    "screenshot",
    "click",
    "type_text",
    "hotkey",
    "mouse_move",
    "scroll",
    "get_screen_size",
    "get_mouse_position",
    "navigate",
    "browser_screenshot",
    "browser_click",
    "browser_fill",
    "wait_for",
    "browser_eval",
    "browser_text",
    # n8n (7 — Phase 8 punch-list item #3 service merge; hoisted to
    # Wylde/N8N/tools/ under the service-owned tools convention)
    "n8n_list_workflows",
    "n8n_get_workflow",
    "n8n_get_execution",
    "n8n_execute_workflow",
    "n8n_create_workflow",
    "n8n_edit_workflow",
    "n8n_delete_workflow",
    # caption (3 — Phase 8.7 service-of-services merge; Florence-2-backed
    # captioner under Wylde/Trainer/Caption/tools/, picked up by the
    # nested service walker. caption_batch is gated.)
    "caption_image",
    "caption_video",
    "caption_batch",
}

# Tools whose manifests declare requires_confirmation: true. The runner
# returns a confirmation_required envelope unless either confirm=True is
# passed to run_tool or WYLDE_AUTO_MODE is truthy.
GATED_TOOLS = {
    "n8n_create_workflow",
    "n8n_edit_workflow",
    "n8n_delete_workflow",
    "caption_batch",
}


def test_catalog_includes_phase6_tools() -> None:
    tooling = _tooling()
    registry = importlib.import_module(tooling.__name__ + ".tool_registry")
    catalog = registry.list_tools()

    missing = EXPECTED_TOOLS - set(catalog.keys())
    assert not missing, f"missing from catalog: {sorted(missing)}"

    # Every entry has the keys our runner will read.
    for tid, entry in catalog.items():
        for required in ("id", "description", "group"):
            assert entry.get(required), f"{tid}: manifest missing {required!r}"


def test_runner_dispatches_tool_search() -> None:
    tooling = _tooling()
    runner = importlib.import_module(tooling.__name__ + ".tool_runner")

    out = runner.run_tool("tool_search", {"query": "git status"}, retry=False)
    assert out["ok"] is True, out
    assert out["tool"] == "tool_search"
    data = out["data"]
    assert "results" in data
    assert isinstance(data["results"], list)
    # The catalog includes git_status, so a query for "git status" should
    # produce at least one ranked match.
    ids = [r["tool_id"] for r in data["results"]]
    assert "git_status" in ids


def test_runner_dispatches_list_files(tmp_path: Path) -> None:
    tooling = _tooling()
    runner = importlib.import_module(tooling.__name__ + ".tool_runner")
    (tmp_path / "a.txt").write_text("a")
    (tmp_path / "sub").mkdir()

    out = runner.run_tool("list_files", {"path": str(tmp_path)}, retry=False)
    assert out["ok"] is True, out
    names = {f["name"] for f in out["data"]["files"]}
    assert names == {"a.txt", "sub"}


def test_runner_unknown_tool_returns_not_found() -> None:
    tooling = _tooling()
    runner = importlib.import_module(tooling.__name__ + ".tool_runner")

    out = runner.run_tool("definitely_not_a_tool", {}, retry=False)
    assert out["ok"] is False
    assert out["error"]["code"] == "not_found"


def test_extension_tool_dispatched_via_handler(monkeypatch: pytest.MonkeyPatch) -> Any:
    """Tools whose manifest carries ``service: "extension"`` must route
    through the extension_bridge's :func:`dispatch` rather than the
    harness tools/ tree.  Regression on ``study_summarize`` triggering
    ``No module named 'Core.harness.tooling.tools.study'``.
    """
    tooling = _tooling()
    runner = importlib.import_module(tooling.__name__ + ".tool_runner")

    # Stub catalog with one extension entry.
    stub_entry = {
        "id": "study_fake",
        "name": "study_fake",
        "description": "test-only extension tool",
        "service": "extension",
        "group": "study",
        "extension": "Wylde_Study_Test",
        "entrypoint": "fake_endpoint",
        "parameters": [],
    }
    monkeypatch.setattr(runner, "list_tools", lambda: {"study_fake": stub_entry})

    # Stub bridge.dispatch — assert the runner forwards (tool_id, params)
    # exactly and surfaces whatever the bridge returned.
    seen = []

    class _StubBridge:
        class ExtensionNotEnabled(RuntimeError):
            pass

        class ExtensionNotFound(RuntimeError):
            pass

        class DispatchError(RuntimeError):
            pass

        @staticmethod
        def dispatch(tool_id: Any, params: Any) -> Any:
            seen.append((tool_id, dict(params)))
            return {"status": "ok", "summary": "stubbed", "key_points": ["p1"]}

    monkeypatch.setattr(runner, "_load_extension_bridge", lambda: _StubBridge)

    out = runner.run_tool("study_fake", {"text": "hello"}, retry=False)
    assert out["ok"] is True, out
    assert out["tool"] == "study_fake"
    assert out["data"] == {
        "status": "ok",
        "summary": "stubbed",
        "key_points": ["p1"],
    }
    assert seen == [("study_fake", {"text": "hello"})], (
        f"bridge.dispatch should have seen one call, got {seen!r}"
    )


def test_extension_tool_propagates_bridge_errors(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Bridge raises one of its typed errors → runner returns
    ``ok=False`` with code ``not_found`` (NotFound / NotEnabled) or
    ``internal_error`` (DispatchError).  Never crashes."""
    tooling = _tooling()
    runner = importlib.import_module(tooling.__name__ + ".tool_runner")

    monkeypatch.setattr(
        runner,
        "list_tools",
        lambda: {
            "study_fake": {
                "id": "study_fake",
                "service": "extension",
                "extension": "Wylde_Study_Test",
                "entrypoint": "x",
            }
        },
    )

    class _BridgeRaisesNotEnabled:
        class ExtensionNotEnabled(RuntimeError):
            pass

        class ExtensionNotFound(RuntimeError):
            pass

        class DispatchError(RuntimeError):
            pass

        @staticmethod
        def dispatch(tool_id: Any, params: Any) -> None:
            raise _BridgeRaisesNotEnabled.ExtensionNotEnabled("extension is disabled")

    monkeypatch.setattr(
        runner, "_load_extension_bridge", lambda: _BridgeRaisesNotEnabled
    )

    out = runner.run_tool("study_fake", {}, retry=False)
    assert out["ok"] is False
    assert out["error"]["code"] == "not_found"
    assert "disabled" in out["error"]["message"]


def test_harness_tool_unchanged() -> None:
    """Regression: extension-routing branch must NOT disturb the
    existing harness-tools path.  A plain harness tool (no
    ``service`` field) still imports + dispatches the same way."""
    tooling = _tooling()
    runner = importlib.import_module(tooling.__name__ + ".tool_runner")

    # Use a real harness tool that we know is in the catalog and has
    # no external deps (tool_search reads the registry itself).
    out = runner.run_tool("tool_search", {"query": "git status"}, retry=False)
    assert out["ok"] is True, out
    assert out["tool"] == "tool_search"
    assert "data" in out
    assert isinstance(out["data"].get("results"), list)


# ── Confirmation gate (Phase 8 / Wylde Design Principle #12) ──────────────


def test_catalog_marks_gated_tools() -> None:
    """Every gated n8n tool's manifest sets requires_confirmation=True."""
    tooling = _tooling()
    registry = importlib.import_module(tooling.__name__ + ".tool_registry")
    catalog = registry.list_tools()

    for tool_id in GATED_TOOLS:
        entry = catalog.get(tool_id)
        assert entry is not None, f"{tool_id} missing from catalog"
        assert entry.get("requires_confirmation") is True, (
            f"{tool_id}: requires_confirmation should be True in manifest"
        )
        assert entry.get("expected_effect"), (
            f"{tool_id}: expected_effect should be a non-empty string"
        )

    # And the read-only n8n tools are NOT gated.
    for tool_id in (
        "n8n_list_workflows",
        "n8n_get_workflow",
        "n8n_get_execution",
        "n8n_execute_workflow",
    ):
        entry = catalog.get(tool_id)
        assert entry is not None, f"{tool_id} missing from catalog"
        assert entry.get("requires_confirmation") is False, (
            f"{tool_id}: requires_confirmation should be False"
        )


def test_runner_returns_confirmation_envelope_for_gated_tool(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Default behaviour: gated tool short-circuits without dispatching."""
    monkeypatch.delenv("WYLDE_AUTO_MODE", raising=False)
    tooling = _tooling()
    runner = importlib.import_module(tooling.__name__ + ".tool_runner")

    out = runner.run_tool(
        "n8n_delete_workflow",
        {"workflow_id": "999"},
        retry=False,
    )
    assert out["ok"] is False, out
    assert out.get("confirmation_required") is True
    assert out["tool"] == "n8n_delete_workflow"
    # The runner echoes back the params and surfaces the manifest fields.
    assert out.get("params") == {"workflow_id": "999"}
    assert out.get("expected_effect"), "expected_effect should be present"
    # Crucially, the underlying tool was NOT invoked — no `data` block,
    # no error from the n8n client.
    assert "data" not in out
    assert "error" not in out


def test_runner_dispatches_gated_tool_with_confirm() -> None:
    """confirm=True bypasses the gate and dispatches the tool."""
    tooling = _tooling()
    runner = importlib.import_module(tooling.__name__ + ".tool_runner")

    out = runner.run_tool(
        "n8n_delete_workflow",
        {"workflow_id": ""},  # empty id → tool returns its own validation error
        retry=False,
        confirm=True,
    )
    # No confirmation_required envelope — the tool actually ran. It
    # returned its own validation error because workflow_id was empty.
    assert (
        out.get("confirmation_required") is None
        or out.get("confirmation_required") is False
    )
    assert out["ok"] is True, out  # tool returned a dict; runner wraps as success
    assert out["data"].get("error") == "workflow_id is required"


def test_runner_dispatches_gated_tool_when_auto_mode(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """WYLDE_AUTO_MODE=true bypasses the gate without confirm=True."""
    monkeypatch.setenv("WYLDE_AUTO_MODE", "true")
    tooling = _tooling()
    runner = importlib.import_module(tooling.__name__ + ".tool_runner")

    out = runner.run_tool(
        "n8n_delete_workflow",
        {"workflow_id": ""},
        retry=False,
    )
    assert (
        out.get("confirmation_required") is None
        or out.get("confirmation_required") is False
    )
    assert out["ok"] is True, out
    assert out["data"].get("error") == "workflow_id is required"


def test_runner_does_not_gate_read_only_tool(monkeypatch: pytest.MonkeyPatch) -> None:
    """Non-gated tools dispatch normally, regardless of confirm/auto-mode."""
    monkeypatch.delenv("WYLDE_AUTO_MODE", raising=False)
    tooling = _tooling()
    runner = importlib.import_module(tooling.__name__ + ".tool_runner")

    # n8n_get_workflow needs workflow_id — tool returns its own error,
    # but the runner shouldn't have stopped at a gate check.
    out = runner.run_tool("n8n_get_workflow", {"workflow_id": ""}, retry=False)
    assert (
        out.get("confirmation_required") is None
        or out.get("confirmation_required") is False
    )
    assert out["ok"] is True, out
    assert "error" in out["data"]
