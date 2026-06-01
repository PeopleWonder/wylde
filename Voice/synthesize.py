"""synthesize.py — Kokoro ONNX text-to-speech.

Single-model, multi-voice: one ONNX model + a combined ``voices.npz``
drives all voices, so switching voices is free (no model reload). The
service ships with the ``onnx-community/Kokoro-82M-v1.0-ONNX`` weights
materialised in the HuggingFace cache by ``download_models.py``.

Output is a float32 mono numpy array at 24 kHz (Kokoro's native rate).
Playback is the caller's responsibility — pipe into
:func:`Voice.device_manager.play_audio`.
"""

from __future__ import annotations

import logging
import threading
import types
from pathlib import Path
from typing import Any, List, Optional

import numpy as np

from .config import TtsConfig

logger = logging.getLogger(__name__)


# Kokoro repo id used for HF-cache lookup. download_models.py is the only
# place that downloads it; this module assumes it's already on disk.
KOKORO_REPO = "onnx-community/Kokoro-82M-v1.0-ONNX"
KOKORO_MODEL_FILE = "onnx/model.onnx"
KOKORO_VOICES_FILE = "voices.npz"  # built by download_models.py
KOKORO_SAMPLE_RATE = 24000


# Catalogue. Prefix codes: a=American, b=British; f=female, m=male.
KOKORO_VOICES: List[dict] = [
    {"name": "af_heart", "lang": "en-us", "gender": "female"},
    {"name": "af_alloy", "lang": "en-us", "gender": "female"},
    {"name": "af_aoede", "lang": "en-us", "gender": "female"},
    {"name": "af_bella", "lang": "en-us", "gender": "female"},
    {"name": "af_jessica", "lang": "en-us", "gender": "female"},
    {"name": "af_kore", "lang": "en-us", "gender": "female"},
    {"name": "af_nicole", "lang": "en-us", "gender": "female"},
    {"name": "af_nova", "lang": "en-us", "gender": "female"},
    {"name": "af_river", "lang": "en-us", "gender": "female"},
    {"name": "af_sarah", "lang": "en-us", "gender": "female"},
    {"name": "af_sky", "lang": "en-us", "gender": "female"},
    {"name": "am_adam", "lang": "en-us", "gender": "male"},
    {"name": "am_echo", "lang": "en-us", "gender": "male"},
    {"name": "am_eric", "lang": "en-us", "gender": "male"},
    {"name": "am_fenrir", "lang": "en-us", "gender": "male"},
    {"name": "am_liam", "lang": "en-us", "gender": "male"},
    {"name": "am_michael", "lang": "en-us", "gender": "male"},
    {"name": "am_onyx", "lang": "en-us", "gender": "male"},
    {"name": "am_puck", "lang": "en-us", "gender": "male"},
    {"name": "am_santa", "lang": "en-us", "gender": "male"},
    {"name": "bf_alice", "lang": "en-gb", "gender": "female"},
    {"name": "bf_emma", "lang": "en-gb", "gender": "female"},
    {"name": "bf_isabella", "lang": "en-gb", "gender": "female"},
    {"name": "bf_lily", "lang": "en-gb", "gender": "female"},
    {"name": "bm_daniel", "lang": "en-gb", "gender": "male"},
    {"name": "bm_fable", "lang": "en-gb", "gender": "male"},
    {"name": "bm_george", "lang": "en-gb", "gender": "male"},
    {"name": "bm_lewis", "lang": "en-gb", "gender": "male"},
]
_VOICE_NAMES = {v["name"] for v in KOKORO_VOICES}


# --------------------------------------------------------------------------- #
# Cache lookup                                                                #
# --------------------------------------------------------------------------- #


def _kokoro_snapshot_dir() -> Path:
    """Return the resolved on-disk path of the Kokoro repo snapshot.

    Uses ``huggingface_hub.snapshot_download`` with ``local_files_only=True``
    so we never accidentally reach for the network here — that's
    ``download_models.py``'s job.
    """
    from huggingface_hub import snapshot_download

    snapshot = snapshot_download(
        repo_id=KOKORO_REPO,
        local_files_only=True,
    )
    return Path(snapshot)


def list_voices(language: Optional[str] = None) -> List[dict]:
    """Return the voice catalogue, optionally filtered by language prefix."""
    out: List[dict] = []
    for v in KOKORO_VOICES:
        if language and not v["lang"].startswith(language.lower().replace("_", "-")):
            continue
        out.append(
            {
                "name": v["name"],
                "language": v["lang"],
                "gender": v["gender"],
                "quality": "high",
                "num_speakers": 1,
            }
        )
    return out


# --------------------------------------------------------------------------- #
# Kokoro speed-dtype patch                                                    #
# --------------------------------------------------------------------------- #


def _patch_kokoro_speed(kokoro: Any) -> None:
    """Coerce the ``speed`` input to float32.

    ``kokoro_onnx`` ships an int32 ``speed`` array; the ONNX session expects
    a float scalar and rejects the request. Override ``_create_audio`` with a
    copy that always passes ``np.float32``.
    """
    from kokoro_onnx.config import MAX_PHONEME_LENGTH as _MAX_PH

    def _fixed_create_audio(self: Any, phonemes: Any, voice: Any, speed: Any) -> Any:
        phonemes = phonemes[:_MAX_PH]
        tokens = np.array(self.tokenizer.tokenize(phonemes), dtype=np.int64)
        voice = voice[len(tokens)]
        input_ids = np.array([[0, *tokens, 0]], dtype=np.int64)
        if "input_ids" in [i.name for i in self.sess.get_inputs()]:
            inputs = {
                "input_ids": input_ids,
                "style": np.array(voice, dtype=np.float32),
                "speed": np.array([speed], dtype=np.float32),
            }
        else:
            inputs = {
                "tokens": input_ids,
                "style": np.array(voice, dtype=np.float32),
                "speed": np.ones(1, dtype=np.float32) * speed,
            }
        audio = self.sess.run(None, inputs)[0]
        return audio, KOKORO_SAMPLE_RATE

    kokoro._create_audio = types.MethodType(_fixed_create_audio, kokoro)


# --------------------------------------------------------------------------- #
# Synthesizer                                                                 #
# --------------------------------------------------------------------------- #


class Synthesizer:
    """Thread-safe Kokoro TTS handle. Construct cheaply, ``load`` once."""

    def __init__(self, cfg: TtsConfig) -> None:
        self._cfg = cfg
        self._voice = cfg.voice
        self._speed = cfg.speed
        self._lock = threading.Lock()
        self._kokoro: Any = None
        self._loaded = False

    # -- lifecycle -------------------------------------------------------- #

    def load(self) -> bool:
        with self._lock:
            if self._loaded:
                return True
        try:
            snapshot = _kokoro_snapshot_dir()
            onnx_path = snapshot / KOKORO_MODEL_FILE
            voices_path = snapshot / KOKORO_VOICES_FILE
            if not onnx_path.is_file() or not voices_path.is_file():
                logger.error(
                    "Kokoro assets missing in %s. Run Voice/download_models.py.",
                    snapshot,
                )
                return False
            from kokoro_onnx import Kokoro

            kokoro = Kokoro(str(onnx_path), str(voices_path))
            _patch_kokoro_speed(kokoro)
            with self._lock:
                self._kokoro = kokoro
                self._loaded = True
            logger.info(
                "Synthesizer loaded (voice=%s, rate=%d)",
                self._voice,
                KOKORO_SAMPLE_RATE,
            )
            return True
        except Exception as exc:
            logger.error("Synthesizer load failed: %s", exc)
            return False

    # -- API -------------------------------------------------------------- #

    def synthesize(
        self,
        text: str,
        *,
        voice: Optional[str] = None,
        speed: Optional[float] = None,
    ) -> np.ndarray:
        """Return float32 audio at 24 kHz, or an empty array on failure."""
        if not self._loaded or not text.strip():
            return np.zeros(0, dtype=np.float32)

        with self._lock:
            kokoro = self._kokoro
            v = voice or self._voice
            spd = speed if speed is not None else self._speed

        if kokoro is None:
            return np.zeros(0, dtype=np.float32)
        try:
            samples, _ = kokoro.create(text, voice=v, speed=spd)
            audio = np.asarray(samples, dtype=np.float32).ravel()
            peak = float(np.abs(audio).max())
            if peak > 0:
                audio = audio / peak * 0.95
            return audio
        except Exception as exc:
            logger.error("Synthesis failed: %s", exc)
            return np.zeros(0, dtype=np.float32)

    def switch_voice(self, voice: str) -> bool:
        """Pick a different Kokoro voice (no model reload)."""
        if voice not in _VOICE_NAMES:
            logger.warning("Unknown Kokoro voice %r", voice)
            return False
        with self._lock:
            self._voice = voice
        logger.info("Synthesizer voice → %s", voice)
        return True

    @property
    def loaded(self) -> bool:
        return self._loaded

    @property
    def voice(self) -> str:
        return self._voice

    @property
    def sample_rate(self) -> int:
        return KOKORO_SAMPLE_RATE


__all__ = ["Synthesizer", "list_voices", "KOKORO_SAMPLE_RATE", "KOKORO_REPO"]
