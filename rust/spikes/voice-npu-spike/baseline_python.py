"""baseline_python.py — three Python baselines for the Rust spike to compare against.

Run with the .venv-probe interpreter; it has onnxruntime-openvino + openvino + librosa.
This file does NOT live in Voice/ on purpose — it's spike-only scratch code.

Baselines:
1. `ort_cpu`: ONNX Runtime CPU EP on the same encoder_model.onnx the Rust spike loads.
2. `ort_openvino_npu`: ONNX Runtime + OpenVINO EP device=NPU on the same .onnx.
   (Mirrors what the Rust spike does, just from Python.)
3. `faster_whisper_cpu`: faster-whisper INT8 on the equivalent CTranslate2 weights —
   this is the Wylde user's currently-default Voice backend.
4. `openvino_ir_npu`: load the Wylde user's existing OpenVINO IR encoder with the static
   reshape, mirroring Voice/transcribe.py exactly.

Each baseline times an encoder-only forward pass with zero-input log-mel
spectrogram of shape (1, 80, 3000) — same dummy data as the Rust spike.
"""

from __future__ import annotations

import os
import statistics
import sys
import time
from pathlib import Path

import numpy as np

WHISPER_ENC = (
    Path.home()
    / ".cache/huggingface/hub/models--onnx-community--whisper-tiny.en/snapshots"
    / "2575352d61be1bf7225cf8f8b268a4678025fc58/onnx/encoder_model.onnx"
)
OV_IR_DIR = (
    Path.home()
    / ".cache/huggingface/hub/models--OpenVINO--whisper-tiny-fp16-ov/snapshots"
    / "5c7191644160f6e90833dfe3fd99a1f409cff976"
)

WARMUP = 1
RUNS = 3
SHAPE = (1, 80, 3000)


def time_runs(fn, label: str) -> dict:
    for _ in range(WARMUP):
        fn()
    samples = []
    for _ in range(RUNS):
        t = time.perf_counter()
        fn()
        samples.append((time.perf_counter() - t) * 1000.0)
    samples.sort()
    res = {
        "label": label,
        "min_ms": samples[0],
        "median_ms": samples[len(samples) // 2],
        "max_ms": samples[-1],
        "runs": samples,
    }
    print(
        f"  [{label}] min/median/max = "
        f"{res['min_ms']:.1f} / {res['median_ms']:.1f} / {res['max_ms']:.1f} ms"
    )
    return res


def bench_ort_cpu() -> dict:
    print("\n--- 1. ONNX Runtime CPU EP (Python) ---")
    import onnxruntime as ort

    sess = ort.InferenceSession(
        str(WHISPER_ENC), providers=["CPUExecutionProvider"]
    )
    input_data = np.zeros(SHAPE, dtype=np.float32)

    def run():
        sess.run(None, {"input_features": input_data})

    return time_runs(run, "ort_cpu_python")


def bench_ort_openvino_npu() -> dict:
    print("\n--- 2. ONNX Runtime + OpenVINO EP, device=NPU (Python) ---")
    # The onnxruntime-openvino wheel doesn't bundle openvino.dll; we need
    # to make sure Windows can resolve it for the providers_openvino DLL.
    import openvino  # noqa: F401  forces preload
    ov_libs = Path(openvino.__file__).parent / "libs"
    if hasattr(os, "add_dll_directory") and ov_libs.is_dir():
        os.add_dll_directory(str(ov_libs))
    import onnxruntime as ort

    providers = [
        (
            "OpenVINOExecutionProvider",
            {
                "device_type": "NPU",
                "reshape_input": "input_features[1,80,3000]",
                "disable_dynamic_shapes": True,
            },
        )
    ]
    sess = ort.InferenceSession(str(WHISPER_ENC), providers=providers)
    print("    active providers:", sess.get_providers())
    input_data = np.zeros(SHAPE, dtype=np.float32)

    def run():
        sess.run(None, {"input_features": input_data})

    return time_runs(run, "ort_openvino_npu_python")


def bench_openvino_ir_npu() -> dict:
    print("\n--- 3. OpenVINO IR direct (Python — mirrors Voice/transcribe.py) ---")
    import openvino as ov

    enc_path = OV_IR_DIR / "openvino_encoder_model.xml"
    if not enc_path.exists():
        print(f"    SKIP: IR not at {enc_path}")
        return {"label": "openvino_ir_npu_python", "skipped": True}

    core = ov.Core()
    model = core.read_model(str(enc_path))
    model.reshape({"input_features": [1, 80, 3000]})
    compiled = core.compile_model(model, "NPU", {"PERFORMANCE_HINT": "LATENCY"})
    input_data = np.zeros(SHAPE, dtype=np.float32)
    req = compiled.create_infer_request()

    def run():
        req.infer({"input_features": input_data})

    return time_runs(run, "openvino_ir_npu_python")


def bench_faster_whisper() -> dict:
    print("\n--- 4. faster-whisper CPU INT8 (the Wylde user's default Voice backend) ---")
    try:
        from faster_whisper import WhisperModel
    except ImportError:
        print("    SKIP: faster_whisper not in this venv")
        return {"label": "faster_whisper_cpu_int8", "skipped": True}

    model = WhisperModel("Systran/faster-whisper-tiny.en", device="cpu", compute_type="int8")
    audio = np.zeros(16000 * 30, dtype=np.float32)  # 30s silence

    def run():
        # End-to-end (decoder + encoder + tokens). Note: this is end-to-end
        # transcription, not encoder-only — the only fair Voice-baseline
        # comparison since the Wylde user's pipeline runs end-to-end.
        segments, _ = model.transcribe(
            audio, language="en", beam_size=1, vad_filter=False
        )
        for _ in segments:
            pass

    return time_runs(run, "faster_whisper_e2e_30s_silence")


def main() -> int:
    print("Whisper encoder latency baselines")
    print(f"  ONNX encoder : {WHISPER_ENC}")
    print(f"  OV IR dir    : {OV_IR_DIR}")
    print(f"  warmup       : {WARMUP}")
    print(f"  runs         : {RUNS}")

    if not WHISPER_ENC.exists():
        print(f"\nERROR: ONNX encoder not found at {WHISPER_ENC}", file=sys.stderr)
        return 1

    results = []
    for fn in (bench_ort_cpu, bench_ort_openvino_npu, bench_openvino_ir_npu, bench_faster_whisper):
        try:
            results.append(fn())
        except Exception as exc:
            print(f"  EXC: {exc!r}")
            results.append({"label": fn.__name__, "error": str(exc)})

    print("\n=== Summary ===")
    for r in results:
        if "error" in r:
            print(f"  {r['label']:30s} ERROR ({r['error'][:80]})")
        elif "skipped" in r:
            print(f"  {r['label']:30s} SKIPPED")
        else:
            print(
                f"  {r['label']:30s} median={r['median_ms']:.1f} ms  "
                f"(min={r['min_ms']:.1f}, max={r['max_ms']:.1f})"
            )

    return 0


if __name__ == "__main__":
    sys.exit(main())
