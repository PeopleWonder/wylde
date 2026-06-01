"""config.py — environment + YAML config for the Voice service.

The service reads ``Voice/config.yaml`` next to this module on import and
overlays a small set of environment variables on top. Everything else is a
hard-coded default so the service can run without a config file (handy in
sandbox / smoke-test environments).

Environment overrides (all optional):

==============================  =============================================
``WYLDE_VOICE_CONFIG``          path to an alternative ``config.yaml``
``WYLDE_VOICE_WHISPER_BACKEND`` ``"cpu"`` (default) or ``"npu"`` — chooses
                                between faster-whisper and OpenVINO/NPU
``WYLDE_VOICE_STT_MODEL``       HF repo id for STT (e.g.
                                ``openai/whisper-small``)
``WYLDE_VOICE_TTS_VOICE``       Kokoro voice name (e.g. ``af_heart``)
==============================  =============================================

The Whisper *backend* is deliberately a runtime knob: install-time picks one
of CPU/NPU weights (see ``download_models.py``), but a developer who has
both sets cached can flip the env var without re-installing.
"""

from __future__ import annotations

import logging
import os
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Dict, Optional

import yaml

logger = logging.getLogger(__name__)

_HERE = Path(__file__).parent.resolve()
_DEFAULT_YAML = _HERE / "config.yaml"


def _load_yaml(path: Path) -> Dict[str, Any]:
    """Read ``config.yaml`` if it exists; tolerate a missing file."""
    if not path.is_file():
        return {}
    try:
        with path.open("r", encoding="utf-8") as fh:
            data = yaml.safe_load(fh) or {}
        if not isinstance(data, dict):
            logger.warning("Voice config: %s is not a mapping; ignoring", path)
            return {}
        return data
    except (OSError, yaml.YAMLError) as exc:
        logger.warning("Voice config: failed to read %s: %s", path, exc)
        return {}


@dataclass(frozen=True)
class AudioConfig:
    input_device: Optional[int]
    output_device: Optional[int]
    sample_rate: int
    tts_sample_rate: int


@dataclass(frozen=True)
class VadConfig:
    threshold: float
    silence_timeout_ms: int


@dataclass(frozen=True)
class SttConfig:
    backend: str  # "cpu" or "npu"
    model: str  # HF repo id, e.g. "openai/whisper-small"
    language: str
    load_in_8bit: bool


@dataclass(frozen=True)
class TtsConfig:
    voice: str  # Kokoro voice name
    speed: float


@dataclass(frozen=True)
class Config:
    audio: AudioConfig
    vad: VadConfig
    stt: SttConfig
    tts: TtsConfig


def _coerce_backend(value: str) -> str:
    v = value.strip().lower()
    if v not in ("cpu", "npu"):
        logger.warning("Voice config: unknown stt.backend %r; using 'cpu'", value)
        return "cpu"
    return v


def load(path: Optional[Path] = None) -> Config:
    """Build a frozen :class:`Config` from YAML + environment overrides."""
    yaml_path = path
    if yaml_path is None:
        env_path = os.getenv("WYLDE_VOICE_CONFIG")
        yaml_path = Path(env_path).expanduser() if env_path else _DEFAULT_YAML
    raw = _load_yaml(yaml_path)

    audio_raw = raw.get("audio") or {}
    vad_raw = raw.get("vad") or {}
    stt_raw = raw.get("stt") or {}
    tts_raw = raw.get("tts") or {}

    audio = AudioConfig(
        input_device=audio_raw.get("input_device"),
        output_device=audio_raw.get("output_device"),
        sample_rate=int(audio_raw.get("sample_rate", 16000)),
        tts_sample_rate=int(audio_raw.get("tts_sample_rate", 24000)),
    )
    vad = VadConfig(
        threshold=float(vad_raw.get("threshold", 0.65)),
        silence_timeout_ms=int(vad_raw.get("silence_timeout_ms", 1800)),
    )
    stt = SttConfig(
        backend=_coerce_backend(
            os.getenv("WYLDE_VOICE_WHISPER_BACKEND", str(stt_raw.get("backend", "cpu")))
        ),
        model=os.getenv(
            "WYLDE_VOICE_STT_MODEL", str(stt_raw.get("model", "openai/whisper-small"))
        ),
        language=str(stt_raw.get("language", "en")),
        load_in_8bit=bool(stt_raw.get("load_in_8bit", True)),
    )
    tts = TtsConfig(
        voice=os.getenv("WYLDE_VOICE_TTS_VOICE", str(tts_raw.get("voice", "af_heart"))),
        speed=float(tts_raw.get("speed", 1.0)),
    )
    return Config(audio=audio, vad=vad, stt=stt, tts=tts)


__all__ = ["Config", "AudioConfig", "VadConfig", "SttConfig", "TtsConfig", "load"]
