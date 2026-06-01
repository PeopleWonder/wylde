"""models.* action handlers — registry surface + STT/TTS + Ollama-side ops."""

from __future__ import annotations

from typing import Any, Dict, Optional, cast

from ._common import (
    _ActionError,
    _model_state_module,
    _ollama_client_module,
    _payload_dict,
)


def _models_list_action(payload: Any) -> Dict[str, Any]:
    r"""Return models known to the in-process registry, optionally
    filtered by ``kind`` (``llm`` | ``stt`` | ``tts`` | ``embed`` |
    ``vision``). The legacy GUI hit
    ``\\.\pipe\wylde-orchestrator /models/models`` for this.  # wylde-check: dead-ref-ok
    """
    try:
        from ..model_registry import list_models
    except ImportError:
        from Core.harness.model_registry import list_models
    kind: Optional[str] = None
    if isinstance(payload, dict):
        raw_kind = payload.get("kind")
        if isinstance(raw_kind, str) and raw_kind:
            kind = raw_kind
    try:
        entries = list_models(kind=cast(Any, kind)) if kind else list_models()
    except (ValueError, TypeError) as exc:
        raise _ActionError("bad_request", f"list_models rejected kind: {exc}")
    out = []
    for entry in entries:
        if hasattr(entry, "__dict__"):
            out.append(dict(entry.__dict__))
        elif isinstance(entry, dict):
            out.append(entry)
        else:
            out.append({"value": str(entry)})
    return {"models": out, "count": len(out), "kind": kind or "all"}


def _models_transcribe_action(payload: Any) -> Dict[str, Any]:
    """Speech-to-text. Voice service hits this with audio bytes; the
    harness runs Whisper (or whatever STT engine the deployment
    configured) and returns the transcript.

    Payload shape: ``{audio_b64, sample_rate?, sample_dtype?,
    language?, model?}``. Audio is base64-encoded so msgpack envelopes
    round-trip cleanly. ``sample_dtype`` is ``"int16"`` (default —
    matches the Voice service's ``record.py`` raw mic output) or
    ``"float32"``.

    The implementation lives at ``Voice.transcribe.Transcriber`` for
    now — that's a transitional path noted in the design (Voice owns
    the code; clients reach it ONLY through this pipe action). A
    follow-up will move the engine into the harness proper.
    """
    p = _payload_dict(payload)
    audio_b64 = p.get("audio_b64")
    if not isinstance(audio_b64, str) or not audio_b64:
        raise _ActionError("bad_request", "audio_b64 is required")
    import base64

    try:
        audio_bytes = base64.b64decode(audio_b64)
    except Exception as exc:  # noqa: BLE001
        raise _ActionError("bad_request", f"audio_b64 decode failed: {exc}")
    language = p.get("language") if isinstance(p.get("language"), str) else None
    model = p.get("model") if isinstance(p.get("model"), str) else None

    try:
        from Voice import transcribe as _voice_transcribe
        from Voice.config import load as _voice_config_load
        import numpy as _np
    except ImportError as exc:
        raise _ActionError("unavailable", f"transcribe backend not importable: {exc}")

    sample_dtype = p.get("sample_dtype") or "int16"
    try:
        if sample_dtype == "float32":
            audio = _np.frombuffer(audio_bytes, dtype=_np.float32)
        else:
            audio_i16 = _np.frombuffer(audio_bytes, dtype=_np.int16)
            audio = audio_i16.astype(_np.float32) / 32768.0
    except Exception as exc:  # noqa: BLE001
        raise _ActionError("bad_request", f"audio decode failed: {exc}")

    cfg = _voice_config_load()
    transcriber = _voice_transcribe.Transcriber(cfg.stt)
    if not getattr(transcriber, "loaded", False):
        try:
            transcriber.load()
        except Exception as exc:  # noqa: BLE001
            raise _ActionError("unavailable", f"STT engine load failed: {exc}")
    try:
        text = transcriber.transcribe(audio, language=language)
    except Exception as exc:  # noqa: BLE001
        raise _ActionError("transcribe_failed", str(exc))
    return {
        "text": text or "",
        "model": model or getattr(cfg.stt, "model", "") or "",
        "sample_rate": int(p.get("sample_rate") or 16000),
    }


def _models_synthesize_action(payload: Any) -> Dict[str, Any]:
    """Text-to-speech. Voice hits this with a string; the harness
    returns audio bytes (base64-encoded float32 PCM at the
    synthesizer's native sample rate).

    Same transitional note as ``models.transcribe``: the implementation
    lives under ``Voice/`` for now and is accessible only via this
    pipe action. Voice's own orchestrator NEVER imports the engine
    directly — it always round-trips through the harness pipe so the
    architectural separation the Wylde user locked in holds at the API level.
    """
    p = _payload_dict(payload)
    text = p.get("text")
    if not isinstance(text, str) or not text.strip():
        raise _ActionError("bad_request", "text is required")
    voice = p.get("voice") if isinstance(p.get("voice"), str) else None
    speed = p.get("speed")
    if not isinstance(speed, (int, float)):
        speed = None

    try:
        from Voice import synthesize as _voice_synth
        from Voice.config import load as _voice_config_load
        import numpy as _np
    except ImportError as exc:
        raise _ActionError("unavailable", f"TTS backend not importable: {exc}")

    cfg = _voice_config_load()
    synth = _voice_synth.Synthesizer(cfg.tts)
    if not getattr(synth, "loaded", False):
        try:
            synth.load()
        except Exception as exc:  # noqa: BLE001
            raise _ActionError("unavailable", f"TTS engine load failed: {exc}")
    try:
        audio = synth.synthesize(text, voice=voice, speed=speed)
    except Exception as exc:  # noqa: BLE001
        raise _ActionError("synthesize_failed", str(exc))
    audio = _np.asarray(audio, dtype=_np.float32)
    import base64

    audio_b64 = base64.b64encode(audio.tobytes()).decode("ascii")
    return {
        "audio_b64": audio_b64,
        "sample_rate": int(getattr(synth, "sample_rate", 24000)),
        "format": "float32_pcm",
        "voice": voice or "",
    }


def _models_get_profile_action(payload: Any) -> Dict[str, Any]:
    """Return the routing profile for a model name (backend, backend_model, etc).

    Mirrors the ``model_registry.get_profile`` signature used internally
    by ``backend_routing._lookup_profile``.
    """
    p = _payload_dict(payload)
    name = p.get("name")
    if not isinstance(name, str) or not name:
        raise _ActionError("bad_request", "name is required")
    try:
        from ..model_registry import get_profile
    except ImportError:
        from Core.harness.model_registry import get_profile
    profile = get_profile(name)
    return {"name": name, "profile": profile or {}}


# ── Ollama-side ops ───────────────────────────────────────────────────


def _models_show_action(payload: Any) -> Dict[str, Any]:
    """Fetch ``/api/show`` metadata for a locally-installed Ollama model."""
    p = _payload_dict(payload)
    name = p.get("name")
    if not isinstance(name, str) or not name:
        raise _ActionError("bad_request", "name is required")
    info: Optional[Dict[str, Any]] = _ollama_client_module().show_model(name)
    if info is None:
        raise _ActionError("not_found", f"model {name!r} not found")
    return info


def _models_delete_action(payload: Any) -> Dict[str, Any]:
    p = _payload_dict(payload)
    name = p.get("name")
    if not isinstance(name, str) or not name:
        raise _ActionError("bad_request", "name is required")
    ok = _ollama_client_module().delete_model(name)
    if ok:
        _model_state_module().forget_model(name)
    return {"ok": ok, "name": name}


def _models_unload_action(payload: Any) -> Dict[str, Any]:
    """Evict the model from VRAM via empty-prompt /api/generate, keep_alive=0."""
    p = _payload_dict(payload)
    name = p.get("name")
    if not isinstance(name, str) or not name:
        raise _ActionError("bad_request", "name is required")
    ok = _ollama_client_module().unload_model(name)
    if ok:
        _model_state_module().forget_model(name)
    return {"ok": ok, "name": name}


def _models_set_active_action(payload: Any) -> Dict[str, Any]:
    """Persist the active-model selection. Empty string clears it."""
    p = _payload_dict(payload)
    raw = p.get("model")
    if raw is not None and not isinstance(raw, str):
        raise _ActionError("bad_request", "model must be a string or omitted")
    new = _model_state_module().set_active_model(raw if isinstance(raw, str) else None)
    return {"model": new or ""}


def _models_set_default_action(payload: Any) -> Dict[str, Any]:
    """Persist the user's starred default model. ``null`` / empty string
    clears the choice (reads then fall back to ``WYLDE_DEFAULT_MODEL``).
    Distinct from ``models.set_active``: the default is the long-lived
    "start new chats with this" preference the Models panel's star drives;
    active is the inference bar's current pick.

    Reply ``{ok, model}`` where ``model`` is the persisted value (``null``
    when cleared)."""
    p = _payload_dict(payload)
    raw = p.get("model")
    if raw is not None and not isinstance(raw, str):
        raise _ActionError("bad_request", "model must be a string or null")
    new = _model_state_module().set_default_model(raw if isinstance(raw, str) else None)
    return {"ok": True, "model": new}


def _models_get_default_action(_payload: Any) -> Dict[str, Any]:
    """Return the starred default model: persisted choice, else the
    ``WYLDE_DEFAULT_MODEL`` env, else ``null``. Reply ``{model}``."""
    return {"model": _model_state_module().get_default_model()}
