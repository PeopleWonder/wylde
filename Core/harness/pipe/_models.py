"""models.* action handlers — registry surface + STT/TTS + Ollama-side ops.

Harness Slice 3b strangler-fig
------------------------------

Eight of the ten ``models.*`` verbs (``list``, ``get_profile``, ``show``,
``delete``, ``unload``, ``set_active``, ``set_default``, ``get_default``)
have Rust handlers in ``wylde-harness`` (Slice 3a, registered on
``\\\\.\\pipe\\wylde-harness``). ``WYLDE_HARNESS_MODELS_IMPL`` selects the
live path; **Slice 3b (2026-06-03) flipped the default from ``python`` to
``rust``**. When ``rust``, each entry point below forwards the action over
the harness pipe and returns the Rust reply verbatim, falling back to the
in-process Python body when the Rust pipe is unreachable / the handler is
still gated off (transport-class error). Set
``WYLDE_HARNESS_MODELS_IMPL=python`` to revert.

The two remaining verbs — ``models.transcribe`` / ``models.synthesize`` —
stay Python-only: they drive the Voice STT/TTS engines, which aren't hosted
in the harness crate, so there's no Rust handler to forward to. They are
deliberately excluded from :data:`_FORWARD_ACTIONS`.

Two deviations from the ``_chat.py`` precedent, both deliberate:

* ``not_found`` is **not** a transport-fallback code here. The harness is a
  pipe-only transport, so a ``not_found`` reply from a ``models.*`` forward
  is the *application-level* "model isn't installed" result
  ``models.show`` emits — it must surface, not silently re-run Python.
* A self-loop guard (:func:`_harness_is_local_server`) suppresses the
  forward when *this* process is itself the live Python harness pipe
  server. ``WYLDE_HARNESS_MODELS_IMPL`` is decoupled from the daemon's
  service-selection flag, so the ``python``-server + ``rust``-models
  misconfiguration is reachable; forwarding to ``wylde-harness`` from the
  server process would loop back into this very dispatcher. (``_chat.py``
  reuses one env var for both decisions, so it can't hit this.)
"""

from __future__ import annotations

import os
from typing import Any, Dict, Optional, cast

from ._common import (
    SERVICE_NAME,
    _ActionError,
    _model_state_module,
    _ollama_client_module,
    _payload_dict,
    logger,
)

# The eight verbs with a Rust handler (Slice 3a). transcribe / synthesize
# are Voice-engine-backed and stay Python-only — see the module docstring.
_FORWARD_ACTIONS = frozenset(
    {
        "models.list",
        "models.get_profile",
        "models.show",
        "models.delete",
        "models.unload",
        "models.set_active",
        "models.set_default",
        "models.get_default",
    }
)

# Error codes that mean "the Rust path didn't actually run" — fall back to
# the Python body. Mirrors _chat.py's set MINUS ``not_found`` (which is an
# application result for models.show on a pipe-only transport, not a
# missing-service signal) PLUS the 3a gate-off marker ``not_implemented``.
_TRANSPORT_FALLBACK_CODES = frozenset(
    {
        "pipe_unavailable",
        "pipe_connect",
        "pipe_timeout",
        "pipe_io",
        "handshake_timeout",
        "handshake_io",
        "handshake_rejected",
        "no_action",
        "not_implemented",
    }
)

# Ollama-side verbs (show/delete/unload) round-trip to wylde-ollama, which
# can be slow; give the forward more headroom than the IPC default.
_FORWARD_TIMEOUT = 30.0


def _models_impl() -> str:
    """Read ``WYLDE_HARNESS_MODELS_IMPL`` once per call.

    Default ``rust`` since Slice 3b (2026-06-03). Anything other than
    ``python`` / ``rust`` is clamped to the default — same fail-safe shape
    as the Rust-side ``rust_enabled()`` gate, so a typo can't silently
    strand the surface on a half-state. (The analogous chat-turn knob
    ``_chat._harness_turn_impl`` was retired in Phase 5.D when the Python
    chat driver was deleted — chat.* now forwards to Rust unconditionally.)
    """
    raw = os.environ.get("WYLDE_HARNESS_MODELS_IMPL")
    if raw is None:
        return "rust"
    val = raw.strip().lower()
    if val in ("python", "rust"):
        return val
    return "rust"


def _harness_is_local_server() -> bool:
    """True when *this* process is the live Python harness pipe server.

    In that case forwarding ``models.*`` to ``\\\\.\\pipe\\wylde-harness``
    would loop back into this same dispatcher (we'd be talking to
    ourselves). The caller treats this like a transport failure and runs
    the Python body locally. Default deployments run the *Rust* harness as
    the pipe server, so an importing client process (GUI, tests) never
    trips this — ``pipe._started`` is only set in the process that called
    ``pipe.start()``.
    """
    try:
        from Core.harness import pipe as _pkg
    except Exception:  # noqa: BLE001 — never let the guard mask a real call
        return False
    return bool(getattr(_pkg, "_started", False))


def _try_forward_models_to_rust(
    action: str, payload: Any, timeout: float = _FORWARD_TIMEOUT
) -> Optional[Dict[str, Any]]:
    """Forward a ``models.*`` action to the Rust ``wylde-harness`` pipe.

    Returns the Rust reply ``data`` (always a dict for these verbs) on
    success, ``None`` on a transport-class failure so the caller can fall
    back to the in-process Python body. Genuine service-level errors (the
    Rust handler returned ``ok=false`` with a non-transport code) are
    re-raised as :class:`_ActionError` so the pipe surfaces the same
    envelope shape a Python failure would.
    """
    # Only the eight Slice-3a verbs have a Rust handler. A caller asking to
    # forward anything else (transcribe/synthesize, a typo) stays on Python.
    if action not in _FORWARD_ACTIONS:
        return None
    # Self-call guard — see _harness_is_local_server. Run Python locally
    # rather than loop the pipe back into this dispatcher.
    if _harness_is_local_server():
        return None
    try:
        from Core.shared.ipc import send_action as _ipc_send_action
    except ImportError:  # pragma: no cover — IPC shim always present in prod
        return None
    try:
        reply = _ipc_send_action(SERVICE_NAME, action, payload, timeout=timeout)
    except Exception:  # noqa: BLE001 — transport failures fall back to Python
        return None

    if not getattr(reply, "ok", False):
        err = getattr(reply, "error", None) or {}
        code = err.get("code") if isinstance(err, dict) else None
        if code in _TRANSPORT_FALLBACK_CODES:
            # Rust unreachable / gated off / verb not registered → Python.
            return None
        # Genuine service-level failure (bad_request, not_found from
        # models.show, an Ollama outage, ...) — surface it as if the Python
        # body had raised so the caller's envelope shape stays consistent.
        message = ""
        if isinstance(err, dict):
            message = str(
                err.get("message") or err.get("code") or "rust models handler error"
            )
        raise _ActionError(str(code or "rust_models_error"), message)

    data = getattr(reply, "data", None)
    if not isinstance(data, dict):
        # All eight forwarded verbs return a dict envelope; anything else is
        # unexpected → fall back so the caller still gets a well-formed body.
        logger.warning(
            "harness models: %s forward returned non-dict data (%r) — Python fallback",
            action,
            type(data).__name__,
        )
        return None
    return data


def _models_list_action(payload: Any) -> Dict[str, Any]:
    r"""Return models known to the in-process registry, optionally
    filtered by ``kind`` (``llm`` | ``stt`` | ``tts`` | ``embed`` |
    ``vision``). The legacy GUI hit
    ``\\.\pipe\wylde-orchestrator /models/models`` for this.  # wylde-check: dead-ref-ok
    """
    if _models_impl() == "rust":
        forwarded = _try_forward_models_to_rust("models.list", payload)
        if forwarded is not None:
            return forwarded
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
    if _models_impl() == "rust":
        forwarded = _try_forward_models_to_rust("models.get_profile", payload)
        if forwarded is not None:
            return forwarded
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
    if _models_impl() == "rust":
        forwarded = _try_forward_models_to_rust("models.show", payload)
        if forwarded is not None:
            return forwarded
    p = _payload_dict(payload)
    name = p.get("name")
    if not isinstance(name, str) or not name:
        raise _ActionError("bad_request", "name is required")
    info: Optional[Dict[str, Any]] = _ollama_client_module().show_model(name)
    if info is None:
        raise _ActionError("not_found", f"model {name!r} not found")
    return info


def _models_delete_action(payload: Any) -> Dict[str, Any]:
    if _models_impl() == "rust":
        forwarded = _try_forward_models_to_rust("models.delete", payload)
        if forwarded is not None:
            return forwarded
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
    if _models_impl() == "rust":
        forwarded = _try_forward_models_to_rust("models.unload", payload)
        if forwarded is not None:
            return forwarded
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
    if _models_impl() == "rust":
        forwarded = _try_forward_models_to_rust("models.set_active", payload)
        if forwarded is not None:
            return forwarded
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
    if _models_impl() == "rust":
        forwarded = _try_forward_models_to_rust("models.set_default", payload)
        if forwarded is not None:
            return forwarded
    p = _payload_dict(payload)
    raw = p.get("model")
    if raw is not None and not isinstance(raw, str):
        raise _ActionError("bad_request", "model must be a string or null")
    new = _model_state_module().set_default_model(raw if isinstance(raw, str) else None)
    return {"ok": True, "model": new}


def _models_get_default_action(_payload: Any) -> Dict[str, Any]:
    """Return the starred default model: persisted choice, else the
    ``WYLDE_DEFAULT_MODEL`` env, else ``null``. Reply ``{model}``."""
    if _models_impl() == "rust":
        forwarded = _try_forward_models_to_rust("models.get_default", _payload)
        if forwarded is not None:
            return forwarded
    return {"model": _model_state_module().get_default_model()}
