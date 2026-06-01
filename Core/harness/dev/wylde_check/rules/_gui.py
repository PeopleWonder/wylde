"""GUI surface rules: Gateway route scope + GUI-no-backend-bypass.

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

``gui_no_backend_bypass`` (rule 10) survives — the "GUI must not touch
backend storage directly" principle is architecture-level, not
Svelte-specific — but is repointed at the gpui panel + shell Rust source.
"""

from __future__ import annotations

import re
import sys as _sys
from typing import List

from .. import Finding
from .._config import GATEWAY_ROUTE_PREFIXES
from .._walkers import _is_excluded, _read_text, _to_rel, _walk

_pkg = _sys.modules[__name__.rsplit(".", 2)[0]]


# ── Rule 8: Gateway route scope ───────────────────────────────────────


_FASTAPI_ROUTE_RE = re.compile(
    r'@\w+\.(?:get|post|put|delete|patch|head|options)\(\s*["\']([^"\']+)["\']'
)

# Picks up ``APIRouter(prefix="/api/foo", ...)`` so the prefix-match
# below sees the full effective URL instead of only the decorator path.
# Matches the first ``APIRouter(prefix="...")`` in a module — multiple
# routers per file would each need their own scan, but the codebase
# uses one-router-per-file by convention.
_APIROUTER_PREFIX_RE = re.compile(r'APIRouter\(\s*prefix\s*=\s*["\']([^"\']+)["\']')


def check_gateway_scope() -> List[Finding]:
    routes_dir = _pkg.WYLDE_ROOT / "Gateway" / "routes"
    out: List[Finding] = []
    if not routes_dir.exists():
        return out
    for path in routes_dir.rglob("*.py"):
        if _is_excluded(path):
            continue
        rel = _to_rel(path)
        text = _read_text(path)
        if not text:
            continue
        # Resolve the router prefix once per file — empty when the
        # module uses ``APIRouter()`` with no prefix (e.g. health.py).
        pm = _APIROUTER_PREFIX_RE.search(text)
        router_prefix = pm.group(1).rstrip("/") if pm else ""
        for lineno, line in enumerate(text.splitlines(), start=1):
            m = _FASTAPI_ROUTE_RE.search(line)
            if not m:
                continue
            decorator_path = m.group(1)
            full_path = router_prefix + decorator_path
            # Strip trailing path-params like /{id} so we match prefixes.
            head = full_path.split("{", 1)[0].rstrip("/")
            if any(head.startswith(p) for p in GATEWAY_ROUTE_PREFIXES):
                continue
            out.append(
                Finding(
                    rule="gateway_scope",
                    severity="warning",
                    file=rel,
                    line=lineno,
                    message=(
                        f"Route {full_path!r} doesn't fit the documented "
                        f"Gateway scope (egress / inbound mobile-future / MCP / "
                        f"extensions).  Confirm or relocate."
                    ),
                    context=line.strip()[:200],
                )
            )
    return out


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
        for lineno, raw_line in enumerate(text.splitlines(), start=1):
            stripped = raw_line.lstrip()
            if stripped.startswith("//"):
                continue
            line = _strip_comment(raw_line, ext)
            if not line.strip():
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
