"""lint_rust — cargo clippy wrapper.

Runs ``cargo clippy --message-format=json`` from ``Core/GUI/`` (the gpui
GUI workspace; the old Tauri ``src-tauri/`` tree was deleted at the gpui
cutover).  NDJSON output is parsed into the normalised findings shape.
"""

from __future__ import annotations

from typing import Any, Dict

from .._lint_lib import (
    envelope_error,
    envelope_ok,
    parse_clippy_json_lines,
    run_lint_subprocess,
    wylde_root,
)


def run_lint_rust(params: Dict[str, Any]) -> Dict[str, Any]:
    del params  # no params today
    gui_dir = wylde_root() / "Core" / "GUI"
    if not gui_dir.exists():
        return envelope_error(
            f"Core/GUI/ not found at {gui_dir}",
            tool="lint_rust",
            code="path_not_found",
        )

    argv = ["cargo", "clippy", "--message-format=json", "--", "-D", "warnings"]
    result = run_lint_subprocess(argv, cwd=gui_dir, timeout=300.0)
    if not result["ok"]:
        return envelope_error(
            result.get("error") or "clippy invocation failed",
            tool="lint_rust",
        )

    findings = parse_clippy_json_lines(result["stdout"])
    return envelope_ok(
        findings,
        tool="lint_rust",
        summary_extra={
            "exit_code": result["returncode"],
            "stderr_preview": result["stderr"][:500] if result["stderr"] else "",
        },
    )
