"""Phase 8.1 smoke test — Webcrawler post-cleanup wiring.

Covers (after the _webcrawler_service/ flat-layout refactor):

1. handler.py imports cleanly via importlib (the same way the
   extension_bridge dispatcher imports it).
2. The three run_* functions (run_fetch, run_scrape, run_extract)
   exist on the loaded handler module and are callable.
3. _load_helper_module("extractor") resolves to the new sibling
   ``extractor.py`` and exposes ``extractor.extract_by_rules``.
4. run_extract works end-to-end against a stub HTML payload (no
   network) — validates the URL→fetch→extractor wiring on the
   html-only path.
5. The Phase 7 enable/disable cycle test still passes, confirming
   the bridge integration is intact.

No network calls; no Gateway dependency. Run via the .bat launcher
in this folder, or directly::

    python "Wylde/Extensions/Webcrawler/tests/smoke_test.py"

Exit code 0 = all checks passed; 1 = at least one failure.
"""

from __future__ import annotations

import importlib.util
import json
import sys
import traceback
from pathlib import Path
from typing import Any, Callable, Dict, List, Tuple

# ── Path setup ──────────────────────────────────────────────────────────────

_HERE = Path(__file__).resolve().parent  # tests/
_WEBCRAWLER_DIR = _HERE.parent  # Webcrawler/
_EXTENSIONS_DIR = _WEBCRAWLER_DIR.parent  # Extensions/
_WYLDE_ROOT = _EXTENSIONS_DIR.parent  # Wylde/
_PROJECT_ROOT = _WYLDE_ROOT.parent  # parent of Wylde/

if str(_PROJECT_ROOT) not in sys.path:
    sys.path.insert(0, str(_PROJECT_ROOT))


# ── Test runner ─────────────────────────────────────────────────────────────


_RESULTS: List[Tuple[str, bool, str]] = []


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


# ── Helpers ─────────────────────────────────────────────────────────────────


def _read_manifest(path: Path) -> Dict[str, Any]:
    data: Dict[str, Any] = json.loads(path.read_text(encoding="utf-8"))
    return data


def _set_enabled_in_file(path: Path, enabled: bool) -> None:
    raw = _read_manifest(path)
    raw["enabled"] = bool(enabled)
    path.write_text(json.dumps(raw, indent=2) + "\n", encoding="utf-8")


def _load_handler_module() -> Any:
    """Import handler.py the same way the dispatcher does."""
    handler_file = _WEBCRAWLER_DIR / "handler.py"
    qual = "wylde_extension.Webcrawler.handler"
    if qual in sys.modules:
        # Force re-import so the test sees fresh state.
        del sys.modules[qual]
    spec = importlib.util.spec_from_file_location(qual, handler_file)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[qual] = module
    spec.loader.exec_module(module)
    return module


# ── Tests ───────────────────────────────────────────────────────────────────


def test_handler_imports_cleanly() -> None:
    """handler.py must import without errors after the refactor."""
    module = _load_handler_module()
    assert module is not None
    assert hasattr(module, "_HERE"), "handler.py missing _HERE — refactor broke layout"
    assert module._HERE == _WEBCRAWLER_DIR, (
        f"_HERE should resolve to Webcrawler/ folder; got {module._HERE}"
    )
    # Sanity: helper loader exists (renamed from _load_staged_module).
    assert hasattr(module, "_load_helper_module"), (
        "handler.py missing _load_helper_module — staged-folder hack still present?"
    )
    # The old name should be gone.
    assert not hasattr(module, "_load_staged_module"), (
        "handler.py still has _load_staged_module — refactor incomplete"
    )


def test_run_functions_exist() -> None:
    """All three tool entrypoints must be callable on the handler module."""
    module = _load_handler_module()
    for fn_name in ("run_fetch", "run_scrape", "run_extract"):
        fn = getattr(module, fn_name, None)
        assert callable(fn), f"handler.{fn_name} is missing or not callable"
    # __all__ must declare them.
    exported = set(getattr(module, "__all__", ()))
    assert {"run_fetch", "run_scrape", "run_extract"}.issubset(exported), (
        f"handler.__all__ must export run_fetch/run_scrape/run_extract; got {exported}"
    )


def test_extractor_helper_loads_from_flat_layout() -> None:
    """_load_helper_module('extractor') must hit the new sibling path."""
    module = _load_handler_module()
    extractor_path = _WEBCRAWLER_DIR / "extractor.py"
    assert extractor_path.is_file(), f"extractor.py must be hoisted to {extractor_path}"
    extractor_module = module._load_helper_module("extractor")
    assert hasattr(extractor_module, "extractor"), (
        "extractor module missing the singleton 'extractor'"
    )
    assert hasattr(extractor_module.extractor, "extract_by_rules"), (
        "extractor.extractor missing extract_by_rules method"
    )


def test_run_extract_html_path() -> None:
    """run_extract(html=..., extraction_rules=...) must work without network."""
    module = _load_handler_module()
    html = (
        "<html><body>"
        "<h1 class='title'>Hello</h1>"
        "<p class='body'>World</p>"
        "<a href='https://example.test/x' class='link'>x</a>"
        "</body></html>"
    )
    rules = {
        "title": {"selector": "h1.title", "attribute": "text"},
        "body": {"selector": "p.body", "attribute": "text"},
        "link_href": {"selector": "a.link", "attribute": "href"},
    }
    result = module.run_extract({"html": html, "extraction_rules": rules})
    assert result.get("status") == "ok", f"unexpected status: {result}"
    extracted = result.get("extracted_data") or {}
    assert extracted.get("title") == "Hello", f"title: {extracted}"
    assert extracted.get("body") == "World", f"body: {extracted}"
    assert extracted.get("link_href") == "https://example.test/x", (
        f"link_href: {extracted}"
    )
    assert result.get("fields_extracted") == 3


def test_phase7_enable_disable_cycle_still_passes() -> None:
    """The Phase 7 bridge test (enable/disable + catalog) must still pass."""
    from Wylde.Extensions import extension_bridge as eb
    from Wylde.Core.harness.tooling.tool_registry import (
        invalidate_cache,
        list_tools,
    )

    wc_path = _EXTENSIONS_DIR / "Webcrawler" / "manifest.json"
    original = _read_manifest(wc_path)["enabled"]
    try:
        eb.enable("Webcrawler")
        invalidate_cache()
        catalog = list_tools()
        assert "scrape" in catalog, "scrape must appear when Webcrawler enabled"
        assert "fetch" in catalog, "fetch must appear when Webcrawler enabled"
        assert "extract" in catalog, "extract must appear when Webcrawler enabled"
        assert catalog["scrape"]["service"] == "extension"
        assert catalog["scrape"]["extension"] == "Webcrawler"

        eb.disable("Webcrawler")
        invalidate_cache()
        catalog = list_tools()
        assert "scrape" not in catalog, (
            "scrape must drop from catalog when Webcrawler disabled"
        )
    finally:
        _set_enabled_in_file(wc_path, original)
        eb.invalidate_loader_cache()
        invalidate_cache()


def test_no_orphan_staged_references_in_handler() -> None:
    """handler.py should no longer mention _webcrawler_service in its code paths."""
    handler_text = (_WEBCRAWLER_DIR / "handler.py").read_text(encoding="utf-8")
    # The string may appear in docstrings ("legacy ``_webcrawler_service/...``")
    # but it must not appear in any active code path. Quick proxy: check it
    # doesn't appear in any non-comment, non-docstring line. We approximate
    # this by checking that the staging Path expression is gone.
    assert '_HERE / "_webcrawler_service"' not in handler_text, (
        "handler.py still constructs a path into _webcrawler_service/"
    )
    assert "_load_staged_module" not in handler_text, (
        "handler.py still calls _load_staged_module"
    )


# ── Main ────────────────────────────────────────────────────────────────────


def main() -> int:
    print("Phase 8.1 smoke test — Webcrawler post-cleanup wiring")
    print(f"  project root: {_PROJECT_ROOT}")
    print(f"  webcrawler dir: {_WEBCRAWLER_DIR}")
    print()

    _run("handler.py imports cleanly via importlib", test_handler_imports_cleanly)
    _run(
        "run_fetch / run_scrape / run_extract exist and are callable",
        test_run_functions_exist,
    )
    _run(
        "_load_helper_module('extractor') resolves to flat-layout extractor.py",
        test_extractor_helper_loads_from_flat_layout,
    )
    _run("run_extract end-to-end on stub HTML (no network)", test_run_extract_html_path)
    _run(
        "handler.py has no orphan _webcrawler_service references",
        test_no_orphan_staged_references_in_handler,
    )
    _run(
        "Phase 7 enable/disable + catalog cycle still passes",
        test_phase7_enable_disable_cycle_still_passes,
    )

    failed = [r for r in _RESULTS if not r[1]]
    print()
    print(
        f"summary: {len(_RESULTS) - len(failed)}/{len(_RESULTS)} passed, "
        f"{len(failed)} failed"
    )
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
