"""Phase 7 smoke test — extension_bridge end-to-end wiring.

Covers:

1. Loader finds extensions correctly (Webcrawler + Wylde_Study).
2. tool_registry includes extension tools when an extension is
   enabled, drops them when disabled. Cache invalidation works.
3. Dispatcher routes calls to the right extension handler (via a
   stub handler we register dynamically — we don't actually hit
   the network or the LLM here; that's an integration concern).
4. Per-tool manifest overlays apply.
5. extension_routes.build_route_table (now Core.shared.extension_routes)
   enumerates the on-disk extension endpoints.
6. The wylde-extension-bridge pipe service dispatches
   ``extensions.dispatch`` with the same outcomes as the in-process
   dispatcher, and extension_routes.handle_extension_request routes
   through it. Skipped when the pipe transport (pywin32/msgpack) is absent.

Designed to run with no network, no Ollama, no Memgraph. Each test
function is idempotent and restores any state it changed (manifest
files, registry caches) so re-running is safe.

Run via the .bat launcher in this folder, or directly::

    python "Wylde/Extensions/extension_bridge/tests/smoke_test.py"

Exit code 0 = all checks passed; 1 = at least one failure.
"""

from __future__ import annotations

import json
import sys
import traceback
from pathlib import Path
from typing import Any, Callable, Dict, List, Optional, Tuple

# ── Path setup ──────────────────────────────────────────────────────────────
#
# The test launches as a script, so we need the Wylde root on sys.path
# before any ``from Wylde.X import Y`` works. Wylde root is the parent
# of the ``Wylde/`` folder (which is a namespace package).

_HERE = Path(__file__).resolve().parent  # tests/
_BRIDGE_DIR = _HERE.parent  # extension_bridge/
_EXTENSIONS_DIR = _BRIDGE_DIR.parent  # Extensions/
_WYLDE_ROOT = _EXTENSIONS_DIR.parent  # Wylde/
_PROJECT_ROOT = _WYLDE_ROOT.parent  # parent of Wylde/

if str(_PROJECT_ROOT) not in sys.path:
    sys.path.insert(0, str(_PROJECT_ROOT))
# The bridge pipe (extension_bridge/pipe.py) and run.py reach the
# shared ipc stack via the bare ``Core.shared.ipc`` import — the form
# the service uses when the daemon launches it with the Wylde root as
# cwd. Put the Wylde root on sys.path too so that bare import resolves
# when this smoke test is run as a standalone script.
if str(_WYLDE_ROOT) not in sys.path:
    sys.path.insert(0, str(_WYLDE_ROOT))


# ── Test runner ─────────────────────────────────────────────────────────────


_RESULTS: List[Tuple[str, bool, str]] = []
_SKIPPED: List[Tuple[str, str]] = []


def _run(name: str, fn: Callable[[], None]) -> None:
    try:
        fn()
        _RESULTS.append((name, True, ""))
        print(f"  PASS  {name}")
    except AssertionError as exc:
        _RESULTS.append((name, False, str(exc) or "assertion failed"))
        print(f"  FAIL  {name}: {exc}")
    except Exception as exc:
        msg = f"{type(exc).__name__}: {exc}\n{traceback.format_exc()}"
        _RESULTS.append((name, False, msg))
        print(f"  ERROR {name}: {msg}")


def _skip(name: str, reason: str) -> None:
    """Record a test that could not run (missing optional dependency).

    Skips do not count toward pass/fail — they are surfaced separately
    so a deps-light environment doesn't show a red run for a transport
    it can't exercise."""
    _SKIPPED.append((name, reason))
    print(f"  SKIP  {name}: {reason}")


_bridge_pipe_ready: Optional[bool] = None


def _ensure_bridge_pipe() -> bool:
    """Start the wylde-extension-bridge pipe service once (cached).

    The pipe surface needs pywin32 + msgpack; returns False — and the
    pipe-backed checks skip — when those are unavailable (e.g. the
    smoke test launched under a bare interpreter)."""
    global _bridge_pipe_ready
    if _bridge_pipe_ready is None:
        try:
            from Wylde.Extensions.extension_bridge import pipe as bridge_pipe

            _bridge_pipe_ready = bridge_pipe.start()
        except Exception:  # noqa: BLE001
            _bridge_pipe_ready = False
    return _bridge_pipe_ready


# ── Helpers ─────────────────────────────────────────────────────────────────


def _read_manifest(path: Path) -> Dict[str, Any]:
    data: Dict[str, Any] = json.loads(path.read_text(encoding="utf-8"))
    return data


def _set_enabled_in_file(path: Path, enabled: bool) -> None:
    raw = _read_manifest(path)
    raw["enabled"] = bool(enabled)
    path.write_text(json.dumps(raw, indent=2) + "\n", encoding="utf-8")


# ── Tests ───────────────────────────────────────────────────────────────────


def test_loader_finds_extensions() -> None:
    from Wylde.Extensions import extension_bridge as eb

    eb.invalidate_loader_cache()
    extensions = eb.discover_extensions()

    assert "Webcrawler" in extensions, (
        f"Webcrawler not discovered (got {list(extensions)})"
    )
    assert "Wylde_Study" in extensions, (
        f"Wylde_Study not discovered (got {list(extensions)})"
    )

    wc = extensions["Webcrawler"]
    assert wc.transport == "http", f"Webcrawler transport: {wc.transport}"
    assert wc.handler_module == "handler"
    tool_ids = {t.tool_id for t in wc.tools}
    assert {"scrape", "fetch", "extract"}.issubset(tool_ids), (
        f"Webcrawler tools missing some of scrape/fetch/extract: {tool_ids}"
    )

    ws = extensions["Wylde_Study"]
    ws_ids = {t.tool_id for t in ws.tools}
    expected = {
        "study_index_page",
        "study_query",
        "study_summarize",
        "study_explain",
        "study_flashcards",
    }
    assert expected.issubset(ws_ids), (
        f"Wylde_Study tools missing: have {ws_ids}, want {expected}"
    )

    # Reserved folder must not be picked up as an extension.
    assert "extension_bridge" not in extensions, (
        "extension_bridge folder must not be discovered as an extension"
    )


def test_tool_overlays_apply() -> None:
    """Per-tool manifest overlays should override description / params."""
    from Wylde.Extensions import extension_bridge as eb

    eb.invalidate_loader_cache()
    wc = eb.get_extension("Webcrawler")
    assert wc is not None
    by_id = {t.tool_id: t for t in wc.tools}
    scrape = by_id["scrape"]
    # The per-tool manifest's description starts with "Scrape HTML";
    # not asserting the entire string so a small reword doesn't break
    # the test, but the field must be non-empty and from the overlay
    # (length > inline declaration is a fair proxy).
    assert scrape.description, "scrape description should not be empty"
    # Per-tool manifest declares 3 parameters: url, selectors, timeout.
    assert len(scrape.parameters) == 3, (
        f"scrape should have 3 parameters from overlay, got {len(scrape.parameters)}"
    )


def test_disabled_extensions_have_no_catalog_tools() -> None:
    from Wylde.Extensions import extension_bridge as eb
    from Wylde.Core.harness.tooling.tool_registry import (
        invalidate_cache,
        list_tools,
    )

    # Both default to disabled per Phase 7. Sanity-check that.
    eb.invalidate_loader_cache()
    extensions = eb.list_extensions()
    assert not extensions["Webcrawler"].enabled, (
        "Webcrawler should default disabled in Phase 7"
    )
    assert not extensions["Wylde_Study"].enabled, (
        "Wylde_Study should default disabled in Phase 7"
    )

    invalidate_cache()
    catalog = list_tools()
    extension_tool_ids = {
        tid for tid, entry in catalog.items() if entry.get("service") == "extension"
    }
    assert extension_tool_ids == set(), (
        f"disabled extensions must contribute zero tools; got {extension_tool_ids}"
    )


def test_enable_disable_cycle_updates_catalog() -> None:
    from Wylde.Extensions import extension_bridge as eb
    from Wylde.Core.harness.tooling.tool_registry import (
        invalidate_cache,
        list_tools,
    )

    wc_path = _EXTENSIONS_DIR / "Webcrawler" / "manifest.json"
    original = _read_manifest(wc_path)["enabled"]
    try:
        # Enable Webcrawler. The bridge writes the manifest, which
        # invalidates both caches.
        eb.enable("Webcrawler")
        invalidate_cache()
        catalog = list_tools()
        assert "scrape" in catalog, "scrape must appear when Webcrawler enabled"
        assert "fetch" in catalog, "fetch must appear when Webcrawler enabled"
        assert "extract" in catalog, "extract must appear when Webcrawler enabled"
        # Tagged with service=extension and extension=Webcrawler
        assert catalog["scrape"]["service"] == "extension"
        assert catalog["scrape"]["extension"] == "Webcrawler"

        # Now disable and confirm they drop.
        eb.disable("Webcrawler")
        invalidate_cache()
        catalog = list_tools()
        assert "scrape" not in catalog, (
            "scrape must drop from catalog when Webcrawler disabled"
        )
    finally:
        # Restore whatever state the manifest was in before the test.
        _set_enabled_in_file(wc_path, original)
        eb.invalidate_loader_cache()
        invalidate_cache()


def test_dispatcher_routes_to_handler() -> None:
    """Stub a tool's handler function and verify dispatch lands on it.

    We don't run the real ``run_fetch`` (would hit the network).
    Instead we monkey-patch a fake function onto the loaded handler
    module and call ``dispatch`` for an enabled tool. Verifies the
    full path: enabled-state check → handler import → function
    resolution → invocation.
    """
    from Wylde.Extensions import extension_bridge as eb

    wc_path = _EXTENSIONS_DIR / "Webcrawler" / "manifest.json"
    original = _read_manifest(wc_path)["enabled"]
    try:
        eb.enable("Webcrawler")

        # Force the dispatcher to load the handler, then patch.
        wc = eb.get_extension("Webcrawler")
        assert wc is not None

        # Trigger handler load via a doomed dispatch attempt; we
        # actually expect dispatch to succeed against the real
        # function, but to keep the test hermetic we replace the
        # function on the loaded module.
        # Internal: dispatcher exposes _load_handler indirectly.
        # We import the dispatcher module's loader directly.
        from importlib import import_module

        dispatcher_mod = import_module("Wylde.Extensions.extension_bridge.dispatcher")
        handler_mod = dispatcher_mod._load_handler(wc)

        captured: Dict[str, Any] = {}

        def fake_run_fetch(params: Dict[str, Any]) -> Dict[str, Any]:
            captured.update(params)
            return {"status": "ok", "stub": True, "echo": params}

        original_fn = getattr(handler_mod, "run_fetch", None)
        handler_mod.run_fetch = fake_run_fetch
        try:
            result = eb.dispatch("fetch", {"url": "http://example.test"})
            assert result == {
                "status": "ok",
                "stub": True,
                "echo": {"url": "http://example.test"},
            }, f"unexpected dispatch result: {result}"
            assert captured == {"url": "http://example.test"}
        finally:
            if original_fn is not None:
                handler_mod.run_fetch = original_fn

        # Disabled extensions raise ExtensionNotEnabled.
        eb.disable("Webcrawler")
        try:
            eb.dispatch("fetch", {"url": "http://example.test"})
        except eb.ExtensionNotEnabled:
            pass
        else:
            raise AssertionError(
                "dispatch on disabled extension must raise ExtensionNotEnabled"
            )

        # Unknown tool_id raises ExtensionNotFound.
        try:
            eb.dispatch("definitely_not_a_real_tool", {})
        except eb.ExtensionNotFound:
            pass
        else:
            raise AssertionError(
                "dispatch on unknown tool must raise ExtensionNotFound"
            )
    finally:
        _set_enabled_in_file(wc_path, original)
        eb.invalidate_loader_cache()


def test_gateway_route_table() -> None:
    """build_route_table() enumerates the on-disk extension endpoints.

    The route table is a diagnostic (it walks the bridge in-process);
    request dispatch is the pipe-backed path covered separately."""
    from Core.shared.extension_routes import build_route_table

    routes = build_route_table()
    assert isinstance(routes, list) and routes, "route table must be non-empty"
    paths = {r["path"] for r in routes}
    assert "/extensions/Webcrawler/run_fetch" in paths, (
        f"missing fetch route; paths: {sorted(paths)}"
    )
    assert "/extensions/Wylde_Study/index_page" in paths, (
        f"missing index_page route; paths: {sorted(paths)}"
    )
    # Default disabled — every route's enabled flag must be False.
    assert all(r["enabled"] is False for r in routes), (
        "all routes should report enabled=False at default state"
    )


def test_pipe_service_round_trip() -> None:
    """The wylde-extension-bridge pipe surfaces the same outcomes the
    in-process dispatcher does, and
    Core.shared.extension_routes.handle_extension_request routes its
    calls through that pipe.

    Requires the pipe transport; the caller gates this on
    :func:`_ensure_bridge_pipe`."""
    from Core.shared import ipc

    from Wylde.Extensions import extension_bridge as eb
    from Core.shared.extension_routes import handle_extension_request

    def _pipe_code(extension: str, endpoint: str) -> str:
        reply = ipc.send_action(
            "wylde-extension-bridge",
            "extensions.dispatch",
            {"extension": extension, "endpoint": endpoint, "params": {}},
        )
        assert reply.ok is False, f"expected an error reply, got {reply!r}"
        err = reply.error or {}
        code = err.get("code")
        assert isinstance(code, str), f"error reply carried no code: {err!r}"
        return code

    # Unknown extension: the in-process dispatcher raises
    # ExtensionNotFound; the pipe maps it to extension_not_found.
    try:
        eb.dispatch_external("DoesNotExist", "anything", {})
    except eb.ExtensionNotFound:
        pass
    else:
        raise AssertionError(
            "in-process dispatch_external should raise ExtensionNotFound"
        )
    assert _pipe_code("DoesNotExist", "anything") == "extension_not_found", (
        "pipe should map ExtensionNotFound → extension_not_found"
    )

    # Disabled extension: Webcrawler defaults disabled — the in-process
    # dispatcher raises ExtensionNotEnabled, the pipe maps it to
    # extension_disabled.
    wc_path = _EXTENSIONS_DIR / "Webcrawler" / "manifest.json"
    original = _read_manifest(wc_path)["enabled"]
    try:
        _set_enabled_in_file(wc_path, False)
        eb.invalidate_loader_cache()
        try:
            eb.dispatch_external("Webcrawler", "run_fetch", {})
        except eb.ExtensionNotEnabled:
            pass
        else:
            raise AssertionError(
                "in-process dispatch_external should raise ExtensionNotEnabled"
            )
        assert _pipe_code("Webcrawler", "run_fetch") == "extension_disabled", (
            "pipe should map ExtensionNotEnabled → extension_disabled"
        )
    finally:
        _set_enabled_in_file(wc_path, original)
        eb.invalidate_loader_cache()

    # handle_extension_request — the Gateway HTTP shim — produces the
    # matching status codes through the same pipe.
    status, _body = handle_extension_request("DoesNotExist", "anything", {})
    assert status == 404, f"unknown extension should be 404, got {status}"
    status, _body = handle_extension_request("Webcrawler", "run_fetch", {})
    assert status == 409, f"disabled extension should be 409, got {status}"
    status, _body = handle_extension_request("Webcrawler", "no_such_endpoint", {})
    assert status == 404, f"unknown endpoint should be 404, got {status}"


def test_catalog_size_with_webcrawler_enabled() -> None:
    """Print baseline + with-extension counts so the smoke run shows it."""
    from Wylde.Extensions import extension_bridge as eb
    from Wylde.Core.harness.tooling.tool_registry import (
        invalidate_cache,
        list_tools,
    )

    invalidate_cache()
    eb.invalidate_loader_cache()
    baseline = list_tools()
    baseline_count = len(baseline)

    wc_path = _EXTENSIONS_DIR / "Webcrawler" / "manifest.json"
    original = _read_manifest(wc_path)["enabled"]
    try:
        eb.enable("Webcrawler")
        invalidate_cache()
        with_wc = list_tools()
        with_count = len(with_wc)
        assert with_count == baseline_count + 3, (
            f"Webcrawler enabled should add 3 tools; baseline={baseline_count}, "
            f"with_wc={with_count}"
        )
        print(f"        catalog baseline (no extensions): {baseline_count} tools")
        print(f"        catalog with Webcrawler enabled: {with_count} tools")
    finally:
        _set_enabled_in_file(wc_path, original)
        eb.invalidate_loader_cache()
        invalidate_cache()


# ── Main ────────────────────────────────────────────────────────────────────


def main() -> int:
    print("Phase 7 smoke test — extension_bridge wiring")
    print(f"  project root: {_PROJECT_ROOT}")
    print(f"  extensions dir: {_EXTENSIONS_DIR}")
    print()

    _run(
        "loader.discover_extensions finds Webcrawler & Wylde_Study",
        test_loader_finds_extensions,
    )
    _run("per-tool manifest overlays apply", test_tool_overlays_apply)
    _run(
        "disabled extensions contribute no catalog tools",
        test_disabled_extensions_have_no_catalog_tools,
    )
    _run(
        "enable/disable cycle updates the tool catalog",
        test_enable_disable_cycle_updates_catalog,
    )
    _run("dispatcher routes to handler module", test_dispatcher_routes_to_handler)
    _run(
        "extension_routes builds a route table",
        test_gateway_route_table,
    )
    _run(
        "catalog size grows by 3 with Webcrawler enabled",
        test_catalog_size_with_webcrawler_enabled,
    )

    # Pipe-backed check — needs the wylde-extension-bridge pipe service,
    # which needs pywin32 + msgpack. Skip cleanly when unavailable.
    pipe_test_name = "extension-bridge pipe round-trips extensions.dispatch"
    if _ensure_bridge_pipe():
        _run(pipe_test_name, test_pipe_service_round_trip)
    else:
        _skip(pipe_test_name, "pipe transport unavailable (pywin32/msgpack)")

    failed = [r for r in _RESULTS if not r[1]]
    print()
    summary = (
        f"summary: {len(_RESULTS) - len(failed)}/{len(_RESULTS)} passed, "
        f"{len(failed)} failed"
    )
    if _SKIPPED:
        summary += f", {len(_SKIPPED)} skipped"
    print(summary)
    if failed:
        print()
        print("failures:")
        for name, _, msg in failed:
            print(f"  - {name}")
            print(f"    {msg}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
