# voice-npu-spike

Out-of-workspace Rust spike for Phase 10 of the Wylde Rust migration.
Question being answered: does `ort` 2.x with the OpenVINO EP let us run
Whisper inference on the Wylde user's Intel NPU from Rust, with the same
static-shape `[1, 80, 3000]` encoder workaround used by `Voice/transcribe.py`?

**Not** a workspace member. **Not** a final implementation. Findings live in
`docs/wylde-voice-npu-spike-findings.md`. After Phase 11 decides, this dir
gets deleted.

## Reproduce

The Rust binary in `target/release/` already has the 23 DLLs (ORT + OpenVINO)
co-located, so it can run standalone:

```
ORT_DYLIB_PATH=./target/release/onnxruntime.dll \
  ./target/release/voice-npu-spike.exe \
    --encoder ~/.cache/huggingface/hub/.../encoder_model.onnx \
    --device NPU
```

To re-run the Python baseline comparisons, recreate the scratch venv:

```
uv venv .venv-probe --python 3.11 --seed
.venv-probe/Scripts/python -m pip install \
  "openvino==2025.4.*" onnxruntime-openvino huggingface_hub librosa soundfile faster-whisper
.venv-probe/Scripts/python baseline_python.py
rm -rf .venv-probe
```

**Don't keep the venv around.** wylde_check walks it and chokes on the
hundreds of third-party rule violations inside `Lib/site-packages/`.
The DLLs we need are already copied into `target/release/` (which IS
excluded from wylde_check via the `/target/` rule).
