"""tools/visual/ — Desktop and browser automation.

Pulled forward from ``_legacy/core/tool-runner/tools/visual_interact.py``,
which packed 15 tools (8 desktop + 7 browser) into one file with a
``VISUAL_TOOLS_CONFIG`` dict. Phase 6 splits each tool into its own folder
so the filesystem-as-registry convention applies uniformly: one folder per
callable, one manifest per folder.

Group layout (kept flat, all 15 share ``group=visual``):

* desktop (PyAutoGUI, OS-level): ``screenshot``, ``click``, ``type_text``,
  ``hotkey``, ``mouse_move``, ``scroll``, ``get_screen_size``,
  ``get_mouse_position``
* browser (Playwright): ``navigate``, ``browser_screenshot``,
  ``browser_click``, ``browser_fill``, ``wait_for``, ``browser_eval``,
  ``browser_text``

Why flat? The runner derives the import path from ``group + tool_id``
(see ``tool_runner._resolve_callable``). Nesting under ``tools/visual/desktop/``
would make ``group=desktop`` and break the derivation. Splitting into
``tools/visual_desktop/`` and ``tools/visual_browser/`` would scatter the
shared lib. Keeping flat gives one place for ``_visual_lib.py``, one tag
prefix (``visual``), and a stable ``group`` for tool-search filtering.

Dependencies (lazy-imported in ``_visual_lib.py``): ``pyautogui`` for
desktop, ``playwright`` for browser. The catalog can list these tools
even when the deps aren't installed; import failures only fire when the
tool actually runs.
"""
