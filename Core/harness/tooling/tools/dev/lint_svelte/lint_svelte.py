"""lint_svelte — svelte-check + eslint wrapper.

Runs from ``Core/GUI/`` so the local ``node_modules/.bin`` resolves.
Aggregates findings from both tools into a single envelope.

svelte-check's output is human-friendly by default; we ask for the
machine-readable ``--output machine`` shape and best-effort parse it.
eslint runs with ``--format json`` which is well-defined.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any, Dict, List

from .._lint_lib import (
    envelope_error,
    envelope_ok,
    parse_svelte_check_text,
    run_lint_subprocess,
    wylde_root,
)


def _parse_eslint_json(stdout: str) -> List[Dict[str, Any]]:
    try:
        files = json.loads(stdout) if stdout.strip() else []
    except (ValueError, TypeError):
        return []
    out: List[Dict[str, Any]] = []
    for file_record in files:
        if not isinstance(file_record, dict):
            continue
        file_path = file_record.get("filePath", "")
        try:
            file_path = str(Path(file_path).relative_to(wylde_root())).replace(
                "\\", "/"
            )
        except (TypeError, ValueError):
            pass
        for msg in file_record.get("messages") or []:
            if not isinstance(msg, dict):
                continue
            out.append(
                {
                    "rule": str(msg.get("ruleId") or "eslint"),
                    "severity": "error" if msg.get("severity") == 2 else "warning",
                    "file": file_path,
                    "line": int(msg.get("line") or 0),
                    "message": str(msg.get("message") or ""),
                    "context": "",
                }
            )
    return out


def run_lint_svelte(params: Dict[str, Any]) -> Dict[str, Any]:
    gui_dir = wylde_root() / "Core" / "GUI"
    if not gui_dir.exists():
        return envelope_error(
            f"Core/GUI/ not found at {gui_dir}",
            tool="lint_svelte",
            code="path_not_found",
        )

    skip_eslint = bool(params.get("skip_eslint", False))

    all_findings: List[Dict[str, Any]] = []
    parts: Dict[str, Any] = {}

    # --- svelte-check pass ---
    svc_argv = ["npx", "--no-install", "svelte-check", "--output", "machine"]
    svc_result = run_lint_subprocess(svc_argv, cwd=gui_dir, timeout=180.0)
    if svc_result["ok"]:
        svc_findings = parse_svelte_check_text(
            svc_result["stdout"], svc_result["stderr"]
        )
        all_findings.extend(svc_findings)
        parts["svelte_check"] = {
            "exit_code": svc_result["returncode"],
            "findings": len(svc_findings),
        }
    else:
        parts["svelte_check"] = {"error": svc_result.get("error")}

    # --- eslint pass ---
    if not skip_eslint:
        es_argv = [
            "npx",
            "--no-install",
            "eslint",
            "--format",
            "json",
            "--ext",
            ".js,.svelte",
            "src",
        ]
        es_result = run_lint_subprocess(es_argv, cwd=gui_dir, timeout=180.0)
        if es_result["ok"]:
            es_findings = _parse_eslint_json(es_result["stdout"])
            all_findings.extend(es_findings)
            parts["eslint"] = {
                "exit_code": es_result["returncode"],
                "findings": len(es_findings),
            }
        else:
            parts["eslint"] = {"error": es_result.get("error")}

    return envelope_ok(
        all_findings,
        tool="lint_svelte",
        summary_extra={"engines": parts},
    )
