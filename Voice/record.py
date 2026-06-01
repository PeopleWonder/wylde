"""record.py — microphone capture with optional VAD-based silence stop.

Two entry points:

* :class:`MicrophoneStream` — low-level wrapper around a sounddevice
  ``InputStream``. Use it when you want raw chunks for your own consumer.
* :func:`record_until_silence` — convenience that opens a stream, runs an
  energy + zero-crossing-rate VAD, and returns a single float32 array
  spanning the speech segment.

The VAD is intentionally tiny and dependency-free (numpy only). It tracks
an asymmetric energy floor and rejects broadband noise (HVAC, fans) via a
zero-crossing-rate gate. Steady-state noise has ZCR > 0.35; voiced speech
sits at ~0.05–0.15. See ``_zcr`` for the exact computation.
"""

from __future__ import annotations

import logging
import queue
import threading
import time
from typing import Any, Optional, Tuple

import numpy as np

try:
    import sounddevice as sd
except Exception as exc:  # pragma: no cover - exercised only without portaudio
    sd = None
    _SOUNDDEVICE_IMPORT_ERROR: Optional[BaseException] = exc
else:
    _SOUNDDEVICE_IMPORT_ERROR = None

from .config import AudioConfig, VadConfig

logger = logging.getLogger(__name__)


# --------------------------------------------------------------------------- #
# VAD                                                                         #
# --------------------------------------------------------------------------- #

# IIR floor coefficients. Alpha: speed of rise toward louder background;
# 0.02 ≈ 1.6 s time constant at 512-sample/16 kHz chunks. Beta: speed of
# fall during quiet gaps. Slow beta keeps the floor near the HVAC average
# so brief gusts don't look like speech.
_FLOOR_ALPHA = 0.02
_FLOOR_BETA = 0.005

# ZCR gate. ZCR is amplitude-independent (computed on the normalised chunk).
# Voiced speech < 0.15; unvoiced fricatives < 0.30; broadband noise > 0.35.
_ZCR_MAX = 0.35


def _zcr(chunk: np.ndarray) -> float:
    """Zero-crossing rate of a normalised chunk, in ``[0, 0.5]``."""
    arr = chunk.astype(np.float32)
    peak = float(np.abs(arr).max())
    if peak < 1e-8:
        return 0.0
    arr = arr / peak
    return float(np.sum(np.abs(np.diff(np.sign(arr)))) / (2 * len(arr)))


class _Vad:
    """Energy-floor + ZCR speech detector. Stateful across a recording."""

    def __init__(self, threshold: float) -> None:
        self._threshold = float(threshold)
        self._energy_floor = 0.001

    def is_speech(self, chunk: np.ndarray) -> bool:
        if _zcr(chunk) > _ZCR_MAX:
            return False
        rms = float(np.sqrt(np.mean(chunk.astype(np.float32) ** 2)))
        if rms < self._energy_floor:
            self._energy_floor += (rms - self._energy_floor) * _FLOOR_BETA
        else:
            self._energy_floor += (rms - self._energy_floor) * _FLOOR_ALPHA
        floor = max(self._energy_floor, 1e-7)
        prob = 1.0 / (1.0 + np.exp(-1.5 * (rms / floor - 3.0)))
        return bool(prob >= self._threshold)


# --------------------------------------------------------------------------- #
# MicrophoneStream                                                            #
# --------------------------------------------------------------------------- #


class MicrophoneStream:
    """Bounded queue of float32 chunks captured from the microphone.

    The queue is sized at 128 chunks (~4 s at 512-sample/16 kHz). When the
    consumer falls behind we drop the *oldest* chunk rather than blocking
    the PortAudio callback (which would crash it). Always close the stream
    via :meth:`stop` or use it as a context manager.
    """

    def __init__(
        self,
        audio_cfg: AudioConfig,
        chunk_size: int = 512,
        device: Optional[int] = None,
    ) -> None:
        if sd is None:
            raise RuntimeError(
                "sounddevice is unavailable; install it (pip install sounddevice)."
            ) from _SOUNDDEVICE_IMPORT_ERROR
        self._cfg = audio_cfg
        self._chunk_size = chunk_size
        self._device = device if device is not None else audio_cfg.input_device
        self._queue: "queue.Queue[np.ndarray]" = queue.Queue(maxsize=128)
        self._stream: Optional["sd.InputStream"] = None

    # Context-manager sugar.
    def __enter__(self) -> "MicrophoneStream":
        self.start()
        return self

    def __exit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        self.stop()

    def start(self) -> "queue.Queue[np.ndarray]":
        if self._stream is not None:
            return self._queue

        def _callback(indata: Any, frames: int, time_info: Any, status: Any) -> None:
            if status:
                logger.debug("audio status: %s", status)
            chunk = indata[:, 0].copy()
            try:
                self._queue.put_nowait(chunk)
            except queue.Full:
                try:
                    self._queue.get_nowait()
                    self._queue.put_nowait(chunk)
                except (queue.Empty, queue.Full):
                    pass

        self._stream = sd.InputStream(
            samplerate=self._cfg.sample_rate,
            channels=1,
            dtype="float32",
            blocksize=self._chunk_size,
            device=self._device,
            callback=_callback,
        )
        self._stream.start()
        logger.info(
            "microphone stream started (rate=%d, chunk=%d, device=%s)",
            self._cfg.sample_rate,
            self._chunk_size,
            self._device,
        )
        return self._queue

    def stop(self) -> None:
        if self._stream is None:
            return
        try:
            self._stream.stop()
            self._stream.close()
        except Exception:
            logger.debug("microphone stream close raised", exc_info=True)
        finally:
            self._stream = None
            logger.info("microphone stream stopped")

    @property
    def queue(self) -> "queue.Queue[np.ndarray]":
        return self._queue

    @property
    def chunk_duration_s(self) -> float:
        return self._chunk_size / self._cfg.sample_rate


# --------------------------------------------------------------------------- #
# record_until_silence                                                        #
# --------------------------------------------------------------------------- #


def record_until_silence(
    audio_cfg: AudioConfig,
    vad_cfg: VadConfig,
    *,
    timeout_s: float = 15.0,
    chunk_size: int = 512,
    device: Optional[int] = None,
    stop_event: Optional[threading.Event] = None,
) -> Tuple[np.ndarray, bool]:
    """Capture mic audio until ``vad_cfg.silence_timeout_ms`` of silence.

    Returns ``(audio, timed_out)``. ``audio`` is a float32 mono numpy array
    at ``audio_cfg.sample_rate`` (empty if no speech was heard). ``timed_out``
    is ``True`` when ``timeout_s`` elapsed before silence was detected.

    ``stop_event`` lets a caller abort cleanly from another thread (the
    function returns whatever speech has been accumulated so far).
    """
    silence_timeout_s = vad_cfg.silence_timeout_ms / 1000.0
    deadline = time.monotonic() + timeout_s
    vad = _Vad(vad_cfg.threshold)

    speech_chunks: list = []
    pending_silence: list = []
    speech_started = False
    silence_s = 0.0
    timed_out = False

    with MicrophoneStream(audio_cfg, chunk_size=chunk_size, device=device) as stream:
        chunk_dur = stream.chunk_duration_s
        while True:
            if stop_event is not None and stop_event.is_set():
                break
            if time.monotonic() >= deadline:
                timed_out = True
                break
            try:
                chunk = stream.queue.get(timeout=0.1)
            except queue.Empty:
                continue

            if vad.is_speech(chunk):
                if not speech_started:
                    speech_started = True
                    logger.debug("VAD: speech start")
                # Mid-word pauses get folded back in.
                speech_chunks.extend(pending_silence)
                pending_silence.clear()
                silence_s = 0.0
                speech_chunks.append(chunk)
            else:
                if speech_started:
                    pending_silence.append(chunk)
                    silence_s += chunk_dur
                    if silence_s >= silence_timeout_s:
                        logger.debug("VAD: speech end (%d chunks)", len(speech_chunks))
                        break

    if not speech_chunks:
        return np.zeros(0, dtype=np.float32), timed_out
    return np.concatenate(speech_chunks).astype(np.float32), timed_out


__all__ = ["MicrophoneStream", "record_until_silence"]
