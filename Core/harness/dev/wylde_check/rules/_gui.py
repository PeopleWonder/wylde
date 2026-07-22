"""GUI surface rule: GUI-no-backend-bypass.

Slice-11 cutover (2026-05-29) retired the Svelte/Tauri-shaped rules that
lived here once `Core/GUI/src/` (Svelte) and `Core/GUI/src-tauri/`
(Tauri) were deleted:

* ``inferencebar_purity`` (rule 7) — keyed on ``InferenceBar.svelte``;
  Svelte is gone.
* ``gui_pipe_constants`` (rule 11) — keyed on ``src/lib/api.js`` ``SVC_*``
  JS constants; subsumed by the gpui contract rules (38/41).
* ``gui_error_reporting`` (rule 30) — keyed on ``.svelte`` ``console.error``
  / ``toast.error``; the gpui panels surface errors as ``Result`` state,
  a different shape — a gpui-era error rule is a possible post-alpha add.

The 2026-07-20 dead-rule retirement removed one more:

* ``gateway_scope`` (rule 8) — walked ``Gateway/routes/**/*.py`` for
  FastAPI route decorators; the Python Gateway tree was deleted in the
  Rust cutover.

``gui_no_backend_bypass`` (rule 10) survives — the "GUI must not touch
backend storage directly" principle is architecture-level, not
Svelte-specific — but is repointed at the gpui panel + shell Rust source.
"""

from __future__ import annotations

import re
import sys as _sys
from typing import List

from .. import Finding
from .._walkers import _read_text, _to_rel, _walk

_pkg = _sys.modules[__name__.rsplit(".", 2)[0]]


# ── Rule 10: GUI does not bypass the backend ─────────────────────────


# Backend-owned storage path fragments. Any literal string in GUI source
# that contains one of these is reaching past the pipe boundary and
# touching backend state directly. Forward slashes only — the backend
# settles on POSIX paths.
_BACKEND_STORAGE_FRAGMENTS = (
    "Core/harness/memory/indexes",
    "Core/harness/memory/workspace_memories",
    "Core/harness/memory/long_term",
    "Core/harness/memory/short_term",
    "data/manifests",
    "data/long_term",
    "data/conversations",
    "data/system_prompts.json",
)


# Manifest paths the GUI is never allowed to read or write directly.
# Service manifests are pipe-served; service.list / .health give the GUI
# what it needs.
_MANIFEST_PATH_RE = re.compile(r"""['"`](?:[A-Za-z0-9_./-]*?/)?manifest\.json['"`]""")


# gpui GUI source roots scanned by rule 10. The panel-registry aggregator
# under ``Core/GUI/Manifest/`` is intentionally OUT of scope — reading
# panel ``manifest.json`` files is its whole job. Panels + shell, by
# contrast, must reach every backend through ``wylde_gui_pipe``.
_GUI_SOURCE_ROOTS = ("Core/GUI/Frontend", "Core/GUI/Shell")


# Test-code region markers. A `#[cfg(test)]` module or `#[test]` fn that
# writes a SYNTHETIC manifest.json to a tempdir (the roster-discovery
# coverage tests) is not the GUI reaching past the pipe boundary — it is a
# fixture. Tracked by brace depth so the exemption ends with the test block.
_CFG_TEST_RE = re.compile(r"#\[\s*cfg\s*\(\s*test\s*\)\s*\]")
_TOKIO_TEST_RE = re.compile(r"#\[\s*tokio::test")
_TEST_ATTR_RE = re.compile(r"#\[\s*test\s*\]")


def _strip_comment(line: str, ext: str) -> str:
    """Drop trailing comment chunks so we don't flag mentions in
    explanatory comments. Crude — we only need to dodge the common
    ``// note about manifest.json`` shape."""
    if ext in (".js", ".svelte", ".rs"):
        idx = line.find("//")
        if idx >= 0:
            line = line[:idx]
    if ext == ".rs":
        idx = line.find("/*")
        if idx >= 0:
            line = line[:idx]
    return line


def check_gui_no_backend_bypass() -> List[Finding]:
    """The gpui GUI panels + shell must not touch backend-owned storage
    paths or service ``manifest.json`` files directly — everything goes
    through a ``wylde_gui_pipe`` action. Repointed at the gpui Rust source
    at the slice-11 cutover (was the deleted Svelte ``src/`` + Tauri
    ``src-tauri/src/`` trees)."""
    out: List[Finding] = []
    targets = []
    for root in _GUI_SOURCE_ROOTS:
        if (_pkg.WYLDE_ROOT / root).exists():
            targets.extend(_walk((".rs",), roots=(root,)))
    for path in targets:
        rel = _to_rel(path)
        text = _read_text(path)
        if not text:
            continue
        ext = path.suffix.lower()
        # Brace-depth tracking to exempt `#[cfg(test)]` / `#[test]` regions.
        allow_starts: List[int] = []  # depths at which a test region opened
        depth = 0
        pending_allow = False  # a test attribute awaiting its block
        for lineno, raw_line in enumerate(text.splitlines(), start=1):
            stripped = raw_line.lstrip()
            if stripped.startswith("//"):
                continue
            line = _strip_comment(raw_line, ext)
            if not line.strip():
                continue

            if (
                _CFG_TEST_RE.search(line)
                or _TOKIO_TEST_RE.search(line)
                or _TEST_ATTR_RE.search(line)
            ):
                pending_allow = True
            open_braces = line.count("{")
            close_braces = line.count("}")
            if pending_allow and open_braces > 0:
                allow_starts.append(depth)
                pending_allow = False
            inside_test = bool(allow_starts)
            depth += open_braces - close_braces
            while allow_starts and depth <= allow_starts[-1]:
                allow_starts.pop()
            if inside_test:
                continue
            # Quick reject: backend bypass always lives inside a string
            # literal. If the line has no quotes, skip the regex work.
            if '"' not in line and "'" not in line and "`" not in line:
                continue
            for frag in _BACKEND_STORAGE_FRAGMENTS:
                if frag in line:
                    out.append(
                        Finding(
                            rule="gui_no_backend_bypass",
                            severity="error",
                            file=rel,
                            line=lineno,
                            message=(
                                f"GUI source references backend-owned "
                                f"storage path {frag!r}.  The GUI must "
                                f"reach this state through a pipe action, "
                                f"not by touching disk directly."
                            ),
                            context=raw_line.strip()[:200],
                        )
                    )
                    break
            else:
                if _MANIFEST_PATH_RE.search(line):
                    out.append(
                        Finding(
                            rule="gui_no_backend_bypass",
                            severity="error",
                            file=rel,
                            line=lineno,
                            message=(
                                "GUI source references a manifest.json "
                                "path literal.  Service manifests are "
                                "pipe-served (service.list / .health); the "
                                "GUI must not read or write them directly."
                            ),
                            context=raw_line.strip()[:200],
                        )
                    )
    return out
