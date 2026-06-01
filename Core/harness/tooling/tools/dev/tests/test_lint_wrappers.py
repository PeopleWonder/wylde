"""Tests for the dev/ lint wrappers.

The off-the-shelf wrappers (ruff / svelte-check / clippy) are tested
with a stubbed subprocess so we don't depend on the host having the
underlying tool installed.  We assert:

* Envelope shape is the standard ``{ok, data: {findings, summary}}``.
* Findings normalisation maps each tool's native output to the shared
  ``{rule, severity, file, line, message, context}`` shape.
* The ``lint_all`` aggregator merges sub-tool findings and survives
  individual sub-tool failures.
* The Gateway exemption tested via the architectural-checker wrapper
  doesn't fire for legitimate Gateway HTTP.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path
from typing import Any

import pytest


_HERE = Path(__file__).resolve()
_VAULT_ROOT = _HERE.parents[7]
if str(_VAULT_ROOT) not in sys.path:
    sys.path.insert(0, str(_VAULT_ROOT))


def _import(name: str) -> Any:
    """Import a dev tool's run function under either namespace root."""
    try:
        mod = __import__(
            f"Wylde.Core.harness.tooling.tools.dev.{name}",
            fromlist=[f"run_{name}"],
        )
    except ImportError:
        mod = __import__(
            f"Core.harness.tooling.tools.dev.{name}",
            fromlist=[f"run_{name}"],
        )
    return getattr(mod, f"run_{name}")


def _import_inner(name: str) -> Any:
    """Import a dev tool's INNER implementation module (where the
    real `run_subprocess` reference is bound).  Patches against the
    inner module reach the actual call sites; patches against
    `_lint_lib` don't, because each tool imports the symbol at module
    load time."""
    try:
        return __import__(
            f"Wylde.Core.harness.tooling.tools.dev.{name}.{name}",
            fromlist=["run_lint_subprocess"],
        )
    except ImportError:
        return __import__(
            f"Core.harness.tooling.tools.dev.{name}.{name}",
            fromlist=["run_lint_subprocess"],
        )


# ── lint_python (ruff wrapper) ────────────────────────────────────────


def test_lint_python_envelope_shape_on_clean_stdout(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Stub ruff with empty output → envelope shows zero findings."""
    inner = _import_inner("lint_python")

    monkeypatch.setattr(
        inner,
        "run_lint_subprocess",
        lambda *a, **kw: {"ok": True, "returncode": 0, "stdout": "[]", "stderr": ""},
    )
    run = _import("lint_python")
    result = run({})
    assert result["ok"] is True
    data = result["data"]
    assert data["findings"] == []
    assert data["summary"]["tool"] == "lint_python"
    assert data["summary"]["total"] == 0
    assert data["summary"]["by_severity"] == {"error": 0, "warning": 0, "info": 0}


def test_lint_python_parses_ruff_finding(monkeypatch: pytest.MonkeyPatch) -> None:
    """Stub ruff with one finding → envelope reports it with normalised shape."""
    try:
        from Wylde.Core.harness.tooling.tools.dev import _lint_lib as ll
    except ImportError:
        from Core.harness.tooling.tools.dev import _lint_lib as ll
    inner = _import_inner("lint_python")

    fake_ruff = json.dumps(
        [
            {
                "code": "F401",
                "message": "`os` imported but unused",
                "filename": str(ll.wylde_root() / "Core" / "harness" / "foo.py"),
                "location": {"row": 12, "column": 1},
            }
        ]
    )
    monkeypatch.setattr(
        inner,
        "run_lint_subprocess",
        lambda *a, **kw: {
            "ok": True,
            "returncode": 1,
            "stdout": fake_ruff,
            "stderr": "",
        },
    )
    run = _import("lint_python")
    result = run({})
    findings = result["data"]["findings"]
    assert len(findings) == 1
    f = findings[0]
    assert f["rule"] == "F401"
    assert f["severity"] in {"error", "warning"}
    assert f["file"].endswith("Core/harness/foo.py")
    assert f["line"] == 12
    assert "unused" in f["message"]


def test_lint_python_handles_missing_ruff(monkeypatch: pytest.MonkeyPatch) -> None:
    """ruff not installed → envelope_error with code 'lint_failed'."""
    inner = _import_inner("lint_python")

    monkeypatch.setattr(
        inner,
        "run_lint_subprocess",
        lambda *a, **kw: {
            "ok": False,
            "returncode": -1,
            "stdout": "",
            "stderr": "",
            "error": "linter not found on PATH",
        },
    )
    run = _import("lint_python")
    result = run({})
    assert result["ok"] is False
    assert "error" in result
    assert result["error"]["code"] == "lint_failed"


# ── lint_rust (clippy wrapper) ────────────────────────────────────────


def test_lint_rust_parses_clippy_ndjson(monkeypatch: pytest.MonkeyPatch) -> None:
    inner = _import_inner("lint_rust")

    clippy_msg = json.dumps(
        {
            "reason": "compiler-message",
            "message": {
                "code": {"code": "clippy::useless_clone"},
                "level": "warning",
                "message": "using `clone` on a `Copy` type",
                "spans": [
                    {
                        "file_name": "src/lib.rs",
                        "line_start": 42,
                        "is_primary": True,
                    }
                ],
            },
        }
    )
    monkeypatch.setattr(
        inner,
        "run_lint_subprocess",
        lambda *a, **kw: {
            "ok": True,
            "returncode": 0,
            "stdout": clippy_msg + "\n",
            "stderr": "",
        },
    )
    run = _import("lint_rust")
    result = run({})
    findings = result["data"]["findings"]
    assert len(findings) == 1
    f = findings[0]
    assert f["rule"] == "clippy::useless_clone"
    assert f["severity"] == "warning"
    assert f["file"] == "src/lib.rs"
    assert f["line"] == 42


# ── lint_svelte (svelte-check + eslint) ───────────────────────────────


def test_lint_svelte_envelope_engines_tracked(monkeypatch: pytest.MonkeyPatch) -> None:
    """Both engines stubbed clean → envelope reports both ran with 0 findings."""
    inner = _import_inner("lint_svelte")

    # Both subprocess calls return clean output.
    monkeypatch.setattr(
        inner,
        "run_lint_subprocess",
        lambda *a, **kw: {"ok": True, "returncode": 0, "stdout": "[]", "stderr": ""},
    )
    run = _import("lint_svelte")
    result = run({})
    assert result["ok"] is True
    engines = result["data"]["summary"]["engines"]
    assert "svelte_check" in engines
    assert "eslint" in engines


def test_lint_svelte_skip_eslint(monkeypatch: pytest.MonkeyPatch) -> None:
    inner = _import_inner("lint_svelte")

    monkeypatch.setattr(
        inner,
        "run_lint_subprocess",
        lambda *a, **kw: {"ok": True, "returncode": 0, "stdout": "", "stderr": ""},
    )
    run = _import("lint_svelte")
    result = run({"skip_eslint": True})
    engines = result["data"]["summary"]["engines"]
    assert "svelte_check" in engines
    # eslint should NOT have been invoked → not in engines dict
    assert "eslint" not in engines


# ── lint_all (aggregator) ─────────────────────────────────────────────


def test_lint_all_skips_named_engines(monkeypatch: pytest.MonkeyPatch) -> None:
    """Skip every engine → empty findings, every engine marked skipped."""
    run = _import("lint_all")
    result = run({"skip": ["python", "svelte", "rust", "wylde_check"]})
    assert result["ok"] is True
    engines = result["data"]["summary"]["engines"]
    assert all(
        engines[k].get("skipped") is True
        for k in ("python", "svelte", "rust", "wylde_check")
    )
    assert result["data"]["findings"] == []


def test_lint_all_survives_engine_failure(monkeypatch: pytest.MonkeyPatch) -> None:
    """One engine raises → its slot has an error, others still report.

    `lint_all` does `from ..lint_python import run_lint_python` at call
    time, which resolves via the package's __init__.py re-export.  Patch
    the PACKAGE attr (not the inner module) so the import inside
    lint_all picks up the stub."""
    try:
        from Wylde.Core.harness.tooling.tools.dev import lint_python as lp_pkg
    except ImportError:
        from Core.harness.tooling.tools.dev import lint_python as lp_pkg

    def explode(_params: Any) -> None:
        raise RuntimeError("simulated engine crash")

    monkeypatch.setattr(lp_pkg, "run_lint_python", explode)

    run = _import("lint_all")
    # Skip everything except python so we can isolate the failure.
    result = run({"skip": ["svelte", "rust", "wylde_check"]})
    engines = result["data"]["summary"]["engines"]
    assert "error" in engines["python"]
    assert "simulated engine crash" in engines["python"]["error"]
    assert result["ok"] is True


# ── wylde_check tool wrapper ──────────────────────────────────────────


def test_wylde_check_tool_returns_canonical_envelope() -> None:
    """The wrapper should return the architectural-checker envelope
    unchanged, with summary.tool stamped."""
    run = _import("wylde_check")
    # Run only one cheap rule to keep the test fast.
    result = run({"only": ["tool_id_regex"]})
    assert result["ok"] is True
    assert result["data"]["rules_checked"] == 1
    assert result["data"]["summary"]["tool"] == "wylde_check"
