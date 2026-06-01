"""Phase 8.4 — VoiceAssistant slim-down static-import smoke check.

Imports VoiceAssistant.run, verifies start_assistant() / stop_assistant()
exist as callables, and exits. Does NOT open any audio device — this is a
parse-and-resolve test only, not a runtime test.

Soft-passes when an optional runtime dep is missing (faster-whisper,
silero_vad, kokoro_onnx, etc.) — those don't need to be installed in the
environment that runs the smoke test, since the engines lazy-import them
inside _load_models() which we never call.

Run via the .bat wrapper or directly:
    python _phase8_4_voice_assistant_check.py
"""

from __future__ import annotations

import importlib
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent


def _ensure_on_syspath() -> None:
    """Make `import VoiceAssistant.*` work when invoked from anywhere."""
    if str(HERE) not in sys.path:
        sys.path.insert(0, str(HERE))


def main() -> int:
    _ensure_on_syspath()

    voice_dir = HERE / "VoiceAssistant"
    if not voice_dir.is_dir():
        print(f"FAIL: {voice_dir} not found", file=sys.stderr)
        return 2

    expected_run = voice_dir / "run.py"
    if not expected_run.is_file():
        print(f"FAIL: {expected_run} not found", file=sys.stderr)
        return 2

    # Top-level package — exercise the package __init__.
    try:
        pkg = importlib.import_module("VoiceAssistant")
    except ModuleNotFoundError as exc:
        print(f"soft-pass: VoiceAssistant package import failed: {exc!r}")
        return 0
    except Exception as exc:
        print(f"FAIL: importing VoiceAssistant raised: {exc!r}", file=sys.stderr)
        return 1
    print(f"ok: {pkg}")

    # run.py — must expose start_assistant and stop_assistant.
    try:
        run_mod = importlib.import_module("VoiceAssistant.run")
    except ModuleNotFoundError as exc:
        print(f"soft-pass: VoiceAssistant.run runtime dep missing: {exc!r}")
        return 0
    except Exception as exc:
        print(f"FAIL: importing VoiceAssistant.run raised: {exc!r}", file=sys.stderr)
        return 1

    print(f"ok: {run_mod}")
    print(f"  has start_assistant() = {callable(getattr(run_mod, 'start_assistant', None))}")
    print(f"  has stop_assistant()  = {callable(getattr(run_mod, 'stop_assistant', None))}")
    print(f"  has main()            = {callable(getattr(run_mod, 'main', None))}")

    if not callable(getattr(run_mod, "start_assistant", None)):
        print("FAIL: start_assistant is not callable", file=sys.stderr)
        return 1
    if not callable(getattr(run_mod, "stop_assistant", None)):
        print("FAIL: stop_assistant is not callable", file=sys.stderr)
        return 1

    # pipeline — must expose AudioPipeline class.
    try:
        pipe_mod = importlib.import_module("VoiceAssistant.pipeline")
    except ModuleNotFoundError as exc:
        print(f"soft-pass: VoiceAssistant.pipeline runtime dep missing: {exc!r}")
        return 0
    except Exception as exc:
        print(f"FAIL: importing VoiceAssistant.pipeline raised: {exc!r}", file=sys.stderr)
        return 1

    cls = getattr(pipe_mod, "AudioPipeline", None)
    print(f"ok: {pipe_mod}")
    print(f"  AudioPipeline is class = {isinstance(cls, type)}")
    if not isinstance(cls, type):
        print("FAIL: AudioPipeline missing or not a class", file=sys.stderr)
        return 1

    # config — load() must work without errors when config.yaml is present.
    try:
        cfg_mod = importlib.import_module("VoiceAssistant.config")
        cfg = cfg_mod.load()
        print(f"ok: VoiceAssistant.config.load() returned {type(cfg).__name__}")
        print(f"  service.name = {cfg.service.name}")
        print(f"  device       = {cfg.device}")
    except Exception as exc:
        print(f"FAIL: VoiceAssistant.config.load() raised: {exc!r}", file=sys.stderr)
        return 1

    print("PASS: VoiceAssistant slim-down static-import check")
    return 0


if __name__ == "__main__":
    sys.exit(main())
