"""Voice — audio I/O for Wylde.

Slim, in-process service that exposes:

* :func:`record.record_until_silence` — microphone capture with VAD-based stop.
* :func:`transcribe.transcribe` — Whisper STT (CPU default, optional NPU).
* :func:`synthesize.synthesize` — Kokoro TTS.
* :func:`device_manager.play_audio` — speaker playback.

Higher-layer behaviour (wake words, command dispatch, HTTP) lives elsewhere.
``run.start_voice()`` / ``run.stop_voice()`` give a process-wide handle for
host integrations (e.g. the harness Lifecycle launcher).
"""

from .run import start_voice, stop_voice  # noqa: F401

__all__ = ["start_voice", "stop_voice"]
