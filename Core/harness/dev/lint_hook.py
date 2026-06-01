"""Claude Code hook entry point — wylde_check + ruff dispatcher.

Wired from ``.claude/settings.json`` at the Wylde repo root.  Two modes:

**Stop event (current default)** — fires when Claude finishes
responding.  No ``file_path`` arrives, so the script runs
:func:`wylde_check.run_all` across the active tree and prints findings
to stderr.  Exit code 2 if any ERROR findings fired (Claude Code
interprets that as a blocking failure on ``Stop`` — Claude is forced to
keep working until findings clear or the user intervenes).  Exit 0 on
WARNING-only or clean.

**Per-file mode (manual / legacy PostToolUse)** — when a ``file_path``
is supplied (stdin JSON ``tool_input.file_path``, ``$CLAUDE_FILE_PATH``,
or argv[1]) the script lints just that file via
:func:`prewrite.evaluate_prewrite` (ruff + per-file wylde_check rules).
Exit 2 on architectural errors, 0 otherwise.

Stop-event stdin payload shape::

    {
      "session_id": "...",
      "transcript_path": "...",
      "hook_event_name": "Stop",
      ...
    }

PostToolUse stdin payload shape::

    {
      "tool_name": "Edit" | "Write" | "MultiEdit",
      "tool_input": {"file_path": "<abs path>", ...},
      ...
    }

The hook is read-only — it never writes a file — so it can't recursively
trigger itself.
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path
from typing import Optional


_HERE = Path(__file__).resolve()
_WYLDE_ROOT = _HERE.parents[3]
# Allow importing the sibling prewrite module under either namespace
# root (``Wylde.Core.*`` or bare ``Core.*``).
for candidate in (_WYLDE_ROOT.parent, _WYLDE_ROOT):
    p = str(candidate)
    if p not in sys.path:
        sys.path.insert(0, p)


_LINTABLE_EXTENSIONS = (".py", ".svelte", ".js", ".ts", ".rs", ".json")


def _resolve_file_path() -> Optional[str]:
    """Pull the file_path the hook should lint, from (in order):

    1. JSON payload on stdin (canonical Claude Code hook format)
    2. ``$CLAUDE_FILE_PATH`` env var (the Wylde user's legacy convention)
    3. argv[1] (manual invocation for testing)
    """
    # Stdin first.
    if not sys.stdin.isatty():
        try:
            raw = sys.stdin.read()
        except OSError:
            raw = ""
        if raw.strip():
            try:
                payload = json.loads(raw)
                tool_input = payload.get("tool_input") or {}
                fp = tool_input.get("file_path") or payload.get("file_path")
                if fp:
                    return str(fp)
            except (ValueError, TypeError):
                pass
    # Env var fallback.
    env_fp = os.environ.get("CLAUDE_FILE_PATH")
    if env_fp:
        return env_fp
    # CLI fallback.
    if len(sys.argv) >= 2 and sys.argv[1]:
        return sys.argv[1]
    return None


def _should_skip(rel_path: str) -> bool:
    """Heuristic skip list — build artifacts, legacy, etc."""
    rel_lower = rel_path.replace("\\", "/").lower()
    skip_segments = (
        "_legacy/",
        "__pycache__/",
        "vendor/",
        "/target/",
        "/build/",
        ".pytest_cache/",
        "docs/refactor-archive/",
    )
    if any(seg in rel_lower for seg in skip_segments):
        return True
    if not rel_path.endswith(_LINTABLE_EXTENSIONS):
        return True
    # Only JSON we care about is tool manifest.json.
    if rel_path.endswith(".json") and not rel_path.endswith("manifest.json"):
        return True
    return False


def _run_full_sweep() -> int:
    """Stop-event mode: run wylde_check.run_all() across the tree.

    Prints every finding to stderr.  Exits 2 if any ERROR findings
    fired (Claude Code surfaces stderr and refuses to let the turn
    end), 0 otherwise.
    """
    try:
        from Core.harness.dev.wylde_check import run_all
    except ImportError as exc:
        sys.stderr.write(f"lint_hook: could not import wylde_check ({exc})\n")
        return 0  # don't block — the hook itself is broken

    try:
        result = run_all()
    except Exception as exc:  # noqa: BLE001
        sys.stderr.write(
            f"lint_hook: wylde_check.run_all raised {type(exc).__name__}: {exc}\n"
        )
        return 0

    data = result.get("data", {})
    findings = data.get("findings", [])
    summary = data.get("summary", {})
    by_sev = summary.get("by_severity", {})
    errors = by_sev.get("error", 0)
    warnings = by_sev.get("warning", 0)

    if not findings:
        return 0

    sys.stderr.write(
        f"\nwylde_check: {summary.get('total', len(findings))} finding(s) "
        f"({errors} error, {warnings} warning)\n"
    )
    for f in findings:
        sev = str(f.get("severity", "warning")).upper()
        rule = f.get("rule", "?")
        file_ = f.get("file", "?")
        ln = f.get("line", 0)
        msg = str(f.get("message", ""))[:200]
        sys.stderr.write(f"  [{sev}] {rule} @ {file_}:{ln} — {msg}\n")

    return 2 if errors else 0


def main() -> int:
    file_path = _resolve_file_path()
    if not file_path:
        # Stop-event / no-arg manual invocation: full architectural sweep.
        return _run_full_sweep()

    try:
        from Core.harness.dev import prewrite
    except ImportError:
        sys.stderr.write("lint_hook: could not import Core.harness.dev.prewrite\n")
        return 0  # don't block — the hook itself is broken

    rel_path = prewrite.normalise_path(file_path)
    if _should_skip(rel_path):
        return 0

    abs_path = Path(file_path)
    if not abs_path.is_absolute():
        abs_path = (_WYLDE_ROOT / abs_path).resolve()
    try:
        content = abs_path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError):
        # File gone or binary — silent skip.
        return 0

    result = prewrite.evaluate_prewrite(file_path, content)
    findings = result["findings"]
    blocking = result["blocking_findings"]

    if not findings:
        return 0

    sys.stderr.write(
        f"\nlint_hook: {len(findings)} finding(s) in {rel_path} "
        f"({len(blocking)} blocking)\n"
    )
    for f in findings:
        sev = f.get("severity", "warning").upper()
        rule = f.get("rule", "?")
        ln = f.get("line", 0)
        msg = f.get("message", "")[:200]
        sys.stderr.write(f"  [{sev}] {rule} @ {rel_path}:{ln} — {msg}\n")

    # Exit 2 if architectural errors — Claude Code treats that as a
    # blocking hook failure on PostToolUse and surfaces the stderr.
    return 2 if blocking else 0


if __name__ == "__main__":
    sys.exit(main())
