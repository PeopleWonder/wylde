"""Tests for the fs write tools (``write_file`` / ``edit_file``) and
the lint_hook entry point.

Architectural-check semantics changed in 2026-05: the per-write lint
that used to live inside the tools was removed.  Each tool now just
writes and records the path on the active turn (via
:func:`Core.harness._tool_context.record_file_written`); the
``wylde_check`` sweep fires once at end-of-turn.  Coverage here:

* ``write_file`` / ``edit_file`` write cleanly and never block on
  content (including content that would have tripped the old lint).
* The ``force`` param was removed (writes are unconditional).
* The ``lint_hook.py`` entry point (used by Claude Code's Stop hook
  and the manual per-file form) still works.

The turn-registry recording path itself (``record_file_written``
appending to ``TurnState.files_written``) is no longer exercised here:
Phase 5.D retired the Python chat-turn driver, so the Python
``record_file_written`` is a no-op and the registry it fed lives in the
Rust ``wylde-harness`` crate (covered by its own tests). The tools'
contract — call the helper, never raise even with no active turn — is
all that remains to pin on the Python side, and the success-path tests
below cover it implicitly.
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path
from typing import Any


_HERE = Path(__file__).resolve()
_VAULT_ROOT = _HERE.parents[7]
if str(_VAULT_ROOT) not in sys.path:
    sys.path.insert(0, str(_VAULT_ROOT))


def _import_write() -> Any:
    try:
        from Wylde.Core.harness.tooling.tools.fs.write_file import run_write_file

        return run_write_file
    except ImportError:
        from Core.harness.tooling.tools.fs.write_file import run_write_file

        return run_write_file


def _import_edit() -> Any:
    try:
        from Wylde.Core.harness.tooling.tools.fs.edit_file import run_edit_file

        return run_edit_file
    except ImportError:
        from Core.harness.tooling.tools.fs.edit_file import run_edit_file

        return run_edit_file


# ── write_file ────────────────────────────────────────────────────────


def test_write_file_succeeds_on_clean_content(tmp_path: Path) -> None:
    run = _import_write()
    target = tmp_path / "Core" / "harness" / "good.py"
    result = run(
        {
            "path": str(target),
            "content": "from Core.shared import ipc\n\ndef hello():\n    return 'hi'\n",
        }
    )
    assert result["status"] == "success"
    assert target.read_text(encoding="utf-8").startswith("from Core.shared")


def test_write_file_writes_even_when_content_would_have_tripped_old_lint(
    tmp_path: Path,
) -> None:
    """Internal-HTTP content used to be a per-write blocker.  The lint
    moved to end-of-turn, so the tool itself now writes unconditionally.
    Outside a turn (tests calling the tool directly), the recording
    helper silently no-ops — the file write itself is unaffected."""
    run = _import_write()
    target = tmp_path / "Core" / "harness" / "evil.py"
    result = run(
        {
            "path": str(target),
            "content": "import requests\nrequests.post('http://127.0.0.1:8005/api/foo')\n",
        }
    )
    assert result["status"] == "success", f"per-write block was removed; got {result}"
    assert target.exists()
    # bytes_written is the encoded length of the in-memory string, not
    # the on-disk size (which may differ on Windows when newline
    # translation kicks in).  Just sanity-check it matches the input.
    assert result["bytes_written"] == len(
        "import requests\nrequests.post('http://127.0.0.1:8005/api/foo')\n".encode(
            "utf-8"
        )
    )


def test_write_file_force_param_is_ignored_no_more_block_path(tmp_path: Path) -> None:
    """The legacy ``force=True`` param was removed with the per-write
    block.  Passing it is harmless — the tool ignores extra kwargs."""
    run = _import_write()
    target = tmp_path / "Core" / "harness" / "should_still_work.py"
    result = run(
        {
            "path": str(target),
            "content": "x = 1\n",
            "force": True,
        }
    )
    assert result["status"] == "success"
    assert target.read_text(encoding="utf-8") == "x = 1\n"


def test_write_file_missing_path_errors() -> None:
    run = _import_write()
    result = run({"content": "x"})
    assert result["status"] == "error"
    assert "path" in result["error"]


def test_write_file_missing_content_errors() -> None:
    run = _import_write()
    result = run({"path": "x.py"})
    assert result["status"] == "error"
    assert "content" in result["error"]


# ── edit_file ────────────────────────────────────────────────────────


def test_edit_file_clean_edit_succeeds(tmp_path: Path) -> None:
    run = _import_edit()
    target = tmp_path / "Core" / "harness" / "ok.py"
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text("x = 1\n", encoding="utf-8")

    result = run({"path": str(target), "old_text": "x = 1", "new_text": "x = 42"})
    assert result["status"] == "success"
    assert "42" in target.read_text(encoding="utf-8")


def test_edit_file_applies_post_edit_content_even_when_old_lint_would_block(
    tmp_path: Path,
) -> None:
    """A post-edit no_internal_http violation used to be a per-edit
    blocker.  The lint moved to end-of-turn, so the edit now lands."""
    run = _import_edit()
    target = tmp_path / "Core" / "harness" / "victim.py"
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text("def hello():\n    return 'hi'\n", encoding="utf-8")

    result = run(
        {
            "path": str(target),
            "old_text": "def hello():\n    return 'hi'\n",
            "new_text": "import requests\nrequests.post('http://127.0.0.1:8005/x')\n",
        }
    )
    assert result["status"] == "success", f"per-edit block was removed; got {result}"
    assert "requests.post" in target.read_text(encoding="utf-8")


def test_edit_file_pattern_not_found_errors(tmp_path: Path) -> None:
    run = _import_edit()
    target = tmp_path / "doc.py"
    target.write_text("hello\n", encoding="utf-8")
    result = run({"path": str(target), "old_text": "missing", "new_text": "x"})
    assert result["status"] == "error"
    assert "pattern not found" in result["error"]


def test_edit_file_missing_file_errors(tmp_path: Path) -> None:
    run = _import_edit()
    result = run(
        {
            "path": str(tmp_path / "nope.py"),
            "old_text": "x",
            "new_text": "y",
        }
    )
    assert result["status"] == "error"
    assert result.get("code") == "not_found"


# ── files_written tracker (record_file_written) ──────────────────────


def test_record_file_written_outside_turn_is_silent_noop() -> None:
    """No active turn context → the helper returns cleanly without
    raising.  Tests calling the fs tools directly rely on this.  Since
    Phase 5.D retired the Python chat-turn driver, the helper is now a
    permanent no-op on the Python side regardless of context — but it
    must still never raise."""
    from Core.harness._tool_context import current_tool_context, record_file_written

    # No driver registry → no active context.
    assert current_tool_context() is None
    # Must not raise.
    record_file_written("anything.py")


# ── lint_hook entry point (unchanged by this refactor) ───────────────


def _hook_path() -> Path:
    # The hook lives at ``<repo>/Core/harness/dev/lint_hook.py``.  Older
    # layouts nested the repo under a ``Wylde/`` dir (now archived as
    # ``Wylde-archive/``); fall back to that path so the test resolves in
    # either tree.  ``_HERE.parents[6]`` is the repo root from this file's
    # location (Core/harness/tooling/tools/fs/tests/).
    candidates = (
        _HERE.parents[6] / "Core" / "harness" / "dev" / "lint_hook.py",
        _VAULT_ROOT / "Wylde" / "Core" / "harness" / "dev" / "lint_hook.py",
    )
    for c in candidates:
        if c.exists():
            return c
    return candidates[0]


def test_lint_hook_skips_unknown_extension(tmp_path: Path) -> None:
    hook = _hook_path()
    target = tmp_path / "notes.txt"
    target.write_text("just text\n", encoding="utf-8")
    proc = subprocess.run(
        [sys.executable, str(hook), str(target)],
        capture_output=True,
        text=True,
        timeout=30.0,
    )
    assert proc.returncode == 0
    assert proc.stderr == ""


def test_lint_hook_skips_legacy_path(tmp_path: Path) -> None:
    hook = _hook_path()
    target = tmp_path / "_legacy" / "ghost.py"
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(
        "import requests\nrequests.post('http://127.0.0.1:8005/x')\n", encoding="utf-8"
    )
    proc = subprocess.run(
        [sys.executable, str(hook), str(target)],
        capture_output=True,
        text=True,
        timeout=30.0,
    )
    assert proc.returncode == 0


def test_lint_hook_reads_stdin_payload(tmp_path: Path) -> None:
    hook = _hook_path()
    target = tmp_path / "Core" / "harness" / "demo.py"
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text("from Core.shared import ipc\n", encoding="utf-8")

    payload = json.dumps(
        {
            "tool_name": "Edit",
            "tool_input": {"file_path": str(target)},
            "tool_response": {},
        }
    )
    proc = subprocess.run(
        [sys.executable, str(hook)],
        input=payload,
        capture_output=True,
        text=True,
        timeout=30.0,
    )
    assert proc.returncode == 0
