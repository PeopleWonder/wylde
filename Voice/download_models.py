"""download_models.py — first-run model bootstrap for Voice.

All model artefacts land in the standard HuggingFace cache
(``$HF_HUB_CACHE`` → ``$HUGGINGFACE_HUB_CACHE`` → ``$HF_HOME/hub`` →
``~/.cache/huggingface/hub``) so ``Core/harness/model_registry`` picks
them up via ``_hf_scanner.py``.

Two backends, mutually exclusive at install time:

* ``--backend cpu`` (default) — pulls a CTranslate2 build of Whisper for
  :mod:`faster_whisper`. Smaller, runs on any CPU, no compilation step.
* ``--backend npu`` — pulls the upstream ``openai/whisper-*`` weights and
  triggers a one-time export to OpenVINO IR (writes to a sibling
  ``ov-export/`` tree inside the HF cache).

The Kokoro TTS repo is the same in both modes; we additionally combine
the per-voice ``.bin`` files into a single ``voices.npz`` next to
``onnx/model.onnx`` so :mod:`synthesize` can load the bundle in one shot.

Usage::

    python -m Voice.download_models                # CPU Whisper + Kokoro
    python -m Voice.download_models --backend npu  # OpenVINO Whisper + Kokoro
    python -m Voice.download_models --whisper openai/whisper-base
"""

from __future__ import annotations

import argparse
import logging
import sys
from pathlib import Path
from typing import Iterable, List

import numpy as np

try:
    from Core.shared.logging_setup import configure_logging
except ImportError:
    configure_logging = None  # type: ignore[assignment]

if configure_logging is not None:
    configure_logging(service="wylde-voice")
log = logging.getLogger(__name__)


# Repo ids matching Voice/manifest.json.
DEFAULT_WHISPER_REPO = "openai/whisper-small"
KOKORO_REPO = "onnx-community/Kokoro-82M-v1.0-ONNX"

# Voice .bin files inside the Kokoro repo we combine into voices.npz.
KOKORO_VOICE_NAMES: List[str] = [
    "af",
    "af_alloy",
    "af_aoede",
    "af_bella",
    "af_heart",
    "af_jessica",
    "af_kore",
    "af_nicole",
    "af_nova",
    "af_river",
    "af_sarah",
    "af_sky",
    "am_adam",
    "am_echo",
    "am_eric",
    "am_fenrir",
    "am_liam",
    "am_michael",
    "am_onyx",
    "am_puck",
    "am_santa",
    "bf_alice",
    "bf_emma",
    "bf_isabella",
    "bf_lily",
    "bm_daniel",
    "bm_fable",
    "bm_george",
    "bm_lewis",
]


# --------------------------------------------------------------------------- #
# Whisper                                                                     #
# --------------------------------------------------------------------------- #


def fetch_whisper_cpu(repo_id: str, *, int8: bool) -> bool:
    """Pull ``repo_id`` for faster-whisper.

    ``faster_whisper.WhisperModel`` resolves the repo via
    ``huggingface_hub`` under the hood, so a successful construction
    means the weights are now in the standard cache.
    """
    log.info("Whisper (cpu / faster-whisper): downloading %s …", repo_id)
    try:
        from faster_whisper import WhisperModel

        compute_type = "int8" if int8 else "float32"
        WhisperModel(repo_id, device="cpu", compute_type=compute_type)
        log.info("Whisper (cpu): %s ready (compute_type=%s)", repo_id, compute_type)
        return True
    except Exception as exc:
        log.error("Whisper (cpu) prefetch failed: %s", exc)
        return False


def _ov_export_dir(repo_id: str) -> Path:
    """Where Voice/transcribe.py expects the OpenVINO IR to live."""
    from huggingface_hub import constants

    cache_root = Path(
        getattr(constants, "HUGGINGFACE_HUB_CACHE", "~/.cache/huggingface/hub")
    )
    return cache_root.expanduser() / "ov-export" / repo_id.replace("/", "--")


def fetch_whisper_npu(repo_id: str, *, int8: bool) -> bool:
    """Download + export ``repo_id`` to OpenVINO IR for the NPU backend."""
    out_dir = _ov_export_dir(repo_id)
    marker = out_dir / "openvino_encoder_model.xml"
    if marker.exists():
        log.info("Whisper (npu): %s already exported at %s", repo_id, out_dir)
        return True

    log.info("Whisper (npu): exporting %s → %s (3-10 min)…", repo_id, out_dir)
    try:
        from optimum.intel import OVModelForSpeechSeq2Seq
        from transformers import AutoProcessor

        out_dir.mkdir(parents=True, exist_ok=True)
        kwargs: dict = {"export": True}
        if int8:
            kwargs["load_in_8bit"] = True
        model = OVModelForSpeechSeq2Seq.from_pretrained(repo_id, **kwargs)
        model.save_pretrained(str(out_dir))
        processor = AutoProcessor.from_pretrained(repo_id)
        processor.save_pretrained(str(out_dir))
        size_mb = (
            sum(f.stat().st_size for f in out_dir.rglob("*") if f.is_file())
            / 1024
            / 1024
        )
        log.info("Whisper (npu): export done (%.0f MB at %s)", size_mb, out_dir)
        return True
    except Exception as exc:
        log.error("Whisper (npu) export failed: %s", exc)
        return False


# --------------------------------------------------------------------------- #
# Kokoro                                                                      #
# --------------------------------------------------------------------------- #


def _kokoro_allow_patterns(voices: Iterable[str]) -> List[str]:
    patterns = ["onnx/model.onnx"]
    patterns.extend(f"voices/{name}.bin" for name in voices)
    return patterns


def fetch_kokoro() -> bool:
    """Snapshot-download Kokoro into HF cache and assemble ``voices.npz``."""
    try:
        from huggingface_hub import snapshot_download

        log.info("Kokoro: snapshot-download %s …", KOKORO_REPO)
        snapshot = Path(
            snapshot_download(
                repo_id=KOKORO_REPO,
                allow_patterns=_kokoro_allow_patterns(KOKORO_VOICE_NAMES),
            )
        )
    except Exception as exc:
        log.error("Kokoro: snapshot_download failed: %s", exc)
        return False

    onnx_path = snapshot / "onnx" / "model.onnx"
    if not onnx_path.is_file():
        log.error("Kokoro: model.onnx missing at %s", onnx_path)
        return False
    log.info(
        "Kokoro: model.onnx at %s (%.1f MB)",
        onnx_path,
        onnx_path.stat().st_size / 1024 / 1024,
    )

    voices_npz = snapshot / "voices.npz"
    if voices_npz.is_file():
        log.info("Kokoro: voices.npz already present at %s", voices_npz)
        return True

    voice_arrays: dict = {}
    for name in KOKORO_VOICE_NAMES:
        bin_path = snapshot / "voices" / f"{name}.bin"
        if not bin_path.is_file():
            log.warning("Kokoro: missing voice file %s", bin_path)
            continue
        try:
            data = bin_path.read_bytes()
            voice_arrays[name] = np.frombuffer(data, dtype=np.float32).reshape(
                -1, 1, 256
            )
        except Exception as exc:
            log.warning("Kokoro: voice %s parse failed: %s", name, exc)

    if not voice_arrays:
        log.error("Kokoro: no voice arrays parsed; voices.npz not built")
        return False

    log.info("Kokoro: combining %d voices → %s", len(voice_arrays), voices_npz)
    np.savez(str(voices_npz), **voice_arrays)
    return True


# --------------------------------------------------------------------------- #
# CLI                                                                         #
# --------------------------------------------------------------------------- #


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Voice service first-run model downloader"
    )
    parser.add_argument(
        "--backend",
        choices=("cpu", "npu"),
        default="cpu",
        help="Whisper backend to install. Default 'cpu' uses faster-whisper; "
        "'npu' exports openai/whisper-* to OpenVINO IR for Intel NPU.",
    )
    parser.add_argument(
        "--whisper",
        default=DEFAULT_WHISPER_REPO,
        help=f"HF repo id for Whisper (default: {DEFAULT_WHISPER_REPO})",
    )
    parser.add_argument(
        "--no-int8",
        action="store_true",
        help="Skip int8 weight compression (larger, sometimes faster on x86 CPUs)",
    )
    parser.add_argument(
        "--skip-kokoro",
        action="store_true",
        help="Don't fetch the Kokoro TTS weights (useful when reinstalling STT only).",
    )
    args = parser.parse_args(argv)

    log.info("=" * 60)
    log.info("Voice — first-run model download")
    log.info("  whisper backend : %s", args.backend)
    log.info("  whisper repo    : %s", args.whisper)
    log.info("  int8            : %s", not args.no_int8)
    log.info("  fetch kokoro    : %s", not args.skip_kokoro)
    log.info("=" * 60)

    int8 = not args.no_int8
    if args.backend == "npu":
        stt_ok = fetch_whisper_npu(args.whisper, int8=int8)
    else:
        stt_ok = fetch_whisper_cpu(args.whisper, int8=int8)

    tts_ok = True if args.skip_kokoro else fetch_kokoro()

    log.info("=" * 60)
    log.info(
        "STT (%s, %s): %s", args.backend, args.whisper, "OK" if stt_ok else "FAILED"
    )
    log.info(
        "TTS (Kokoro):           %s",
        "SKIPPED" if args.skip_kokoro else ("OK" if tts_ok else "FAILED"),
    )
    log.info("=" * 60)

    return 0 if (stt_ok and tts_ok) else 1


if __name__ == "__main__":  # pragma: no cover
    sys.exit(main())
