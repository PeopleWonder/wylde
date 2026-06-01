"""lint_all — run every dev/ lint tool and consolidate the output.

Calls the four sibling tools directly (in-process) and merges their
``findings`` lists.  Each finding is tagged with its source tool via
the ``rule`` prefix (already done by the sub-tools).

Per-sub-tool failures (linter missing, timeout, etc.) become an entry
in ``summary.engines[<name>] = {error: "..."}`` without taking down
the whole run.
"""

from __future__ import annotations

from typing import Any, Callable, Dict, List, Sequence, Tuple


def _normalise_skip(value: Any) -> Sequence[str]:
    if value is None:
        return ()
    if isinstance(value, str):
        return (value,)
    if isinstance(value, (list, tuple)):
        return tuple(str(v).lower() for v in value)
    return ()


def run_lint_all(params: Dict[str, Any]) -> Dict[str, Any]:
    skip = set(_normalise_skip(params.get("skip")))

    # Lazy imports — each sub-tool is independent.
    from ..lint_python import run_lint_python
    from ..lint_svelte import run_lint_svelte
    from ..lint_rust import run_lint_rust
    from ..wylde_check import run_wylde_check

    runners: Tuple[
        Tuple[str, str, Callable[[Dict[str, Any]], Dict[str, Any]], Dict[str, Any]], ...
    ] = (
        ("python", "python", run_lint_python, {}),
        ("svelte", "svelte", run_lint_svelte, {}),
        ("rust", "rust", run_lint_rust, {}),
        ("wylde_check", "wylde_check", run_wylde_check, {}),
    )

    findings: List[Dict[str, Any]] = []
    engines: Dict[str, Any] = {}
    for short_name, _, runner, sub_params in runners:
        if short_name in skip:
            engines[short_name] = {"skipped": True}
            continue
        try:
            result = runner(sub_params)
        except Exception as exc:  # noqa: BLE001
            engines[short_name] = {"error": f"{type(exc).__name__}: {exc}"}
            continue
        data = result.get("data") or {}
        if not result.get("ok") and not data.get("findings"):
            engines[short_name] = {
                "error": (result.get("error") or {}).get("message", "failed")
            }
            continue
        sub_findings = data.get("findings") or []
        findings.extend(sub_findings)
        engines[short_name] = {
            "findings": len(sub_findings),
            "summary": data.get("summary", {}),
        }

    # Re-aggregate severity counts.
    by_sev = {"error": 0, "warning": 0, "info": 0}
    for f in findings:
        sev = f.get("severity", "warning")
        if sev in by_sev:
            by_sev[sev] += 1

    return {
        "ok": True,
        "data": {
            "findings": findings,
            "summary": {
                "tool": "lint_all",
                "total": len(findings),
                "by_severity": by_sev,
                "engines": engines,
            },
        },
    }
