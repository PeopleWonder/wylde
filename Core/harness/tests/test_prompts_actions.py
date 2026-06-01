"""Smoke for the ``prompts.*`` pipe actions.

Each handler wraps ``Core.shared.system_prompts`` (override store) and
``Core.shared.system_prompts_catalog`` (defaults). We point the override
store at a fresh tmp file via ``WYLDE_ROOT`` + a reload so the tests
don't touch the real ``data/system_prompts.json``.
"""

from __future__ import annotations

from typing import Any, Generator

import sys
from pathlib import Path

import pytest

_HERE = Path(__file__).resolve()
_VAULT_ROOT = _HERE.parents[4]
_WYLDE_ROOT = _HERE.parents[3]
# Both paths so `Wylde.Core.X` (vault-root style) and `Core.X`
# (wylde-root style) both resolve — the prompt-store fallback chain
# inside ``Core/shared/system_prompts.py`` needs ``Core`` reachable
# during module reload.
for _p in (_VAULT_ROOT, _WYLDE_ROOT):
    if str(_p) not in sys.path:
        sys.path.insert(0, str(_p))


@pytest.fixture
def isolated_prompts(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> Generator[Any, None, None]:
    """Redirect the override-store path at a tmp file so each test gets a
    pristine ``system_prompts.json`` and the real config (if any) stays
    untouched. Monkey-patching the module-level path is simpler than
    reloading the module — the same lock/cache state is reused, just
    pointed at a different file."""
    import importlib as _importlib

    try:
        sp = _importlib.import_module("Core.shared.system_prompts")
        harness_pipe = _importlib.import_module("Core.harness.pipe")
    except ImportError:
        sp = _importlib.import_module("Wylde.Core.shared.system_prompts")
        harness_pipe = _importlib.import_module("Wylde.Core.harness.pipe")
    tmp_overrides = tmp_path / "data" / "system_prompts.json"
    monkeypatch.setattr(sp, "_OVERRIDES_PATH", tmp_overrides)
    sp.reload()  # drop any leftover cache so the new path is consulted.
    yield harness_pipe, sp
    sp.reload()  # restore a clean cache for the next test.


def _existing_prompt_id() -> str:
    """Pull the first catalog id so tests don't hard-code a string that
    might change as the catalog grows."""
    try:
        from Core.shared import system_prompts_catalog as cat
    except ImportError:
        from Wylde.Core.shared import system_prompts_catalog as cat  # type: ignore[no-redef]
    ids = cat.all_ids()
    assert ids, "catalog is empty — test setup is broken"
    return ids[0]


def test_prompts_actions_registered(isolated_prompts: Any) -> None:
    harness_pipe, _ = isolated_prompts
    for name in (
        "prompts.list",
        "prompts.save",
        "prompts.save_preset",
        "prompts.set_active",
        "prompts.delete_preset",
    ):
        assert name in harness_pipe._ACTIONS, f"{name} missing from _ACTIONS"


def test_prompts_list_returns_groups_and_catalog(isolated_prompts: Any) -> None:
    harness_pipe, _ = isolated_prompts
    resp = harness_pipe._prompts_list_action(None)
    assert isinstance(resp.get("groups"), list)
    assert isinstance(resp.get("catalog"), list)
    assert resp["catalog"], "catalog should be non-empty"
    assert resp.get("overrides") == {}
    assert resp.get("presets") == {}
    assert resp.get("active_preset") == "Default"
    # Group entries should carry id/label/blurb.
    sample = resp["groups"][0]
    assert {"id", "label", "blurb"} <= set(sample.keys())
    # Catalog entries should carry id/group/label/desc/default.
    centry = resp["catalog"][0]
    assert {"id", "group", "label", "desc", "default"} <= set(centry.keys())


def test_prompts_save_persists_override(isolated_prompts: Any) -> None:
    harness_pipe, _ = isolated_prompts
    pid = _existing_prompt_id()
    resp = harness_pipe._prompts_save_action({"id": pid, "text": "CUSTOM"})
    assert resp["overrides"].get(pid) == "CUSTOM"


def test_prompts_save_null_text_clears_override(isolated_prompts: Any) -> None:
    harness_pipe, _ = isolated_prompts
    pid = _existing_prompt_id()
    harness_pipe._prompts_save_action({"id": pid, "text": "CUSTOM"})
    resp = harness_pipe._prompts_save_action({"id": pid, "text": None})
    assert pid not in resp["overrides"]


def test_prompts_save_unknown_id_raises(isolated_prompts: Any) -> None:
    harness_pipe, _ = isolated_prompts
    with pytest.raises(Exception) as exc_info:
        harness_pipe._prompts_save_action({"id": "no.such.prompt", "text": "x"})
    assert getattr(exc_info.value, "code", None) == "bad_request"


def test_prompts_save_preset_snapshots_and_activates(isolated_prompts: Any) -> None:
    harness_pipe, _ = isolated_prompts
    pid = _existing_prompt_id()
    harness_pipe._prompts_save_action({"id": pid, "text": "CUSTOM"})
    resp = harness_pipe._prompts_save_preset_action({"name": "MyPreset"})
    assert "MyPreset" in resp["presets"]
    assert resp["presets"]["MyPreset"].get(pid) == "CUSTOM"
    assert resp["active_preset"] == "MyPreset"


def test_prompts_save_preset_rejects_default_name(isolated_prompts: Any) -> None:
    harness_pipe, _ = isolated_prompts
    with pytest.raises(Exception) as exc_info:
        harness_pipe._prompts_save_preset_action({"name": "Default"})
    assert getattr(exc_info.value, "code", None) == "bad_request"


def test_prompts_set_active_default_clears_overrides(isolated_prompts: Any) -> None:
    harness_pipe, _ = isolated_prompts
    pid = _existing_prompt_id()
    harness_pipe._prompts_save_action({"id": pid, "text": "CUSTOM"})
    resp = harness_pipe._prompts_set_active_action({"name": "Default"})
    assert resp["overrides"] == {}
    assert resp["active_preset"] == "Default"


def test_prompts_set_active_unknown_preset_raises_not_found(
    isolated_prompts: Any,
) -> None:
    harness_pipe, _ = isolated_prompts
    with pytest.raises(Exception) as exc_info:
        harness_pipe._prompts_set_active_action({"name": "no-such-preset"})
    assert getattr(exc_info.value, "code", None) == "not_found"


def test_prompts_delete_preset_removes_named_bundle(isolated_prompts: Any) -> None:
    harness_pipe, _ = isolated_prompts
    pid = _existing_prompt_id()
    harness_pipe._prompts_save_action({"id": pid, "text": "CUSTOM"})
    harness_pipe._prompts_save_preset_action({"name": "Doomed"})
    resp = harness_pipe._prompts_delete_preset_action({"name": "Doomed"})
    assert "Doomed" not in resp["presets"]
    # The active preset falls back to Default once the active one is gone.
    assert resp["active_preset"] == "Default"


def test_prompts_delete_preset_rejects_default(isolated_prompts: Any) -> None:
    harness_pipe, _ = isolated_prompts
    with pytest.raises(Exception) as exc_info:
        harness_pipe._prompts_delete_preset_action({"name": "Default"})
    assert getattr(exc_info.value, "code", None) == "bad_request"
