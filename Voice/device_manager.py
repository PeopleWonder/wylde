"""device_manager.py — audio device enumeration + speaker playback.

Capture (microphone) is owned by :mod:`record`; output is owned here so
``synthesize`` can stay a pure text→array function. Playback is done in a
short-lived child process: PiperVoice/onnxruntime sharing a process with
sounddevice causes garbled output on Windows, so we isolate the audio
driver behind ``winsound`` in a subprocess. The same pattern works on
non-Windows platforms via ``sounddevice.play``.
"""

from __future__ import annotations

import logging
import os
import platform
import subprocess
import sys
import tempfile
import threading
import wave
from typing import List, Optional

import numpy as np

try:
    import sounddevice as sd
except Exception as exc:  # pragma: no cover - exercised only without portaudio
    sd = None
    _SOUNDDEVICE_IMPORT_ERROR: Optional[BaseException] = exc
else:
    _SOUNDDEVICE_IMPORT_ERROR = None

logger = logging.getLogger(__name__)


def _require_sd() -> None:
    if sd is None:
        raise RuntimeError(
            "sounddevice is unavailable; install it (pip install sounddevice) and "
            "ensure portaudio is on PATH."
        ) from _SOUNDDEVICE_IMPORT_ERROR


def list_devices() -> List[dict]:
    """Return one dict per audio device with input/output channel counts."""
    _require_sd()
    devices = sd.query_devices()
    try:
        default_in, default_out = sd.default.device
    except Exception:
        default_in = default_out = -1
    out: List[dict] = []
    for idx, dev in enumerate(devices):
        out.append(
            {
                "index": idx,
                "name": dev["name"],
                "max_input_channels": dev["max_input_channels"],
                "max_output_channels": dev["max_output_channels"],
                "default_samplerate": dev["default_samplerate"],
                "is_default_input": idx == default_in,
                "is_default_output": idx == default_out,
            }
        )
    return out


def default_device_info() -> dict:
    """Return ``{"input": ..., "output": ...}`` for the system defaults."""
    _require_sd()
    try:
        default_in, default_out = sd.default.device
        inp = sd.query_devices(default_in)
        outp = sd.query_devices(default_out)
        return {
            "input": {"name": inp["name"], "index": default_in},
            "output": {"name": outp["name"], "index": default_out},
        }
    except Exception as exc:
        return {"error": str(exc)}


_play_lock = threading.Lock()


def _wav_bytes_path(audio: np.ndarray, sample_rate: int) -> str:
    """Encode ``audio`` to a temporary 16-bit PCM WAV; return the path."""
    int16 = (audio * 32767).clip(-32768, 32767).astype(np.int16)
    tmp = tempfile.NamedTemporaryFile(suffix=".wav", delete=False)
    try:
        with wave.open(tmp, "wb") as wf:
            wf.setnchannels(1)
            wf.setsampwidth(2)
            wf.setframerate(sample_rate)
            wf.writeframes(int16.tobytes())
    finally:
        tmp.close()
    return tmp.name


def play_audio(
    audio: np.ndarray,
    sample_rate: int,
    *,
    blocking: bool = True,
    output_device: Optional[int] = None,
) -> None:
    """Play a float32 audio array through the configured output device.

    On Windows we shell out to ``winsound`` in a child process to keep
    PortAudio out of the same process as onnxruntime / OpenVINO. Elsewhere
    we use ``sounddevice.play``; failures are logged but never raised — a
    silent speaker should not bring down the caller.
    """
    if audio is None or len(audio) == 0:
        return

    with _play_lock:
        if platform.system() == "Windows":
            tmp_path: Optional[str] = None
            try:
                tmp_path = _wav_bytes_path(audio, sample_rate)
                proc = subprocess.run(
                    [
                        sys.executable,
                        "-c",
                        "import sys, winsound; winsound.PlaySound(sys.argv[1], winsound.SND_FILENAME)",
                        tmp_path,
                    ],
                    timeout=60,
                    check=False,
                )
                if proc.returncode != 0:
                    logger.debug("winsound subprocess returned %s", proc.returncode)
            except Exception as exc:
                logger.error("Audio playback failed: %s", exc)
            finally:
                if tmp_path:
                    try:
                        os.unlink(tmp_path)
                    except OSError:
                        pass
            return

        # Non-Windows: use sounddevice directly.
        try:
            _require_sd()
            sd.play(
                audio, samplerate=sample_rate, device=output_device, blocking=blocking
            )
            if blocking:
                sd.wait()
        except Exception as exc:
            logger.error("Audio playback failed: %s", exc)


__all__ = ["list_devices", "default_device_info", "play_audio"]
