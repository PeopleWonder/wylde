"""Audio I/O surface — capture from mic, play to speakers.

A thin protocol layer over ``sounddevice`` so tests can substitute a
fake. The orchestrator only ever talks to the protocol; it doesn't
import sounddevice directly.

Design notes:

* **Capture returns raw int16 PCM bytes** — the harness's
  ``models.transcribe`` action expects that shape (and falls back to
  float32 when ``sample_dtype="float32"`` is set). Keeping the wire
  format int16 lets us stream over the pipe without doubling the
  byte count.
* **Playback consumes float32 PCM** — that's what
  ``models.synthesize`` returns from Kokoro / Piper. We don't
  sample-rate-convert here; the synthesizer's native SR is passed
  through.
* **No VAD in this module.** VAD is the recorder's concern, not
  Voice's — Voice asks for "record this many seconds" or "record
  until I say stop." the Wylde user's earlier I/O code had a VAD; we keep the
  capture surface simpler so the orchestrator's contract is explicit.

If sounddevice isn't available (no PortAudio, headless CI), the
default capture / playback raise ``AudioUnavailable``. Tests inject a
fake instead; the orchestrator handles ``AudioUnavailable`` by
recording the failure on the session and ending cleanly.
"""

from __future__ import annotations

import logging
import threading
import time
from typing import Any, Optional, Protocol

logger = logging.getLogger("wylde.voice.audio_io")


class AudioUnavailable(Exception):
    """Raised when no audio backend is available."""


class AudioCaptureProtocol(Protocol):
    """One-shot capture interface. ``capture(max_seconds)`` blocks until
    the session ends and returns raw int16 PCM bytes at ``sample_rate``."""

    def capture(self, *, max_seconds: float = 30.0) -> bytes: ...
    def stop(self) -> None: ...
    @property
    def sample_rate(self) -> int: ...


class AudioPlaybackProtocol(Protocol):
    """Blocking float32 playback. ``play(audio, sample_rate)`` returns
    when the audio has finished (or raises ``AudioUnavailable``)."""

    def play(self, audio: bytes, *, sample_rate: int) -> None: ...


# ── Default sounddevice-backed implementations ─────────────────────────


def _try_import_sounddevice() -> Any:
    try:
        import sounddevice as sd

        return sd
    except Exception:  # noqa: BLE001
        return None


class SounddeviceCapture:
    """Production capture backend. Lazy-imports sounddevice; raises
    :class:`AudioUnavailable` when PortAudio isn't installed."""

    def __init__(self, *, sample_rate: int = 16000) -> None:
        self._sample_rate = sample_rate
        self._stop_event = threading.Event()

    @property
    def sample_rate(self) -> int:
        return self._sample_rate

    def capture(self, *, max_seconds: float = 30.0) -> bytes:
        sd = _try_import_sounddevice()
        if sd is None:
            raise AudioUnavailable("sounddevice / PortAudio not installed")
        import numpy as np

        frames_per_chunk = max(160, int(self._sample_rate * 0.05))  # 50 ms
        deadline = time.monotonic() + max(0.0, max_seconds)
        chunks = []
        self._stop_event.clear()

        with sd.InputStream(
            samplerate=self._sample_rate,
            channels=1,
            dtype="int16",
            blocksize=frames_per_chunk,
        ) as stream:
            while not self._stop_event.is_set() and time.monotonic() < deadline:
                data, _ = stream.read(frames_per_chunk)
                # data is shape (n, 1), int16 — flatten + collect bytes.
                chunks.append(np.asarray(data, dtype=np.int16).tobytes())

        return b"".join(chunks)

    def stop(self) -> None:
        self._stop_event.set()


class SounddevicePlayback:
    """Production playback backend."""

    def play(self, audio: bytes, *, sample_rate: int) -> None:
        sd = _try_import_sounddevice()
        if sd is None:
            raise AudioUnavailable("sounddevice / PortAudio not installed")
        import numpy as np

        arr = np.frombuffer(audio, dtype=np.float32)
        if arr.size == 0:
            return
        sd.play(arr, samplerate=int(sample_rate))
        sd.wait()


# ── Fake implementations for tests ─────────────────────────────────────


class FakeCapture:
    """Test double — returns a canned bytes payload, records that
    capture was called."""

    def __init__(
        self, *, payload: bytes = b"\x00\x00" * 1600, sample_rate: int = 16000
    ) -> None:
        self._payload = payload
        self._sample_rate = sample_rate
        self.called_with_max_seconds: Optional[float] = None

    @property
    def sample_rate(self) -> int:
        return self._sample_rate

    def capture(self, *, max_seconds: float = 30.0) -> bytes:
        self.called_with_max_seconds = max_seconds
        return self._payload

    def stop(self) -> None:
        pass


class FakePlayback:
    """Test double — records playback calls."""

    def __init__(self) -> None:
        self.calls: list[dict[str, int]] = []

    def play(self, audio: bytes, *, sample_rate: int) -> None:
        self.calls.append({"bytes": len(audio), "sample_rate": sample_rate})


__all__ = [
    "AudioUnavailable",
    "AudioCaptureProtocol",
    "AudioPlaybackProtocol",
    "SounddeviceCapture",
    "SounddevicePlayback",
    "FakeCapture",
    "FakePlayback",
]
