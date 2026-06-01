"""Shared bootstrap for tools/visual/.

PyAutoGUI and Playwright are heavy and optional. The Phase 6 contract is
"the catalog can list a tool even if its deps aren't installed; the
import failure only happens when the tool actually runs". So both libs
are lazy-imported here, and the helpers below own the singletons that
multiple tools share (the Playwright browser/page, in particular).

Public helpers:

* :func:`get_pyautogui` — returns the configured pyautogui module.
* :func:`get_page`      — returns the active Playwright page (lazy-launches
  the browser on first call).
* :func:`shutdown_browser` — close everything Playwright owns. Called on
  process exit (atexit) or by tests; safe to call when nothing's open.
* :func:`screenshot_to_b64` — encode a PNG byte stream as a string for
  multimodal LLM input.
"""

from __future__ import annotations

import atexit
import base64
import io
from typing import Any

# ── Module-level singletons ──────────────────────────────────────────────

_pyautogui: Any = None
_playwright: Any = None
_browser: Any = None
_browser_context: Any = None
_page: Any = None


# ── PyAutoGUI ────────────────────────────────────────────────────────────


def get_pyautogui() -> Any:
    """Lazy-import + configure PyAutoGUI on first call."""
    global _pyautogui
    if _pyautogui is None:
        import pyautogui  # noqa: F401  — lazy by design

        # Failsafe: jamming the mouse into a corner aborts. Small pause
        # between actions so consecutive calls don't race the UI thread.
        pyautogui.FAILSAFE = True
        pyautogui.PAUSE = 0.1
        _pyautogui = pyautogui
    return _pyautogui


# ── Playwright ───────────────────────────────────────────────────────────


def get_page(
    *, headless: bool = False, viewport_width: int = 1920, viewport_height: int = 1080
) -> Any:
    """Return the singleton Playwright page, launching the browser if needed.

    The first call boots Chromium with the given viewport. Subsequent calls
    reuse the same page (and therefore the same cookie jar / DOM state).
    Tools that need a clean slate should navigate explicitly.
    """
    global _playwright, _browser, _browser_context, _page
    if _page is not None:
        return _page

    from playwright.sync_api import sync_playwright

    _playwright = sync_playwright().start()
    _browser = _playwright.chromium.launch(headless=headless)
    _browser_context = _browser.new_context(
        viewport={"width": viewport_width, "height": viewport_height},
    )
    _page = _browser_context.new_page()
    return _page


def shutdown_browser() -> None:
    """Close Playwright resources. Idempotent."""
    global _playwright, _browser, _browser_context, _page
    try:
        if _page is not None:
            _page.close()
    except Exception:
        pass
    try:
        if _browser_context is not None:
            _browser_context.close()
    except Exception:
        pass
    try:
        if _browser is not None:
            _browser.close()
    except Exception:
        pass
    try:
        if _playwright is not None:
            _playwright.stop()
    except Exception:
        pass
    _page = _browser_context = _browser = _playwright = None


atexit.register(shutdown_browser)


# ── Screenshot helpers ───────────────────────────────────────────────────


def screenshot_to_b64(image: Any) -> tuple[str, int, int, int]:
    """Encode a PIL.Image as base64 PNG. Returns (b64, width, height, size_bytes)."""
    buf = io.BytesIO()
    image.save(buf, format="PNG", optimize=True)
    raw = buf.getvalue()
    return (
        base64.b64encode(raw).decode("utf-8"),
        image.width,
        image.height,
        len(raw),
    )


def encode_b64(raw: bytes) -> str:
    """Encode raw bytes as base64 (used for Playwright screenshots that
    arrive as bytes rather than a PIL image)."""
    return base64.b64encode(raw).decode("utf-8")


__all__ = [
    "get_pyautogui",
    "get_page",
    "shutdown_browser",
    "screenshot_to_b64",
    "encode_b64",
]
