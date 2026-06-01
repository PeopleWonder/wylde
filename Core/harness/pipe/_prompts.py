"""prompts.* action handlers — system-prompt overrides and presets."""

from __future__ import annotations

from typing import Any, Dict

from ._common import _ActionError, _payload_dict


def _prompts_modules() -> tuple[Any, Any]:
    _sp: Any
    _cat: Any
    try:
        from Core.shared import system_prompts as _sp_mod
        from Core.shared import system_prompts_catalog as _cat_mod

        _sp = _sp_mod
        _cat = _cat_mod
    except ImportError:  # pragma: no cover — orchestrator-style bootstrap
        import system_prompts as _sp_mod2
        import system_prompts_catalog as _cat_mod2

        _sp = _sp_mod2
        _cat = _cat_mod2
    return _sp, _cat


def _prompts_envelope(store: Dict[str, Any], _cat: Any) -> Dict[str, Any]:
    return {
        "groups": _cat.groups_dicts(),
        "catalog": _cat.catalog_dicts(),
        "overrides": dict(store.get("overrides") or {}),
        "presets": {k: dict(v) for k, v in (store.get("presets") or {}).items()},
        "active_preset": store.get("active_preset") or "Default",
    }


def _prompts_list_action(_payload: Any) -> Dict[str, Any]:
    """Return groups + catalog + overrides + presets + active_preset.

    The Settings page hits this on mount; one round-trip covers everything
    it needs to render the prompt-editor section.
    """
    _sp, _cat = _prompts_modules()
    return _prompts_envelope(_sp.read_store(), _cat)


def _prompts_save_action(payload: Any) -> Dict[str, Any]:
    """Save an override for one prompt id. ``text=None`` (or matching the
    catalog default) clears the override."""
    p = _payload_dict(payload)
    pid = p.get("id")
    if not isinstance(pid, str) or not pid:
        raise _ActionError("bad_request", "id is required")
    text = p.get("text")
    if text is not None and not isinstance(text, str):
        raise _ActionError("bad_request", "text must be a string or null")
    _sp, _cat = _prompts_modules()
    try:
        store = _sp.set_override(pid, text)
    except ValueError as exc:
        raise _ActionError("bad_request", str(exc))
    return _prompts_envelope(store, _cat)


def _prompts_save_preset_action(payload: Any) -> Dict[str, Any]:
    """Snapshot the current overrides into a named preset and activate it."""
    p = _payload_dict(payload)
    name = p.get("name")
    if not isinstance(name, str) or not name.strip():
        raise _ActionError("bad_request", "name is required")
    _sp, _cat = _prompts_modules()
    try:
        store = _sp.save_preset(name)
    except ValueError as exc:
        raise _ActionError("bad_request", str(exc))
    return _prompts_envelope(store, _cat)


def _prompts_set_active_action(payload: Any) -> Dict[str, Any]:
    """Activate the named preset (or reset to catalog defaults for 'Default')."""
    p = _payload_dict(payload)
    name = p.get("name")
    if not isinstance(name, str) or not name:
        raise _ActionError("bad_request", "name is required")
    _sp, _cat = _prompts_modules()
    try:
        store = _sp.load_preset(name)
    except LookupError as exc:
        raise _ActionError("not_found", str(exc))
    except ValueError as exc:
        raise _ActionError("bad_request", str(exc))
    return _prompts_envelope(store, _cat)


def _prompts_delete_preset_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    name = p.get("name")
    if not isinstance(name, str) or not name:
        raise _ActionError("bad_request", "name is required")
    _sp, _cat = _prompts_modules()
    try:
        store = _sp.delete_preset(name)
    except ValueError as exc:
        raise _ActionError("bad_request", str(exc))
    return _prompts_envelope(store, _cat)
