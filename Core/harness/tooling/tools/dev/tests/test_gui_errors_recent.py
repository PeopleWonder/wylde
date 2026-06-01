"""Tests for the ``gui_errors_recent`` dev tool.

Drives ``run_gui_errors_recent`` against a synthetic
``logs/gui_errors.jsonl`` under a tmp ``WYLDE_ROOT``, covering
tail-first ordering, the limit cap/floor, every filter
(since / severity / source / route), and corrupt-line tolerance.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path
from typing import Any, Dict, List

import pytest

_HERE = Path(__file__).resolve()
_VAULT_ROOT = _HERE.parents[7]
if str(_VAULT_ROOT) not in sys.path:
    sys.path.insert(0, str(_VAULT_ROOT))


def _import() -> Any:
    """Import the tool's run function under either namespace root."""
    try:
        from Wylde.Core.harness.tooling.tools.dev.gui_errors_recent import (
            run_gui_errors_recent,
        )
    except ImportError:
        from Core.harness.tooling.tools.dev.gui_errors_recent import (
            run_gui_errors_recent,
        )
    return run_gui_errors_recent


def _event(i: int, **overrides: Any) -> Dict[str, Any]:
    base: Dict[str, Any] = {
        "timestamp_iso": f"2026-05-22T10:{i:02d}:00Z",
        "source": "window_error",
        "message": f"event {i}",
        "stack": None,
        "route": "dashboard",
        "severity": "error",
        "context": {},
    }
    base.update(overrides)
    return base


def _write_log(root: Path, records: List[Dict[str, Any]]) -> None:
    log = root / "logs" / "gui_errors.jsonl"
    log.parent.mkdir(parents=True, exist_ok=True)
    log.write_text("".join(json.dumps(r) + "\n" for r in records), encoding="utf-8")


@pytest.fixture()
def wylde_root(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """Point the tool at an isolated repo root for the duration of a test."""
    monkeypatch.setenv("WYLDE_ROOT", str(tmp_path))
    return tmp_path


# ── Basic reads ────────────────────────────────────────────────────────


def test_missing_log_returns_empty(wylde_root: Path) -> None:
    result = _import()({})
    assert result == {"events": [], "count": 0, "total_in_log": 0}


def test_returns_events_newest_first(wylde_root: Path) -> None:
    _write_log(wylde_root, [_event(0), _event(1), _event(2)])
    result = _import()({})
    assert result["total_in_log"] == 3
    assert result["count"] == 3
    assert [e["message"] for e in result["events"]] == [
        "event 2",
        "event 1",
        "event 0",
    ]


# ── Limit ──────────────────────────────────────────────────────────────


def test_limit_caps_returned_events(wylde_root: Path) -> None:
    _write_log(wylde_root, [_event(i) for i in range(10)])
    result = _import()({"limit": 3})
    assert result["count"] == 3
    assert result["total_in_log"] == 10
    assert [e["message"] for e in result["events"]] == [
        "event 9",
        "event 8",
        "event 7",
    ]


def test_limit_is_clamped_and_coerced(wylde_root: Path) -> None:
    _write_log(wylde_root, [_event(i) for i in range(5)])
    run = _import()
    # Above the 200 cap — only the 5 real events exist anyway.
    assert run({"limit": 9999})["count"] == 5
    # Below the floor of 1.
    assert run({"limit": 0})["count"] == 1
    # Non-numeric → default of 20.
    assert run({"limit": "nonsense"})["count"] == 5


# ── Filters ────────────────────────────────────────────────────────────


def test_severity_filter(wylde_root: Path) -> None:
    _write_log(
        wylde_root,
        [
            _event(0, severity="error"),
            _event(1, severity="warn"),
            _event(2, severity="info"),
        ],
    )
    result = _import()({"severity": "warn"})
    assert result["count"] == 1
    assert result["total_in_log"] == 3
    assert result["events"][0]["message"] == "event 1"


def test_source_filter(wylde_root: Path) -> None:
    _write_log(
        wylde_root,
        [_event(0, source="window_error"), _event(1, source="toast_error")],
    )
    result = _import()({"source": "toast_error"})
    assert [e["message"] for e in result["events"]] == ["event 1"]


def test_route_filter(wylde_root: Path) -> None:
    _write_log(
        wylde_root,
        [_event(0, route="dashboard"), _event(1, route="settings")],
    )
    result = _import()({"route": "settings"})
    assert [e["message"] for e in result["events"]] == ["event 1"]


def test_since_filter(wylde_root: Path) -> None:
    # Events at 10:00, 10:05, 10:10.
    _write_log(wylde_root, [_event(0), _event(5), _event(10)])
    result = _import()({"since": "2026-05-22T10:05:00Z"})
    assert [e["message"] for e in result["events"]] == ["event 10", "event 5"]
    assert result["total_in_log"] == 3


def test_combined_filters(wylde_root: Path) -> None:
    _write_log(
        wylde_root,
        [
            _event(0, severity="error", route="dashboard"),
            _event(1, severity="warn", route="dashboard"),
            _event(2, severity="error", route="settings"),
        ],
    )
    result = _import()({"severity": "error", "route": "dashboard"})
    assert [e["message"] for e in result["events"]] == ["event 0"]


# ── Robustness ─────────────────────────────────────────────────────────


def test_corrupt_line_is_skipped(wylde_root: Path) -> None:
    log = wylde_root / "logs" / "gui_errors.jsonl"
    log.parent.mkdir(parents=True, exist_ok=True)
    log.write_text(
        json.dumps(_event(0))
        + "\n{ this is not valid json\n"
        + json.dumps(_event(1))
        + "\n",
        encoding="utf-8",
    )
    result = _import()({})
    assert result["total_in_log"] == 2
    assert result["count"] == 2
