"""Unit tests for the updater.* lifecycle pipe verbs.

Drives the handlers directly (no pipe) the way control.py's other
action tests do, with ``_PREFS_PATH`` redirected to a per-test tmp file
so nothing touches the real ``data/preferences/updater.json``.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from Core.Lifecycle import updater_prefs
from Core.shared.ipc import IpcError


@pytest.fixture(autouse=True)
def sandboxed_prefs_path(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """Redirect the module's prefs file into a per-test tmp directory."""
    prefs = tmp_path / "preferences" / "updater.json"
    monkeypatch.setattr(updater_prefs, "_PREFS_PATH", prefs)
    return prefs


def test_get_prefs_missing_file_returns_defaults() -> None:
    out = updater_prefs.updater_get_prefs_action({})
    assert out == {
        "enabled": False,
        "auto_check": False,
        "frequency": "weekly",
        "last_checked": None,
    }


def test_get_prefs_accepts_none_payload() -> None:
    # The lifecycle dispatcher passes payload.get("payload") which is
    # None when the caller omits it.
    out = updater_prefs.updater_get_prefs_action(None)
    assert out["frequency"] == "weekly"


def test_set_prefs_persists_and_round_trips(sandboxed_prefs_path: Path) -> None:
    merged = updater_prefs.updater_set_prefs_action({"enabled": True})
    assert merged["enabled"] is True
    # File written atomically (no leftover temp).
    assert sandboxed_prefs_path.exists()
    assert not sandboxed_prefs_path.with_suffix(".json.tmp").exists()
    on_disk = json.loads(sandboxed_prefs_path.read_text(encoding="utf-8"))
    assert on_disk["enabled"] is True
    # A fresh read sees the persisted value.
    assert updater_prefs.updater_get_prefs_action({})["enabled"] is True


def test_set_prefs_merges_successive_partial_patches() -> None:
    updater_prefs.updater_set_prefs_action({"enabled": True})
    updater_prefs.updater_set_prefs_action({"auto_check": True})
    merged = updater_prefs.updater_set_prefs_action({"frequency": "daily"})
    # All three independent writes survive — the patch merges over the
    # prior on-disk shape rather than clobbering it.
    assert merged == {
        "enabled": True,
        "auto_check": True,
        "frequency": "daily",
        "last_checked": None,
    }


def test_set_prefs_accepts_last_checked_int_and_null() -> None:
    assert updater_prefs.updater_set_prefs_action({"last_checked": 1_700_000_000})[
        "last_checked"
    ] == 1_700_000_000
    assert (
        updater_prefs.updater_set_prefs_action({"last_checked": None})["last_checked"]
        is None
    )


def test_set_prefs_rejects_bad_frequency() -> None:
    with pytest.raises(IpcError) as exc:
        updater_prefs.updater_set_prefs_action({"frequency": "hourly"})
    assert exc.value.code == "bad_request"


def test_set_prefs_rejects_non_bool_enabled() -> None:
    with pytest.raises(IpcError) as exc:
        updater_prefs.updater_set_prefs_action({"enabled": "yes"})
    assert exc.value.code == "bad_request"


def test_set_prefs_rejects_bool_last_checked() -> None:
    # bool is an int subclass — must not masquerade as a timestamp.
    with pytest.raises(IpcError) as exc:
        updater_prefs.updater_set_prefs_action({"last_checked": True})
    assert exc.value.code == "bad_request"


def test_set_prefs_rejects_negative_last_checked() -> None:
    with pytest.raises(IpcError) as exc:
        updater_prefs.updater_set_prefs_action({"last_checked": -1})
    assert exc.value.code == "bad_request"


def test_set_prefs_rejects_non_object_payload() -> None:
    with pytest.raises(IpcError) as exc:
        updater_prefs.updater_set_prefs_action(["not", "a", "dict"])
    assert exc.value.code == "bad_request"


def test_set_prefs_ignores_unknown_keys() -> None:
    merged = updater_prefs.updater_set_prefs_action({"enabled": True, "bogus": 123})
    assert "bogus" not in merged
    assert merged["enabled"] is True


def test_get_prefs_degrades_corrupt_file_to_defaults(sandboxed_prefs_path: Path) -> None:
    sandboxed_prefs_path.parent.mkdir(parents=True, exist_ok=True)
    sandboxed_prefs_path.write_text("{ not json", encoding="utf-8")
    out = updater_prefs.updater_get_prefs_action({})
    assert out == dict(updater_prefs._DEFAULTS)


def test_get_prefs_drops_stale_unknown_on_disk_key(sandboxed_prefs_path: Path) -> None:
    sandboxed_prefs_path.parent.mkdir(parents=True, exist_ok=True)
    sandboxed_prefs_path.write_text(
        json.dumps({"enabled": True, "legacy_field": "x"}), encoding="utf-8"
    )
    out = updater_prefs.updater_get_prefs_action({})
    assert out["enabled"] is True
    assert "legacy_field" not in out


def test_actions_map_wires_both_verbs() -> None:
    assert set(updater_prefs.ACTIONS) == {"updater.get_prefs", "updater.set_prefs"}
    assert updater_prefs.ACTIONS["updater.get_prefs"] is updater_prefs.updater_get_prefs_action
    assert updater_prefs.ACTIONS["updater.set_prefs"] is updater_prefs.updater_set_prefs_action


def test_control_registers_updater_actions() -> None:
    # The daemon's ACTIONS map must surface the updater verbs so they
    # register on the wylde-lifecycle pipe alongside service.*.
    from Core.Lifecycle import control

    assert "updater.get_prefs" in control.ACTIONS
    assert "updater.set_prefs" in control.ACTIONS
