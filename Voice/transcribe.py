"""transcribe.py — Whisper speech-to-text with CPU/NPU backends.

Two backends, picked by ``stt.backend`` in ``config.yaml`` (or the
``WYLDE_VOICE_WHISPER_BACKEND`` env var):

* ``cpu`` — :mod:`faster_whisper`. The default. Pulls a CTranslate2 build
  of the model on first use (cached under the standard HuggingFace cache
  via ``huggingface_hub``). Runs on any CPU; no compilation step.
* ``npu`` — :mod:`optimum.intel` + OpenVINO with a static-shape encoder.
  The VPUX compiler aborts on Whisper's ``conv1`` when the input shape is
  dynamic ("Channels count -9223372036854775808 != 80"), so we read the
  exported encoder, reshape ``input_features`` to ``[1, 80, 3000]``, and
  reload via ``HETERO:NPU,CPU`` so the dynamic decoder lands on CPU. See
  ``_ensure_npu_static_encoder`` for the rebuild step.

Models live in the standard HuggingFace cache (``~/.cache/huggingface/hub``);
``download_models.py`` is the only place that materialises them. Both
backends pull from the same cache, so switching ``backend`` doesn't
re-download the weights.
"""

from __future__ import annotations

import logging
import shutil
import threading
from pathlib import Path
from typing import Any, Optional

import numpy as np

from .config import SttConfig

logger = logging.getLogger(__name__)


# --------------------------------------------------------------------------- #
# Public API                                                                  #
# --------------------------------------------------------------------------- #


class Transcriber:
    """Lazy-loaded Whisper transcriber.

    Construct cheaply, call :meth:`load` to materialise the backend, then
    feed audio via :meth:`transcribe`. The class is thread-safe — the
    underlying pipelines are not, so the public methods serialise on a
    single mutex. For high-throughput callers, run multiple ``Transcriber``
    instances rather than sharing one.
    """

    def __init__(self, cfg: SttConfig) -> None:
        self._cfg = cfg
        self._lock = threading.Lock()
        self._loaded = False
        self._backend_handle = None  # WhisperModel or HF pipeline
        self._actual_device = "unknown"

    # -- lifecycle -------------------------------------------------------- #

    def load(self) -> bool:
        """Materialise the configured backend. Returns ``True`` on success."""
        with self._lock:
            if self._loaded:
                return True
            try:
                if self._cfg.backend == "npu":
                    self._backend_handle = self._load_npu()
                else:
                    self._backend_handle = self._load_cpu()
                self._loaded = True
                logger.info(
                    "Transcriber loaded (backend=%s, device=%s, model=%s)",
                    self._cfg.backend,
                    self._actual_device,
                    self._cfg.model,
                )
                return True
            except Exception as exc:
                logger.error("Transcriber load failed: %s", exc)
                self._backend_handle = None
                self._loaded = False
                return False

    # -- inference -------------------------------------------------------- #

    def transcribe(self, audio: np.ndarray, language: Optional[str] = None) -> str:
        """Transcribe a float32 mono 16 kHz array. Returns the text (or "")."""
        if audio is None or len(audio) == 0:
            return ""
        if not self._loaded:
            return ""

        # Peak-normalise; both backends accept ``[-1, 1]`` floats.
        audio = audio.astype(np.float32, copy=False)
        peak = float(np.abs(audio).max())
        if peak > 0:
            audio = audio / peak

        lang = language or self._cfg.language or None
        with self._lock:
            handle = self._backend_handle
            backend = self._cfg.backend

        if handle is None:
            return ""

        try:
            if backend == "npu":
                return _transcribe_via_pipeline(handle, audio, lang)
            return _transcribe_via_faster_whisper(handle, audio, lang)
        except Exception as exc:
            logger.error("Transcribe failed: %s", exc)
            return ""

    @property
    def loaded(self) -> bool:
        return self._loaded

    @property
    def device(self) -> str:
        return self._actual_device

    @property
    def backend(self) -> str:
        return self._cfg.backend

    # -- backend loaders -------------------------------------------------- #

    def _load_cpu(self) -> Any:
        """faster-whisper backend — runs on any CPU."""
        from faster_whisper import WhisperModel

        # ``faster_whisper`` accepts both bare names ("small") and HF repo
        # ids. We pass the raw model id so users can pin specific weights.
        model_id = self._cfg.model
        compute_type = "int8" if self._cfg.load_in_8bit else "float32"
        model = WhisperModel(model_id, device="cpu", compute_type=compute_type)
        self._actual_device = "CPU"
        return model

    def _load_npu(self) -> Any:
        """OpenVINO/NPU backend with static-encoder fix."""
        from optimum.intel import OVModelForSpeechSeq2Seq
        from transformers import AutoProcessor, pipeline as hf_pipeline

        model_dir = _ensure_ov_export(self._cfg)
        npu_dir = _ensure_npu_static_encoder(model_dir)

        chain = []
        if npu_dir is not None:
            chain.append(("HETERO:NPU,CPU", npu_dir))
        chain += [("GPU", model_dir), ("CPU", model_dir)]

        ov_model = None
        last_err: Optional[BaseException] = None
        for device, load_dir in chain:
            try:
                logger.info("STT: loading OV model on %s…", device)
                ov_model = OVModelForSpeechSeq2Seq.from_pretrained(
                    str(load_dir),
                    device=device,
                    ov_config={"PERFORMANCE_HINT": "LATENCY"},
                )
                self._actual_device = device
                break
            except Exception as exc:
                logger.warning("STT: %s failed: %s", device, exc)
                last_err = exc
                continue

        if ov_model is None:
            raise RuntimeError(f"All OV devices failed for STT. Last error: {last_err}")

        processor = AutoProcessor.from_pretrained(str(model_dir))
        return hf_pipeline(
            "automatic-speech-recognition",
            model=ov_model,
            tokenizer=processor.tokenizer,
            feature_extractor=processor.feature_extractor,
            chunk_length_s=30,
            stride_length_s=5,
        )


# --------------------------------------------------------------------------- #
# Helpers                                                                     #
# --------------------------------------------------------------------------- #


def _transcribe_via_faster_whisper(
    model: Any, audio: np.ndarray, lang: Optional[str]
) -> str:
    segments, _info = model.transcribe(
        audio,
        language=lang,
        vad_filter=False,  # caller's record_until_silence already trimmed
        beam_size=1,  # latency over WER
    )
    text_parts = [seg.text for seg in segments]
    return "".join(text_parts).strip()


def _transcribe_via_pipeline(pipe: Any, audio: np.ndarray, lang: Optional[str]) -> str:
    gen_kwargs = {"language": lang} if lang else None
    result = pipe(audio.copy(), generate_kwargs=gen_kwargs, return_timestamps=False)
    return (result.get("text") or "").strip()


# --------------------------------------------------------------------------- #
# OpenVINO export / NPU static-shape rebuild                                  #
# --------------------------------------------------------------------------- #


def _ov_export_dir(cfg: SttConfig) -> Path:
    """Where the OpenVINO IR for this model lives.

    We co-locate it inside the HuggingFace hub cache under a sibling
    ``ov-export/<repo>`` directory so it travels with the original weights
    and gets cleaned up by ``huggingface-cli scan-cache --delete``.
    """
    from huggingface_hub import constants

    cache_root = Path(
        getattr(constants, "HUGGINGFACE_HUB_CACHE", "~/.cache/huggingface/hub")
    )
    return cache_root.expanduser() / "ov-export" / cfg.model.replace("/", "--")


def _ensure_ov_export(cfg: SttConfig) -> Path:
    """Export the HF Whisper repo to OpenVINO IR if not already cached."""
    out_dir = _ov_export_dir(cfg)
    marker = out_dir / "openvino_encoder_model.xml"
    if marker.exists():
        return out_dir

    from optimum.intel import OVModelForSpeechSeq2Seq
    from transformers import AutoProcessor

    out_dir.mkdir(parents=True, exist_ok=True)
    logger.info(
        "STT: exporting %s to OpenVINO IR (first run, can take a few minutes)…",
        cfg.model,
    )
    kwargs: dict = {"export": True}
    if cfg.load_in_8bit:
        kwargs["load_in_8bit"] = True
    model = OVModelForSpeechSeq2Seq.from_pretrained(cfg.model, **kwargs)
    model.save_pretrained(str(out_dir))
    processor = AutoProcessor.from_pretrained(cfg.model)
    processor.save_pretrained(str(out_dir))
    return out_dir


_NPU_COMPILE_CACHE_DIRS = ("encoder", "decoder")


def _ensure_npu_static_encoder(model_dir: Path) -> Optional[Path]:
    """Build ``<model_dir>-npu/`` with a static-shape encoder for VPUX.

    The VPUX compiler aborts on Whisper's ``conv1`` when the encoder input
    shape is dynamic. Reshape ``input_features`` to ``[1, 80, 3000]`` and
    save the rebuilt encoder alongside the unmodified decoder/config files.
    Returns ``None`` (and logs) on failure so the caller can fall through
    to GPU/CPU.
    """
    npu_dir = model_dir.parent / (model_dir.name + "-npu")
    if (npu_dir / "openvino_encoder_model.xml").exists():
        _purge_stale_compile_caches(npu_dir)
        return npu_dir

    try:
        from openvino import Core, save_model

        npu_dir.mkdir(parents=True, exist_ok=True)
        core = Core()
        enc_model = core.read_model(str(model_dir / "openvino_encoder_model.xml"))
        enc_model.reshape({"input_features": [1, 80, 3000]})
        save_model(enc_model, str(npu_dir / "openvino_encoder_model.xml"))
        logger.info("STT: rebuilt static encoder at %s", npu_dir)

        for src in model_dir.iterdir():
            if src.name.startswith("openvino_encoder_model"):
                continue
            if src.is_dir() and src.name in _NPU_COMPILE_CACHE_DIRS:
                continue
            dest = npu_dir / src.name
            if dest.exists():
                continue
            if src.is_dir():
                shutil.copytree(src, dest)
            else:
                shutil.copy2(src, dest)
        return npu_dir
    except Exception as exc:
        logger.warning("STT: NPU static encoder build failed: %s", exc)
        shutil.rmtree(npu_dir, ignore_errors=True)
        return None


def _purge_stale_compile_caches(npu_dir: Path) -> None:
    """Drop compile-cache subdirs older than the static-encoder XML.

    Earlier builds of this script copied the *dynamic* encoder's compile
    cache into the NPU dir; OpenVINO would then load that cache and crash
    in the VPUX compiler. Wipe anything older than the reshaped encoder so
    the next load recompiles from the static IR.
    """
    enc_xml = npu_dir / "openvino_encoder_model.xml"
    if not enc_xml.exists():
        return
    xml_mtime = enc_xml.stat().st_mtime
    for name in _NPU_COMPILE_CACHE_DIRS:
        cache_dir = npu_dir / name
        if not cache_dir.is_dir():
            continue
        try:
            if cache_dir.stat().st_mtime < xml_mtime:
                logger.warning("STT: purging stale NPU compile cache at %s", cache_dir)
                shutil.rmtree(cache_dir, ignore_errors=True)
        except OSError as exc:
            logger.warning("STT: could not inspect %s: %s", cache_dir, exc)


__all__ = ["Transcriber"]
