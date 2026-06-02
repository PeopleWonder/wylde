//! `wylde-voice-bench` — measure the Whisper encoder on CPU vs NPU and
//! print the recommended shipped default (Slice 5 —
//! `docs/plans/voice-rust-port.md`).
//!
//! This is the harness the NPU spike asked for
//! (`docs/wylde-voice-npu-spike-findings.md` recommendation #5: "Build a
//! small bench script into wylde-voice that runs encoder both ways … and
//! picks"). It is **operator-run, not part of the service** — it never
//! registers a pipe action, so it adds no action-contract surface.
//!
//! It does no network I/O and downloads nothing: point it at an
//! already-exported Whisper encoder ONNX (the file Slice 4's model
//! bootstrap fetches, or a hand `optimum-cli export`). With no model it
//! prints usage and exits 0 — so a plain `cargo run` in CI just documents
//! itself instead of failing.
//!
//! Usage:
//!   set WYLDE_VOICE_STT_ENCODER_PATH=...\encoder_model.onnx
//!   cargo run --release -p wylde-voice --bin wylde-voice-bench               # CPU only
//!   cargo run --release -p wylde-voice --bin wylde-voice-bench --features openvino   # CPU + NPU
//!
//! The NPU path requires the `openvino` feature AND the 23-DLL bundle on
//! the search path (see `dll_bundle` + the spike findings); without either
//! it reports the NPU leg as unavailable and recommends CPU.

use std::path::PathBuf;
use std::time::Instant;

use wylde_voice::bench::{self, BenchConfig, BenchResult};
use wylde_voice::config::{Config, SttBackend};
use wylde_voice::dll_bundle;
use wylde_voice::transcribe::WhisperEncoder;

fn main() {
    // Same DLL discovery the service does, so `ORT_DYLIB_PATH` need not be
    // exported by hand when the bundle sits beside the bench binary.
    match dll_bundle::ensure_ort_dylib_path() {
        Ok(Some(p)) => eprintln!("[bench] ORT_DYLIB_PATH -> {}", p.display()),
        Ok(None) => eprintln!("[bench] ORT_DYLIB_PATH not set from bundle (relying on ort default)"),
        Err(e) => eprintln!("[bench] DLL discovery error: {e}"),
    }

    let cfg = Config::get();
    let encoder_path = match resolve_encoder_path(cfg) {
        Some(p) => p,
        None => {
            print_usage();
            return;
        }
    };

    if !encoder_path.is_file() {
        eprintln!(
            "[bench] encoder ONNX not found at {} — set WYLDE_VOICE_STT_ENCODER_PATH",
            encoder_path.display()
        );
        return;
    }

    let bench_cfg = BenchConfig::default();
    let mel = bench::zero_mel();
    eprintln!(
        "[bench] model={} encoder={} warmup={} iters={}",
        cfg.stt_model,
        encoder_path.display(),
        bench_cfg.warmup,
        bench_cfg.iters
    );

    // CPU leg — always runs.
    let cpu = match run_leg(&encoder_path, SttBackend::Cpu, cfg, &mel, &bench_cfg) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[bench] CPU leg failed: {e}");
            return;
        }
    };
    report(&cpu);

    // NPU leg — only if openvino compiled in; otherwise report why it's skipped.
    let npu = if bench::openvino_compiled() {
        match run_leg(&encoder_path, SttBackend::Npu, cfg, &mel, &bench_cfg) {
            Ok(r) => {
                report(&r);
                Some(r)
            }
            Err(e) => {
                eprintln!("[bench] NPU leg unavailable: {e}");
                None
            }
        }
    } else {
        eprintln!(
            "[bench] NPU leg skipped: built without `openvino` feature \
             (rebuild with --features openvino)"
        );
        None
    };

    let (recommended, why) = bench::recommend(&cpu.summary, npu.as_ref().map(|r| &r.summary));
    println!();
    println!("RECOMMENDED DEFAULT: {}", recommended.as_str());
    println!("REASON: {why}");
    println!(
        "To flip: set WYLDE_VOICE_WHISPER_BACKEND={} (NPU also needs --features openvino + DLL bundle)",
        recommended.as_str()
    );
}

fn resolve_encoder_path(cfg: &Config) -> Option<PathBuf> {
    cfg.stt_encoder_path_override.clone()
}

fn run_leg(
    encoder_path: &std::path::Path,
    backend: SttBackend,
    cfg: &Config,
    mel: &[f32],
    bench_cfg: &BenchConfig,
) -> Result<BenchResult, String> {
    let t0 = Instant::now();
    let encoder = WhisperEncoder::load(encoder_path, backend, &cfg.ov_device, &cfg.ov_cache_dir)
        .map_err(|e| e.to_string())?;
    let load_ms = t0.elapsed().as_secs_f64() * 1000.0;
    bench::bench_encoder(&encoder, mel, load_ms, bench_cfg).map_err(|e| e.to_string())
}

fn report(r: &BenchResult) {
    println!(
        "[{:>3}] device={:<14} load={:>7.1}ms  p50={:>6.1}ms p90={:>6.1}ms \
         mean={:>6.1}ms min={:>6.1}ms max={:>6.1}ms (n={})",
        r.backend.as_str(),
        r.device,
        r.load_ms,
        r.summary.p50_ms,
        r.summary.p90_ms,
        r.summary.mean_ms,
        r.summary.min_ms,
        r.summary.max_ms,
        r.iters,
    );
}

fn print_usage() {
    eprintln!(
        "wylde-voice-bench — Whisper encoder CPU-vs-NPU benchmark\n\
         \n\
         Set WYLDE_VOICE_STT_ENCODER_PATH to an exported encoder_model.onnx, then:\n\
         \x20 cargo run --release -p wylde-voice --bin wylde-voice-bench\n\
         \x20 cargo run --release -p wylde-voice --bin wylde-voice-bench --features openvino\n\
         \n\
         No model set — nothing to benchmark. Exiting cleanly."
    );
}
